//! Per-chat agent ownership, event sequencing, replay, and authenticated operations.

mod catalog;
mod deletion;
mod extensions;
mod files;
mod git;
mod group;
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
use crate::computer_runtime::desktop::DesktopControl;
use crate::config::{
    ChatSpec, ConfigStore, CredentialStore, GatewayConfig,
    create_workspace_directory as create_workspace_directory_on_disk,
};
use crate::extensions::ExtensionStore;
use crate::groups::{GroupDelivery, GroupMessage, GroupRunOutcome, GroupStore};
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
    SessionCatalogMetadata, activity_catalog, background_approvals, hidden_bot_session_catalog,
    load_session_metadata, restore_pending_approval_activities, save_session_metadata,
    session_catalog, update_session_activity, validate_session_title,
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum GroupDeliveryAttempt {
    Submitted(String),
    Rejected(String),
}

impl GroupDeliveryAttempt {
    fn message_id(&self) -> &str {
        match self {
            Self::Submitted(message_id) | Self::Rejected(message_id) => message_id,
        }
    }
}

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
    group: Arc<GroupStore>,
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
    group_delivery_task: Option<JoinHandle<()>>,
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
        let (group, deliveries) = GroupStore::new(store.state_dir(), Arc::clone(&bots))?;
        let group = Arc::new(group);
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        let activities = Arc::new(Mutex::new(catalog::SessionCatalog::default()));
        restore_pending_approval_activities(&checkpoints, &activities).await?;
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
                group,
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
                group_delivery_task: None,
            })),
            events,
        };
        host.reconcile_pending_bot_deletion()
            .await
            .map_err(|rejection| Error::Config(rejection.message))?;
        host.reconcile_deleted_group_chats()
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
        let group_delivery_task = host.spawn_group_deliveries(deliveries);
        host.state.lock().await.group_delivery_task = Some(group_delivery_task);
        Ok(host)
    }

    pub(crate) async fn shutdown(&self) {
        let (group_delivery_task, cleanup_tasks) = {
            let mut state = self.state.lock().await;
            (
                state.group_delivery_task.take(),
                std::mem::take(&mut state.idle_cleanup_tasks),
            )
        };
        if let Some(task) = group_delivery_task {
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
        state.group.retry_pending();
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

    pub(crate) async fn create_session(
        &self,
        workspace: &Path,
        bot_id: &str,
    ) -> std::result::Result<HostHandle, Rejection> {
        self.create_session_with_id(
            workspace,
            bot_id,
            Uuid::new_v4().to_string(),
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
        let state = self.state.lock().await;
        state.bots.bot(bot_id).map_err(invalid_bot)?;
        hidden_bot_session_catalog(&state.checkpoints, &state.activities, bot_id)
            .await
            .map_err(internal)
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
        let mut state = self.state.lock().await;
        state.ensure_capacity().await?;
        let tls = state
            .config
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .tls
            .clone();
        let bot = state.bots.bot(bot_id).map_err(invalid_bot)?;
        if let Some(chat_id) = crate::groups::participant_chat_id(&session_id) {
            let chat = state
                .group
                .load(chat_id)
                .await
                .map_err(internal)?
                .ok_or_else(unknown_session)?;
            if !chat.member_bot_ids.contains(&bot.id) {
                return Err(invalid_session_bot());
            }
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
            Arc::clone(&state.group),
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
        let bot_id = host.bot_id().await?;
        self.state.lock().await.group.notify_pending(&bot_id);
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
        if let Some(host) = state.sessions.get(session_id)
            && host.is_alive()
        {
            return Ok((host.clone(), false));
        }
        state.sessions.remove(session_id);
        state.ensure_capacity().await?;
        if let Some(chat) = state.group.load(session_id).await.map_err(internal)? {
            let host = self.group_handle(&state, chat);
            if cache {
                state.sessions.insert(session_id.into(), host.clone());
            }
            return Ok((host, !cache));
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
        let starting = state.reserve_start(session_id, &checkpoint.session_context.bot_id)?;
        let bots = Arc::clone(&state.bots);
        let state_dir = state.store.state_dir().to_path_buf();
        drop(state);
        drop(mutation);
        let metadata = checkpoint.metadata;
        let mut spec = tokio::task::spawn_blocking(move || {
            ChatSpec::from_metadata(&metadata, &bots, &state_dir, tls.as_ref())
        })
        .await
        .map_err(internal)?
        .map_err(invalid_config)?;
        spec.catalog_visible = checkpoint.catalog_visible;
        if checkpoint.session_context.bot_id != spec.bot_id {
            return Err(invalid_session_bot());
        }
        let workspace = spec.workspace_info();
        let workspace_label = workspace.path.display().to_string();
        if checkpoint.session_context.workspace_id.as_deref() != Some(workspace.id.as_str())
            || checkpoint.session_context.workspace_label.as_deref()
                != Some(workspace_label.as_str())
        {
            return Err(invalid_session_workspace());
        }
        let host = self
            .start_reserved_session(spec, starting, "mobius-gateway", cache)
            .await?;
        Ok((host, !cache))
    }

    fn spawn_group_deliveries(
        &self,
        mut deliveries: mpsc::UnboundedReceiver<GroupDelivery>,
    ) -> JoinHandle<()> {
        let state = Arc::downgrade(&self.state);
        let events = self.events.clone();
        let desktop = Arc::clone(&self.desktop);
        tokio::spawn(async move {
            let Some(gateway_state) = state.upgrade() else {
                return;
            };
            let group = Arc::clone(&gateway_state.lock().await.group);
            let startup = match group.pending_recipient_bot_ids().await {
                Ok(startup) => startup,
                Err(error) => {
                    let _ = events.send(ServerFrame::new(ServerMessage::Error {
                        code: "group_delivery".into(),
                        message: error.to_string(),
                        fatal: false,
                    }));
                    Vec::new()
                }
            };
            drop(gateway_state);

            let mut attempts = HashMap::new();
            for target_bot_id in startup {
                let Some(gateway_state) = state.upgrade() else {
                    return;
                };
                let gateway = Self {
                    state: gateway_state,
                    events: events.clone(),
                    desktop: Arc::clone(&desktop),
                };
                gateway
                    .handle_group_delivery(GroupDelivery::Pending { target_bot_id }, &mut attempts)
                    .await;
            }

            while let Some(delivery) = deliveries.recv().await {
                let Some(gateway_state) = state.upgrade() else {
                    return;
                };
                let gateway = Self {
                    state: gateway_state,
                    events: events.clone(),
                    desktop: Arc::clone(&desktop),
                };
                gateway.handle_group_delivery(delivery, &mut attempts).await;
            }
        })
    }

    async fn handle_group_delivery(
        &self,
        delivery: GroupDelivery,
        attempts: &mut HashMap<String, GroupDeliveryAttempt>,
    ) {
        if let GroupDelivery::Changed { chat_id, records } = delivery {
            let host = self.state.lock().await.sessions.get(&chat_id).cloned();
            if let Some(host) = host {
                for record in records {
                    let _ = host
                        .inner
                        .events
                        .send(ServerFrame::new(ServerMessage::AgentEvent {
                            session_id: chat_id.clone(),
                            record: group::record(record),
                        }));
                }
            }
            if let Err(rejection) = self.broadcast_sessions().await {
                let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                    code: "session_catalog".into(),
                    message: rejection.message,
                    fatal: false,
                }));
            }
            return;
        }
        if matches!(&delivery, GroupDelivery::RetryPending) {
            attempts.retain(|_, attempt| !matches!(attempt, GroupDeliveryAttempt::Rejected(_)));
            let group = Arc::clone(&self.state.lock().await.group);
            match group.pending_recipient_bot_ids().await {
                Ok(targets) => {
                    for target_bot_id in targets {
                        group.notify_pending(&target_bot_id);
                    }
                }
                Err(error) => {
                    let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                        code: "group_delivery".into(),
                        message: error.to_string(),
                        fatal: false,
                    }));
                }
            }
            return;
        }
        let target_bot_id = match delivery {
            GroupDelivery::Changed { .. } | GroupDelivery::RetryPending => {
                unreachable!("handled above")
            }
            GroupDelivery::Acknowledged {
                target_bot_id,
                message_id,
            } => {
                if attempts
                    .get(&target_bot_id)
                    .is_some_and(|current| current.message_id() != message_id)
                {
                    return;
                }
                attempts.remove(&target_bot_id);
                target_bot_id
            }
            GroupDelivery::Rejected {
                target_bot_id,
                message_id,
            } => {
                let Some(GroupDeliveryAttempt::Submitted(current)) = attempts.get(&target_bot_id)
                else {
                    return;
                };
                if current != &message_id {
                    return;
                }
                attempts.insert(target_bot_id, GroupDeliveryAttempt::Rejected(message_id));
                return;
            }
            GroupDelivery::CapacityAvailable { target_bot_id } => {
                if !matches!(
                    attempts.get(&target_bot_id),
                    Some(GroupDeliveryAttempt::Rejected(_))
                ) {
                    return;
                }
                attempts.remove(&target_bot_id);
                target_bot_id
            }
            GroupDelivery::Pending { target_bot_id } => {
                if attempts.contains_key(&target_bot_id) {
                    return;
                }
                target_bot_id
            }
        };
        if let Err(rejection) = self
            .deliver_next_group_message(&target_bot_id, attempts)
            .await
        {
            let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                code: "group_delivery".into(),
                message: rejection.message,
                fatal: false,
            }));
        }
    }

    async fn deliver_next_group_message(
        &self,
        target_bot_id: &str,
        attempts: &mut HashMap<String, GroupDeliveryAttempt>,
    ) -> std::result::Result<(), Rejection> {
        let (group, bots, session_mutations) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.group),
                Arc::clone(&state.bots),
                Arc::clone(&state.session_mutations),
            )
        };
        let _mutation = Arc::clone(&session_mutations).read_owned().await;
        if bots.pending_bot_deletion().map_err(internal)?.is_some() {
            return Ok(());
        }
        let Some(claim) = group
            .claim_next_delivery(target_bot_id)
            .await
            .map_err(internal)?
        else {
            return Ok(());
        };
        let target_bot_id = target_bot_id.to_owned();
        let message_id = claim.delivery().entry.id.clone();
        attempts.insert(
            target_bot_id.clone(),
            GroupDeliveryAttempt::Submitted(message_id.clone()),
        );
        let gateway = self.clone();
        drop(_mutation);
        let task = tokio::spawn(async move {
            if let Err(rejection) = gateway
                .deliver_group_claim(claim, &target_bot_id, Arc::clone(&group), session_mutations)
                .await
            {
                group.notify_rejected(&message_id, &target_bot_id);
                let _ = gateway.events.send(ServerFrame::new(ServerMessage::Error {
                    code: "group_delivery".into(),
                    message: rejection.message,
                    fatal: false,
                }));
            }
        });
        let mut state = self.state.lock().await;
        state.idle_cleanup_tasks.retain(|task| !task.is_finished());
        state.idle_cleanup_tasks.push(task);
        Ok(())
    }

    async fn deliver_group_claim(
        &self,
        claim: crate::groups::GroupDeliveryClaim,
        target_bot_id: &str,
        group: Arc<GroupStore>,
        session_mutations: Arc<RwLock<()>>,
    ) -> std::result::Result<(), Rejection> {
        let message_id = claim.delivery().entry.id.clone();
        let session_id = claim.session_id().to_owned();
        let (checkpoint_exists, message_recorded) = {
            let state = self.state.lock().await;
            let checkpoint_exists = state
                .checkpoints
                .load(&session_id)
                .await
                .map_err(internal)?
                .is_some();
            let message_recorded = if checkpoint_exists {
                journal_contains_submission(state.checkpoints.as_ref(), &session_id, &message_id)
                    .await
                    .map_err(internal)?
            } else {
                false
            };
            (checkpoint_exists, message_recorded)
        };
        let host = if checkpoint_exists {
            self.open_session_with_cache(&session_id, true).await?.0
        } else {
            let origin_label = "Group chat";
            self.create_session_with_id(
                &claim.delivery().workspace,
                target_bot_id,
                session_id,
                false,
                origin_label,
            )
            .await?
        };
        if message_recorded {
            return Ok(());
        }
        let files = self.state.lock().await.session_files.clone();
        for attachment in &claim.delivery().entry.message.attachments {
            files
                .grant_upload(&claim.delivery().chat_id, host.session_id(), attachment)
                .await
                .map_err(internal)?;
        }
        let submission = group_message_submission(claim.delivery().entry.clone());
        let mutation = Arc::clone(&session_mutations).read_owned().await;
        let Some(submission) = claim
            .accept(host.submit(submission))
            .await
            .map_err(internal)?
        else {
            return Ok(());
        };
        drop(mutation);
        match submission {
            Ok(()) => Ok(()),
            Err(rejection) if matches!(rejection.code, "agent_busy" | "agent_stopped") => {
                let target_bot_id = target_bot_id.to_owned();
                group.notify_rejected(&message_id, &target_bot_id);
                tokio::spawn(async move {
                    host.wait_idle().await;
                    group.notify_capacity_available(&target_bot_id);
                });
                Ok(())
            }
            Err(rejection) if rejection.code == "gateway_busy" => {
                group.notify_rejected(&message_id, target_bot_id);
                tokio::spawn(notify_group_delivery_after_mutation(
                    session_mutations,
                    group,
                    target_bot_id.to_owned(),
                ));
                Ok(())
            }
            Err(rejection) => Err(rejection),
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
                Arc::clone(&state.group),
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
        let (mut profile, checkpoints, config, store) = {
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
            profile.recent_run_groups = recent_run_groups(recent_runs, &sessions, &metadata);
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
        state.group.retry_pending();
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
}

async fn notify_group_delivery_after_mutation(
    session_mutations: Arc<RwLock<()>>,
    group: Arc<GroupStore>,
    target_bot_id: String,
) {
    let completed = session_mutations.read_owned().await;
    drop(completed);
    group.notify_capacity_available(&target_bot_id);
}

async fn journal_contains_submission(
    checkpoints: &dyn CheckpointStore,
    session_id: &str,
    submission_id: &str,
) -> Result<bool> {
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
    async fn visible_sessions(&self) -> std::result::Result<Vec<SessionRecord>, Rejection> {
        let mut sessions = session_catalog(&self.checkpoints, &self.activities)
            .await
            .map_err(internal)?;
        catalog::include_group_chats(&mut sessions, &self.group, &self.activities)
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

    async fn ensure_capacity(&mut self) -> std::result::Result<(), Rejection> {
        if self.resident_sessions() < MAX_ACTIVE_SESSIONS {
            return Ok(());
        }
        let candidates = self
            .sessions
            .iter()
            .filter(|(_, host)| host.is_unreferenced())
            .map(|(id, host)| (id.clone(), host.clone()))
            .collect::<Vec<_>>();
        for (id, host) in candidates {
            if host.stop_if_idle().await {
                self.sessions.remove(&id);
                self.group.retry_pending();
                if self.resident_sessions() < MAX_ACTIVE_SESSIONS {
                    return Ok(());
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

async fn receive<T>(
    receiver: oneshot::Receiver<std::result::Result<T, Rejection>>,
) -> std::result::Result<T, Rejection> {
    receiver.await.map_err(|_| stopped())?
}

fn group_message_submission(entry: GroupMessage) -> Submission {
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
        .group
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

fn invalid_group(error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "invalid_group",
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
