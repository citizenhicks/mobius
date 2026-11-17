//! Provider quota reads shared by every client for a short freshness window, through
//! credentials loaded once and kept until provider sign-ins change.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use mobius::backend::model::ModelTransportSettings;
use mobius::backend::model::provider::{ProviderAuth, ProviderCredential, provider};
use tokio::sync::Mutex;

use crate::Result;
use crate::config::{ConfigStore, GatewayConfig};
use crate::wire::ProviderUsage;

/// How long one quota read answers every client; quotas move slowly and each read is a
/// provider round trip.
const FRESH_FOR: Duration = Duration::from_secs(60);

/// What a quota read needs from the configuration, taken while it is locked.
pub(crate) struct UsageRequest {
    provider_ids: BTreeSet<String>,
    auth_path: PathBuf,
    transport: ModelTransportSettings,
    generation: u64,
}

#[derive(Default)]
pub(crate) struct ProviderUsageCache {
    /// Held across a read, so concurrent requests share one provider round trip.
    state: Mutex<State>,
    /// Bumped when provider sign-ins change; a read or credential from before is dropped.
    generation: AtomicU64,
}

#[derive(Default)]
struct State {
    generation: u64,
    fresh: Option<Fresh>,
    /// Loaded credentials by provider, each with its own HTTP client and token.
    credentials: BTreeMap<String, ProviderCredential>,
}

struct Fresh {
    read_at: Instant,
    usage: Arc<[ProviderUsage]>,
}

impl ProviderUsageCache {
    /// Takes the provider configuration and its cache epoch while the caller holds the
    /// configuration lock, before a newer provider setup can invalidate it.
    pub(crate) fn request(&self, config: &GatewayConfig, store: &ConfigStore) -> UsageRequest {
        UsageRequest {
            provider_ids: config
                .configured_providers
                .values()
                .map(|configured| configured.selection.provider.clone()) // rules-allow: the request outlives the configuration borrow
                .collect(),
            auth_path: store.provider_auth_path(),
            transport: config.model_transport,
            generation: self.generation.load(Ordering::Acquire),
        }
    }

    /// The fresh shared read, or a new read of every signed-in provider's quota.
    pub(crate) async fn get(&self, request: UsageRequest) -> Result<Arc<[ProviderUsage]>> {
        let mut state = self.state.lock().await;
        let generation = self.generation.load(Ordering::Acquire);
        if request.generation != generation {
            return Err(stale_request());
        }
        if state.generation != generation {
            *state = State {
                generation,
                ..State::default()
            };
        }
        if let Some(fresh) = state
            .fresh
            .as_ref()
            .filter(|fresh| fresh.read_at.elapsed() < FRESH_FOR)
        {
            let usage = Arc::clone(&fresh.usage);
            drop(state);
            return Ok(usage);
        }
        let mut reads = Vec::new();
        for provider_id in request.provider_ids {
            let ProviderAuth::Browser(auth) = provider(&provider_id)?.auth() else {
                continue;
            };
            if !auth
                .configured(&request.auth_path)
                .is_ok_and(|configured| configured)
            {
                continue;
            }
            if !state.credentials.contains_key(&provider_id) {
                match auth.load_with_transport(&request.auth_path, request.transport) {
                    Ok(credential) => {
                        state.credentials.insert(provider_id.clone(), credential); // rules-allow: the map and the read each name the provider
                    }
                    Err(error) => {
                        reads.push(failed(provider_id, &error));
                        continue;
                    }
                }
            }
            let Some(credential) = state.credentials.get(&provider_id) else {
                continue;
            };
            if let Some(read) = auth.usage_limits(credential) {
                reads.push(Box::pin(async move {
                    match read.await {
                        Ok(limits) => ProviderUsage {
                            provider: provider_id,
                            limits: Some(limits),
                            error: None,
                        },
                        Err(error) => failed_usage(provider_id, &error),
                    }
                }));
            }
        }
        let usage: Arc<[ProviderUsage]> = futures_util::future::join_all(reads).await.into();
        let current = self.generation.load(Ordering::Acquire);
        if generation != current {
            *state = State {
                generation: current,
                ..State::default()
            };
            drop(state);
            return Err(stale_request());
        }
        state.fresh = Some(Fresh {
            read_at: Instant::now(),
            usage: Arc::clone(&usage),
        });
        drop(state);
        Ok(usage)
    }

    /// Drops the shared read and loaded credentials after a provider sign-in change.
    pub(crate) fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

fn stale_request() -> crate::Error {
    crate::Error::Invalid(
        "Provider settings changed while reading usage. Refresh to try again.".into(),
    )
}

fn failed_usage(provider: String, error: &mobius::Error) -> ProviderUsage {
    ProviderUsage {
        provider,
        limits: None,
        error: Some(error.to_string()),
    }
}

fn failed(
    provider: String,
    error: &mobius::Error,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProviderUsage> + Send>> {
    let usage = failed_usage(provider, error);
    Box::pin(async move { usage })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_read_answers_until_invalidated() {
        let root = tempfile::TempDir::new().unwrap();
        let (store, config) = ConfigStore::initialize(
            root.path().join("gateway"),
            crate::config::DEFAULT_LISTEN,
            None,
        )
        .unwrap();
        let cache = ProviderUsageCache::default();
        let first = cache.get(cache.request(&config, &store)).await.unwrap();
        assert!(Arc::ptr_eq(
            &first,
            &cache.get(cache.request(&config, &store)).await.unwrap()
        ));
        let old_request = cache.request(&config, &store);
        cache.invalidate();
        assert!(
            cache.get(old_request).await.is_err(),
            "an old configuration must not seed the new cache"
        );
        assert!(!Arc::ptr_eq(
            &first,
            &cache.get(cache.request(&config, &store)).await.unwrap()
        ));
    }
}
