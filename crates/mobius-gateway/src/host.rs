//! Per-chat agent ownership, event sequencing, replay, and authenticated operations.

mod catalog;
mod extensions;
mod files;
mod git;
mod profile;
mod providers;
mod replay;
mod session;
mod ssh;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use chrono::Utc;
use mobius::Error as MobiusError;
use mobius::agent::{AgentConfig, AgentSender};
use mobius::backend::checkpoint::{
    ActiveExecution, Checkpoint, CheckpointStore, EventPageRequest, ExecutionOutcome,
    ExecutionRecord, ExecutionStats, JournalEvent, SessionPageRequest, SessionSummary,
    event_turn_page, sqlite::SqliteCheckpoint,
};
use mobius::backend::model::ModelRouter;
use mobius::middleware::cron::{
    CronCommand, CronCommandHandler, CronCommandResult, CronRun as CronRunRecord,
    CronTask as CronTaskRecord,
};
use mobius::middleware::scratchpad::ScratchpadStore;
use mobius::middleware::session_files::SessionFileStore;
use mobius::middleware::{FrontendExtensions, Middleware as _};
use mobius::protocol::{
    Event, EventMsg, FrontendContribution, FrontendEvent, FrontendPreviewEvent, ModelStepOutcome,
    Op, RenderedBlock, ReviewDecision, Submission,
};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot};
use uuid::Uuid;

use crate::assembly::{BuiltAgent, assemble};
use crate::config::{
    ChatSpec, ConfigStore, CredentialStore, GatewayConfig,
    create_workspace_directory as create_workspace_directory_on_disk,
};
use crate::cron::{ActiveCronRun, BeginRun, CronStore};
use crate::extensions::ExtensionStore;
use crate::provider_catalog::{
    configured_model_choices, configured_model_routes, configured_provider_for_route,
    provider_instances, provider_statuses,
};
use crate::sandbox::GatewaySandbox;
use crate::wire::{
    AgentComposition, CronRunStatus, GitDiffScope, MAX_FRAME_BYTES, ProfileSnapshot,
    ProviderConfig, ReadyPayload, RecordedEvent, RenderedEvent, RenderedPreview, RunStats,
    RunSummary, ServerFrame, ServerMessage, SessionActivity, SessionActivityState, SessionOutcome,
    SessionReadyPayload, SessionRecord, SessionRunGroup, SessionWidget, SshIdentityRecord,
    VersionedAgentConfig, WorkspaceFileScope, validate_session_id,
};
use crate::{Error, Result};

use self::catalog::{
    SessionCatalogMetadata, load_session_metadata, save_session_metadata, session_catalog,
    validate_session_title,
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
pub(crate) use self::session::HostHandle;
use self::session::*;
use self::ssh::{generate as generate_ssh_identity_on_host, identities as ssh_identities_on_host};

const COMMAND_CAPACITY: usize = 128;
const BROADCAST_CAPACITY: usize = 512;
const REPLAY_CAPACITY: usize = 1024;
const REPLAY_LOAD_PAGE_SIZE: usize = 8;
const MAX_REPLAY_BYTES: usize = MAX_FRAME_BYTES;
const SESSION_PAGE_SIZE: usize = 100;
const RECENT_RUN_LIMIT: usize = 30;
pub(crate) const MAX_ACTIVE_SESSIONS: usize = 32;

fn cron_execution_spec(mut spec: ChatSpec) -> ChatSpec {
    spec.agent.config.middleware.set_enabled("cron", false);
    spec.agent.config.system_prompt.push_str(
        "\n\nExecute the scheduled task in the next user message. Do not create or modify schedules.",
    );
    spec
}

type SessionActivities = Arc<StdMutex<HashMap<String, SessionActivity>>>;

/// Machine-wide chat registry. A session has at most one resident agent owner.
#[derive(Clone)]
pub(crate) struct GatewayHost {
    state: Arc<Mutex<GatewayState>>,
    events: broadcast::Sender<ServerFrame>,
}

struct GatewayState {
    store: ConfigStore,
    config: Arc<StdMutex<GatewayConfig>>,
    credentials: Arc<CredentialStore>,
    cron: Arc<CronStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    scratchpad: ScratchpadStore,
    session_files: SessionFileStore,
    contributions: Vec<FrontendContribution>,
    // ponytail: one lock is enough for at most 32 tiny catalog writes.
    catalog_lock: Arc<Mutex<()>>,
    session_mutations: Arc<RwLock<()>>,
    extension_mutations: Arc<Mutex<()>>,
    provider_epoch: Arc<AtomicU64>,
    activities: SessionActivities,
    provider_login: Arc<StdMutex<Option<String>>>,
    sessions: HashMap<String, HostHandle>,
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
    pub(crate) fn start(
        store: ConfigStore,
        config: GatewayConfig,
        credentials: Arc<CredentialStore>,
        cron: Arc<CronStore>,
    ) -> Result<Self> {
        let extensions = ExtensionStore::new(&store);
        extensions.prune(&config)?;
        extensions.verify_installed_snapshots(&config)?;
        let contributions =
            vec![mobius::middleware::extensions::Extensions::discover_installed([])?.frontend()];
        let checkpoints: Arc<dyn CheckpointStore> =
            Arc::new(SqliteCheckpoint::new(store.checkpoints_path())?);
        let scratchpad = ScratchpadStore::new(Arc::clone(&checkpoints));
        let session_files = SessionFileStore::new(store.state_dir());
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        Ok(Self {
            state: Arc::new(Mutex::new(GatewayState {
                store,
                config: Arc::new(StdMutex::new(config)),
                credentials,
                cron,
                checkpoints,
                scratchpad,
                session_files,
                contributions,
                catalog_lock: Arc::new(Mutex::new(())),
                session_mutations: Arc::new(RwLock::new(())),
                extension_mutations: Arc::new(Mutex::new(())),
                provider_epoch: Arc::new(AtomicU64::new(0)),
                activities: Arc::new(StdMutex::new(HashMap::new())),
                provider_login: Arc::new(StdMutex::new(None)),
                sessions: HashMap::new(),
            })),
            events,
        })
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ServerFrame> {
        self.events.subscribe()
    }

    fn cron_command_handler(&self) -> CronCommandHandler {
        let state = Arc::downgrade(&self.state);
        let events = self.events.clone();
        Arc::new(move |session_id, command| {
            let state = state.clone();
            let events = events.clone();
            Box::pin(async move {
                let state = state
                    .upgrade()
                    .ok_or_else(|| MobiusError::Stopped("gateway stopped".into()))?;
                GatewayHost { state, events }
                    .execute_cron_command(session_id, command)
                    .await
            })
        })
    }

    fn try_command_state(&self) -> mobius::Result<tokio::sync::MutexGuard<'_, GatewayState>> {
        self.state
            .try_lock()
            .map_err(|_| MobiusError::Busy("retry after the gateway update finishes".into()))
    }

    async fn execute_cron_command(
        &self,
        session_id: String,
        command: CronCommand,
    ) -> mobius::Result<CronCommandResult> {
        let cron = {
            let state = self.try_command_state()?;
            Arc::clone(&state.cron)
        };
        let config_error = |error: crate::Error| MobiusError::Config(error.to_string());
        match command {
            CronCommand::List => cron
                .records(&session_id)
                .map(|tasks| {
                    CronCommandResult::Tasks(
                        tasks
                            .into_iter()
                            .map(|task| CronTaskRecord {
                                id: task.id,
                                schedule: task.schedule,
                                task: task.task,
                            })
                            .collect(),
                    )
                })
                .map_err(config_error),
            CronCommand::New { task } => {
                let host = self
                    .try_command_state()?
                    .sessions
                    .get(&session_id)
                    .cloned()
                    .ok_or_else(|| MobiusError::Config("unknown cron session".into()))?;
                host.start_cron_setup(task)
                    .await
                    .map_err(|rejection| MobiusError::Config(rejection.message))?;
                Ok(CronCommandResult::None)
            }
            CronCommand::Reschedule { id, schedule } => cron
                .reschedule(&session_id, &id, &schedule)
                .map(|_| CronCommandResult::None)
                .map_err(config_error),
            CronCommand::Delete { id } => cron
                .delete(&session_id, &id)
                .map(|_| CronCommandResult::None)
                .map_err(config_error),
            CronCommand::Run { id } => {
                let state = self.try_command_state()?;
                self.run_cron_with_state(state, session_id, id)
                    .await
                    .map_err(|rejection| MobiusError::Config(rejection.message))?;
                Ok(CronCommandResult::None)
            }
            CronCommand::History { id } => cron
                .history(&session_id, id.as_deref())
                .map(|runs| {
                    CronCommandResult::History(
                        runs.into_iter()
                            .map(|run| CronRunRecord {
                                id: run.id,
                                task_id: run.task_id,
                                status: format!("{:?}", run.status),
                                started_at: run.started_at,
                            })
                            .collect(),
                    )
                })
                .map_err(config_error),
        }
    }

    pub(crate) async fn session_file_store(&self) -> SessionFileStore {
        self.state.lock().await.session_files.clone()
    }

    pub(crate) async fn ready(&self) -> std::result::Result<ReadyPayload, Rejection> {
        let state = self.state.lock().await;
        gateway_ready(&state).await
    }

    pub(crate) async fn sessions(&self) -> std::result::Result<Vec<SessionRecord>, Rejection> {
        let state = self.state.lock().await;
        session_catalog(&state.checkpoints, &state.activities)
            .await
            .map_err(internal)
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
    ) -> std::result::Result<HostHandle, Rejection> {
        let cron_commands = self.cron_command_handler();
        let mut state = self.state.lock().await;
        state.ensure_capacity().await?;
        let (default_agent, tls) = {
            let config = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?;
            (
                config
                    .default_agent
                    .clone()
                    .unwrap_or_else(setup_agent_config),
                config.tls.clone(),
            )
        };
        let spec = ChatSpec::new(
            workspace,
            default_agent,
            state.store.state_dir(),
            tls.as_ref(),
        )
        .map_err(invalid_workspace)?;
        let session_id = Uuid::new_v4().to_string();
        let host = HostHandle::start(
            state.store.clone(),
            Arc::clone(&state.config),
            spec,
            Arc::clone(&state.credentials),
            Arc::clone(&state.cron),
            cron_commands,
            Arc::clone(&state.checkpoints),
            state.scratchpad.clone(),
            state.session_files.clone(),
            Arc::clone(&state.catalog_lock),
            Arc::clone(&state.session_mutations),
            Arc::clone(&state.provider_epoch),
            Arc::clone(&state.activities),
            self.events.clone(),
            session_id.clone(),
            "mobius-gateway",
        )
        .await
        .map_err(internal)?;
        state.sessions.insert(session_id, host.clone());
        drop(state);
        self.broadcast_sessions().await?;
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
        validate_session_id(session_id).map_err(|_| invalid_session_id())?;
        let cron_commands = self.cron_command_handler();
        let mut state = self.state.lock().await;
        if let Some(host) = state.sessions.get(session_id)
            && host.is_alive()
        {
            return Ok(host.clone());
        }
        state.sessions.remove(session_id);
        state.ensure_capacity().await?;
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
        let spec =
            ChatSpec::from_metadata(&checkpoint.metadata, state.store.state_dir(), tls.as_ref())
                .map_err(invalid_config)?;
        let workspace = spec.workspace_info();
        let workspace_label = workspace.path.display().to_string();
        if checkpoint.session_context.workspace_id.as_deref() != Some(workspace.id.as_str())
            || checkpoint.session_context.workspace_label.as_deref()
                != Some(workspace_label.as_str())
        {
            return Err(invalid_session_workspace());
        }
        let host = HostHandle::start(
            state.store.clone(),
            Arc::clone(&state.config),
            spec,
            Arc::clone(&state.credentials),
            Arc::clone(&state.cron),
            cron_commands,
            Arc::clone(&state.checkpoints),
            state.scratchpad.clone(),
            state.session_files.clone(),
            Arc::clone(&state.catalog_lock),
            Arc::clone(&state.session_mutations),
            Arc::clone(&state.provider_epoch),
            Arc::clone(&state.activities),
            self.events.clone(),
            session_id.into(),
            "mobius-gateway",
        )
        .await
        .map_err(internal)?;
        state.sessions.insert(session_id.into(), host.clone());
        Ok(host)
    }

    pub(crate) async fn delete_session(
        &self,
        session_id: &str,
    ) -> std::result::Result<(), Rejection> {
        let mut state = self.state.lock().await;
        let summaries = gateway_session_summaries(&state.checkpoints)
            .await
            .map_err(internal)?;
        let session_ids = session_tree_ids(session_id, &summaries).ok_or_else(unknown_session)?;
        for id in &session_ids {
            state.cron.require_session_idle(id).map_err(internal)?;
        }
        for id in &session_ids {
            let Some(host) = state.sessions.get(id).cloned() else {
                continue;
            };
            if !host.stop_if_idle().await {
                return Err(Rejection {
                    code: "agent_busy",
                    message: "finish or interrupt the active turn before deleting this chat".into(),
                    fatal: false,
                });
            }
        }
        for id in &session_ids {
            state.sessions.remove(id);
        }
        for id in &session_ids {
            state.cron.delete_session(id).map_err(internal)?;
            state
                .session_files
                .delete_session(id)
                .await
                .map_err(internal)?;
        }
        let catalog_lock = Arc::clone(&state.catalog_lock);
        let _catalog = catalog_lock.lock().await;
        let mut metadata = load_session_metadata(&state.checkpoints)
            .await
            .map_err(internal)?;
        for id in &session_ids {
            metadata.remove(id);
        }
        save_session_metadata(&state.checkpoints, &metadata)
            .await
            .map_err(internal)?;
        if !state
            .checkpoints
            .delete_session(session_id)
            .await
            .map_err(internal)?
        {
            return Err(unknown_session());
        }
        state
            .activities
            .lock()
            .map_err(|_| internal("session activity lock is poisoned"))?
            .retain(|id, _| !session_ids.iter().any(|deleted| deleted == id));
        drop(_catalog);
        drop(state);
        self.broadcast_sessions().await
    }

    pub(crate) async fn run_cron(
        &self,
        source_session_id: String,
        task_id: String,
    ) -> std::result::Result<(), Rejection> {
        let state = self.state.lock().await;
        self.run_cron_with_state(state, source_session_id, task_id)
            .await
    }

    async fn run_cron_with_state(
        &self,
        mut state: tokio::sync::MutexGuard<'_, GatewayState>,
        source_session_id: String,
        task_id: String,
    ) -> std::result::Result<(), Rejection> {
        let cron_commands = self.cron_command_handler();
        let task = state
            .cron
            .task(&source_session_id, &task_id)
            .map_err(invalid_cron)?;
        let (_, input) = state.cron.task_input(&task.id).map_err(invalid_cron)?;
        if let Err(rejection) = state.ensure_capacity().await {
            state
                .cron
                .skip_run(&task.id, "the gateway active-chat limit was reached")
                .map_err(internal)?;
            return Err(rejection);
        }
        let checkpoint = state
            .checkpoints
            .load(&source_session_id)
            .await
            .map_err(internal)?
            .ok_or_else(unknown_session)?;
        let tls = state
            .config
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .tls
            .clone();
        let spec = cron_execution_spec(
            ChatSpec::from_metadata(&checkpoint.metadata, state.store.state_dir(), tls.as_ref())
                .map_err(invalid_config)?,
        );
        let workspace = spec.workspace_info();
        let workspace_label = workspace.path.display().to_string();
        if checkpoint.session_context.workspace_id.as_deref() != Some(workspace.id.as_str())
            || checkpoint.session_context.workspace_label.as_deref()
                != Some(workspace_label.as_str())
        {
            return Err(invalid_session_workspace());
        }
        let execution_metadata = spec.metadata().map_err(invalid_config)?;
        let source_sequence = checkpoint.sequence;
        let session_id = Uuid::new_v4().to_string();
        let label = format!("cron · {}", task.id.get(..8).unwrap_or(&task.id));
        let run = match state.cron.begin_run(&task.id).map_err(invalid_cron)? {
            BeginRun::Started(run) => run,
            BeginRun::Skipped => {
                return Err(Rejection {
                    code: "cron_overlap",
                    message: format!("cron task {} is already running", task.id),
                    fatal: false,
                });
            }
        };
        let mut checkpoint = cron_execution_checkpoint(&checkpoint, &session_id, &label);
        checkpoint.metadata.extend(execution_metadata);
        if let Err(error) = state
            .checkpoints
            .fork(&source_session_id, source_sequence, &checkpoint)
            .await
        {
            let message = error.to_string();
            state
                .cron
                .finish_run(run, CronRunStatus::Failed, Some(message.clone()))
                .map_err(internal)?;
            return Err(internal(message));
        }
        if let Err(error) = state.cron.attach_execution_session(&run, &session_id) {
            let message = error.to_string();
            state
                .cron
                .finish_run(run, CronRunStatus::Failed, Some(message.clone()))
                .map_err(internal)?;
            hide_checkpoint(&state.checkpoints, &session_id)
                .await
                .map_err(internal)?;
            return Err(invalid_cron(message));
        }
        let host = match HostHandle::start(
            state.store.clone(),
            Arc::clone(&state.config),
            spec,
            Arc::clone(&state.credentials),
            Arc::clone(&state.cron),
            cron_commands,
            Arc::clone(&state.checkpoints),
            state.scratchpad.clone(),
            state.session_files.clone(),
            Arc::clone(&state.catalog_lock),
            Arc::clone(&state.session_mutations),
            Arc::clone(&state.provider_epoch),
            Arc::clone(&state.activities),
            self.events.clone(),
            session_id.clone(),
            &label,
        )
        .await
        {
            Ok(host) => host,
            Err(error) => {
                let message = error.to_string();
                state
                    .cron
                    .finish_run(run, CronRunStatus::Failed, Some(message.clone()))
                    .map_err(internal)?;
                hide_checkpoint(&state.checkpoints, &session_id)
                    .await
                    .map_err(internal)?;
                return Err(internal(message));
            }
        };
        let cron = Arc::clone(&state.cron);
        let checkpoints = Arc::clone(&state.checkpoints);
        state.sessions.insert(session_id.clone(), host.clone());
        drop(state);
        match host.run_cron(run, input, &cron).await {
            Ok(()) => {
                let gateway = self.clone();
                tokio::spawn(async move {
                    host.wait_idle().await;
                    gateway.state.lock().await.sessions.remove(&session_id);
                });
                Ok(())
            }
            Err(rejection) => {
                let _ = host.stop_if_idle().await;
                self.state.lock().await.sessions.remove(&session_id);
                hide_checkpoint(&checkpoints, &session_id)
                    .await
                    .map_err(internal)?;
                Err(rejection)
            }
        }
    }

    pub(crate) async fn profile(&self) -> std::result::Result<ProfileSnapshot, Rejection> {
        let (mut profile, checkpoints) = {
            let state = self.state.lock().await;
            let profile = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?
                .profile();
            (profile, Arc::clone(&state.checkpoints))
        };
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
        Ok(profile)
    }

    async fn broadcast_sessions(&self) -> std::result::Result<(), Rejection> {
        let state = self.state.lock().await;
        let sessions = session_catalog(&state.checkpoints, &state.activities)
            .await
            .map_err(internal)?;
        drop(state);
        let _ = self.events.send(ServerFrame::new(ServerMessage::Sessions {
            request_id: None,
            sessions,
        }));
        Ok(())
    }
}

impl GatewayState {
    async fn ensure_capacity(&mut self) -> std::result::Result<(), Rejection> {
        if self.sessions.len() < MAX_ACTIVE_SESSIONS {
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
                if self.sessions.len() < MAX_ACTIVE_SESSIONS {
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

fn stopped() -> Rejection {
    Rejection {
        code: "gateway_stopped",
        message: "the gateway host stopped".into(),
        fatal: true,
    }
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

fn unknown_session() -> Rejection {
    Rejection {
        code: "unknown_session",
        message: "the requested chat does not exist".into(),
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

fn invalid_cron(error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "invalid_cron",
        message: error.to_string(),
        fatal: false,
    }
}

#[cfg(test)]
mod tests;
