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
    ActiveExecution, CheckpointStore, EventPageRequest, ExecutionOutcome, ExecutionRecord,
    ExecutionStats, JournalEvent, SessionPageRequest, SessionSummary, event_turn_page,
    sqlite::SqliteCheckpoint,
};
use mobius::backend::model::ModelRouter;
use mobius::middleware::scratchpad::ScratchpadStore;
use mobius::middleware::session_files::{SessionFileDeletion, SessionFileStore};
use mobius::middleware::{FrontendExtensions, Middleware as _};
use mobius::protocol::{
    Event, EventMsg, FrontendContribution, FrontendEvent, FrontendPreviewEvent, MessageAuthor,
    MessageSubmission, ModelStepContentPhase, Op, RenderedBlock, ReviewDecision, Submission,
};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot};
use uuid::Uuid;

use crate::assembly::{BuiltAgent, assemble};
use crate::bots::swarm::{
    BoardEntry, SwarmDelivery, SwarmRunOutcome, SwarmStore, validate_swarm_members,
};
use crate::bots::{ActiveRoutineRun, BeginRun, BotStore};
use crate::config::{
    ChatSpec, ConfigStore, CredentialStore, GatewayConfig,
    create_workspace_directory as create_workspace_directory_on_disk, prepare_background_workspace,
};
use crate::extensions::ExtensionStore;
use crate::provider_catalog::{
    configured_model_choices, configured_model_routes, provider_instances, provider_statuses,
};
use crate::sandbox::GatewaySandbox;
use crate::wire::{
    AgentComposition, GitDiffScope, MAX_FRAME_BYTES, ProfileSnapshot, ProviderConfig, ReadyPayload,
    RecordedEvent, RenderedEvent, RenderedPreview, RoutineRunStatus, RunStats, RunSummary,
    ServerFrame, ServerMessage, SessionActivity, SessionActivityState, SessionOutcome,
    SessionReadyPayload, SessionRecord, SessionRunGroup, SessionWidget, SshIdentityRecord,
    SwarmAttention, SwarmRecord, WorkspaceFileScope, validate_session_id,
};
use crate::{Error, Result};

use self::catalog::{
    SessionCatalogMetadata, background_approvals, hidden_bot_session_catalog,
    load_session_metadata, restore_pending_approval_activities, save_session_metadata,
    session_catalog, validate_session_title,
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
const MAX_SESSION_DELETE_ROOTS: usize = 1_024;
const RECENT_RUN_LIMIT: usize = 30;
pub(crate) const MAX_ACTIVE_SESSIONS: usize = 32;

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
    bots: Arc<BotStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    scratchpad: ScratchpadStore,
    session_files: SessionFileStore,
    background_workspace: PathBuf,
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
        let contributions =
            vec![mobius::middleware::extensions::Extensions::discover_installed([])?.frontend()];
        let checkpoints: Arc<dyn CheckpointStore> =
            Arc::new(SqliteCheckpoint::new(store.checkpoints_path())?);
        let scratchpad = ScratchpadStore::new(Arc::clone(&checkpoints));
        let session_files = SessionFileStore::new(store.state_dir());
        let background_workspace =
            prepare_background_workspace(store.state_dir(), config.tls.as_ref())?;
        let config = Arc::new(StdMutex::new(config));
        let (swarm, deliveries) = SwarmStore::new(
            Arc::clone(&checkpoints),
            Arc::clone(&bots),
            Arc::clone(&config),
        );
        let swarm = Arc::new(swarm);
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        let activities = Arc::new(StdMutex::new(HashMap::new()));
        restore_pending_approval_activities(&checkpoints, &activities).await?;
        let host = Self {
            state: Arc::new(Mutex::new(GatewayState {
                store,
                config,
                credentials,
                bots,
                checkpoints,
                scratchpad,
                session_files,
                background_workspace,
                swarm,
                contributions,
                catalog_lock: Arc::new(Mutex::new(())),
                session_mutations: Arc::new(RwLock::new(())),
                extension_mutations: Arc::new(Mutex::new(())),
                provider_epoch: Arc::new(AtomicU64::new(0)),
                activities,
                provider_login: Arc::new(StdMutex::new(None)),
                sessions: HashMap::new(),
            })),
            events,
        };
        host.reconcile_pending_bot_deletion()
            .await
            .map_err(|rejection| Error::Config(rejection.message))?;
        host.spawn_swarm_deliveries(deliveries);
        Ok(host)
    }

    pub(crate) async fn reconcile_pending_bot_deletion(
        &self,
    ) -> std::result::Result<(), Rejection> {
        if self
            .state
            .lock()
            .await
            .bots
            .pending_bot_deletion()
            .map_err(internal)?
            .is_none()
        {
            return Ok(());
        }
        let mutation_gate = Arc::clone(&self.state.lock().await.session_mutations);
        let _mutation = mutation_gate.write_owned().await;
        let mut state = self.state.lock().await;
        let Some(intent) = state.bots.pending_bot_deletion().map_err(internal)? else {
            return Ok(());
        };
        let mut deletion = state
            .bots
            .bots()
            .map_err(internal)?
            .iter()
            .any(|bot| bot.id == intent.bot_id)
            .then(|| {
                state
                    .bots
                    .prepare_bot_deletion(&intent.bot_id, intent.expected_revision)
                    .map_err(invalid_bot)
            })
            .transpose()?;
        if let Some(deletion) = &mut deletion {
            deletion.release_state_lock();
        }
        let mut file_deletion =
            prepare_session_tree_deletion(&mut state, &intent.session_ids, true).await?;
        let bot_store = Arc::clone(&state.bots);
        let swarm_store = Arc::clone(&state.swarm);
        let scratchpad = state.scratchpad.clone();
        drop(state);

        swarm_store
            .remove_bot(&intent.bot_id)
            .await
            .map_err(invalid_swarm)?;
        if let Some(deletion) = deletion {
            bot_store.delete_bot(deletion).map_err(invalid_bot)?;
        }
        let mut state = self.state.lock().await;
        let file_warning = remove_session_trees(
            &mut state,
            &intent.session_roots,
            &intent.session_ids,
            &mut file_deletion,
            true,
        )
        .await?;
        drop(state);
        if let Some(warning) = file_warning {
            let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                code: "session_cleanup".into(),
                message: warning.message,
                fatal: false,
            }));
        }
        bot_store
            .cleanup_bot_deletion_files(&intent)
            .map_err(internal)?;
        if intent.disbanded_swarm
            && let Some(swarm_id) = &intent.swarm_id
        {
            scratchpad.clear_swarm(swarm_id).await.map_err(internal)?;
        }
        bot_store
            .clear_bot_deletion(&intent.bot_id)
            .map_err(internal)
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
        let _access = self.begin_mutation().await?;
        let state = self.state.lock().await;
        gateway_ready(&state).await
    }

    pub(crate) async fn submit_scratchpad(
        &self,
        scope: &crate::wire::ScratchpadScope,
        operation: Op,
    ) -> std::result::Result<FrontendContribution, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let Op::CapabilityCommand {
            capability,
            command,
            arguments,
            input,
            target,
        } = operation
        else {
            return Err(invalid_scratchpad_operation());
        };
        if capability != "scratchpad" || command != "scratchpad" || target.is_some() {
            return Err(invalid_scratchpad_operation());
        }
        let mut arguments = arguments.split_whitespace();
        let operation = arguments.next();
        let argument_scope = arguments.next();
        let id = arguments.next();
        if arguments.next().is_some() {
            return Err(invalid_scratchpad_operation());
        }
        let (scratchpad, swarm) = {
            let state = self.state.lock().await;
            (state.scratchpad.clone(), Arc::clone(&state.swarm))
        };
        if let crate::wire::ScratchpadScope::Swarm { id } = scope
            && !swarm
                .contains_swarm(id)
                .await
                .map_err(invalid_scratchpad_store)?
        {
            return Err(Rejection {
                code: "unknown_swarm",
                message: format!("unknown swarm `{id}`"),
                fatal: false,
            });
        }
        match (scope, operation, argument_scope, id, input.as_deref()) {
            (crate::wire::ScratchpadScope::Global, Some("refresh"), None, None, None) => {
                scratchpad.global_contribution().await
            }
            (crate::wire::ScratchpadScope::Global, Some("add"), None, None, Some(note)) => {
                scratchpad.add_global(note).await
            }
            (
                crate::wire::ScratchpadScope::Global,
                Some("edit"),
                Some("global"),
                Some(id),
                Some(note),
            ) => scratchpad.edit_global(id, note).await,
            (
                crate::wire::ScratchpadScope::Global,
                Some("forget"),
                Some("global"),
                Some(id),
                None,
            ) => scratchpad.forget_global(id).await,
            (crate::wire::ScratchpadScope::Swarm { id }, Some("refresh"), None, None, None) => {
                scratchpad.swarm_contribution(id).await
            }
            (crate::wire::ScratchpadScope::Swarm { id }, Some("add"), None, None, Some(note)) => {
                scratchpad.add_swarm(id, note).await
            }
            (
                crate::wire::ScratchpadScope::Swarm { id: swarm_id },
                Some("edit"),
                Some("swarm"),
                Some(id),
                Some(note),
            ) => scratchpad.edit_swarm(swarm_id, id, note).await,
            (
                crate::wire::ScratchpadScope::Swarm { id: swarm_id },
                Some("forget"),
                Some("swarm"),
                Some(id),
                None,
            ) => scratchpad.forget_swarm(swarm_id, id).await,
            _ => return Err(invalid_scratchpad_operation()),
        }
        .map_err(scratchpad_error)
    }

    pub(crate) async fn sessions(&self) -> std::result::Result<Vec<SessionRecord>, Rejection> {
        let _access = self.begin_mutation().await?;
        let state = self.state.lock().await;
        session_catalog(&state.checkpoints, &state.activities)
            .await
            .map_err(internal)
    }

    pub(crate) async fn create_swarm(
        &self,
        title: String,
        leader_bot_id: String,
        member_bot_ids: Vec<String>,
    ) -> std::result::Result<Vec<SwarmRecord>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let (swarm, ids) = {
            let state = self.state.lock().await;
            let mut ids = vec![leader_bot_id.clone()];
            ids.extend(
                member_bot_ids
                    .into_iter()
                    .filter(|bot_id| bot_id != &leader_bot_id),
            );
            for bot_id in &ids {
                state.bots.bot(bot_id).map_err(invalid_bot)?;
            }
            validate_swarm_members(&leader_bot_id, &ids).map_err(invalid_swarm)?;
            (Arc::clone(&state.swarm), ids)
        };
        swarm
            .create(title, leader_bot_id, ids)
            .await
            .map_err(invalid_swarm)?;
        let swarms = swarm.records().await.map_err(internal)?;
        self.broadcast_swarms(&swarms);
        Ok(swarms)
    }

    pub(crate) async fn add_swarm_member(
        &self,
        swarm_id: &str,
        bot_id: String,
    ) -> std::result::Result<Vec<SwarmRecord>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let swarm = {
            let state = self.state.lock().await;
            state.bots.bot(&bot_id).map_err(invalid_bot)?;
            Arc::clone(&state.swarm)
        };
        swarm.join(swarm_id, bot_id).await.map_err(invalid_swarm)?;
        let swarms = swarm.records().await.map_err(internal)?;
        self.broadcast_swarms(&swarms);
        Ok(swarms)
    }

    pub(crate) async fn leave_swarm(
        &self,
        swarm_id: &str,
        bot_id: &str,
    ) -> std::result::Result<Vec<SwarmRecord>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let swarm = {
            let state = self.state.lock().await;
            state.bots.bot(bot_id).map_err(invalid_bot)?;
            Arc::clone(&state.swarm)
        };
        swarm.leave(swarm_id, bot_id).await.map_err(invalid_swarm)?;
        let swarms = swarm.records().await.map_err(internal)?;
        self.broadcast_swarms(&swarms);
        Ok(swarms)
    }

    pub(crate) async fn rename_swarm(
        &self,
        swarm_id: &str,
        title: String,
    ) -> std::result::Result<Vec<SwarmRecord>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let swarms = {
            let state = self.state.lock().await;
            state
                .swarm
                .rename(swarm_id, title)
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
        let _mutation = self.begin_mutation().await?;
        let (swarm, scratchpad) = {
            let state = self.state.lock().await;
            (Arc::clone(&state.swarm), state.scratchpad.clone())
        };
        disband_swarm_with_scratchpad(&swarm, &scratchpad, swarm_id).await?;
        let swarms = swarm.records().await.map_err(internal)?;
        self.broadcast_swarms(&swarms);
        Ok(swarms)
    }

    pub(crate) async fn post_swarm_message(
        &self,
        swarm_id: &str,
        text: String,
    ) -> std::result::Result<Vec<SwarmRecord>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let swarm = Arc::clone(&self.state.lock().await.swarm);
        swarm
            .post_user(swarm_id, text)
            .await
            .map_err(invalid_swarm)?;
        let swarms = swarm.records().await.map_err(internal)?;
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
        let _mutation = self.begin_exclusive_mutation().await?;
        let mut state = self.state.lock().await;
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
        let residents = bot_update_residents(&mut state, id).await?;
        if residents.iter().any(|(_, status)| !status.idle) {
            return Err(Rejection {
                code: "agent_busy",
                message: "finish or interrupt active Bot turns before updating its profile".into(),
                fatal: false,
            });
        }
        let residents = residents
            .into_iter()
            .map(|(host, _)| host)
            .collect::<Vec<_>>();
        let bot =
            match state
                .bots
                .update_bot(id, expected_revision, name, description, tint, config)
            {
                Ok(bot) => bot,
                Err(error) => return Err(invalid_bot(error)),
            };
        for (index, host) in residents.iter().enumerate() {
            if let Err(rejection) = host.reload_bot(bot.clone()).await {
                let rejection =
                    rollback_bot_update(&mut state, &residents, index, &previous, &bot, rejection)
                        .await;
                let bots = state.bots.bots();
                drop(state);
                if let Ok(bots) = bots {
                    self.broadcast_bots(&bots);
                }
                return Err(rejection);
            }
        }
        let bots = state.bots.bots().map_err(internal)?;
        drop(state);
        self.broadcast_bots(&bots);
        Ok(bot)
    }

    pub(crate) async fn delete_bot(
        &self,
        id: &str,
        expected_revision: u64,
    ) -> std::result::Result<(Vec<crate::wire::BotRecord>, Vec<String>), Rejection> {
        let _mutation = self.begin_exclusive_mutation().await?;
        let mut state = self.state.lock().await;
        let bot = state.bots.bot(id).map_err(invalid_bot)?;
        if bot.config.revision != expected_revision {
            return Err(Rejection {
                code: "revision_conflict",
                message: format!("Bot configuration revision is now {}", bot.config.revision),
                fatal: false,
            });
        }
        if bot.handle == "mobius" {
            return Err(bot_delete_rejection(
                "bot_undeletable",
                "the built-in @mobius Bot cannot be deleted",
            ));
        }
        let (session_roots, session_ids, mut file_deletion) =
            prepare_bot_session_tree_deletion(&mut state, id).await?;
        let planned_swarm = state
            .swarm
            .planned_bot_removal(id)
            .await
            .map_err(invalid_swarm)?;
        let mut deletion = state
            .bots
            .prepare_bot_deletion(id, expected_revision)
            .map_err(invalid_bot)?;
        let bot_store = Arc::clone(&state.bots);
        let swarm_store = Arc::clone(&state.swarm);
        let scratchpad = state.scratchpad.clone();
        let intent = bot_store
            .record_bot_deletion(
                &mut deletion,
                &session_roots,
                &session_ids,
                planned_swarm
                    .as_ref()
                    .map(|removal| (removal.swarm_id.as_str(), removal.disbanded)),
            )
            .map_err(invalid_bot)?;
        drop(state);
        let swarm = swarm_store.remove_bot(id).await.map_err(invalid_swarm)?;

        bot_store.delete_bot(deletion).map_err(invalid_bot)?;
        let bots = bot_store.bots().map_err(internal)?;
        let mut state = self.state.lock().await;
        let mut cleanup_errors = Vec::new();
        match remove_session_trees(
            &mut state,
            &session_roots,
            &session_ids,
            &mut file_deletion,
            true,
        )
        .await
        {
            Ok(Some(warning)) => cleanup_errors.push(warning.message),
            Ok(None) => {}
            Err(rejection) => return Err(rejection),
        }
        drop(state);
        bot_store
            .cleanup_bot_deletion_files(&intent)
            .map_err(internal)?;
        if intent.disbanded_swarm
            && let Some(swarm_id) = &intent.swarm_id
            && let Err(error) = scratchpad.clear_swarm(swarm_id).await
        {
            return Err(internal(error));
        }
        bot_store.clear_bot_deletion(id).map_err(invalid_bot)?;
        if !session_ids.is_empty()
            && let Err(rejection) = self.broadcast_sessions().await
        {
            cleanup_errors.push(rejection.message);
        }
        if swarm.is_some() {
            match swarm_store.records().await {
                Ok(swarms) => self.broadcast_swarms(&swarms),
                Err(error) => cleanup_errors.push(error.to_string()),
            }
        }
        self.broadcast_bots(&bots);
        for message in cleanup_errors {
            let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                code: "bot_cleanup".into(),
                message,
                fatal: false,
            }));
        }
        Ok((bots, session_ids))
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
        let _mutation = self.begin_mutation().await?;
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
        let mut state = self.state.lock().await;
        state.ensure_capacity().await?;
        let tls = {
            let config = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?;
            config.tls.clone()
        };
        let bot = state.bots.bot(bot_id).map_err(invalid_bot)?;
        let mut spec = ChatSpec::for_bot(workspace, &bot, state.store.state_dir(), tls.as_ref())
            .map_err(invalid_workspace)?;
        spec.catalog_visible = catalog_visible;
        let host = HostHandle::start(
            state.store.clone(),
            Arc::clone(&state.config),
            spec,
            Arc::clone(&state.credentials),
            Arc::clone(&state.bots),
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
            origin_label,
        )
        .await
        .map_err(internal)?;
        state.sessions.insert(session_id, host.clone());
        drop(state);
        if catalog_visible {
            self.broadcast_sessions().await?;
        }
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
        let _mutation = self.begin_mutation().await?;
        let host = self
            .open_session_with_cache(session_id, true)
            .await
            .map(|(host, _)| host)?;
        self.state.lock().await.swarm.notify_pending(host.bot_id());
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
        let mut spec = ChatSpec::from_metadata(
            &checkpoint.metadata,
            &state.bots,
            state.store.state_dir(),
            tls.as_ref(),
        )
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
        let host = HostHandle::start(
            state.store.clone(),
            Arc::clone(&state.config),
            spec,
            Arc::clone(&state.credentials),
            Arc::clone(&state.bots),
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
            let (swarm, terminal_runs) = {
                let gateway = gateway_state.lock().await;
                (
                    Arc::clone(&gateway.swarm),
                    gateway.bots.history(None).unwrap_or_default(),
                )
            };
            for run in terminal_runs
                .iter()
                .filter(|run| run.status != RoutineRunStatus::Running)
            {
                let _ = swarm.project_routine_outcome(run, None).await;
            }
            let startup = match swarm.pending_recipient_bot_ids().await {
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
            for target_bot_id in startup {
                let Some(gateway_state) = state.upgrade() else {
                    return;
                };
                let gateway = Self {
                    state: gateway_state,
                    events: events.clone(),
                };
                gateway
                    .handle_swarm_delivery(SwarmDelivery::Pending { target_bot_id }, &mut attempts)
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
            match (swarm.records().await, swarm.pending_attentions().await) {
                (Ok(swarms), Ok(attentions)) => {
                    self.broadcast_swarms(&swarms);
                    self.broadcast_swarm_attentions(&attentions);
                }
                (Err(error), _) | (_, Err(error)) => {
                    let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                        code: "swarm_delivery".into(),
                        message: error.to_string(),
                        fatal: false,
                    }));
                }
            }
            return;
        }
        if matches!(&delivery, SwarmDelivery::CatalogChanged) {
            match self.bots().await {
                Ok(bots) => self.broadcast_bots(&bots),
                Err(error) => {
                    let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                        code: "bot_catalog".into(),
                        message: error.message,
                        fatal: false,
                    }));
                }
            }
            return;
        }
        if matches!(&delivery, SwarmDelivery::RetryPending) {
            let swarm = Arc::clone(&self.state.lock().await.swarm);
            match swarm.pending_recipient_bot_ids().await {
                Ok(targets) => {
                    for target_bot_id in targets {
                        swarm.notify_pending(&target_bot_id);
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
        let target_bot_id = match delivery {
            SwarmDelivery::Changed
            | SwarmDelivery::CatalogChanged
            | SwarmDelivery::RetryPending => {
                unreachable!("handled above")
            }
            SwarmDelivery::Acknowledged {
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
            SwarmDelivery::Rejected {
                target_bot_id,
                message_id,
            } => {
                let Some(SwarmDeliveryAttempt::Submitted(current)) = attempts.get(&target_bot_id)
                else {
                    return;
                };
                if current != &message_id {
                    return;
                }
                attempts.insert(target_bot_id, SwarmDeliveryAttempt::Rejected(message_id));
                return;
            }
            SwarmDelivery::CapacityAvailable { target_bot_id } => {
                if !matches!(
                    attempts.get(&target_bot_id),
                    Some(SwarmDeliveryAttempt::Rejected(_))
                ) {
                    return;
                }
                attempts.remove(&target_bot_id);
                target_bot_id
            }
            SwarmDelivery::Pending { target_bot_id } => {
                if attempts.contains_key(&target_bot_id) {
                    return;
                }
                target_bot_id
            }
        };
        if let Err(rejection) = self
            .deliver_next_swarm_message(&target_bot_id, attempts)
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
        target_bot_id: &str,
        attempts: &mut HashMap<String, SwarmDeliveryAttempt>,
    ) -> std::result::Result<(), Rejection> {
        let (swarm, bots, session_mutations, background_workspace) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.swarm),
                Arc::clone(&state.bots),
                Arc::clone(&state.session_mutations),
                state.background_workspace.clone(),
            )
        };
        let _mutation = Arc::clone(&session_mutations).read_owned().await;
        if bots.pending_bot_deletion().map_err(internal)?.is_some() {
            return Ok(());
        }
        let Some(claim) = swarm
            .claim_next_delivery(target_bot_id)
            .await
            .map_err(internal)?
        else {
            return Ok(());
        };
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
            let origin_label = format!("Swarm Chat · {}", claim.delivery().swarm_title);
            self.create_session_with_id(
                &background_workspace,
                target_bot_id,
                session_id,
                false,
                &origin_label,
            )
            .await?
        };
        if message_recorded {
            if claim
                .accept(std::future::ready(()))
                .await
                .map_err(internal)?
                .is_some()
            {
                attempts.insert(
                    target_bot_id.to_owned(),
                    SwarmDeliveryAttempt::Submitted(message_id),
                );
            }
            return Ok(());
        }
        let submission = swarm_message_submission(claim.delivery().entry.clone());
        let Some(submission) = claim
            .accept(host.submit(submission))
            .await
            .map_err(internal)?
        else {
            return Ok(());
        };
        match submission {
            Ok(()) => {
                attempts.insert(
                    target_bot_id.to_owned(),
                    SwarmDeliveryAttempt::Submitted(message_id),
                );
                Ok(())
            }
            Err(rejection) if matches!(rejection.code, "agent_busy" | "agent_stopped") => {
                let target_bot_id = target_bot_id.to_owned();
                tokio::spawn(async move {
                    host.wait_idle().await;
                    swarm.notify_pending(&target_bot_id);
                });
                Ok(())
            }
            Err(rejection) if rejection.code == "gateway_busy" => {
                tokio::spawn(notify_swarm_delivery_after_mutation(
                    session_mutations,
                    swarm,
                    target_bot_id.to_owned(),
                ));
                Ok(())
            }
            Err(rejection) => Err(rejection),
        }
    }

    pub(crate) async fn rename_session(
        &self,
        session_id: &str,
        title: &str,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        let title = validate_session_title(title)?;
        let (host, checkpoints, catalog_lock) = {
            let state = self.state.lock().await;
            require_catalog_session(&state, session_id).await?;
            (
                state
                    .sessions
                    .get(session_id)
                    .filter(|host| host.is_alive())
                    .cloned(),
                Arc::clone(&state.checkpoints),
                Arc::clone(&state.catalog_lock),
            )
        };
        if let Some(host) = host {
            return host.rename_session(session_id.into(), title.into()).await;
        }
        let _catalog = catalog_lock.lock().await;
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
        metadata.entry(session_id.into()).or_default().title = Some(title.into());
        save_session_metadata(&checkpoints, &metadata)
            .await
            .map_err(internal)?;
        drop(_catalog);
        self.broadcast_sessions().await
    }

    pub(crate) async fn set_session_pinned(
        &self,
        session_id: &str,
        pinned: bool,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        let (host, checkpoints, catalog_lock) = {
            let state = self.state.lock().await;
            require_catalog_session(&state, session_id).await?;
            (
                state
                    .sessions
                    .get(session_id)
                    .filter(|host| host.is_alive())
                    .cloned(),
                Arc::clone(&state.checkpoints),
                Arc::clone(&state.catalog_lock),
            )
        };
        if let Some(host) = host {
            return host.set_session_pinned(session_id.into(), pinned).await;
        }
        let _catalog = catalog_lock.lock().await;
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
        metadata.entry(session_id.into()).or_default().pinned = pinned;
        save_session_metadata(&checkpoints, &metadata)
            .await
            .map_err(internal)?;
        drop(_catalog);
        self.broadcast_sessions().await
    }

    pub(crate) async fn delete_sessions(
        &self,
        session_ids: &[String],
    ) -> std::result::Result<Vec<String>, Rejection> {
        if session_ids.is_empty() || session_ids.len() > MAX_SESSION_DELETE_ROOTS {
            return Err(Rejection {
                code: "invalid_session_selection",
                message: format!("select between 1 and {MAX_SESSION_DELETE_ROOTS} chats to delete"),
                fatal: false,
            });
        }
        let mut seen = HashSet::new();
        let selected = session_ids
            .iter()
            .filter(|session_id| seen.insert(session_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for session_id in &selected {
            validate_session_id(session_id).map_err(|_| invalid_session_id())?;
        }
        let _mutation = self.begin_exclusive_mutation().await?;
        let mut state = self.state.lock().await;
        let summaries = gateway_session_summaries(&state.checkpoints)
            .await
            .map_err(internal)?;
        if selected.iter().any(|selected| {
            !summaries
                .iter()
                .any(|session| session.catalog_visible && session.session_id == *selected)
        }) {
            return Err(unknown_session());
        }
        let selected_set = selected.iter().map(String::as_str).collect::<HashSet<_>>();
        let parents = summaries
            .iter()
            .map(|session| {
                (
                    session.session_id.as_str(),
                    session.parent_session_id.as_deref(),
                )
            })
            .collect::<HashMap<_, _>>();
        let roots = selected
            .iter()
            .filter(|session_id| {
                let mut ancestor = parents.get(session_id.as_str()).copied().flatten();
                let mut visited = HashSet::new();
                while let Some(parent) = ancestor {
                    if !visited.insert(parent) {
                        break;
                    }
                    if selected_set.contains(parent) {
                        return false;
                    }
                    ancestor = parents.get(parent).copied().flatten();
                }
                true
            })
            .cloned()
            .collect::<Vec<_>>();
        let (_, deleted) = session_trees(roots.clone(), &summaries);
        let cleanup = delete_session_trees(&mut state, &roots, &deleted, false).await?;
        drop(state);
        if let Some(rejection) = cleanup {
            let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                code: "session_cleanup".into(),
                message: rejection.message,
                fatal: false,
            }));
        }
        if let Err(rejection) = self.broadcast_sessions().await {
            let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                code: "session_catalog".into(),
                message: rejection.message,
                fatal: false,
            }));
        }
        Ok(deleted)
    }

    pub(crate) async fn create_routine(
        &self,
        bot_id: &str,
        workspace: &Path,
        instructions: &str,
        schedule: crate::wire::RoutineSchedule,
        ends_at: Option<i64>,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        validate_bot_workspace(&state, bot_id, workspace)?;
        state
            .bots
            .create_routine(bot_id, workspace, instructions, schedule, ends_at)
            .map(|_| ())
            .map_err(invalid_routine)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "a routine replacement is one wire record"
    )]
    pub(crate) async fn update_routine(
        &self,
        id: &str,
        bot_id: &str,
        workspace: &Path,
        instructions: &str,
        schedule: crate::wire::RoutineSchedule,
        ends_at: Option<i64>,
        enabled: bool,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        validate_bot_workspace(&state, bot_id, workspace)?;
        state
            .bots
            .update_routine(
                id,
                bot_id,
                workspace,
                instructions,
                schedule,
                ends_at,
                enabled,
            )
            .map(|_| ())
            .map_err(invalid_routine)
    }

    pub(crate) async fn delete_routine(
        &self,
        routine_id: &str,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_exclusive_mutation().await?;
        let mut state = self.state.lock().await;
        let roots = state
            .bots
            .history(Some(routine_id))
            .map_err(invalid_routine)?
            .into_iter()
            .filter_map(|run| run.session_id)
            .collect();
        let summaries = gateway_session_summaries(&state.checkpoints)
            .await
            .map_err(internal)?;
        let session_ids = session_trees(roots, &summaries).1;
        let mut file_deletion =
            prepare_session_tree_deletion(&mut state, &session_ids, false).await?;
        let summaries = gateway_session_summaries(&state.checkpoints)
            .await
            .map_err(internal)?;
        let deletion = state
            .bots
            .prepare_routine_deletion(routine_id)
            .map_err(invalid_routine)?;
        let roots = deletion
            .session_ids()
            .iter()
            .filter(|root| summaries.iter().any(|session| session.session_id == **root))
            .cloned()
            .collect();
        let (session_roots, session_ids) = session_trees(roots, &summaries);
        state
            .bots
            .delete_routine(deletion)
            .map_err(invalid_routine)?;
        let cleanup = remove_session_trees(
            &mut state,
            &session_roots,
            &session_ids,
            &mut file_deletion,
            true,
        )
        .await;
        drop(state);
        if !session_ids.is_empty() {
            let _ = self.broadcast_sessions().await;
        }
        if let Some(rejection) = match cleanup {
            Ok(warning) => warning,
            Err(rejection) => Some(rejection),
        } {
            let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                code: "routine_cleanup".into(),
                message: rejection.message,
                fatal: false,
            }));
        }
        Ok(())
    }

    pub(crate) async fn run_routine(
        &self,
        routine_id: String,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        let run = match state.bots.begin_run(&routine_id).map_err(invalid_routine)? {
            BeginRun::Started(run) => run,
            BeginRun::Skipped => {
                return Err(Rejection {
                    code: "routine_overlap",
                    message: format!("routine {routine_id} is already running"),
                    fatal: false,
                });
            }
        };
        self.run_routine_with_state(state, routine_id, run).await
    }

    pub(crate) async fn run_due_routine(
        &self,
        routine_id: String,
        run: ActiveRoutineRun,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = match self.begin_mutation().await {
            Ok(mutation) => mutation,
            Err(rejection) => {
                let (swarm, completed) = {
                    let state = self.state.lock().await;
                    let completed = state
                        .bots
                        .finish_run(
                            run,
                            RoutineRunStatus::Skipped,
                            Some(rejection.message.clone()),
                        )
                        .map_err(internal)?;
                    (Arc::clone(&state.swarm), completed)
                };
                swarm
                    .project_routine_outcome(&completed, None)
                    .await
                    .map_err(internal)?;
                return Err(rejection);
            }
        };
        let state = self.state.lock().await;
        self.run_routine_with_state(state, routine_id, run).await
    }

    pub(crate) async fn routine_run_preview(
        &self,
        run_id: &str,
        before_sequence: Option<u64>,
    ) -> std::result::Result<crate::wire::RoutineRunPreview, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let (bots, run) = {
            let state = self.state.lock().await;
            let bots = Arc::clone(&state.bots);
            let run = bots.run(run_id).map_err(invalid_routine)?;
            (bots, run)
        };
        let session_id = run.session_id.clone().ok_or_else(|| Rejection {
            code: "routine_run_unavailable",
            message: "this routine run has no execution session".into(),
            fatal: false,
        })?;
        let (host, temporary) = self.open_session_with_cache(&session_id, false).await?;
        let page = host.history_page(before_sequence).await;
        if temporary {
            let _ = host.stop_if_idle().await;
        }
        let page = page?;
        let routine = bots
            .routine_record(&run.routine_id, Utc::now().timestamp())
            .map_err(invalid_routine)?;
        Ok(crate::wire::RoutineRunPreview {
            routine,
            run,
            records: page.records,
            next_before_sequence: page.next_before_sequence,
        })
    }

    pub(crate) async fn delete_routine_run(
        &self,
        run_id: &str,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_exclusive_mutation().await?;
        let mut state = self.state.lock().await;
        let run = state.bots.run(run_id).map_err(invalid_routine)?;
        if run.status == RoutineRunStatus::Running {
            return Err(Rejection {
                code: "routine_run_active",
                message: format!("routine run {run_id} is currently running"),
                fatal: false,
            });
        }
        let session_root = run.session_id;
        let session_ids = if let Some(session_id) = session_root.as_deref() {
            let summaries = gateway_session_summaries(&state.checkpoints)
                .await
                .map_err(internal)?;
            session_tree_ids(session_id, &summaries).unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut file_deletion =
            prepare_session_tree_deletion(&mut state, &session_ids, false).await?;
        state.bots.delete_run(run_id).map_err(invalid_routine)?;
        let cleanup = if let Some(session_root) = session_root.filter(|_| !session_ids.is_empty()) {
            remove_session_trees(
                &mut state,
                &[session_root],
                &session_ids,
                &mut file_deletion,
                true,
            )
            .await
        } else {
            Ok(None)
        };
        drop(state);
        if !session_ids.is_empty() {
            let _ = self.broadcast_sessions().await;
        }
        if let Some(rejection) = match cleanup {
            Ok(warning) => warning,
            Err(rejection) => Some(rejection),
        } {
            let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                code: "routine_cleanup".into(),
                message: rejection.message,
                fatal: false,
            }));
        }
        Ok(())
    }

    async fn run_routine_with_state(
        &self,
        mut state: tokio::sync::MutexGuard<'_, GatewayState>,
        routine_id: String,
        run: ActiveRoutineRun,
    ) -> std::result::Result<(), Rejection> {
        let preflight: std::result::Result<_, Rejection> = (|| {
            let routine = state.bots.routine(&routine_id).map_err(invalid_routine)?;
            let (_, input) = state
                .bots
                .routine_input(&routine.id)
                .map_err(invalid_routine)?;
            let bot = state.bots.bot(&routine.bot_id).map_err(invalid_bot)?;
            let tls = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?
                .tls
                .clone();
            let mut spec = ChatSpec::for_bot(
                &routine.workspace,
                &bot,
                state.store.state_dir(),
                tls.as_ref(),
            )
            .map_err(invalid_workspace)?;
            spec.catalog_visible = false;
            Ok((routine, input, spec))
        })();
        let (routine, input, spec) = match preflight {
            Ok(preflight) => preflight,
            Err(rejection) => {
                let completed = state
                    .bots
                    .finish_run(
                        run,
                        crate::wire::RoutineRunStatus::Failed,
                        Some(rejection.message.clone()),
                    )
                    .map_err(internal)?;
                state
                    .swarm
                    .project_routine_outcome(&completed, None)
                    .await
                    .map_err(internal)?;
                return Err(rejection);
            }
        };
        if let Err(rejection) = state.ensure_capacity().await {
            let completed = state
                .bots
                .finish_run(
                    run,
                    crate::wire::RoutineRunStatus::Skipped,
                    Some("the gateway active-chat limit was reached".into()),
                )
                .map_err(internal)?;
            state
                .swarm
                .project_routine_outcome(&completed, None)
                .await
                .map_err(internal)?;
            return Err(rejection);
        }
        let session_id = run.session_id().to_owned();
        let label = format!("routine · {}", routine.id.get(..8).unwrap_or(&routine.id));
        let host = match HostHandle::start(
            state.store.clone(),
            Arc::clone(&state.config),
            spec,
            Arc::clone(&state.credentials),
            Arc::clone(&state.bots),
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
                let completed = state
                    .bots
                    .finish_run(
                        run,
                        crate::wire::RoutineRunStatus::Failed,
                        Some(message.clone()),
                    )
                    .map_err(internal)?;
                state
                    .swarm
                    .project_routine_outcome(&completed, None)
                    .await
                    .map_err(internal)?;
                return Err(internal(message));
            }
        };
        let bots = Arc::clone(&state.bots);
        state.sessions.insert(session_id.clone(), host.clone());
        let accepted =
            accept_routine_while_state_locked(&mut state, &host, run, input, bots.as_ref()).await;
        drop(state);
        match accepted {
            Ok(()) => {
                let broadcast = self.broadcast_sessions().await;
                let gateway = self.clone();
                tokio::spawn(async move {
                    host.wait_idle().await;
                    gateway.state.lock().await.sessions.remove(&session_id);
                    let _ = gateway.broadcast_sessions().await;
                });
                broadcast
            }
            Err(rejection) => {
                let completed = {
                    let state = self.state.lock().await;
                    state
                        .bots
                        .history(None)
                        .map(|runs| {
                            runs.into_iter()
                                .find(|run| {
                                    run.status != RoutineRunStatus::Running
                                        && run.session_id.as_deref() == Some(session_id.as_str())
                                })
                                .map(|run| (Arc::clone(&state.swarm), run))
                        })
                        .map_err(internal)
                };
                let projection = match completed {
                    Ok(Some((swarm, run))) => swarm
                        .project_routine_outcome(&run, None)
                        .await
                        .map(|_| ())
                        .map_err(internal),
                    Ok(None) => Ok(()),
                    Err(error) => Err(error),
                };
                let _ = host.stop_if_idle().await;
                self.state.lock().await.sessions.remove(&session_id);
                projection?;
                Err(rejection)
            }
        }
    }

    pub(crate) async fn profile(&self) -> std::result::Result<ProfileSnapshot, Rejection> {
        let _access = self.begin_mutation().await?;
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
        let approvals = background_approvals(&state.checkpoints, &state.activities)
            .await
            .map_err(internal)?;
        state.swarm.retry_pending();
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

    fn broadcast_swarms(&self, swarms: &[SwarmRecord]) {
        let _ = self.events.send(ServerFrame::new(ServerMessage::Swarms {
            request_id: None,
            swarms: swarms.to_vec(),
        }));
    }

    fn broadcast_swarm_attentions(&self, attentions: &[SwarmAttention]) {
        let _ = self
            .events
            .send(ServerFrame::new(ServerMessage::SwarmAttentions {
                attentions: attentions.to_vec(),
            }));
    }
}

async fn accept_routine_while_state_locked(
    _state: &mut tokio::sync::MutexGuard<'_, GatewayState>,
    host: &HostHandle,
    run: ActiveRoutineRun,
    input: String,
    bots: &BotStore,
) -> std::result::Result<(), Rejection> {
    host.run_routine(run, input, bots).await
}

async fn rollback_bot_update(
    state: &mut GatewayState,
    residents: &[HostHandle],
    configured_count: usize,
    previous: &crate::wire::BotRecord,
    attempted: &crate::wire::BotRecord,
    cause: Rejection,
) -> Rejection {
    let mut failures = Vec::new();
    if let Err(error) = state.bots.restore_bot(previous.clone()) {
        failures.push(format!("restoring the Bot profile failed: {error}"));
    }
    let authoritative = match state.bots.bot(&previous.id) {
        Ok(bot) => Some(bot),
        Err(error) => {
            failures.push(format!(
                "reading the authoritative Bot profile failed: {error}"
            ));
            None
        }
    };
    for (index, host) in residents.iter().enumerate() {
        let proven = authoritative.as_ref().is_some_and(|authoritative| {
            (index < configured_count && authoritative == attempted)
                || (index > configured_count && authoritative == previous)
        });
        if proven {
            continue;
        }
        let restored = match &authoritative {
            Some(authoritative) => host.reload_bot(authoritative.clone()).await.is_ok(),
            None => false,
        };
        if restored {
            continue;
        }
        failures.push(format!(
            "session {} could not be reconciled to the authoritative Bot profile",
            host.session_id()
        ));
        if !host.stop_if_idle().await {
            failures.push(format!(
                "session {} could not be stopped after rollback",
                host.session_id()
            ));
        }
        state.sessions.remove(host.session_id());
    }
    if failures.is_empty() {
        cause
    } else {
        internal(format!(
            "{}; Bot rollback was incomplete: {}",
            cause.message,
            failures.join("; ")
        ))
    }
}

async fn bot_update_residents(
    state: &mut GatewayState,
    bot_id: &str,
) -> std::result::Result<Vec<(HostHandle, ProviderCutoverStatus)>, Rejection> {
    let sessions = state
        .sessions
        .iter()
        .filter(|(_, host)| host.bot_id() == bot_id)
        .map(|(id, host)| (id.clone(), host.clone()))
        .collect::<Vec<_>>();
    let mut sessions = sessions;
    sessions.sort_by(|left, right| left.0.cmp(&right.0));
    let mut residents = Vec::new();
    let mut stopped = Vec::new();
    for (id, host) in sessions {
        if !host.is_alive() {
            stopped.push(id);
            continue;
        }
        match host.provider_cutover_status().await {
            Ok(status) => residents.push((host, status)),
            Err(rejection) if rejection.code == "gateway_stopped" => stopped.push(id),
            Err(rejection) => return Err(rejection),
        }
    }
    for id in stopped {
        state.sessions.remove(&id);
    }
    Ok(residents)
}

async fn notify_swarm_delivery_after_mutation(
    session_mutations: Arc<RwLock<()>>,
    swarm: Arc<SwarmStore>,
    target_bot_id: String,
) {
    let completed = session_mutations.read_owned().await;
    drop(completed);
    swarm.notify_pending(&target_bot_id);
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

fn bot_session_trees(bot_id: &str, summaries: &[SessionSummary]) -> (Vec<String>, Vec<String>) {
    let owned = summaries
        .iter()
        .filter(|session| session.session_context.bot_id == bot_id)
        .map(|session| session.session_id.clone())
        .collect::<HashSet<_>>();
    let roots = summaries
        .iter()
        .filter(|session| {
            owned.contains(&session.session_id)
                && session
                    .parent_session_id
                    .as_ref()
                    .is_none_or(|parent| !owned.contains(parent))
        })
        .map(|session| session.session_id.clone())
        .collect::<Vec<_>>();
    session_trees(roots, summaries)
}

async fn prepare_bot_session_tree_deletion(
    state: &mut GatewayState,
    bot_id: &str,
) -> std::result::Result<(Vec<String>, Vec<String>, SessionFileDeletion), Rejection> {
    let summaries = gateway_session_summaries(&state.checkpoints)
        .await
        .map_err(internal)?;
    let (_, session_ids) = bot_session_trees(bot_id, &summaries);
    let file_deletion = prepare_session_tree_deletion(state, &session_ids, true).await?;
    let summaries = gateway_session_summaries(&state.checkpoints)
        .await
        .map_err(internal)?;
    let (session_roots, session_ids) = bot_session_trees(bot_id, &summaries);
    Ok((session_roots, session_ids, file_deletion))
}

fn session_trees(roots: Vec<String>, summaries: &[SessionSummary]) -> (Vec<String>, Vec<String>) {
    let mut seen = HashSet::new();
    let mut session_ids = Vec::new();
    for root in &roots {
        if let Some(tree) = session_tree_ids(root, summaries) {
            for session_id in tree {
                if seen.insert(session_id.clone()) {
                    session_ids.push(session_id);
                }
            }
        }
    }
    (roots, session_ids)
}

async fn delete_session_trees(
    state: &mut GatewayState,
    roots: &[String],
    session_ids: &[String],
    allow_pending_swarm: bool,
) -> std::result::Result<Option<Rejection>, Rejection> {
    let mut file_deletion =
        prepare_session_tree_deletion(state, session_ids, allow_pending_swarm).await?;
    remove_session_trees(state, roots, session_ids, &mut file_deletion, false).await
}

async fn prepare_session_tree_deletion(
    state: &mut GatewayState,
    session_ids: &[String],
    allow_pending_swarm: bool,
) -> std::result::Result<SessionFileDeletion, Rejection> {
    if session_ids.is_empty() {
        return state
            .session_files
            .prepare_delete_sessions(session_ids)
            .await
            .map_err(internal);
    }
    if !allow_pending_swarm
        && state
            .swarm
            .has_pending_source_sessions(session_ids)
            .await
            .map_err(internal)?
    {
        return Err(Rejection {
            code: "session_has_pending_swarm_delivery",
            message: "wait for this chat's pending Swarm deliveries before deleting it".into(),
            fatal: false,
        });
    }
    let residents = session_ids
        .iter()
        .filter_map(|id| state.sessions.get(id).cloned())
        .collect::<Vec<_>>();
    for host in &residents {
        match host.provider_cutover_status().await {
            Ok(status) if status.idle => {}
            Ok(_) => {
                return Err(Rejection {
                    code: "agent_busy",
                    message: "finish or interrupt the active turn before deleting this chat".into(),
                    fatal: false,
                });
            }
            Err(rejection) if rejection.code == "gateway_stopped" => {}
            Err(rejection) => return Err(rejection),
        }
    }
    let file_deletion = state
        .session_files
        .prepare_delete_sessions(session_ids)
        .await
        .map_err(internal)?;
    for host in residents {
        if !host.stop_if_idle().await {
            return Err(Rejection {
                code: "agent_busy",
                message: "finish or interrupt the active turn before deleting this chat".into(),
                fatal: false,
            });
        }
    }
    Ok(file_deletion)
}

async fn remove_session_trees(
    state: &mut GatewayState,
    roots: &[String],
    session_ids: &[String],
    file_deletion: &mut SessionFileDeletion,
    missing_roots_are_deleted: bool,
) -> std::result::Result<Option<Rejection>, Rejection> {
    if session_ids.is_empty() {
        return Ok(None);
    }
    let roots = if missing_roots_are_deleted {
        let mut existing = Vec::new();
        for root in roots {
            if state
                .checkpoints
                .load(root)
                .await
                .map_err(internal)?
                .is_some()
            {
                existing.push(root.clone());
            }
        }
        existing
    } else {
        roots.to_vec()
    };
    if !state
        .checkpoints
        .delete_sessions(&roots)
        .await
        .map_err(internal)?
    {
        return Err(unknown_session());
    }

    for id in session_ids {
        state.sessions.remove(id);
    }
    let mut cleanup_errors = Vec::new();
    if let Err(error) = file_deletion.delete().await {
        cleanup_errors.push(error.to_string());
    }
    let catalog_lock = Arc::clone(&state.catalog_lock);
    let _catalog = catalog_lock.lock().await;
    match load_session_metadata(&state.checkpoints).await {
        Ok(mut metadata) => {
            for id in session_ids {
                metadata.remove(id);
            }
            if let Err(error) = save_session_metadata(&state.checkpoints, &metadata).await {
                cleanup_errors.push(error.to_string());
            }
        }
        Err(error) => cleanup_errors.push(error.to_string()),
    }
    match state.activities.lock() {
        Ok(mut activities) => {
            activities.retain(|id, _| !session_ids.iter().any(|deleted| deleted == id));
        }
        Err(_) => cleanup_errors.push("session activity lock is poisoned".into()),
    }
    Ok((!cleanup_errors.is_empty()).then(|| internal(cleanup_errors.join("; "))))
}

async fn disband_swarm_with_scratchpad(
    swarm: &SwarmStore,
    scratchpad: &ScratchpadStore,
    swarm_id: &str,
) -> std::result::Result<(), Rejection> {
    swarm.disband(swarm_id).await.map_err(invalid_swarm)?;
    let _ = scratchpad.clear_swarm(swarm_id).await;
    Ok(())
}

async fn receive<T>(
    receiver: oneshot::Receiver<std::result::Result<T, Rejection>>,
) -> std::result::Result<T, Rejection> {
    receiver.await.map_err(|_| stopped())?
}

fn swarm_message_submission(entry: BoardEntry) -> Submission {
    let message_id = entry.id;
    let author = if entry.author.bot_id == "user" {
        MessageAuthor::User
    } else {
        MessageAuthor::Peer {
            message_id: message_id.clone(),
            session_id: entry.source_session_id,
            handle: entry.author.handle,
        }
    };
    Submission {
        id: message_id.clone(),
        op: Op::Message {
            message: MessageSubmission {
                author,
                text: entry.text,
                attachments: Vec::new(),
                reply: None,
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

fn invalid_scratchpad_operation() -> Rejection {
    Rejection {
        code: "invalid_scratchpad",
        message: "scratchpad operation does not match its selected management scope".into(),
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

fn invalid_scratchpad_store(error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "invalid_scratchpad",
        message: error.to_string(),
        fatal: false,
    }
}

#[cfg(test)]
mod tests;
