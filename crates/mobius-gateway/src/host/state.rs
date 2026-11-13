//! Gateway state and snapshots taken before releasing its locks.

use super::profile::gateway_ready;
use super::rejection::{Rejection, internal, session_limit};
use super::session::HostHandle;
use super::{SessionActivities, providers};

use crate::bots::BotStore;

use crate::config::{ConfigStore, CredentialStore, GatewayConfig};
use crate::wire::{AgentComposition, ReadyPayload};
use mobius::SessionId;
use mobius::backend::checkpoint::CheckpointStore;
use mobius::backend::session_files::SessionFileStore;
use mobius::middleware::memory::Memory;
use mobius::protocol::FrontendContribution;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
pub(super) struct GatewayState {
    pub(super) store: ConfigStore,
    pub(super) config: Arc<StdMutex<GatewayConfig>>,
    pub(super) usage: Arc<crate::config::UsageStore>,
    pub(super) credentials: Arc<CredentialStore>,
    pub(super) bots: Arc<BotStore>,
    pub(super) checkpoints: Arc<dyn CheckpointStore>,
    pub(super) memory: Arc<Memory>,
    pub(super) session_files: Arc<SessionFileStore>,
    pub(super) contributions: Vec<FrontendContribution>,
    // ponytail: one lock is enough for at most 32 tiny catalog writes.
    pub(super) catalog_lock: Arc<Mutex<()>>,
    pub(super) credential_catalog_gate: Arc<Mutex<()>>,
    pub(super) session_mutations: Arc<RwLock<()>>,
    pub(super) extension_mutations: Arc<Mutex<()>>,
    pub(super) discovery_gate: Arc<Mutex<()>>,
    pub(super) provider_epoch: Arc<AtomicU64>,
    pub(super) activities: SessionActivities,
    pub(super) provider_login: Arc<StdMutex<providers::ProviderLogins>>,
    pub(super) sessions: HashMap<SessionId, HostHandle>,
    pub(super) starting_sessions: Arc<StdMutex<HashMap<SessionId, String>>>,
    pub(super) idle_cleanup_tasks: Vec<JoinHandle<()>>,
}

pub(super) struct GatewayReadySnapshot {
    pub(super) store: ConfigStore,
    pub(super) configured_providers: BTreeMap<String, crate::config::ConfiguredProvider>,
    pub(super) bot_defaults: Option<crate::wire::VersionedAgentConfig<AgentComposition>>,
    pub(super) subagent_ceilings: mobius::middleware::subagents::SubagentCeilings,
    pub(super) extensions: Vec<crate::wire::ExtensionRecord>,
    pub(super) computer_view: crate::wire::ComputerView,
    pub(super) max_active_sessions: usize,
    pub(super) credentials: Arc<CredentialStore>,
    pub(super) credential_catalog_gate: Arc<Mutex<()>>,
    pub(super) bots: Arc<BotStore>,
    pub(super) checkpoints: Arc<dyn CheckpointStore>,
    pub(super) memory: Arc<Memory>,
    pub(super) contributions: Vec<FrontendContribution>,
    pub(super) activities: SessionActivities,
}

pub(super) async fn gateway_ready_after_unlock(
    state: tokio::sync::MutexGuard<'_, GatewayState>,
) -> std::result::Result<ReadyPayload, Rejection> {
    let snapshot = state.ready_snapshot()?;
    drop(state);
    gateway_ready(snapshot).await
}

pub(super) struct SessionStartGuard {
    pub(super) id: SessionId,
    pub(super) starting: Arc<StdMutex<HashMap<SessionId, String>>>,
}

impl Drop for SessionStartGuard {
    fn drop(&mut self) {
        // A poisoned start map is never read; the gateway reports it on the next start.
        if let Ok(mut starting) = self.starting.lock() {
            starting.remove(&self.id);
        }
    }
}

impl GatewayState {
    pub(super) fn config(
        &self,
    ) -> std::result::Result<std::sync::MutexGuard<'_, GatewayConfig>, Rejection> {
        crate::config::lock(&self.config).map_err(internal)
    }

    pub(super) fn ready_snapshot(&self) -> std::result::Result<GatewayReadySnapshot, Rejection> {
        let config = self.config()?;
        Ok(GatewayReadySnapshot {
            store: self.store.clone(), // rules-allow: the ready snapshot outlives the state lock
            configured_providers: config.configured_providers.clone(), // rules-allow: the ready snapshot outlives the config lock
            bot_defaults: config.bot_defaults.clone(), // rules-allow: the ready snapshot outlives the config lock
            subagent_ceilings: config.execution.subagent_ceilings,
            extensions: crate::harness::mobius::extensions::records(&config),
            computer_view: if !config.desktop_enabled {
                crate::wire::ComputerView::Unavailable
            } else if cfg!(target_os = "linux") {
                crate::wire::ComputerView::RemoteDesktop
            } else {
                crate::wire::ComputerView::EmbeddedBrowser
            },
            max_active_sessions: config.connections.active_sessions,
            credentials: Arc::clone(&self.credentials),
            credential_catalog_gate: Arc::clone(&self.credential_catalog_gate),
            bots: Arc::clone(&self.bots),
            checkpoints: Arc::clone(&self.checkpoints),
            memory: Arc::clone(&self.memory),
            contributions: self.contributions.clone(), // rules-allow: the ready snapshot outlives the state lock
            activities: Arc::clone(&self.activities),
        })
    }

    pub(super) fn reserve_start(
        &self,
        id: &SessionId,
        bot_id: &str,
    ) -> std::result::Result<SessionStartGuard, Rejection> {
        if self.resident_sessions()? >= self.session_capacity()? {
            return Err(session_limit(self.session_capacity()?));
        }
        let mut starting = self.starting_sessions()?;
        if starting.contains_key(id) {
            return Err(Rejection::new(
                "session_starting",
                "this chat is starting; retry shortly",
            ));
        }
        starting.insert(id.share(), bot_id.into());
        drop(starting);
        Ok(SessionStartGuard {
            id: id.share(),
            starting: Arc::clone(&self.starting_sessions),
        })
    }

    pub(super) fn resident_sessions(&self) -> std::result::Result<usize, Rejection> {
        Ok(self.sessions.len() + self.starting_sessions()?.len())
    }

    /// Locks the chats that are starting; a poisoned map fails the request.
    pub(super) fn starting_sessions(
        &self,
    ) -> std::result::Result<std::sync::MutexGuard<'_, HashMap<SessionId, String>>, Rejection> {
        self.starting_sessions
            .lock()
            .map_err(crate::poisoned("starting chats"))
            .map_err(internal)
    }

    pub(super) fn session_capacity(&self) -> std::result::Result<usize, Rejection> {
        Ok(self.config()?.connections.active_sessions)
    }
}
