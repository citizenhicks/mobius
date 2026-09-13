//! Per-chat agent ownership, event sequencing, replay, and authenticated operations.

mod approvals;
mod catalog;
mod chat;
mod conversations;
mod deletion;
mod extensions;
mod files;
mod git;
mod profile;
mod providers;
mod replay;
mod routines;
mod session;
mod ssh;

#[cfg(test)]
use routines::accept_routine_while_state_locked;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use chrono::Utc;
use mobius::agent::{AgentConfig, AgentSender};
use mobius::backend::checkpoint::{
    ActiveExecution, CheckpointStore, EventPageRequest, ExecutionOutcome, ExecutionRecord,
    JournalEvent, SessionPageRequest, SessionSummary, event_turn_page, sqlite::SqliteCheckpoint,
};
use mobius::backend::model::ModelRouter;
use mobius::backend::session_files::SessionFileStore;
use mobius::middleware::scratchpad::ScratchpadStore;
use mobius::middleware::{FrontendExtensions, Middleware as _};
use mobius::protocol::{
    Event, EventMsg, FrontendContribution, FrontendEvent, FrontendPreviewEvent, MessageAuthor,
    MessageSubmission, ModelStepContentPhase, Op, RenderedBlock, ReviewDecision, Submission,
};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::assembly::{BuiltAgent, assemble};
use crate::bots::{ActiveRoutineRun, BeginRun, BotStore};
use crate::chats::{ChatDelivery, ChatMessage, ChatRunOutcome, ChatStore};
use crate::computer_runtime::desktop::DesktopControl;
use crate::config::{
    ChatSpec, ConfigStore, CredentialStore, GatewayConfig,
    create_workspace_directory as create_workspace_directory_on_disk,
};
use crate::extensions::ExtensionStore;
use crate::provider_catalog::{
    configured_model_choices, configured_model_routes, provider_instances, provider_statuses,
};
use crate::sandbox::GatewaySandbox;
use crate::wire::{
    AgentComposition, GitDiffScope, MAX_FRAME_BYTES, ProfileSnapshot, ProviderConfig, ReadyPayload,
    RecordedEvent, RenderedEvent, RenderedPreview, RoutineRunStatus, RunStats, RunSummary,
    ServerFrame, ServerMessage, SessionActivity, SessionActivityState, SessionReadyPayload,
    SessionRecord, SessionRunGroup, SessionWidget, SshIdentityRecord, WorkspaceFileScope,
    validate_session_id,
};
use crate::{Error, Result};

use self::catalog::{
    SessionCatalogMetadata, activity_catalog, background_approvals, load_session_metadata,
    restore_pending_approval_activities, save_session_metadata, session_catalog,
    update_session_activity, validate_session_title,
};
use self::files::{
    WorkspaceFiles, WorkspaceRead, list as list_workspace_files, read as read_workspace_file,
    write as write_workspace_file,
};
use self::git::{
    approve_credential as approve_git_credential_on_host, diff as workspace_git_diff,
    probe_credential as probe_git_credential_on_host, status as git_status,
    switch_branch as switch_workspace_branch,
};
use self::profile::*;
use self::replay::*;
use self::session::*;
pub(crate) use self::session::{HostHandle, RealtimeModel};
use self::ssh::{generate as generate_ssh_identity_on_host, identities as ssh_identities_on_host};

const COMMAND_CAPACITY: usize = 128;
const BROADCAST_CAPACITY: usize = 512;
const REPLAY_CAPACITY: usize = 1024;
const REPLAY_LOAD_PAGE_SIZE: usize = 8;
const MAX_REPLAY_BYTES: usize = MAX_FRAME_BYTES;
const SESSION_PAGE_SIZE: usize = 100;
const MAX_SESSION_DELETE_ROOTS: usize = 1_024;
const RECENT_RUN_LIMIT: usize = 30;
pub(crate) const MAX_ACTIVE_SESSIONS: usize = 32;

type SessionActivities = Arc<Mutex<catalog::SessionCatalog>>;

/// Machine-wide chat registry. A session has at most one resident agent owner.
#[derive(Clone)]
pub(crate) struct GatewayHost {
    pub(crate) desktop: Arc<DesktopControl>,
    state: Arc<Mutex<GatewayState>>,
    events: broadcast::Sender<ServerFrame>,
}

struct GatewayState {
    store: ConfigStore,
    config: Arc<StdMutex<GatewayConfig>>,
    credentials: Arc<CredentialStore>,
    bots: Arc<BotStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    scratchpad: ScratchpadStore,
    session_files: SessionFileStore,
    chat_store: Arc<ChatStore>,
    contributions: Vec<FrontendContribution>,
    // ponytail: one lock is enough for at most 32 tiny catalog writes.
    catalog_lock: Arc<Mutex<()>>,
    session_mutations: Arc<RwLock<()>>,
    extension_mutations: Arc<Mutex<()>>,
    discovery_gate: Arc<Mutex<()>>,
    provider_epoch: Arc<AtomicU64>,
    activities: SessionActivities,
    provider_login: Arc<StdMutex<providers::ProviderLogins>>,
    sessions: HashMap<String, HostHandle>,
    starting_sessions: Arc<StdMutex<HashMap<String, String>>>,
    idle_cleanup_tasks: Vec<JoinHandle<()>>,
    chat_delivery_task: Option<JoinHandle<()>>,
}

pub(crate) struct HostSnapshot {
    pub(crate) ready: SessionReadyPayload,
    pub(crate) replay: Vec<ServerFrame>,
}

pub(crate) struct SessionHistoryPage {
    pub(crate) records: Vec<RecordedEvent>,
    pub(crate) next_before_sequence: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct Rejection {
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) fatal: bool,
}

impl GatewayHost {
    pub(crate) async fn start(
        store: ConfigStore,
        config: GatewayConfig,
        credentials: Arc<CredentialStore>,
        bots: Arc<BotStore>,
    ) -> Result<Self> {
        if let Some(defaults) = &config.bot_defaults {
            bots.seed_default(defaults)?;
        }
        let extensions = ExtensionStore::new(&store);
        extensions.prune(&config)?;
        extensions.verify_installed_snapshots(&config)?;
        if let Some(default) = &config.bot_defaults {
            extensions.resolve(&config, &default.config.extensions)?;
        }
        let models = configured_model_choices(&config, &store, &credentials)?;
        for bot in bots.bots()? {
            config.validate_provider_selection(&bot.config.config.provider)?;
            crate::middleware_manifest::validate_choices(&bot.config.config.middleware, &models)?;
            extensions.resolve(&config, &bot.config.config.extensions)?;
        }
        let discovery_gate = Arc::new(Mutex::new(()));
        let contributions = crate::assembly::run_discovery(Arc::clone(&discovery_gate), || {
            Ok(vec![
                mobius::middleware::extensions::Extensions::discover_installed([])?.frontend(),
            ])
        })
        .await?;
        let checkpoints: Arc<dyn CheckpointStore> =
            Arc::new(SqliteCheckpoint::new(store.checkpoints_path())?);
        let scratchpad = ScratchpadStore::new(Arc::clone(&checkpoints));
        let session_files = SessionFileStore::new(store.state_dir());
        let config = Arc::new(StdMutex::new(config));
        let (chat_store, deliveries) = ChatStore::new(store.state_dir(), Arc::clone(&bots))?;
        let chat_store = Arc::new(chat_store);
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        let activities = Arc::new(Mutex::new(catalog::SessionCatalog::default()));
        restore_pending_approval_activities(&checkpoints, &chat_store, &bots, &activities).await?;
        let host = Self {
            desktop: Arc::new(DesktopControl::default()),
            state: Arc::new(Mutex::new(GatewayState {
                store,
                config,
                credentials,
                bots,
                checkpoints,
                scratchpad,
                session_files,
                chat_store,
                contributions,
                catalog_lock: Arc::new(Mutex::new(())),
                session_mutations: Arc::new(RwLock::new(())),
                extension_mutations: Arc::new(Mutex::new(())),
                discovery_gate,
                provider_epoch: Arc::new(AtomicU64::new(0)),
                activities,
                provider_login: Arc::new(StdMutex::new(providers::ProviderLogins::default())),
                sessions: HashMap::new(),
                starting_sessions: Arc::default(),
                idle_cleanup_tasks: Vec::new(),
                chat_delivery_task: None,
            })),
            events,
        };
        host.reconcile_pending_bot_deletion()
            .await
            .map_err(|rejection| Error::Config(rejection.message))?;
        host.reconcile_deleted_chats()
            .await
            .map_err(|rejection| Error::Config(rejection.message))?;
        host.reconcile_chat_cancellations()
            .await
            .map_err(|rejection| Error::Config(rejection.message))?;
        let files = host.state.lock().await.session_files.clone();
        let events = host.events.clone();
        tokio::spawn(async move {
            if let Err(error) = files.cleanup_deleted_sessions().await {
                let _ = events.send(ServerFrame::new(ServerMessage::Error {
                    code: "session_cleanup".into(),
                    message: error.to_string(),
                    fatal: false,
                }));
            }
        });
        let chat_delivery_task = host.spawn_chat_deliveries(deliveries);
        host.state.lock().await.chat_delivery_task = Some(chat_delivery_task);
        Ok(host)
    }

    pub(crate) async fn shutdown(&self) {
        let (chat_delivery_task, cleanup_tasks) = {
            let mut state = self.state.lock().await;
            (
                state.chat_delivery_task.take(),
                std::mem::take(&mut state.idle_cleanup_tasks),
            )
        };
        if let Some(task) = chat_delivery_task {
            task.abort();
            let _ = task.await;
        }
        for task in cleanup_tasks {
            task.abort();
            let _ = task.await;
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
    }

    async fn begin_mutation(
        &self,
    ) -> std::result::Result<tokio::sync::OwnedRwLockReadGuard<()>, Rejection> {
        let (gate, bots) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.session_mutations),
                Arc::clone(&state.bots),
            )
        };
        let mutation = gate.read_owned().await;
        reject_pending_bot_deletion(&bots)?;
        Ok(mutation)
    }

    async fn begin_exclusive_mutation(
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

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ServerFrame> {
        self.events.subscribe()
    }

    pub(crate) async fn session_file_store(&self) -> SessionFileStore {
        self.state.lock().await.session_files.clone()
    }

    pub(crate) async fn ready(&self) -> std::result::Result<ReadyPayload, Rejection> {
        self.reconcile_pending_bot_deletion().await?;
        let state = self.state.lock().await;
        gateway_ready(&state).await
    }

    pub(crate) async fn contributions(
        &self,
    ) -> std::result::Result<Vec<FrontendContribution>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let (scratchpad, mut contributions) = self.contribution_state().await;
        let contribution = scratchpad
            .global_contribution()
            .await
            .map_err(scratchpad_error)?;
        contributions.push(contribution);
        Ok(contributions)
    }

    pub(crate) async fn submit_contribution(
        &self,
        operation: Op,
    ) -> std::result::Result<Vec<FrontendContribution>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let (scratchpad, mut contributions) = self.contribution_state().await;
        contributions.push(
            scratchpad
                .management_command(&operation)
                .await
                .map_err(scratchpad_error)?,
        );
        Ok(contributions)
    }

    async fn contribution_state(&self) -> (ScratchpadStore, Vec<FrontendContribution>) {
        let state = self.state.lock().await;
        (state.scratchpad.clone(), state.contributions.clone())
    }

    pub(crate) async fn sessions(&self) -> std::result::Result<Vec<SessionRecord>, Rejection> {
        let _access = self.begin_mutation().await?;
        let state = self.state.lock().await;
        state.visible_sessions().await
    }

    pub(crate) async fn probe_git_credential(
        &self,
        target: &str,
    ) -> std::result::Result<Option<String>, Rejection> {
        probe_git_credential_on_host(target).await
    }

    pub(crate) async fn approve_git_credential(
        &self,
        target: &str,
        username: &str,
        token: &str,
    ) -> std::result::Result<String, Rejection> {
        approve_git_credential_on_host(target, username, token).await
    }

    pub(crate) async fn bots(&self) -> std::result::Result<Vec<crate::wire::BotRecord>, Rejection> {
        let _access = self.begin_mutation().await?;
        self.state.lock().await.bots.bots().map_err(internal)
    }

    pub(crate) async fn create_bot(
        &self,
        name: &str,
        description: &str,
    ) -> std::result::Result<crate::wire::BotRecord, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        let defaults = state
            .config
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .bot_defaults
            .clone()
            .ok_or_else(|| Rejection {
                code: "gateway_setup_required",
                message: "configure a provider before creating a Bot".into(),
                fatal: false,
            })?;
        let config = defaults.config;
        validate_bot_config(&state, &config)?;
        let bot = state
            .bots
            .create_bot(name, description, config)
            .map_err(invalid_bot)?;
        let bots = state.bots.bots().map_err(internal)?;
        drop(state);
        self.broadcast_bots(&bots);
        Ok(bot)
    }

    pub(crate) async fn update_bot(
        &self,
        id: &str,
        expected_revision: u64,
        name: &str,
        description: &str,
        tint: crate::wire::ProviderTint,
        config: AgentComposition,
    ) -> std::result::Result<crate::wire::BotRecord, Rejection> {
        let (store, runtime_changed) = {
            let _access = self.begin_mutation().await?;
            let state = self.state.lock().await;
            validate_bot_config(&state, &config)?;
            let previous = state.bots.bot(id).map_err(invalid_bot)?;
            if previous.config.revision != expected_revision {
                return Err(Rejection {
                    code: "revision_conflict",
                    message: format!(
                        "Bot configuration revision is now {}",
                        previous.config.revision
                    ),
                    fatal: false,
                });
            }
            (
                state.store.clone(),
                previous.description != description || previous.config.config != config,
            )
        };
        // Installation can take minutes. Do not hold gateway locks or save the Bot yet.
        if runtime_changed {
            crate::computer_runtime::prepare(store.state_dir(), &config.middleware)
                .await
                .map_err(invalid_config)?;
        }
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        validate_bot_config(&state, &config)?;
        let previous = state.bots.bot(id).map_err(invalid_bot)?;
        let prepared = if runtime_changed {
            let mut candidate = previous.clone();
            candidate.description = description.into();
            candidate.config.config = config.clone();
            let gateway = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?
                .clone();
            Some(
                crate::assembly::prepare_bot(
                    &gateway,
                    candidate,
                    &state.store,
                    &state.credentials,
                    state.session_files.clone(),
                    state.provider_epoch.load(Ordering::Acquire),
                )
                .await
                .map_err(invalid_config)?,
            )
        } else {
            None
        };
        let bot = state
            .bots
            .update_bot(id, expected_revision, name, description, tint, config)
            .map_err(invalid_bot)?;
        if let Some(mut prepared) = prepared {
            prepared.bot = bot.clone();
            if let Some(previous) = state
                .bots
                .prepared
                .lock()
                .await
                .insert(id.into(), Arc::new(prepared))
            {
                previous.invalidate();
            }
        }
        let bots = state.bots.bots().map_err(internal)?;
        state.chat_store.retry_pending();
        drop(state);
        self.broadcast_bots(&bots);
        Ok(bot)
    }

    pub(crate) async fn ssh_identities(
        &self,
    ) -> std::result::Result<Vec<SshIdentityRecord>, Rejection> {
        tokio::task::spawn_blocking(ssh_identities_on_host)
            .await
            .map_err(internal)?
    }

    pub(crate) async fn generate_ssh_identity(
        &self,
    ) -> std::result::Result<(SshIdentityRecord, String), Rejection> {
        generate_ssh_identity_on_host().await
    }

    #[cfg(test)]
    pub(crate) async fn create_session(
        &self,
        workspace: &Path,
        bot_id: &str,
    ) -> std::result::Result<HostHandle, Rejection> {
        self.create_chat(workspace, &[bot_id.to_owned()], Some(bot_id))
            .await
    }

    async fn create_session_with_id(
        &self,
        workspace: &Path,
        bot_id: &str,
        session_id: String,
        catalog_visible: bool,
        origin_label: &str,
    ) -> std::result::Result<HostHandle, Rejection> {
        validate_session_id(&session_id).map_err(|_| invalid_session_id())?;
        let mutation = self.begin_mutation().await?;
        let state = self.ensure_capacity(self.state.lock().await).await?;
        let tls = state
            .config
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .tls
            .clone();
        let bot = state.bots.bot(bot_id).map_err(invalid_bot)?;
        if let Some(chat) = state
            .chat_store
            .chat_for_session(&session_id)
            .await
            .map_err(internal)?
            && (chat.session_id(&bot.id) != Some(session_id.as_str())
                || chat.workspace != workspace)
        {
            return Err(invalid_session_bot());
        }
        let starting = state.reserve_start(&session_id, &bot.id)?;
        let state_dir = state.store.state_dir().to_path_buf();
        let workspace = workspace.to_path_buf();
        drop(state);
        drop(mutation);
        let mut spec = tokio::task::spawn_blocking(move || {
            ChatSpec::for_bot(&workspace, &bot, &state_dir, tls.as_ref())
        })
        .await
        .map_err(internal)?
        .map_err(invalid_workspace)?;
        spec.catalog_visible = catalog_visible;
        let host = self
            .start_reserved_session(spec, starting, origin_label, true)
            .await?;
        if catalog_visible {
            self.broadcast_sessions().await?;
        }
        Ok(host)
    }

    async fn start_reserved_session(
        &self,
        spec: ChatSpec,
        starting: SessionStartGuard,
        origin_label: &str,
        cache: bool,
    ) -> std::result::Result<HostHandle, Rejection> {
        let state = self.state.lock().await;
        let start = HostHandle::start(
            state.store.clone(),
            Arc::clone(&state.config),
            spec,
            Arc::clone(&state.credentials),
            Arc::clone(&state.bots),
            Arc::clone(&state.checkpoints),
            state.scratchpad.clone(),
            state.session_files.clone(),
            Arc::clone(&state.chat_store),
            Arc::clone(&state.session_mutations),
            Arc::clone(&state.discovery_gate),
            Arc::clone(&self.desktop),
            Arc::clone(&state.provider_epoch),
            Arc::clone(&state.activities),
            self.events.clone(),
            starting.id.clone(),
            origin_label,
        );
        drop(state);
        let host = start.await.map_err(internal)?;
        let mut state = self.state.lock().await;
        if cache {
            state.sessions.insert(starting.id.clone(), host.clone());
        }
        drop(starting);
        Ok(host)
    }

    async fn cancel_execution(&self, session_id: &str) -> std::result::Result<(), Rejection> {
        let mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        let checkpoint = state
            .checkpoints
            .load(session_id)
            .await
            .map_err(internal)?
            .ok_or_else(unknown_session)?;
        let tls = state
            .config
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .tls
            .clone();
        let mut spec = ChatSpec::from_metadata(
            &checkpoint.metadata,
            &state.bots,
            state.store.state_dir(),
            tls.as_ref(),
        )
        .map_err(invalid_config)?;
        spec.catalog_visible = false;
        let starting = state.reserve_start(session_id, &spec.bot_id)?;
        let cancel = session::cancel_execution(
            state.store.clone(),
            Arc::clone(&state.config),
            spec,
            Arc::clone(&state.credentials),
            Arc::clone(&state.bots),
            Arc::clone(&state.checkpoints),
            state.scratchpad.clone(),
            state.session_files.clone(),
            Arc::clone(&state.chat_store),
            Arc::clone(&state.discovery_gate),
            Arc::clone(&self.desktop),
            Arc::clone(&state.provider_epoch),
            Arc::clone(&state.activities),
            session_id.into(),
        );
        drop(state);
        drop(mutation);
        let result = cancel.await.map_err(internal);
        drop(starting);
        result
    }

    async fn validate_chat_bot(
        &self,
        workspace: &Path,
        bot_id: &str,
    ) -> std::result::Result<(), Rejection> {
        let mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        let store = state.store.clone();
        let tls = state
            .config
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .tls
            .clone();
        let bot = state.bots.bot(bot_id).map_err(invalid_bot)?;
        let spec = ChatSpec::for_bot(workspace, &bot, store.state_dir(), tls.as_ref())
            .map_err(invalid_config)?;
        let prepare = session::prepare_agent(
            Arc::clone(&state.config),
            &spec,
            &store,
            Arc::clone(&state.credentials),
            Arc::clone(&state.bots),
            Arc::clone(&state.checkpoints),
            state.scratchpad.clone(),
            state.session_files.clone(),
            Arc::clone(&state.chat_store),
            Arc::clone(&state.discovery_gate),
            Arc::clone(&self.desktop),
            Uuid::new_v4().to_string(),
            "Chat",
            None,
            Arc::clone(&state.provider_epoch),
        );
        drop(state);
        drop(mutation);
        drop(prepare.await.map_err(internal)?);
        Ok(())
    }

    pub(crate) async fn create_workspace_directory(
        &self,
        parent: &Path,
        name: &str,
    ) -> std::result::Result<PathBuf, Rejection> {
        let (state_dir, tls) = {
            let state = self.state.lock().await;
            let config = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?;
            (state.store.state_dir().to_path_buf(), config.tls.clone())
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
        session_id: &str,
    ) -> std::result::Result<HostHandle, Rejection> {
        let host = self
            .open_session_with_cache(session_id, true)
            .await
            .map(|(host, _)| host)?;
        self.state
            .lock()
            .await
            .chat_store
            .notify_pending(host.session_id());
        Ok(host)
    }

    async fn open_session_with_cache(
        &self,
        session_id: &str,
        cache: bool,
    ) -> std::result::Result<(HostHandle, bool), Rejection> {
        validate_session_id(session_id).map_err(|_| invalid_session_id())?;
        let mutation = self.begin_mutation().await?;
        let mut state = self.state.lock().await;
        if let Some(chat) = state.chat_store.load(session_id).await.map_err(internal)? {
            if let Some(host) = state.cached_session(session_id)? {
                return Ok((host, false));
            }
            state = self.ensure_capacity(state).await?;
            if let Some(host) = state.cached_session(session_id)? {
                return Ok((host, false));
            }
            let host = self.chat_handle(&state, chat);
            if cache {
                state.sessions.insert(session_id.into(), host.clone());
            }
            return Ok((host, !cache));
        }
        if let Some(chat) = state
            .chat_store
            .chat_for_session(session_id)
            .await
            .map_err(internal)?
        {
            let bot_id = chat
                .participants
                .iter()
                .find(|participant| participant.session_id == session_id)
                .ok_or_else(invalid_session_bot)?
                .bot_id
                .clone();
            drop(state);
            drop(mutation);
            let host = Box::pin(self.open_session_with_cache(&chat.id, true))
                .await?
                .0;
            return Ok((host.participant(bot_id).await?, false));
        }
        drop(state);
        drop(mutation);
        self.open_execution_with_cache(session_id, cache).await
    }

    async fn open_execution_with_cache(
        &self,
        session_id: &str,
        cache: bool,
    ) -> std::result::Result<(HostHandle, bool), Rejection> {
        let mutation = self.begin_mutation().await?;
        let mut state = self.state.lock().await;
        if let Some(host) = state.cached_session(session_id)? {
            return Ok((host, false));
        }
        state.sessions.remove(session_id);
        state = self.ensure_capacity(state).await?;
        if let Some(host) = state.cached_session(session_id)? {
            return Ok((host, false));
        }
        let checkpoint = state
            .checkpoints
            .load(session_id)
            .await
            .map_err(internal)?
            .ok_or_else(unknown_session)?;
        let tls = state
            .config
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .tls
            .clone();
        let mut spec = ChatSpec::from_metadata(
            &checkpoint.metadata,
            &state.bots,
            state.store.state_dir(),
            tls.as_ref(),
        )
        .map_err(invalid_config)?;
        spec.catalog_visible = checkpoint.catalog_visible;
        let workspace = spec.workspace_info();
        let workspace_label = workspace.path.display().to_string();
        if checkpoint.session_context.workspace_id.as_deref() != Some(workspace.id.as_str())
            || checkpoint.session_context.workspace_label.as_deref()
                != Some(workspace_label.as_str())
        {
            return Err(invalid_session_workspace());
        }
        let starting = state.reserve_start(session_id, &spec.bot_id)?;
        drop(state);
        drop(mutation);
        let host = self
            .start_reserved_session(spec, starting, "mobius-gateway", cache)
            .await?;
        Ok((host, !cache))
    }

    fn spawn_chat_deliveries(
        &self,
        mut deliveries: mpsc::UnboundedReceiver<ChatDelivery>,
    ) -> JoinHandle<()> {
        let state = Arc::downgrade(&self.state);
        let events = self.events.clone();
        let desktop = Arc::clone(&self.desktop);
        tokio::spawn(async move {
            let Some(gateway_state) = state.upgrade() else {
                return;
            };
            let gateway = Self {
                state: gateway_state,
                events: events.clone(),
                desktop: Arc::clone(&desktop),
            };
            gateway
                .handle_chat_delivery(ChatDelivery::RetryPending)
                .await;
            drop(gateway);
            while let Some(delivery) = deliveries.recv().await {
                let Some(gateway_state) = state.upgrade() else {
                    return;
                };
                Self {
                    state: gateway_state,
                    events: events.clone(),
                    desktop: Arc::clone(&desktop),
                }
                .handle_chat_delivery(delivery)
                .await;
            }
        })
    }

    async fn handle_chat_delivery(&self, delivery: ChatDelivery) {
        let result = async {
            match delivery {
                ChatDelivery::Changed { chat_id, records } => {
                    let host = self
                        .state
                        .lock()
                        .await
                        .sessions
                        .get(&chat_id)
                        .filter(|host| host.is_alive())
                        .cloned();
                    if let Some(host) = host {
                        host.send(HostCommand::Publish { records }).await?;
                    }
                    self.broadcast_sessions().await?;
                }
                ChatDelivery::Pending { chat_id } => {
                    let host = match self.open_session_with_cache(&chat_id, true).await {
                        Ok((host, _)) => host,
                        // Capacity removal retries durable deliveries after termination.
                        Err(rejection) if rejection.code == "session_stopping" => return Ok(()),
                        Err(rejection) => return Err(rejection),
                    };
                    host.send(HostCommand::Dispatch).await?;
                }
                ChatDelivery::RetryPending => {
                    let chats = Arc::clone(&self.state.lock().await.chat_store);
                    for chat_id in chats.pending_chat_ids().await.map_err(internal)? {
                        chats.notify_pending(&chat_id);
                    }
                }
            }
            Ok::<_, Rejection>(())
        }
        .await;
        if let Err(rejection) = result {
            let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                code: "chat_delivery".into(),
                message: rejection.message,
                fatal: false,
            }));
        }
    }

    pub(crate) async fn reassign_session(
        &self,
        session_id: &str,
        bot_id: &str,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        {
            let state = self.state.lock().await;
            require_catalog_session(&state, session_id).await?;
            state.bots.bot(bot_id).map_err(internal)?;
        }
        drop(_mutation);
        let (host, _) = self.open_session_with_cache(session_id, true).await?;
        host.reassign_bot(bot_id.to_owned()).await?;
        self.broadcast_sessions().await
    }

    pub(crate) async fn rename_session(
        &self,
        session_id: &str,
        title: &str,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        let title = validate_session_title(title)?;
        self.update_session_metadata(session_id, |metadata| metadata.title = Some(title.into()))
            .await
    }

    pub(crate) async fn set_session_pinned(
        &self,
        session_id: &str,
        pinned: bool,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        self.update_session_metadata(session_id, |metadata| metadata.pinned = pinned)
            .await
    }

    async fn update_session_metadata(
        &self,
        session_id: &str,
        update: impl FnOnce(&mut catalog::SessionMetadata),
    ) -> std::result::Result<(), Rejection> {
        let (checkpoints, catalog_lock, groups) = {
            let state = self.state.lock().await;
            require_catalog_session(&state, session_id).await?;
            (
                Arc::clone(&state.checkpoints),
                Arc::clone(&state.catalog_lock),
                Arc::clone(&state.chat_store),
            )
        };
        let _catalog = catalog_lock.lock().await;
        if checkpoints
            .load(session_id)
            .await
            .map_err(internal)?
            .is_none()
            && groups.load(session_id).await.map_err(internal)?.is_none()
        {
            return Err(unknown_session());
        }
        let mut metadata = load_session_metadata(&checkpoints)
            .await
            .map_err(internal)?;
        update(metadata.entry(session_id.into()).or_default());
        save_session_metadata(&checkpoints, &metadata)
            .await
            .map_err(internal)?;
        drop(_catalog);
        self.broadcast_sessions().await
    }

    pub(crate) async fn profile(
        &self,
        include_provider_usage: bool,
    ) -> std::result::Result<ProfileSnapshot, Rejection> {
        let _access = self.begin_mutation().await?;
        let (mut profile, checkpoints, config, store, chats) = {
            let state = self.state.lock().await;
            let config = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?
                .clone();
            let profile = config.profile();
            (
                profile,
                Arc::clone(&state.checkpoints),
                config,
                state.store.clone(),
                state.chat_store.chats(false).await.map_err(internal)?,
            )
        };
        drop(_access);
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
            profile.recent_run_groups =
                recent_run_groups(recent_runs, &sessions, &chats, &metadata);
        }
        if include_provider_usage {
            profile.provider_usage = provider_usage(&config, &store).await.map_err(internal)?;
        }
        Ok(profile)
    }

    async fn broadcast_sessions(&self) -> std::result::Result<(), Rejection> {
        let state = self.state.lock().await;
        let sessions = state.visible_sessions().await?;
        let approvals = background_approvals(&state.activities).await;
        state.chat_store.retry_pending();
        drop(state);
        let _ = self.events.send(ServerFrame::new(ServerMessage::Sessions {
            request_id: None,
            sessions,
        }));
        let _ = self
            .events
            .send(ServerFrame::new(ServerMessage::BackgroundApprovals {
                approvals,
            }));
        Ok(())
    }

    fn broadcast_bots(&self, bots: &[crate::wire::BotRecord]) {
        let _ = self.events.send(ServerFrame::new(ServerMessage::Bots {
            request_id: None,
            bots: bots.to_vec(),
        }));
    }

    async fn ensure_capacity<'a>(
        &'a self,
        mut state: tokio::sync::MutexGuard<'a, GatewayState>,
    ) -> std::result::Result<tokio::sync::MutexGuard<'a, GatewayState>, Rejection> {
        if state.resident_sessions() < MAX_ACTIVE_SESSIONS {
            return Ok(state);
        }
        let candidates = state
            .sessions
            .iter()
            .filter(|(_, host)| host.is_unreferenced())
            .map(|(id, host)| (id.clone(), host.clone()))
            .collect::<Vec<_>>();
        for (id, host) in candidates {
            // The cache and this candidate must still be the only owners.
            if Arc::strong_count(&host.inner) != 2
                || !state
                    .sessions
                    .get(&id)
                    .is_some_and(|cached| Arc::ptr_eq(&cached.inner, &host.inner))
            {
                continue;
            }
            // Unreferenced actors have no queued or running admission commands.
            // Keep selection atomic, but let lifecycle shutdown re-enter the gateway.
            if host.request_stop_if_idle().await {
                drop(state);
                host.wait_terminated().await;
                state = self.state.lock().await;
                if state
                    .sessions
                    .get(&id)
                    .is_some_and(|cached| Arc::ptr_eq(&cached.inner, &host.inner))
                {
                    state.sessions.remove(&id);
                }
                state.chat_store.retry_pending();
                if state.resident_sessions() < MAX_ACTIVE_SESSIONS {
                    return Ok(state);
                }
            }
        }
        Err(Rejection {
            code: "session_limit",
            message: format!(
                "this gateway already has {MAX_ACTIVE_SESSIONS} connected or running chats"
            ),
            fatal: false,
        })
    }
}

async fn accepted_submission(
    checkpoints: &dyn CheckpointStore,
    session_id: &str,
    submission_id: &str,
) -> Result<bool> {
    // ponytail: restart dedup scans one private journal; add a submission index if long chats make recovery slow.
    if checkpoints
        .load(session_id)
        .await?
        .is_some_and(|checkpoint| {
            checkpoint
                .pending_messages
                .iter()
                .any(|message| message.id() == submission_id)
        })
    {
        return Ok(true);
    }
    let mut before_sequence = None;
    loop {
        let page = checkpoints
            .event_page(
                session_id,
                EventPageRequest {
                    before_sequence,
                    limit: REPLAY_CAPACITY,
                },
            )
            .await?;
        if page.events.iter().any(|record| {
            record.event.submission_id.as_deref() == Some(submission_id)
                && matches!(record.event.msg, EventMsg::Message(_))
        }) {
            return Ok(true);
        }
        let Some(next) = page.next_before_sequence else {
            return Ok(false);
        };
        before_sequence = Some(next);
    }
}

fn validate_bot_config(
    state: &GatewayState,
    config: &AgentComposition,
) -> std::result::Result<(), Rejection> {
    let gateway = state
        .config
        .lock()
        .map_err(|_| internal("gateway configuration lock is poisoned"))?;
    gateway
        .validate_provider_selection(&config.provider)
        .map_err(invalid_config)?;
    let models =
        configured_model_choices(&gateway, &state.store, &state.credentials).map_err(internal)?;
    crate::middleware_manifest::validate_choices(&config.middleware, &models)
        .map_err(invalid_config)?;
    ExtensionStore::new(&state.store)
        .resolve(&gateway, &config.extensions)
        .map(|_| ())
        .map_err(invalid_config)
}

fn validate_bot_workspace(
    state: &GatewayState,
    bot_id: &str,
    workspace: &Path,
) -> std::result::Result<(), Rejection> {
    let bot = state.bots.bot(bot_id).map_err(invalid_bot)?;
    let tls = state
        .config
        .lock()
        .map_err(|_| internal("gateway configuration lock is poisoned"))?
        .tls
        .clone();
    ChatSpec::for_bot(workspace, &bot, state.store.state_dir(), tls.as_ref())
        .map(|_| ())
        .map_err(invalid_workspace)
}

struct SessionStartGuard {
    id: String,
    starting: Arc<StdMutex<HashMap<String, String>>>,
}

impl Drop for SessionStartGuard {
    fn drop(&mut self) {
        self.starting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

impl GatewayState {
    fn cached_session(&self, id: &str) -> std::result::Result<Option<HostHandle>, Rejection> {
        match self.sessions.get(id) {
            Some(host) if host.is_alive() => Ok(Some(host.clone())),
            Some(host) if !host.inner.terminated.load(Ordering::Acquire) => Err(Rejection {
                code: "session_stopping",
                message: "this chat is stopping; retry shortly".into(),
                fatal: false,
            }),
            _ => Ok(None),
        }
    }

    async fn visible_sessions(&self) -> std::result::Result<Vec<SessionRecord>, Rejection> {
        let sessions = session_catalog(&self.checkpoints, &self.chat_store, &self.activities)
            .await
            .map_err(internal)?;
        Ok(sessions)
    }

    fn reserve_start(
        &self,
        id: &str,
        bot_id: &str,
    ) -> std::result::Result<SessionStartGuard, Rejection> {
        let mut starting = self
            .starting_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if starting.contains_key(id) {
            return Err(Rejection {
                code: "session_starting",
                message: "this chat is starting; retry shortly".into(),
                fatal: false,
            });
        }
        starting.insert(id.into(), bot_id.into());
        Ok(SessionStartGuard {
            id: id.into(),
            starting: Arc::clone(&self.starting_sessions),
        })
    }

    fn resident_sessions(&self) -> usize {
        self.sessions.len()
            + self
                .starting_sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
    }
}

async fn receive<T>(
    receiver: oneshot::Receiver<std::result::Result<T, Rejection>>,
) -> std::result::Result<T, Rejection> {
    receiver.await.map_err(|_| stopped())?
}

fn chat_message_submission(mut entry: ChatMessage) -> Submission {
    if let Some(reply) = entry.message.reply.take() {
        let text = format!(
            "Replying to this earlier message:\n\n> {}\n\n{}",
            reply.text.replace('\n', "\n> "),
            entry.message.text,
        );
        if text.len() <= mobius::protocol::MAX_MESSAGE_BYTES {
            entry.message.text = text;
        }
    }
    Submission {
        id: entry.id,
        op: Op::Message {
            message: entry.message,
        },
    }
}

fn stopped() -> Rejection {
    Rejection {
        code: "gateway_stopped",
        message: "the gateway host stopped".into(),
        fatal: true,
    }
}

fn reject_pending_bot_deletion(bots: &BotStore) -> std::result::Result<(), Rejection> {
    if bots.pending_bot_deletion().map_err(internal)?.is_some() {
        return Err(Rejection {
            code: "bot_deletion_recovery",
            message: "finish Bot deletion recovery before changing gateway state".into(),
            fatal: false,
        });
    }
    Ok(())
}

fn internal(error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "gateway_error",
        message: error.to_string(),
        fatal: false,
    }
}

fn invalid_config(error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "invalid_config",
        message: error.to_string(),
        fatal: false,
    }
}

fn bot_delete_rejection(code: &'static str, message: &str) -> Rejection {
    Rejection {
        code,
        message: message.into(),
        fatal: false,
    }
}

fn invalid_workspace(error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "invalid_workspace",
        message: error.to_string(),
        fatal: false,
    }
}

fn invalid_session_workspace() -> Rejection {
    Rejection {
        code: "invalid_session_workspace",
        message: "the requested session belongs to another workspace".into(),
        fatal: false,
    }
}

fn invalid_session_bot() -> Rejection {
    Rejection {
        code: "invalid_session_bot",
        message: "the requested session Bot identity does not match its durable owner".into(),
        fatal: false,
    }
}

fn unknown_session() -> Rejection {
    Rejection {
        code: "unknown_session",
        message: "the requested chat does not exist".into(),
        fatal: false,
    }
}

async fn require_catalog_session(
    state: &GatewayState,
    session_id: &str,
) -> std::result::Result<(), Rejection> {
    validate_session_id(session_id).map_err(|_| invalid_session_id())?;
    if state
        .chat_store
        .load(session_id)
        .await
        .map_err(internal)?
        .is_some()
    {
        return Ok(());
    }
    let checkpoint = state
        .checkpoints
        .load(session_id)
        .await
        .map_err(internal)?
        .ok_or_else(unknown_session)?;
    if !checkpoint.catalog_visible {
        return Err(unknown_session());
    }
    Ok(())
}

fn invalid_chat(error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "invalid_chat",
        message: error.to_string(),
        fatal: false,
    }
}

fn invalid_session_id() -> Rejection {
    Rejection {
        code: "invalid_session_id",
        message: "session ID must be 1–4096 bytes".into(),
        fatal: false,
    }
}

fn invalid_bot(error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "invalid_bot",
        message: error.to_string(),
        fatal: false,
    }
}

fn invalid_routine(error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "invalid_routine",
        message: error.to_string(),
        fatal: false,
    }
}

fn scratchpad_error(error: mobius::Error) -> Rejection {
    match error {
        mobius::Error::Tool(message) => Rejection {
            code: "invalid_scratchpad",
            message,
            fatal: false,
        },
        error => internal(error),
    }
}

#[cfg(test)]
mod tests;
