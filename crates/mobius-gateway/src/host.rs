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
use mobius::agent::{AgentConfig, AgentSender};
use mobius::backend::checkpoint::{
    ActiveExecution, Checkpoint, CheckpointStore, EventPageRequest, ExecutionOutcome,
    ExecutionRecord, ExecutionStats, JournalEvent, SessionPageRequest, SessionSummary,
    event_turn_page, sqlite::SqliteCheckpoint,
};
use mobius::backend::model::ModelRouter;
use mobius::middleware::scratchpad::ScratchpadStore;
use mobius::middleware::session_files::SessionFileStore;
use mobius::middleware::{FrontendExtensions, Middleware as _};
use mobius::protocol::{
    Event, EventMsg, FrontendContribution, FrontendEvent, FrontendPreviewEvent, MessageAuthor,
    MessageEvent, MessageSubmission, Op, RenderedBlock, ReviewDecision, Submission,
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
use crate::swarm::{BoardEntry, SwarmDelivery, SwarmStore, validate_swarm_members};
use crate::wire::{
    AgentComposition, CronRunStatus, GitDiffScope, MAX_FRAME_BYTES, ProfileSnapshot,
    ProviderConfig, ReadyPayload, RecordedEvent, RenderedEvent, RenderedPreview, RunStats,
    RunSummary, ServerFrame, ServerMessage, SessionActivity, SessionActivityState, SessionOutcome,
    SessionReadyPayload, SessionRecord, SessionRunGroup, SessionWidget, SshIdentityRecord,
    SwarmRecord, VersionedAgentConfig, WorkspaceFileScope, validate_session_id,
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

fn scheduled_execution_spec(mut spec: ChatSpec) -> ChatSpec {
    spec.agent.config.system_prompt.push_str(
        "\n\nExecute the scheduled task in the next user message. Do not create or modify schedules.",
    );
    spec
}

type SessionActivities = Arc<StdMutex<HashMap<String, SessionActivity>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum SwarmDeliveryAttempt {
    Submitted(String),
    Rejected(String),
}

impl SwarmDeliveryAttempt {
    fn message_id(&self) -> &str {
        match self {
            Self::Submitted(message_id) | Self::Rejected(message_id) => message_id,
        }
    }
}

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
    swarm: Arc<SwarmStore>,
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
        let (swarm, deliveries) = SwarmStore::new(Arc::clone(&checkpoints));
        let swarm = Arc::new(swarm);
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        let host = Self {
            state: Arc::new(Mutex::new(GatewayState {
                store,
                config: Arc::new(StdMutex::new(config)),
                credentials,
                cron,
                checkpoints,
                scratchpad,
                session_files,
                swarm,
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
        };
        host.spawn_swarm_deliveries(deliveries);
        Ok(host)
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ServerFrame> {
        self.events.subscribe()
    }

    pub(crate) async fn session_file_store(&self) -> SessionFileStore {
        self.state.lock().await.session_files.clone()
    }

    pub(crate) async fn ready(&self) -> std::result::Result<ReadyPayload, Rejection> {
        let state = self.state.lock().await;
        gateway_ready(&state).await
    }

    pub(crate) async fn submit_global_scratchpad(
        &self,
        operation: Op,
    ) -> std::result::Result<FrontendContribution, Rejection> {
        let Op::CapabilityCommand {
            capability,
            command,
            arguments,
            input,
            target,
        } = operation
        else {
            return Err(invalid_global_scratchpad_operation());
        };
        if capability != "scratchpad" || command != "scratchpad" || target.is_some() {
            return Err(invalid_global_scratchpad_operation());
        }
        let mut arguments = arguments.split_whitespace();
        let operation = arguments.next();
        let scope = arguments.next();
        let id = arguments.next();
        if arguments.next().is_some() {
            return Err(invalid_global_scratchpad_operation());
        }
        let scratchpad = self.state.lock().await.scratchpad.clone();
        match (operation, scope, id, input.as_deref()) {
            (Some("refresh"), None, None, None) => scratchpad.global_contribution().await,
            (Some("edit"), Some("global"), Some(id), Some(note)) => {
                scratchpad.edit_global(id, note).await
            }
            (Some("forget"), Some("global"), Some(id), None) => scratchpad.forget_global(id).await,
            _ => return Err(invalid_global_scratchpad_operation()),
        }
        .map_err(global_scratchpad_error)
    }

    pub(crate) async fn sessions(&self) -> std::result::Result<Vec<SessionRecord>, Rejection> {
        let state = self.state.lock().await;
        session_catalog(&state.checkpoints, &state.activities)
            .await
            .map_err(internal)
    }

    pub(crate) async fn create_swarm(
        &self,
        leader_session_id: String,
        member_session_ids: Vec<String>,
    ) -> std::result::Result<Vec<SwarmRecord>, Rejection> {
        validate_swarm_members(&leader_session_id, &member_session_ids).map_err(invalid_swarm)?;
        let swarms = {
            let state = self.state.lock().await;
            require_swarm_sessions(&state, &member_session_ids).await?;
            state
                .swarm
                .create(leader_session_id, member_session_ids)
                .await
                .map_err(invalid_swarm)?;
            state.swarm.records().await.map_err(internal)?
        };
        self.broadcast_swarms(&swarms);
        Ok(swarms)
    }

    pub(crate) async fn add_swarm_member(
        &self,
        swarm_id: &str,
        session_id: String,
    ) -> std::result::Result<Vec<SwarmRecord>, Rejection> {
        let swarms = {
            let state = self.state.lock().await;
            let swarm = state
                .swarm
                .summaries()
                .await
                .map_err(internal)?
                .into_iter()
                .find(|swarm| swarm.id == swarm_id)
                .ok_or_else(|| invalid_swarm(format!("unknown swarm `{swarm_id}`")))?;
            let mut member_session_ids = swarm
                .members
                .into_iter()
                .map(|member| member.session_id)
                .collect::<Vec<_>>();
            member_session_ids.push(session_id.clone());
            require_swarm_sessions(&state, &member_session_ids).await?;
            state
                .swarm
                .join(swarm_id, session_id)
                .await
                .map_err(invalid_swarm)?;
            state.swarm.records().await.map_err(internal)?
        };
        self.broadcast_swarms(&swarms);
        Ok(swarms)
    }

    pub(crate) async fn leave_swarm(
        &self,
        swarm_id: &str,
        session_id: &str,
    ) -> std::result::Result<Vec<SwarmRecord>, Rejection> {
        let swarms = {
            let state = self.state.lock().await;
            require_catalog_session(&state, session_id).await?;
            state
                .swarm
                .leave(swarm_id, session_id)
                .await
                .map_err(invalid_swarm)?;
            state.swarm.records().await.map_err(internal)?
        };
        self.broadcast_swarms(&swarms);
        Ok(swarms)
    }

    pub(crate) async fn disband_swarm(
        &self,
        swarm_id: &str,
    ) -> std::result::Result<Vec<SwarmRecord>, Rejection> {
        let swarms = {
            let state = self.state.lock().await;
            state.swarm.disband(swarm_id).await.map_err(invalid_swarm)?;
            state.swarm.records().await.map_err(internal)?
        };
        self.broadcast_swarms(&swarms);
        Ok(swarms)
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
            Arc::clone(&state.checkpoints),
            state.scratchpad.clone(),
            state.session_files.clone(),
            state.swarm.clone(),
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
        let host = self
            .open_session_with_cache(session_id, true)
            .await
            .map(|(host, _)| host)?;
        self.state.lock().await.swarm.notify_pending(session_id);
        Ok(host)
    }

    async fn open_session_with_cache(
        &self,
        session_id: &str,
        cache: bool,
    ) -> std::result::Result<(HostHandle, bool), Rejection> {
        validate_session_id(session_id).map_err(|_| invalid_session_id())?;
        let mut state = self.state.lock().await;
        if let Some(host) = state.sessions.get(session_id)
            && host.is_alive()
        {
            return Ok((host.clone(), false));
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
            Arc::clone(&state.checkpoints),
            state.scratchpad.clone(),
            state.session_files.clone(),
            state.swarm.clone(),
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
        if cache {
            state.sessions.insert(session_id.into(), host.clone());
        }
        Ok((host, !cache))
    }

    fn spawn_swarm_deliveries(&self, mut deliveries: mpsc::UnboundedReceiver<SwarmDelivery>) {
        let state = Arc::downgrade(&self.state);
        let events = self.events.clone();
        tokio::spawn(async move {
            let Some(gateway_state) = state.upgrade() else {
                return;
            };
            let swarm = Arc::clone(&gateway_state.lock().await.swarm);
            let startup = match swarm.pending_recipient_session_ids().await {
                Ok(startup) => startup,
                Err(error) => {
                    let _ = events.send(ServerFrame::new(ServerMessage::Error {
                        code: "swarm_delivery".into(),
                        message: error.to_string(),
                        fatal: false,
                    }));
                    Vec::new()
                }
            };
            drop(gateway_state);

            let mut attempts = HashMap::new();
            for target_session_id in startup {
                let Some(gateway_state) = state.upgrade() else {
                    return;
                };
                let gateway = Self {
                    state: gateway_state,
                    events: events.clone(),
                };
                gateway
                    .handle_swarm_delivery(
                        SwarmDelivery::Pending { target_session_id },
                        &mut attempts,
                    )
                    .await;
            }

            while let Some(delivery) = deliveries.recv().await {
                let Some(gateway_state) = state.upgrade() else {
                    return;
                };
                let gateway = Self {
                    state: gateway_state,
                    events: events.clone(),
                };
                gateway.handle_swarm_delivery(delivery, &mut attempts).await;
            }
        });
    }

    async fn handle_swarm_delivery(
        &self,
        delivery: SwarmDelivery,
        attempts: &mut HashMap<String, SwarmDeliveryAttempt>,
    ) {
        if matches!(&delivery, SwarmDelivery::Changed) {
            let swarm = Arc::clone(&self.state.lock().await.swarm);
            match swarm.records().await {
                Ok(swarms) => self.broadcast_swarms(&swarms),
                Err(error) => {
                    let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                        code: "swarm_delivery".into(),
                        message: error.to_string(),
                        fatal: false,
                    }));
                }
            }
            return;
        }
        if matches!(&delivery, SwarmDelivery::RetryPending) {
            let swarm = Arc::clone(&self.state.lock().await.swarm);
            match swarm.pending_recipient_session_ids().await {
                Ok(targets) => {
                    for target_session_id in targets {
                        swarm.notify_pending(&target_session_id);
                    }
                }
                Err(error) => {
                    let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                        code: "swarm_delivery".into(),
                        message: error.to_string(),
                        fatal: false,
                    }));
                }
            }
            return;
        }
        let target_session_id = match delivery {
            SwarmDelivery::Changed | SwarmDelivery::RetryPending => {
                unreachable!("handled above")
            }
            SwarmDelivery::Acknowledged {
                target_session_id,
                message_id,
            } => {
                if attempts
                    .get(&target_session_id)
                    .is_some_and(|current| current.message_id() != message_id)
                {
                    return;
                }
                attempts.remove(&target_session_id);
                target_session_id
            }
            SwarmDelivery::Rejected {
                target_session_id,
                message_id,
            } => {
                let Some(SwarmDeliveryAttempt::Submitted(current)) =
                    attempts.get(&target_session_id)
                else {
                    return;
                };
                if current != &message_id {
                    return;
                }
                attempts.insert(
                    target_session_id,
                    SwarmDeliveryAttempt::Rejected(message_id),
                );
                return;
            }
            SwarmDelivery::CapacityAvailable { target_session_id } => {
                if !matches!(
                    attempts.get(&target_session_id),
                    Some(SwarmDeliveryAttempt::Rejected(_))
                ) {
                    return;
                }
                attempts.remove(&target_session_id);
                target_session_id
            }
            SwarmDelivery::Pending { target_session_id } => {
                if attempts.contains_key(&target_session_id) {
                    let alive = self
                        .state
                        .lock()
                        .await
                        .sessions
                        .get(&target_session_id)
                        .is_some_and(HostHandle::is_alive);
                    if alive {
                        return;
                    }
                    attempts.remove(&target_session_id);
                }
                target_session_id
            }
        };
        if let Err(rejection) = self
            .deliver_next_swarm_message(&target_session_id, attempts)
            .await
        {
            let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                code: "swarm_delivery".into(),
                message: rejection.message,
                fatal: false,
            }));
        }
    }

    async fn deliver_next_swarm_message(
        &self,
        target_session_id: &str,
        attempts: &mut HashMap<String, SwarmDeliveryAttempt>,
    ) -> std::result::Result<(), Rejection> {
        let swarm = Arc::clone(&self.state.lock().await.swarm);
        if swarm
            .pending_deliveries(target_session_id)
            .await
            .map_err(internal)?
            .is_empty()
        {
            return Ok(());
        }
        let (host, _) = self
            .open_session_with_cache(target_session_id, true)
            .await?;
        let Some(delivery) = swarm
            .pending_deliveries(target_session_id)
            .await
            .map_err(internal)?
            .into_iter()
            .next()
        else {
            return Ok(());
        };
        let message_id = delivery.entry.id.clone();
        let submission = peer_message_submission(delivery.entry);
        match host.submit(submission).await {
            Ok(()) => {
                attempts.insert(
                    target_session_id.to_owned(),
                    SwarmDeliveryAttempt::Submitted(message_id),
                );
                Ok(())
            }
            Err(rejection) if matches!(rejection.code, "agent_busy" | "agent_stopped") => {
                let target_session_id = target_session_id.to_owned();
                tokio::spawn(async move {
                    host.wait_idle().await;
                    swarm.notify_pending(&target_session_id);
                });
                Ok(())
            }
            Err(rejection) => Err(rejection),
        }
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
            if state
                .swarm
                .snapshot_for_session(id)
                .await
                .map_err(internal)?
                .is_some()
            {
                return Err(Rejection {
                    code: "session_in_swarm",
                    message: "leave or disband the swarm before deleting this chat".into(),
                    fatal: false,
                });
            }
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

    pub(crate) async fn create_cron(
        &self,
        source_session_id: &str,
        task: &str,
        schedule: crate::wire::CronSchedule,
        ends_at: Option<i64>,
    ) -> std::result::Result<(), Rejection> {
        let state = self.state.lock().await;
        require_cron_source(&state, source_session_id).await?;
        state
            .cron
            .create(source_session_id, task, schedule, ends_at)
            .map(|_| ())
            .map_err(invalid_cron)
    }

    pub(crate) async fn update_cron(
        &self,
        id: &str,
        source_session_id: &str,
        task: &str,
        schedule: crate::wire::CronSchedule,
        ends_at: Option<i64>,
        enabled: bool,
    ) -> std::result::Result<(), Rejection> {
        let state = self.state.lock().await;
        require_cron_source(&state, source_session_id).await?;
        state
            .cron
            .reschedule(id, source_session_id, task, schedule, ends_at, enabled)
            .map(|_| ())
            .map_err(invalid_cron)
    }

    pub(crate) async fn run_cron(&self, task_id: String) -> std::result::Result<(), Rejection> {
        let state = self.state.lock().await;
        let run = match state.cron.begin_run(&task_id).map_err(invalid_cron)? {
            BeginRun::Started(run) => run,
            BeginRun::Skipped => {
                return Err(Rejection {
                    code: "cron_overlap",
                    message: format!("cron task {task_id} is already running"),
                    fatal: false,
                });
            }
        };
        self.run_cron_with_state(state, task_id, run).await
    }

    pub(crate) async fn run_due_cron(
        &self,
        task_id: String,
        run: ActiveCronRun,
    ) -> std::result::Result<(), Rejection> {
        let state = self.state.lock().await;
        self.run_cron_with_state(state, task_id, run).await
    }

    pub(crate) async fn is_cron_execution_session(
        &self,
        session_id: &str,
    ) -> std::result::Result<bool, Rejection> {
        validate_session_id(session_id).map_err(|_| invalid_session_id())?;
        let checkpoints = Arc::clone(&self.state.lock().await.checkpoints);
        Ok(checkpoints
            .load(session_id)
            .await
            .map_err(internal)?
            .as_ref()
            .is_some_and(is_cron_execution_checkpoint))
    }

    pub(crate) async fn cron_run_preview(
        &self,
        run_id: &str,
        before_sequence: Option<u64>,
    ) -> std::result::Result<crate::wire::CronRunPreview, Rejection> {
        let (cron, run) = {
            let state = self.state.lock().await;
            let cron = Arc::clone(&state.cron);
            let run = cron.run(run_id).map_err(invalid_cron)?;
            (cron, run)
        };
        let session_id = run.session_id.clone().ok_or_else(|| Rejection {
            code: "cron_run_unavailable",
            message: "this scheduled run has no execution session".into(),
            fatal: false,
        })?;
        let (host, temporary) = self.open_session_with_cache(&session_id, false).await?;
        let page = host.history_page(before_sequence).await;
        if temporary {
            let _ = host.stop_if_idle().await;
        }
        let page = page?;
        let task = cron
            .record(&run.task_id, Utc::now().timestamp())
            .map_err(invalid_cron)?;
        Ok(crate::wire::CronRunPreview {
            task,
            run,
            records: page.records,
            next_before_sequence: page.next_before_sequence,
        })
    }

    async fn run_cron_with_state(
        &self,
        mut state: tokio::sync::MutexGuard<'_, GatewayState>,
        task_id: String,
        run: ActiveCronRun,
    ) -> std::result::Result<(), Rejection> {
        let preflight: std::result::Result<_, Rejection> = async {
            let task = state.cron.task(&task_id).map_err(invalid_cron)?;
            let source_session_id = task.session_id.clone();
            let (_, input) = state.cron.task_input(&task.id).map_err(invalid_cron)?;
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
            let spec = scheduled_execution_spec(
                ChatSpec::from_metadata(
                    &checkpoint.metadata,
                    state.store.state_dir(),
                    tls.as_ref(),
                )
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
            Ok((
                task,
                source_session_id,
                input,
                checkpoint,
                spec,
                execution_metadata,
            ))
        }
        .await;
        let (task, source_session_id, input, checkpoint, spec, execution_metadata) = match preflight
        {
            Ok(preflight) => preflight,
            Err(rejection) => {
                state
                    .cron
                    .finish_run(run, CronRunStatus::Failed, Some(rejection.message.clone()))
                    .map_err(internal)?;
                return Err(rejection);
            }
        };
        if let Err(rejection) = state.ensure_capacity().await {
            state
                .cron
                .finish_run(
                    run,
                    CronRunStatus::Skipped,
                    Some("the gateway active-chat limit was reached".into()),
                )
                .map_err(internal)?;
            return Err(rejection);
        }
        let source_sequence = checkpoint.sequence;
        let session_id = Uuid::new_v4().to_string();
        let label = format!("cron · {}", task.id.get(..8).unwrap_or(&task.id));
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
            Arc::clone(&state.checkpoints),
            state.scratchpad.clone(),
            state.session_files.clone(),
            state.swarm.clone(),
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
        state.swarm.retry_pending();
        drop(state);
        let _ = self.events.send(ServerFrame::new(ServerMessage::Sessions {
            request_id: None,
            sessions,
        }));
        Ok(())
    }

    fn broadcast_swarms(&self, swarms: &[SwarmRecord]) {
        let _ = self.events.send(ServerFrame::new(ServerMessage::Swarms {
            request_id: None,
            swarms: swarms.to_vec(),
        }));
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
                self.swarm.retry_pending();
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

fn peer_message_submission(entry: BoardEntry) -> Submission {
    let message_id = entry.id;
    Submission {
        id: message_id.clone(),
        op: Op::Message {
            message: MessageSubmission {
                author: MessageAuthor::Peer {
                    message_id,
                    session_id: entry.author.session_id,
                    handle: entry.author.handle,
                },
                text: entry.text,
                attachments: Vec::new(),
                requested_delivery: None,
                target_turn_id: None,
            },
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

async fn require_cron_source(
    state: &GatewayState,
    session_id: &str,
) -> std::result::Result<(), Rejection> {
    require_catalog_session(state, session_id).await
}

async fn require_catalog_session(
    state: &GatewayState,
    session_id: &str,
) -> std::result::Result<(), Rejection> {
    validate_session_id(session_id).map_err(|_| invalid_session_id())?;
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

async fn require_swarm_sessions(
    state: &GatewayState,
    session_ids: &[String],
) -> std::result::Result<(), Rejection> {
    let sessions = session_catalog(&state.checkpoints, &state.activities)
        .await
        .map_err(internal)?;
    let mut workspace_id = None;
    for session_id in session_ids {
        let session = sessions
            .iter()
            .find(|session| &session.session_id == session_id)
            .ok_or_else(unknown_session)?;
        if session.parent_session_id.is_some() {
            return Err(invalid_swarm("a swarm can contain only top-level chats"));
        }
        let current_workspace = session
            .session_context
            .workspace_id
            .as_deref()
            .ok_or_else(|| invalid_swarm("a swarm chat has no workspace identity"))?;
        if workspace_id.is_some_and(|workspace| workspace != current_workspace) {
            return Err(invalid_swarm(
                "all swarm chats must belong to the same workspace",
            ));
        }
        workspace_id = Some(current_workspace);
    }
    Ok(())
}

fn invalid_swarm(error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "invalid_swarm",
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

fn invalid_cron(error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "invalid_cron",
        message: error.to_string(),
        fatal: false,
    }
}

fn invalid_global_scratchpad_operation() -> Rejection {
    Rejection {
        code: "invalid_global_scratchpad",
        message: "global scratchpad accepts only refresh, edit global <id>, or forget global <id>"
            .into(),
        fatal: false,
    }
}

fn global_scratchpad_error(error: mobius::Error) -> Rejection {
    match error {
        mobius::Error::Tool(message) => Rejection {
            code: "invalid_global_scratchpad",
            message,
            fatal: false,
        },
        error => internal(error),
    }
}

#[cfg(test)]
mod tests;
