//! ChatGPT-authenticated Codex Responses provider.

use std::sync::Arc;

use self::auth::BROWSER_AUTH;
use self::auth::ChatGptAuth;
use super::openai::CATALOG;
use super::openai_socket::OpenAiSocket;
use super::provider::HostedWebSearch;
use super::provider::ProviderAuth;
use super::provider::ProviderBuildConfig;
use super::provider::ProviderDefinition;
use crate::Result;

#[path = "openai_codex_auth.rs"]
mod auth;

pub use self::auth::{BrowserLogin, DeviceLogin};

pub(super) static MANIFEST: std::sync::LazyLock<super::provider::ProviderMetadata> =
    std::sync::LazyLock::new(|| {
        crate::config::embedded(include_str!("openai_codex_provider.toml"))
    });
const PROVIDER_ID: &str = "openai_codex";

pub(super) fn provider() -> ProviderDefinition {
    ProviderDefinition::from_metadata(
        PROVIDER_ID,
        &MANIFEST,
        ProviderAuth::Browser(&BROWSER_AUTH),
        Some(&CATALOG),
        build_provider,
    )
    .with_image_input()
    .with_image_generation()
    .with_realtime_voices(&super::realtime::CODEX_VOICES)
    .with_native_custom_endpoints()
}

fn build_provider(config: ProviderBuildConfig) -> Result<Arc<dyn super::Model>> {
    let auth = config.credential.into_browser::<ChatGptAuth>(PROVIDER_ID)?;
    let base_url = config
        .base_url
        .as_deref()
        .unwrap_or(MANIFEST.base_url.as_str());
    let mut socket_url =
        reqwest::Url::parse(&format!("{}/responses", base_url.trim_end_matches('/')))
            .map_err(|_| crate::Error::Config("invalid Codex endpoint".into()))?;
    let scheme = if socket_url.scheme() == "https" {
        "wss"
    } else {
        "ws"
    };
    socket_url
        .set_scheme(scheme)
        .map_err(|_| crate::Error::Config("invalid Codex socket scheme".into()))?;
    let provider = OpenAiSocket::with_authorization(
        auth,
        base_url,
        socket_url.as_str(),
        config.model,
        config.http,
        config.transport,
    )?
    .with_service_tier(config.service_tier)
    .with_codex_realtime_voice(base_url)?
    .with_image_api(Some(&super::image_generation::IMAGE_APIS["codex"]));
    let provider = match config.reasoning_effort {
        Some(effort) => provider.with_reasoning_effort(effort)?,
        None => provider,
    };
    let provider = match config.web_search {
        HostedWebSearch::Off => provider,
        HostedWebSearch::Cached => provider.with_cached_web_search(),
        HostedWebSearch::Live => provider.with_web_search(),
    };
    Ok(Arc::new(provider))
}
