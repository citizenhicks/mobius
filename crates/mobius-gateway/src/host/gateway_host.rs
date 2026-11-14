//! Machine-wide gateway lifecycle, admission and chat ownership.

use super::activity::WorkActivity;
use super::catalog::{
    gateway_catalog, hidden_bot_session_catalog, load_session_metadata,
    restore_pending_approval_activities, save_session_metadata, session_catalog,
    validate_session_title,
};
use super::composition::validate_bot_config;
use super::memory::with_memory_contribution;
use super::profile::{
    gateway_ready, gateway_run_stats, gateway_session_summaries, provider_usage, recent_run_groups,
};
use super::publication::{GatewayUpdate, broadcast, send_catalog, send_event, send_warning};
use super::rejection::{
    Rejection, harness_unavailable, internal, invalid_bot, invalid_config, invalid_session_bot,
    invalid_session_workspace, invalid_workspace, memory_error, session_limit, unknown_session,
};
use super::replay::{FrameworkPresentation, SessionHistoryPage, durable_history_page};
use super::session::HostHandle;
use super::state::{GatewayState, SessionStartGuard, gateway_ready_after_unlock};
use super::{
    BROADCAST_CAPACITY, RECENT_RUN_LIMIT, catalog, deletion, live_chats, providers, telemetry,
};
use crate::bots::BotStore;
use crate::computer_runtime::desktop::DesktopControl;
use crate::computer_runtime::remote_desktop::RemoteDesktop;
use crate::config::{
    ChatSpec, ConfigStore, CredentialStore, GatewayConfig,
    create_workspace_directory as create_workspace_directory_on_disk,
};
use crate::harness::mobius::extensions::ExtensionStore;
use crate::harness::mobius::provider_catalog::configured_model_catalog;
use crate::wire::{
    HarnessConfig, ProfileSnapshot, ReadyPayload, RunStats, ServerMessage, SessionRecord,
    VersionedAgentConfig,
};
use crate::{Error, Result};
use futures_util::future::OptionFuture;
use mobius::SessionId;
use mobius::backend::checkpoint::sqlite::SqliteCheckpoint;
use mobius::backend::checkpoint::{CheckpointStore, SessionSummary};
use mobius::backend::session_files::SessionFileStore;
use mobius::middleware::memory::Memory;
use mobius::protocol::{FrontendContribution, Op};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, RwLock, broadcast};
/// Machine-wide chat registry. A session has at most one resident agent owner.
#[derive(Clone)]
pub(crate) struct GatewayHost {
    pub(crate) desktop: Arc<DesktopControl>,
    pub(crate) remote_desktop: Arc<RemoteDesktop>,
    pub(super) state: Arc<Mutex<GatewayState>>,
    pub(super) capacity_gate: Arc<Mutex<()>>,
    pub(super) storage_reads: Arc<telemetry::StorageMeasurements>,
    pub(super) events: broadcast::Sender<GatewayUpdate>,
    pub(super) work_activity: Arc<WorkActivity>,
    pub(crate) telemetry: Arc<crate::telemetry::Telemetry>,
}

/// How a chat request is admitted against session mutations.
pub(super) enum Admission {
    /// May start the chat, so remote execution must be available.
    Execute,
    /// Only reads durable state.
    Observe,
}

/// A chat that passed selection checks.
pub(super) enum AuthorizedChat {
    Resident(HostHandle),
    /// A Bot's persistent chat that has never started.
    Unstarted {
        bot_id: Arc<str>,
    },
    Stored(ChatSpec),
}

pub(crate) type HostAccess = Arc<dyn Fn() -> Result<GatewayHost> + Send + Sync>;

impl GatewayHost {
    pub(super) async fn publish_ready_after_unlock(
        &self,
        state: tokio::sync::MutexGuard<'_, GatewayState>,
    ) -> std::result::Result<ReadyPayload, Rejection> {
        let payload = gateway_ready_after_unlock(state).await?;
        send_event(
            &self.events,
            ServerMessage::Ready {
                payload: payload.clone(), // rules-allow: the broadcast and requester each own the ready payload
            },
        );
        Ok(payload)
    }

    pub(crate) async fn pending_upload_capacity(&self) -> Result<usize> {
        let state = self.state.lock().await;
        let config = crate::config::lock(&state.config)?;
        Ok(config.connections.pending_uploads)
    }

    pub(super) fn access(&self) -> HostAccess {
        let state = Arc::downgrade(&self.state);
        let desktop = Arc::clone(&self.desktop);
        let remote_desktop = Arc::clone(&self.remote_desktop);
        let capacity_gate = Arc::clone(&self.capacity_gate);
        let events = self.events.clone(); // rules-allow: the access closure owns an event sender
        let work_activity = Arc::clone(&self.work_activity);
        let telemetry = Arc::clone(&self.telemetry);
        let storage_reads = Arc::clone(&self.storage_reads);
        Arc::new(move || {
            Ok(Self {
                state: state
                    .upgrade()
                    .ok_or_else(|| Error::Config("gateway stopped".into()))?,
                desktop: Arc::clone(&desktop),
                remote_desktop: Arc::clone(&remote_desktop),
                capacity_gate: Arc::clone(&capacity_gate),
                events: events.clone(), // rules-allow: each upgraded host owns an event sender
                work_activity: Arc::clone(&work_activity),
                telemetry: Arc::clone(&telemetry),
                storage_reads: Arc::clone(&storage_reads),
            })
        })
    }

    pub(crate) async fn start(
        store: ConfigStore,
        config: GatewayConfig,
        credentials: Arc<CredentialStore>,
        bots: Arc<BotStore>,
    ) -> Result<Self> {
        if let Some(hook) = &config.telemetry.activity_hook {
            hook.validate_roots([store.state_dir()])?;
        }
        bots.set_voice_routes(crate::harness::mobius::provider_catalog::VoiceRoutes::new(
            &config.configured_providers,
            &store,
            &credentials,
        )?)?;
        if let Some(defaults) = &config.bot_defaults {
            bots.seed_default(defaults)?;
        }
        let extensions = ExtensionStore::new(&store);
        extensions.prune(&config)?;
        extensions.verify_installed_snapshots(&config)?;
        if let Some(default) = &config.bot_defaults {
            extensions.resolve(&config, &default.config.extensions)?;
        }
        let models = configured_model_catalog(&config)?;
        let records = bots.bots()?;
        for composition in records.iter().filter_map(|bot| bot.config.config.mobius()) {
            crate::config::validate_bot_compatibility(&config, composition, models.catalogs())?;
            extensions.resolve(&config, &composition.extensions)?;
        }
        let discovery_gate = Arc::new(Mutex::new(()));
        let contributions =
            crate::harness::mobius::assembly::run_discovery(Arc::clone(&discovery_gate), || {
                Ok(vec![
                    mobius::middleware::extensions::Extensions::installed_contribution()?,
                ])
            })
            .await?;
        let checkpoints: Arc<dyn CheckpointStore> =
            Arc::new(SqliteCheckpoint::new(store.checkpoints_path())?);
        let memory = Arc::new(Memory::new(
            Arc::clone(&checkpoints),
            store.state_dir().join("memory"),
        ));
        let session_files = Arc::new(SessionFileStore::new(store.state_dir(), None));
        let remote_desktop = Arc::new(RemoteDesktop::new(
            store.state_dir(),
            config.desktop_enabled,
            config.computer.clone(), // rules-allow: the remote desktop owns its settings while the gateway config keeps them
        ));
        let telemetry = Arc::new(crate::telemetry::Telemetry::new(
            &config.telemetry,
            store.state_dir(),
        ));
        let work_activity = Arc::new(WorkActivity::new());
        bots.storage
            .attach_telemetry_notify(&telemetry.notify, &work_activity);
        let usage = Arc::new(crate::config::UsageStore::open(store.usage_path())?);
        let config = Arc::new(StdMutex::new(config));
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        let activities = Arc::new(Mutex::new(catalog::SessionCatalog::default()));
        restore_pending_approval_activities(&checkpoints, &activities).await?;
        let host = Self {
            desktop: Arc::new(DesktopControl::default()),
            remote_desktop,
            state: Arc::new(Mutex::new(GatewayState {
                store,
                config,
                usage,
                credentials,
                bots,
                checkpoints,
                memory,
                session_files,
                contributions,
                catalog_lock: Arc::new(Mutex::new(())),
                credential_catalog_gate: Arc::new(Mutex::new(())),
                session_mutations: Arc::new(RwLock::new(())),
                extension_mutations: Arc::new(Mutex::new(())),
                discovery_gate,
                provider_epoch: Arc::new(AtomicU64::new(0)),
                activities,
                provider_login: Arc::new(StdMutex::new(providers::ProviderLogins::default())),
                sessions: HashMap::new(),
                starting_sessions: Arc::default(),
                idle_cleanup_tasks: Vec::new(),
            })),
            capacity_gate: Arc::new(Mutex::new(())),
            storage_reads: Arc::default(),
            events,
            work_activity,
            telemetry,
        };
        {
            let state = host.state.lock().await;
            deletion::recover_session_hook_closures(&state).await?;
        }
        host.reconcile_pending_bot_deletion()
            .await
            .map_err(|rejection| Error::Config(rejection.message.to_string()))?;
        host.recover_routine_outcomes().await?;
        let files = Arc::clone(&host.state.lock().await.session_files);
        let events = host.events.clone(); // rules-allow: the cleanup task owns an event sender
        tokio::spawn(async move {
            if let Err(error) = files.cleanup_deleted_sessions().await {
                send_warning(&events, "session_cleanup", error.to_string().into());
            }
        });
        Ok(host)
    }

    pub(crate) async fn shutdown(&self) {
        let cleanup_tasks = std::mem::take(&mut self.state.lock().await.idle_cleanup_tasks);
        for task in cleanup_tasks {
            task.abort();
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                tracing::warn!(%error, "idle session cleanup task failed");
            }
        }

        let residents = {
            let mut state = self.state.lock().await;
            state
                .sessions
                .drain()
                .map(|(_, host)| host)
                .collect::<Vec<_>>()
        };
        for host in residents {
            host.shutdown().await;
        }
        self.remote_desktop.shutdown().await;
    }

    pub(crate) async fn begin_mutation(
        &self,
    ) -> std::result::Result<tokio::sync::OwnedRwLockReadGuard<()>, Rejection> {
        self.remote_desktop
            .check_execution()
            .map_err(invalid_config)?;
        let access = self.begin_access().await?;
        self.remote_desktop
            .check_execution()
            .map_err(invalid_config)?;
        Ok(access)
    }

    // Observers still obey idle shutdown and deletion admission while execution is held.
    pub(crate) async fn begin_access(
        &self,
    ) -> std::result::Result<tokio::sync::OwnedRwLockReadGuard<()>, Rejection> {
        let (gate, bots) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.session_mutations),
                Arc::clone(&state.bots),
            )
        };
        // Admission serializes with exclusive session mutations.
        let mutation = gate.read_owned().await;
        reject_pending_bot_deletion(&bots)?;
        Ok(mutation)
    }

    pub(super) async fn begin_credential_mutation(&self) -> tokio::sync::OwnedMutexGuard<()> {
        // Return an owned guard without retaining the gateway-wide state lock while waiting.
        let gate = Arc::clone(&self.state.lock().await.credential_catalog_gate);
        gate.lock_owned().await
    }

    pub(super) async fn begin_exclusive_mutation(
        &self,
    ) -> std::result::Result<tokio::sync::OwnedRwLockWriteGuard<()>, Rejection> {
        let (gate, bots) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.session_mutations),
                Arc::clone(&state.bots),
            )
        };
        let mutation = gate.write_owned().await;
        reject_pending_bot_deletion(&bots)?;
        Ok(mutation)
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<GatewayUpdate> {
        self.events.subscribe()
    }

    pub(crate) async fn session_file_store(&self) -> Arc<SessionFileStore> {
        Arc::clone(&self.state.lock().await.session_files)
    }

    pub(crate) async fn ready(&self) -> std::result::Result<ReadyPayload, Rejection> {
        self.reconcile_pending_bot_deletion().await?;
        let _mutation = self.begin_access().await?;
        let snapshot = self.state.lock().await.ready_snapshot()?;
        gateway_ready(snapshot).await
    }

    pub(crate) async fn contributions(
        &self,
    ) -> std::result::Result<Vec<FrontendContribution>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let (memory, contributions) = self.contribution_state().await;
        with_memory_contribution(&memory, contributions)
            .await
            .map_err(memory_error)
    }

    pub(crate) async fn submit_contribution(
        &self,
        operation: Op,
    ) -> std::result::Result<Vec<FrontendContribution>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let (memory, mut contributions) = self.contribution_state().await;
        contributions.push(memory.manage(&operation).await.map_err(memory_error)?);
        Ok(contributions)
    }

    pub(super) async fn contribution_state(&self) -> (Arc<Memory>, Vec<FrontendContribution>) {
        let state = self.state.lock().await;
        (Arc::clone(&state.memory), state.contributions.clone()) // rules-allow: the response extends a copy of the gateway contributions
    }

    pub(crate) async fn sessions(&self) -> std::result::Result<Vec<SessionRecord>, Rejection> {
        let _access = self.begin_mutation().await?;
        // Catalog reads await storage after releasing GatewayState, retaining only these shared owners.
        let (checkpoints, activities) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.checkpoints),
                Arc::clone(&state.activities),
            )
        };
        session_catalog(&checkpoints, &activities)
            .await
            .map_err(internal)
    }

    pub(crate) async fn bots(&self) -> std::result::Result<Vec<crate::wire::BotRecord>, Rejection> {
        let _access = self.begin_mutation().await?;
        self.state.lock().await.bots.bots().map_err(internal)
    }

    pub(crate) async fn create_bot(
        &self,
        harness: crate::wire::HarnessKind,
        name: &str,
        description: &str,
    ) -> std::result::Result<crate::wire::BotRecord, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        let gateway = state.config()?;
        if harness != crate::wire::HarnessKind::Mobius {
            return Err(harness_unavailable(crate::harness::unavailable(harness)));
        }
        let mut composition = gateway
            .bot_defaults
            .as_ref()
            .map(|defaults| defaults.config.clone()) // rules-allow: the new Bot owns a copy of the configured default composition
            .ok_or_else(|| {
                Rejection::new(
                    "gateway_setup_required",
                    "configure a provider before creating a Bot",
                )
            })?;
        let models = crate::harness::mobius::provider_catalog::configured_model_choices(
            &gateway,
            &state.store,
            &state.credentials,
        )
        .map_err(internal)?;
        crate::harness::mobius::middleware_manifest::drop_unadvertised_choices(
            &mut composition.middleware,
            models.catalogs(),
        );
        drop(gateway);
        let config = HarnessConfig::Mobius(Arc::new(composition));
        let config = validate_bot_config(&state, config)?;
        let bot = state
            .bots
            .create_bot(name, description, config)
            .map_err(invalid_bot)?;
        drop(state);
        self.broadcast_bots();
        Ok(bot)
    }

    pub(crate) async fn update_bot(
        &self,
        id: &str,
        expected_revision: u64,
        identity: crate::bots::BotIdentity<'_>,
        config: HarnessConfig,
    ) -> std::result::Result<Arc<crate::wire::BotRecord>, Rejection> {
        let (state_dir, runtime_changed, config) = {
            let _access = self.begin_mutation().await?;
            let state = self.state.lock().await;
            let previous = state.bots.bot(id).map_err(invalid_bot)?;
            if previous.harness != config.kind() {
                return Err(Rejection::new(
                    "harness_change",
                    "a Bot keeps the harness it was created on",
                ));
            }
            let config = validate_bot_config(&state, config)?;
            if previous.config.revision != expected_revision {
                return Err(Rejection::new(
                    "revision_conflict",
                    format!(
                        "Bot configuration revision is now {}",
                        previous.config.revision
                    ),
                ));
            }
            (
                state.store.state_dir().to_path_buf(),
                !crate::harness::same_runtime(&previous, identity.description, config.config()),
                config.into_parts().0,
            )
        };
        // Installation can take minutes. Do not hold gateway locks or save the Bot yet.
        if runtime_changed && let HarnessConfig::Mobius(composition) = &config {
            crate::computer_runtime::prepare(
                &state_dir,
                &composition.middleware,
                self.remote_desktop.configuration(),
            )
            .await
            .map_err(invalid_config)?;
        }
        let mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        let previous = state.bots.bot(id).map_err(invalid_bot)?;
        let (config, approval) = validate_bot_config(&state, config)?.into_parts();
        let config = Arc::new(VersionedAgentConfig {
            revision: expected_revision
                .checked_add(1)
                .ok_or_else(|| invalid_bot("Bot revision overflow"))?,
            config,
        });
        let prepared = if runtime_changed
            && let (
                HarnessConfig::Mobius(composition),
                crate::harness::ApprovalMode::Mobius(settings),
            ) = (&config.config, &approval)
        {
            let mut candidate = previous;
            candidate.description = identity.description.into();
            candidate.config = Arc::clone(&config);
            let preparation = {
                let gateway = state.config()?;
                crate::harness::mobius::assembly::prepare_bot(
                    &gateway,
                    crate::bots::RuntimeBot {
                        record: Arc::new(candidate),
                        composition: Arc::clone(composition),
                        settings: Arc::clone(settings),
                    },
                    &state.store,
                    &state.credentials,
                    Arc::clone(&state.session_files),
                    state.provider_epoch.load(Ordering::Acquire),
                    Arc::clone(self.remote_desktop.configuration()),
                )
            };
            Some(preparation.await.map_err(invalid_config)?)
        } else {
            None
        };
        let bot = Arc::new(
            state
                .bots
                .update_bot(id, identity, config, approval)
                .map_err(invalid_bot)?,
        );
        if let Some(mut prepared) = prepared {
            prepared.bot = Arc::clone(&bot);
            state.bots.prepared.replace(prepared).await;
        }
        let residents: Vec<_> = state.sessions.values().cloned().collect(); // rules-allow: handles are shared channels, released before the chats rebind
        drop(state);
        drop(mutation);
        // Idle chats show the new settings (such as the approval policy) without waiting for a message.
        for host in &residents {
            host.refresh_bot();
        }
        self.broadcast_bots();
        Ok(bot)
    }

    pub(crate) async fn create_session(
        &self,
        workspace: &Path,
        bot_id: &str,
    ) -> std::result::Result<HostHandle, Rejection> {
        self.create_session_with_id(
            Some(workspace),
            bot_id,
            SessionId::generate(),
            true,
            "mobius-gateway",
        )
        .await
    }

    pub(crate) async fn hidden_bot_sessions(
        &self,
        bot_id: &str,
    ) -> std::result::Result<Vec<SessionRecord>, Rejection> {
        let _access = self.begin_mutation().await?;
        let (checkpoints, activities) = {
            let state = self.state.lock().await;
            state.bots.require_bot(bot_id).map_err(invalid_bot)?;
            (
                Arc::clone(&state.checkpoints),
                Arc::clone(&state.activities),
            )
        };
        hidden_bot_session_catalog(&checkpoints, &activities, bot_id)
            .await
            .map_err(internal)
    }

    pub(super) async fn create_session_with_id(
        &self,
        workspace: Option<&Path>,
        bot_id: &str,
        session_id: SessionId,
        catalog_visible: bool,
        origin_label: &str,
    ) -> std::result::Result<HostHandle, Rejection> {
        let mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        // Blocking workspace validation must not retain the configuration lock.
        let tls = state.config()?.tls.clone(); // rules-allow: blocking validation owns the TLS settings after the config lock drops
        let bot = state.bots.bot(bot_id).map_err(invalid_bot)?;
        let state_dir = state.store.state_dir().to_path_buf();
        let workspace = workspace.map(Path::to_path_buf);
        drop(state);
        let mut spec = tokio::task::spawn_blocking(move || match workspace {
            Some(workspace) => ChatSpec::for_bot(&workspace, &bot, &state_dir, tls.as_ref()),
            None => Ok(ChatSpec::persistent(&bot)),
        })
        .await
        .map_err(internal)?
        .map_err(invalid_workspace)?;
        spec.catalog_visible = catalog_visible;
        let capacity = self.ensure_capacity().await?;
        let state = self.state.lock().await;
        if let Some(host) = state
            .sessions
            .get(&session_id)
            .filter(|host| host.is_alive())
        {
            return Ok(HostHandle {
                inner: Arc::clone(&host.inner),
            });
        }
        let starting = state.reserve_start(&session_id, &spec.bot_id)?;
        drop(state);
        drop(mutation);
        let host = self
            .start_reserved_session(spec, starting, origin_label, true)
            .await?;
        drop(capacity);
        if catalog_visible {
            self.broadcast_sessions().await?;
        }
        Ok(host)
    }

    pub(super) async fn start_reserved_session(
        &self,
        spec: ChatSpec,
        starting: SessionStartGuard,
        origin_label: &str,
        cache: bool,
    ) -> std::result::Result<HostHandle, Rejection> {
        self.work_activity.mark();
        let (harness, session_mutations, activities) = {
            let state = self.state.lock().await;
            let harness = Arc::new(crate::harness::HarnessContext {
                store: state.store.clone(), // rules-allow: the chat actor owns its config store
                gateway: Arc::clone(&state.config),
                usage: Arc::clone(&state.usage),
                credentials: Arc::clone(&state.credentials),
                bots: Arc::clone(&state.bots),
                checkpoints: Arc::clone(&state.checkpoints),
                memory: Arc::clone(&state.memory),
                session_files: Arc::clone(&state.session_files),
                discovery_gate: Arc::clone(&state.discovery_gate),
                desktop: Arc::clone(&self.desktop),
                remote_desktop: Arc::clone(&self.remote_desktop),
                provider_epoch: Arc::clone(&state.provider_epoch),
                live_chats: Arc::new(live_chats::GatewayLiveChats(
                    Arc::downgrade(&self.state),
                    self.access(),
                )),
                host_access: self.access(),
            });
            (
                harness,
                Arc::clone(&state.session_mutations),
                Arc::clone(&state.activities),
            )
        };
        let host = HostHandle::start(
            harness,
            spec,
            session_mutations,
            activities,
            Arc::clone(&self.work_activity),
            self.events.clone(), // rules-allow: the chat actor owns an event sender
            starting.id.clone(), // rules-allow: the start guard keeps its id until the chat is registered
            origin_label,
        )
        .await
        .map_err(harness_unavailable)?;
        if let Err(error) = self.remote_desktop.check_execution() {
            host.shutdown().await;
            return Err(invalid_config(error));
        }
        let mut state = self.state.lock().await;
        if cache {
            let resident = HostHandle {
                inner: Arc::clone(&host.inner),
            };
            state.sessions.insert(starting.id.clone(), resident); // rules-allow: the start guard keeps its id until the chat is registered
        }
        // Release the reservation under the registry lock so capacity never counts the chat twice.
        drop(starting);
        drop(state);
        Ok(host)
    }

    pub(crate) async fn create_workspace_directory(
        &self,
        parent: &Path,
        name: &str,
    ) -> std::result::Result<PathBuf, Rejection> {
        let (state_dir, tls) = {
            let state = self.state.lock().await;
            let config = state.config()?;
            (state.store.state_dir().to_path_buf(), config.tls.clone()) // rules-allow: blocking creation owns the TLS settings after the config lock drops
        };
        let parent = parent.to_owned();
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || {
            create_workspace_directory_on_disk(&parent, &name, &state_dir, tls.as_ref())
        })
        .await
        .map_err(|error| internal(error.to_string()))?
        .map_err(invalid_workspace)
    }

    pub(crate) async fn open_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<HostHandle, Rejection> {
        self.open_session_with_cache(session_id, true)
            .await
            .map(|(host, _)| host)
    }

    pub(super) async fn open_session_with_cache(
        &self,
        session_id: &SessionId,
        cache: bool,
    ) -> std::result::Result<(HostHandle, bool), Rejection> {
        let (chat, mutation) = self.authorize_chat(session_id, Admission::Execute).await?;
        let spec = match chat {
            AuthorizedChat::Resident(host) => return Ok((host, false)),
            AuthorizedChat::Unstarted { bot_id } => {
                drop(mutation);
                let host = self
                    .create_session_with_id(
                        None,
                        &bot_id,
                        session_id.share(),
                        true,
                        "persistent chat",
                    )
                    .await?;
                return Ok((host, false));
            }
            AuthorizedChat::Stored(spec) => spec,
        };
        let capacity = self.ensure_capacity().await?;
        let mut state = self.state.lock().await;
        if let Some(host) = state.sessions.get(session_id)
            && host.is_alive()
        {
            return Ok((
                HostHandle {
                    inner: Arc::clone(&host.inner),
                },
                false,
            ));
        }
        state.sessions.remove(session_id);
        let starting = state.reserve_start(session_id, &spec.bot_id)?;
        drop(state);
        drop(mutation);
        let host = self
            .start_reserved_session(spec, starting, "mobius-gateway", cache)
            .await?;
        drop(capacity);
        Ok((host, !cache))
    }

    /// Applies every check selecting `session_id` makes, without starting it.
    pub(super) async fn authorize_chat(
        &self,
        session_id: &SessionId,
        admission: Admission,
    ) -> std::result::Result<(AuthorizedChat, tokio::sync::OwnedRwLockReadGuard<()>), Rejection>
    {
        let guard = match admission {
            Admission::Execute => self.begin_mutation().await?,
            Admission::Observe => self.begin_access().await?,
        };
        {
            let mut state = self.state.lock().await;
            if let Some(host) = state.sessions.get(session_id)
                && host.is_alive()
            {
                return Ok((
                    AuthorizedChat::Resident(HostHandle {
                        inner: Arc::clone(&host.inner),
                    }),
                    guard,
                ));
            }
            state.sessions.remove(session_id);
        }
        // Reopening awaits checkpoint and filesystem work without keeping GatewayState locked.
        let (checkpoints, config, bots, state_dir) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.checkpoints),
                Arc::clone(&state.config),
                Arc::clone(&state.bots),
                state.store.state_dir().to_path_buf(),
            )
        };
        let Some(checkpoint) = checkpoints.load(session_id).await.map_err(internal)? else {
            let bot = crate::bots::conversation_owner(session_id)
                .map(|bot_id| bots.find_bot(bot_id))
                .transpose()
                .map_err(internal)?
                .flatten()
                .ok_or_else(unknown_session)?;
            return Ok((AuthorizedChat::Unstarted { bot_id: bot.id }, guard));
        };
        let tls = crate::config::lock(&config).map_err(internal)?.tls.clone(); // rules-allow: blocking resolution owns the TLS settings after the config lock drops
        let metadata = checkpoint.metadata;
        let mut spec = tokio::task::spawn_blocking(move || {
            ChatSpec::from_metadata(&metadata, &bots, &state_dir, tls.as_ref())
        })
        .await
        .map_err(internal)?
        .map_err(invalid_config)?;
        spec.catalog_visible = checkpoint.catalog_visible;
        if checkpoint.session_context.owner_id != spec.bot_id {
            return Err(invalid_session_bot());
        }
        let workspace = spec.workspace_info();
        let workspace_label = workspace
            .as_ref()
            .map(|workspace| workspace.path.display().to_string());
        if checkpoint.session_context.workspace_id.as_deref()
            != workspace.as_ref().map(|workspace| workspace.id.as_str())
            || checkpoint.session_context.workspace_label.as_deref() != workspace_label.as_deref()
        {
            return Err(invalid_session_workspace());
        }
        Ok((AuthorizedChat::Stored(spec), guard))
    }

    /// One durable history page for a chat this client could select, read without
    /// loading the chat: a resident chat answers itself, a stopped one is read from
    /// its event journal with framework-rendered blocks only.
    pub(crate) async fn read_session_history(
        &self,
        session_id: &SessionId,
        before: Option<crate::wire::HistoryCursor>,
    ) -> std::result::Result<SessionHistoryPage, Rejection> {
        let (chat, access) = self.authorize_chat(session_id, Admission::Observe).await?;
        if let AuthorizedChat::Resident(host) = chat {
            drop(access);
            return host.history_page(before).await;
        }
        let checkpoints = Arc::clone(&self.state.lock().await.checkpoints);
        durable_history_page(
            checkpoints.as_ref(),
            session_id,
            &FrameworkPresentation,
            before,
        )
        .await
    }

    pub(crate) async fn reassign_session(
        &self,
        session_id: &SessionId,
        bot_id: &str,
    ) -> std::result::Result<(), Rejection> {
        let mutation = self.begin_mutation().await?;
        let (bot_id, checkpoints) = {
            let state = self.state.lock().await;
            let bot = state.bots.bot(bot_id).map_err(internal)?;
            (bot.id, Arc::clone(&state.checkpoints))
        };
        let summary = require_catalog_session(&checkpoints, session_id).await?;
        reject_persistent_session(&summary)?;
        drop(mutation);
        let (host, _) = self.open_session_with_cache(session_id, true).await?;
        host.reassign_bot(bot_id).await?;
        self.broadcast_sessions().await
    }

    pub(crate) async fn rename_session(
        &self,
        session_id: &SessionId,
        title: &str,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        let checkpoints = Arc::clone(&self.state.lock().await.checkpoints);
        let summary = require_catalog_session(&checkpoints, session_id).await?;
        reject_persistent_session(&summary)?;
        let title = validate_session_title(title)?;
        self.update_session_metadata(session_id, |metadata| metadata.title = Some(title.into()))
            .await?;
        let activities = Arc::clone(&self.state.lock().await.activities);
        let mut activities = activities.lock().await;
        activities.revision = activities.revision.wrapping_add(1);
        drop(activities);
        Ok(())
    }

    pub(crate) async fn set_session_pinned(
        &self,
        session_id: &SessionId,
        pinned: bool,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        self.update_session_metadata(session_id, |metadata| metadata.pinned = pinned)
            .await
    }

    pub(super) async fn update_session_metadata(
        &self,
        session_id: &SessionId,
        update: impl FnOnce(&mut catalog::SessionMetadata),
    ) -> std::result::Result<(), Rejection> {
        let (checkpoints, catalog_lock) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.checkpoints),
                Arc::clone(&state.catalog_lock),
            )
        };
        require_catalog_session(&checkpoints, session_id).await?;
        let catalog = catalog_lock.lock().await;
        if checkpoints
            .load(session_id)
            .await
            .map_err(internal)?
            .is_none()
        {
            return Err(unknown_session());
        }
        let mut metadata = load_session_metadata(&checkpoints)
            .await
            .map_err(internal)?;
        update(metadata.entry(session_id.share()).or_default());
        save_session_metadata(&checkpoints, &metadata)
            .await
            .map_err(internal)?;
        drop(catalog);
        self.broadcast_sessions().await
    }

    pub(crate) async fn profile(
        &self,
        include_provider_usage: bool,
    ) -> std::result::Result<ProfileSnapshot, Rejection> {
        let access = self.begin_mutation().await?;
        let (mut profile, checkpoints, usage_request) = {
            let state = self.state.lock().await;
            let config = state.config()?;
            (
                ProfileSnapshot {
                    user_name: crate::config::local_user_name(),
                    daily_usage: state.usage.daily_usage().map_err(internal)?,
                    provider_usage: Vec::new(),
                    run_stats: RunStats::default(),
                    recent_run_groups: Vec::new(),
                },
                Arc::clone(&state.checkpoints),
                include_provider_usage.then(|| provider_usage(&config, &state.store)),
            )
        };
        drop(access);
        let local = async {
            let sessions = gateway_session_summaries(&checkpoints)
                .await
                .map_err(internal)?;
            profile.run_stats = gateway_run_stats(&sessions).map_err(internal)?;
            let recent_runs = checkpoints
                .recent_executions(RECENT_RUN_LIMIT)
                .await
                .map_err(internal)?;
            if !recent_runs.is_empty() {
                let metadata = load_session_metadata(&checkpoints)
                    .await
                    .map_err(internal)?;
                profile.recent_run_groups = recent_run_groups(recent_runs, &sessions, &metadata);
            }
            Ok(())
        };
        // A local failure still wins at once and abandons the provider request; a provider
        // failure surfaces only after the local reads succeed.
        let usage = async { Ok(OptionFuture::from(usage_request).await) };
        let ((), usage) = tokio::try_join!(local, usage)?;
        if let Some(usage) = usage {
            profile.provider_usage = usage.map_err(internal)?;
        }
        Ok(profile)
    }

    pub(super) async fn broadcast_sessions(&self) -> std::result::Result<(), Rejection> {
        self.invalidate_storage_usage();
        let (checkpoints, activities) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.checkpoints),
                Arc::clone(&state.activities),
            )
        };
        let (sessions, approvals) = gateway_catalog(&checkpoints, &activities)
            .await
            .map_err(internal)?;
        send_catalog(&self.events, sessions, approvals);
        Ok(())
    }

    pub(super) fn broadcast_bots(&self) {
        broadcast(&self.events, GatewayUpdate::BotsChanged);
    }
}

impl GatewayHost {
    pub(super) async fn ensure_capacity(
        &self,
    ) -> std::result::Result<tokio::sync::MutexGuard<'_, ()>, Rejection> {
        let capacity = self.capacity_gate.lock().await;
        self.remote_desktop
            .check_execution()
            .map_err(invalid_config)?;
        for attempt in 0..2 {
            let candidates = {
                let state = self.state.lock().await;
                if state.resident_sessions()? < state.session_capacity()? {
                    return Ok(capacity);
                }
                state
                    .sessions
                    .iter()
                    .filter(|(_, host)| host.is_unreferenced())
                    .map(|(id, _)| id.clone()) // rules-allow: stop candidates are revisited after the state lock is released
                    .collect::<Vec<_>>()
            };
            for id in candidates {
                let Some(host) = ({
                    let mut state = self.state.lock().await;
                    state
                        .sessions
                        .get(&id)
                        .is_some_and(HostHandle::is_unreferenced)
                        .then(|| state.sessions.remove(&id))
                        .flatten()
                }) else {
                    continue;
                };
                if host.stop_if_idle().await {
                    let state = self.state.lock().await;
                    if state.resident_sessions()? < state.session_capacity()? {
                        return Ok(capacity);
                    }
                } else {
                    self.state.lock().await.sessions.entry(id).or_insert(host);
                }
            }
            if attempt == 0 {
                // A command can deliver its reply just before its ownership guard drops.
                tokio::task::yield_now().await;
            }
        }
        Err(session_limit(self.state.lock().await.session_capacity()?))
    }
}

pub(super) fn reject_pending_bot_deletion(bots: &BotStore) -> std::result::Result<(), Rejection> {
    if bots.has_pending_bot_deletion().map_err(internal)? {
        return Err(Rejection::new(
            "bot_deletion_recovery",
            "finish Bot deletion recovery before changing gateway state",
        ));
    }
    Ok(())
}

pub(super) async fn require_catalog_session(
    checkpoints: &Arc<dyn CheckpointStore>,
    session_id: &SessionId,
) -> std::result::Result<SessionSummary, Rejection> {
    let checkpoint = checkpoints
        .session_summary(session_id)
        .await
        .map_err(internal)?
        .ok_or_else(unknown_session)?;
    if !checkpoint.catalog_visible {
        return Err(unknown_session());
    }
    Ok(checkpoint)
}

pub(super) fn reject_persistent_session(
    summary: &SessionSummary,
) -> std::result::Result<(), Rejection> {
    if crate::bots::is_persistent(&summary.session_id, &summary.session_context.owner_id) {
        return Err(Rejection::new(
            "persistent_chat",
            "this conversation belongs permanently to its Bot",
        ));
    }
    Ok(())
}
