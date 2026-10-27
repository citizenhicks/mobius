//! Per-chat agent ownership, event sequencing, replay, and authenticated operations.

use mobius::SessionId;
mod bot_events;
mod catalog;
mod deletion;
mod desktop;
#[path = "harness/mobius/host/extensions.rs"]
mod extensions;
mod files;
mod git;
mod harnesses;
#[path = "harness/mobius/host/live_chats.rs"]
mod live_chats;
mod memory;
mod profile;
#[path = "harness/mobius/host/providers.rs"]
mod providers;
mod replay;
mod routines;
mod session;
mod ssh;
mod telemetry;
mod transcript;

#[cfg(feature = "fixtures")]
pub use transcript::TranscriptReducer;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use chrono::Utc;
use mobius::backend::checkpoint::{
    ActiveExecution, CheckpointStore, EventPageRequest, ExecutionOutcome, ExecutionRecord,
    JournalEvent, SessionPageRequest, SessionSummary, event_turn_page, sqlite::SqliteCheckpoint,
};
use mobius::backend::session_files::SessionFileStore;
use mobius::middleware::FrontendExtensions;
use mobius::middleware::memory::Memory;
use mobius::protocol::{
    Event, EventMsg, FrontendContribution, FrontendEvent, MessageAuthor, MessageSubmission,
    ModelStepContentPhase, Op, RenderedBlock, ReviewDecision, Submission,
};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::bots::{ActiveRoutineRun, BeginRun, BotStore};
use crate::computer_runtime::{desktop::DesktopControl, remote_desktop::RemoteDesktop};
use crate::config::{
    ChatSpec, ConfigStore, CredentialStore, GatewayConfig,
    create_workspace_directory as create_workspace_directory_on_disk,
};
use crate::harness::ValidatedSubmission;
use crate::harness::mobius::extensions::ExtensionStore;
use crate::harness::mobius::provider_catalog::{
    configured_model_catalog, configured_model_choices, configured_model_routes,
    provider_instances, provider_statuses, route_choices,
};
use crate::sandbox::GatewaySandbox;
use crate::wire::{
    AgentComposition, GitDiffScope, HarnessConfig, MAX_FRAME_BYTES, ProfileSnapshot,
    ProviderConfig, ReadyPayload, RecordedEvent, RenderedPreview, RoutineRunStatus, RunStats,
    RunSummary, ServerFrame, ServerMessage, SessionActivity, SessionActivityState,
    SessionReadyPayload, SessionRecord, SessionRunGroup, SessionWidget, SharedFrame,
    SshIdentityRecord, VersionedAgentConfig, WorkspaceFileScope,
};
use crate::{Error, Result};

use self::catalog::{
    SessionCatalogMetadata, activity_catalog, gateway_catalog, hidden_bot_session_catalog,
    load_session_metadata, restore_pending_approval_activities, save_session_metadata,
    session_catalog, update_session_activity, validate_session_title,
};
use self::files::{
    WorkspaceFiles, WorkspaceRead, list as list_workspace_files, read as read_workspace_file,
    remove as delete_workspace_file, write as write_workspace_file,
};
use self::git::{
    approve_credential as approve_git_credential_on_host, diff as workspace_git_diff,
    diff_totals as workspace_git_diff_totals, probe_credential as probe_git_credential_on_host,
    status as git_status, switch_branch as switch_workspace_branch,
};
use self::profile::{
    active_run_summary, gateway_ready, gateway_run_stats, gateway_session_summaries,
    local_machine_name, provider_usage, recent_run_groups, session_tree_ids,
};
pub(crate) use self::replay::RenderEvents;
use self::replay::{
    FrameworkPresentation, ReplayEntry, classify_journal_sequence, compact_replay_deltas,
    durable_history_page, event_sequence, load_replay, project_record, publish_ready_and_pending,
    record_and_publish, update_widgets, validate_gateway_event,
};
pub(crate) use self::session::{HostHandle, SessionRuntimeState};
use self::session::{
    JournalDelivery, JournalSequence, LoadedReplay, SessionWidgets, TurnLifecycle, turn_lifecycle,
};
use self::ssh::{generate as generate_ssh_identity_on_host, identities as ssh_identities_on_host};

const COMMAND_CAPACITY: usize = 128;
const BROADCAST_CAPACITY: usize = 512;
const REPLAY_CAPACITY: usize = 1024;
const REPLAY_LOAD_PAGE_SIZE: usize = 8;
const MAX_REPLAY_BYTES: usize = MAX_FRAME_BYTES;
const SESSION_PAGE_SIZE: usize = 100;
const MAX_SESSION_DELETE_ROOTS: usize = 1_024;
const RECENT_RUN_LIMIT: usize = 30;

type SessionActivities = Arc<Mutex<catalog::SessionCatalog>>;

/// Machine-wide chat registry. A session has at most one resident agent owner.
#[derive(Clone)]
pub(crate) struct GatewayHost {
    pub(crate) desktop: Arc<DesktopControl>,
    pub(crate) remote_desktop: Arc<RemoteDesktop>,
    state: Arc<Mutex<GatewayState>>,
    capacity_gate: Arc<Mutex<()>>,
    storage_reads: Arc<telemetry::StorageMeasurements>,
    events: broadcast::Sender<ServerFrame>,
    work_activity: Arc<WorkActivity>,
    pub(crate) telemetry: Arc<crate::telemetry::Telemetry>,
}

pub(crate) struct WorkActivity {
    instance: Uuid,
    revision: watch::Sender<u64>,
}

impl WorkActivity {
    pub(crate) fn new() -> Self {
        Self {
            instance: Uuid::new_v4(),
            revision: watch::Sender::new(0),
        }
    }

    pub(crate) fn mark(&self) {
        self.revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub(crate) fn wake(&self) {
        // Storage commits need a fresh snapshot, not another execution grace period.
        self.revision.send_modify(|_| {});
    }
}

pub(crate) struct RuntimeActivity {
    pub(crate) idle: bool,
    pub(crate) activity_revision: String,
    pub(crate) next_routine_at: Option<String>,
    pub(crate) active_sessions: usize,
    pub(crate) running_routines: u64,
}

struct GatewayState {
    store: ConfigStore,
    config: Arc<StdMutex<GatewayConfig>>,
    credentials: Arc<CredentialStore>,
    bots: Arc<BotStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    memory: Arc<Memory>,
    session_files: Arc<SessionFileStore>,
    contributions: Vec<FrontendContribution>,
    // ponytail: one lock is enough for at most 32 tiny catalog writes.
    catalog_lock: Arc<Mutex<()>>,
    credential_catalog_gate: Arc<Mutex<()>>,
    session_mutations: Arc<RwLock<()>>,
    extension_mutations: Arc<Mutex<()>>,
    discovery_gate: Arc<Mutex<()>>,
    provider_epoch: Arc<AtomicU64>,
    activities: SessionActivities,
    provider_login: Arc<StdMutex<providers::ProviderLogins>>,
    sessions: HashMap<SessionId, HostHandle>,
    starting_sessions: Arc<StdMutex<HashMap<SessionId, String>>>,
    idle_cleanup_tasks: Vec<JoinHandle<()>>,
}

struct GatewayReadySnapshot {
    store: ConfigStore,
    configured_providers: BTreeMap<String, crate::config::ConfiguredProvider>,
    bot_defaults: Option<crate::wire::VersionedAgentConfig<AgentComposition>>,
    subagent_ceilings: mobius::middleware::subagents::SubagentCeilings,
    extensions: Vec<crate::wire::ExtensionRecord>,
    computer_view: crate::wire::ComputerView,
    max_active_sessions: usize,
    credentials: Arc<CredentialStore>,
    credential_catalog_gate: Arc<Mutex<()>>,
    bots: Arc<BotStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    memory: Arc<Memory>,
    contributions: Vec<FrontendContribution>,
    activities: SessionActivities,
}

async fn gateway_ready_after_unlock(
    state: tokio::sync::MutexGuard<'_, GatewayState>,
) -> std::result::Result<ReadyPayload, Rejection> {
    let snapshot = state.ready_snapshot()?;
    drop(state);
    gateway_ready(snapshot).await
}

pub(crate) struct HostSnapshot {
    pub(crate) ready: SessionReadyPayload,
    pub(crate) replay: OpenReplay,
}

/// What an opening client receives after `session_opened`.
pub(crate) enum OpenReplay {
    /// The replay frames after the client's resume cursor.
    Frames(Vec<crate::wire::SharedFrame>),
    /// The whole live transcript, for a fresh open or a cursor outside the window.
    Transcript {
        through_sequence: u64,
        page: crate::wire::TranscriptPage,
        next: Option<crate::wire::HistoryCursor>,
    },
}

pub(crate) struct SessionHistoryPage {
    pub(crate) page: crate::wire::TranscriptPage,
    pub(crate) next: Option<crate::wire::HistoryCursor>,
}

/// How a chat request is admitted against session mutations.
enum Admission {
    /// May start the chat, so remote execution must be available.
    Execute,
    /// Only reads durable state.
    Observe,
}

/// A chat that passed selection checks.
enum AuthorizedChat {
    Resident(HostHandle),
    /// A Bot's persistent chat that has never started.
    Unstarted {
        bot_id: Arc<str>,
    },
    Stored(ChatSpec),
}

#[derive(Debug, Clone)]
pub(crate) struct Rejection {
    pub(crate) code: &'static str,
    pub(crate) message: Arc<str>,
    pub(crate) fatal: bool,
}

impl Rejection {
    pub(crate) fn new(code: &'static str, message: impl Into<Arc<str>>) -> Self {
        Self {
            code,
            message: message.into(),
            fatal: false,
        }
    }

    pub(crate) fn fatal(mut self) -> Self {
        self.fatal = true;
        self
    }
}

pub(crate) type HostAccess = Arc<dyn Fn() -> Result<GatewayHost> + Send + Sync>;

impl GatewayHost {
    pub(crate) async fn pending_upload_capacity(&self) -> Result<usize> {
        let state = self.state.lock().await;
        let config = crate::config::lock(&state.config)?;
        Ok(config.connections.pending_uploads)
    }

    fn access(&self) -> HostAccess {
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
        let memory = Arc::new(Memory::new(Arc::clone(&checkpoints)));
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
        bots.attach_telemetry_notify(&telemetry.notify, &work_activity);
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

    pub(crate) async fn runtime_activity(&self) -> std::result::Result<RuntimeActivity, Rejection> {
        let before = *self.work_activity.revision.borrow();
        // Keep the registry stable while asking each resident actor. Actor idle
        // checks never acquire GatewayState; hidden routine sessions are included.
        let state = self.state.lock().await;
        let starting = !state
            .starting_sessions
            .lock()
            .map_err(|error| internal(format!("session startup lock is poisoned: {error}")))?
            .is_empty();
        let mut idle = !starting
            && !state.bots.has_running_routines().map_err(internal)?
            && !state.bots.has_pending_deliveries().map_err(internal)?;
        let mut active_sessions = 0;
        for session in state.sessions.values() {
            if session.inner.alive.load(Ordering::Acquire) && !session.runtime_is_idle().await? {
                idle = false;
                active_sessions += 1;
            }
        }
        // Routine reservations happen outside the registry lock; observe them
        // again and reject an idle result if any work changed during this query.
        let running_routines = state.bots.running_routine_count().map_err(internal)?;
        let now = Utc::now().timestamp();
        let next_routine_at = state.bots.next_routine_at(now).map_err(internal)?;
        idle &= running_routines == 0
            && next_routine_at.is_none_or(|at| at.timestamp() > now)
            && !state.bots.has_pending_deliveries().map_err(internal)?;
        drop(state);
        let after = *self.work_activity.revision.borrow();
        Ok(RuntimeActivity {
            active_sessions,
            running_routines,
            idle: idle && before == after,
            activity_revision: format!("{}:{after}", self.work_activity.instance),
            next_routine_at: next_routine_at.map(|at| at.to_rfc3339()),
        })
    }

    pub(crate) fn mark_runtime_activity(&self) {
        self.work_activity.mark();
    }

    pub(crate) fn activity_changes(&self) -> watch::Receiver<u64> {
        self.work_activity.revision.subscribe()
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

    async fn begin_credential_mutation(&self) -> tokio::sync::OwnedMutexGuard<()> {
        // Return an owned guard without retaining the gateway-wide state lock while waiting.
        let gate = Arc::clone(&self.state.lock().await.credential_catalog_gate);
        gate.lock_owned().await
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

    async fn contribution_state(&self) -> (Arc<Memory>, Vec<FrontendContribution>) {
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
        let config = HarnessConfig::Mobius(Arc::new(
            gateway
                .bot_defaults
                .as_ref()
                .map(|defaults| defaults.config.clone()) // rules-allow: the new Bot owns a copy of the configured default composition
                .ok_or_else(|| {
                    Rejection::new(
                        "gateway_setup_required",
                        "configure a provider before creating a Bot",
                    )
                })?,
        ));
        drop(gateway);
        let config = validate_bot_config(&state, config)?;
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
        let _mutation = self.begin_mutation().await?;
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
                crate::harness::ApprovalMode::Mobius(approval_policy),
            ) = (&config.config, approval)
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
                        approval_policy,
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
            let previous = state
                .bots
                .prepared
                .lock()
                .await
                .insert(id.into(), Arc::new(prepared));
            if let Some(previous) = previous {
                previous.invalidate();
            }
        }
        let bots = state.bots.bots().map_err(internal)?;
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
            state.bots.bot(bot_id).map_err(invalid_bot)?;
            (
                Arc::clone(&state.checkpoints),
                Arc::clone(&state.activities),
            )
        };
        hidden_bot_session_catalog(&checkpoints, &activities, bot_id)
            .await
            .map_err(internal)
    }

    async fn create_session_with_id(
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

    async fn start_reserved_session(
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

    async fn open_session_with_cache(
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
    async fn authorize_chat(
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
            .await
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

    async fn update_session_metadata(
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
                config.profile(),
                Arc::clone(&state.checkpoints),
                include_provider_usage.then(|| provider_usage(&config, &state.store)),
            )
        };
        drop(access);
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
        if let Some(usage_request) = usage_request {
            profile.provider_usage = usage_request.await.map_err(internal)?;
        }
        Ok(profile)
    }

    async fn broadcast_sessions(&self) -> std::result::Result<(), Rejection> {
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

    fn broadcast_bots(&self, bots: &[crate::wire::BotRecord]) {
        send_event(
            &self.events,
            ServerMessage::Bots {
                request_id: None,
                bots: bots.to_vec(),
            },
        );
    }
}

/// A Bot-defaults composition checked against the gateway's catalog and installed extensions.
#[derive(Debug)]
pub(crate) struct ValidatedComposition {
    config: AgentComposition,
}

impl ValidatedComposition {
    pub(crate) fn into_inner(self) -> AgentComposition {
        self.config
    }
}

impl std::ops::Deref for ValidatedComposition {
    type Target = AgentComposition;

    fn deref(&self) -> &AgentComposition {
        &self.config
    }
}

/// Checks a Bot configuration: a möbius composition against the gateway, others against
/// their harness.
fn validate_bot_config(
    state: &GatewayState,
    config: HarnessConfig,
) -> std::result::Result<crate::harness::ValidatedConfig, Rejection> {
    match config {
        HarnessConfig::Mobius(composition) => {
            let approval_policy = check_composition(state, &*state.config()?, &composition)?;
            Ok(crate::harness::ValidatedConfig::mobius(
                composition,
                approval_policy,
            ))
        }
        config => crate::harness::validate_cli(config).map_err(harness_unavailable),
    }
}

/// Checks a Bot or Bot-defaults composition against the gateway.
fn validate_composition(
    state: &GatewayState,
    gateway: &GatewayConfig,
    config: AgentComposition,
) -> std::result::Result<ValidatedComposition, Rejection> {
    check_composition(state, gateway, &config)?;
    Ok(ValidatedComposition { config })
}

/// The approval policy of a composition the gateway's catalog and extensions support.
fn check_composition(
    state: &GatewayState,
    gateway: &GatewayConfig,
    config: &AgentComposition,
) -> std::result::Result<mobius::backend::sandbox::ApprovalPolicy, Rejection> {
    let models =
        configured_model_choices(gateway, &state.store, &state.credentials).map_err(internal)?;
    let approval_policy =
        crate::config::validate_bot_compatibility(gateway, config, models.catalogs())
            .map_err(invalid_config)?;
    ExtensionStore::new(&state.store)
        .resolve(gateway, &config.extensions)
        .map_err(invalid_config)?;
    Ok(approval_policy)
}

fn validate_bot_workspace(
    state: &GatewayState,
    bot_id: &str,
    workspace: &Path,
) -> std::result::Result<crate::config::CanonicalWorkspace, Rejection> {
    state.bots.bot(bot_id).map_err(invalid_bot)?;
    crate::config::validate_chat_workspace(
        workspace,
        state.store.state_dir(),
        state.config()?.tls.as_ref(),
    )
    .map_err(invalid_workspace)
}

struct SessionStartGuard {
    id: SessionId,
    starting: Arc<StdMutex<HashMap<SessionId, String>>>,
}

impl Drop for SessionStartGuard {
    fn drop(&mut self) {
        mobius::sync::recover_lock(&self.starting).remove(&self.id);
    }
}

impl GatewayState {
    fn config(&self) -> std::result::Result<std::sync::MutexGuard<'_, GatewayConfig>, Rejection> {
        crate::config::lock(&self.config).map_err(internal)
    }

    fn ready_snapshot(&self) -> std::result::Result<GatewayReadySnapshot, Rejection> {
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

    fn reserve_start(
        &self,
        id: &SessionId,
        bot_id: &str,
    ) -> std::result::Result<SessionStartGuard, Rejection> {
        if self.resident_sessions() >= self.session_capacity()? {
            return Err(session_limit(self.session_capacity()?));
        }
        let mut starting = mobius::sync::recover_lock(&self.starting_sessions);
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

    fn resident_sessions(&self) -> usize {
        self.sessions.len() + mobius::sync::recover_lock(&self.starting_sessions).len()
    }

    fn session_capacity(&self) -> std::result::Result<usize, Rejection> {
        Ok(self.config()?.connections.active_sessions)
    }
}

impl GatewayHost {
    async fn ensure_capacity(
        &self,
    ) -> std::result::Result<tokio::sync::MutexGuard<'_, ()>, Rejection> {
        let capacity = self.capacity_gate.lock().await;
        self.remote_desktop
            .check_execution()
            .map_err(invalid_config)?;
        for attempt in 0..2 {
            let candidates = {
                let state = self.state.lock().await;
                if state.resident_sessions() < state.session_capacity()? {
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
                    if state.resident_sessions() < state.session_capacity()? {
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

fn session_limit(capacity: usize) -> Rejection {
    Rejection::new(
        "session_limit",
        format!("this gateway already has {capacity} connected or running chats"),
    )
}

async fn receive<T>(
    receiver: oneshot::Receiver<std::result::Result<T, Rejection>>,
) -> std::result::Result<T, Rejection> {
    receiver.await.map_err(stopped_by)?
}

fn stopped_by(error: impl std::fmt::Display) -> Rejection {
    Rejection::new(
        "gateway_stopped",
        format!("the gateway host stopped: {error}"),
    )
    .fatal()
}

fn stopped() -> Rejection {
    Rejection::new("gateway_stopped", "the gateway host stopped").fatal()
}

fn reject_pending_bot_deletion(bots: &BotStore) -> std::result::Result<(), Rejection> {
    if bots.has_pending_bot_deletion().map_err(internal)? {
        return Err(Rejection::new(
            "bot_deletion_recovery",
            "finish Bot deletion recovery before changing gateway state",
        ));
    }
    Ok(())
}

// Live mutations must complete follow-up effects without losing an earlier applied-write failure.
pub(crate) fn finish_publication<T>(
    publication: crate::Result<()>,
    follow_up: std::result::Result<T, Rejection>,
) -> std::result::Result<T, Rejection> {
    match (publication, follow_up) {
        (Ok(()), result) => result,
        (Err(error), Ok(_)) => Err(internal(error)),
        (Err(error), Err(follow_up)) => Err(internal(format!(
            "{error}; follow-up failed ({}): {}",
            follow_up.code, follow_up.message
        ))),
    }
}

fn internal(error: impl std::fmt::Display) -> Rejection {
    Rejection::new("gateway_error", error.to_string())
}

fn send_event(events: &broadcast::Sender<ServerFrame>, message: ServerMessage) {
    if let Err(error) = events.send(ServerFrame::new(message)) {
        tracing::debug!(%error, "gateway event had no subscribers");
    }
}

/// Publishes the session list and the background approvals it carries.
fn send_catalog(
    events: &broadcast::Sender<ServerFrame>,
    sessions: Vec<SessionRecord>,
    approvals: Vec<Arc<crate::wire::BackgroundApproval>>,
) {
    send_event(
        events,
        ServerMessage::Sessions {
            request_id: None,
            sessions,
        },
    );
    send_event(events, ServerMessage::BackgroundApprovals { approvals });
}

/// Tells clients about a failure that leaves the gateway usable.
fn send_warning(events: &broadcast::Sender<ServerFrame>, code: &'static str, message: Arc<str>) {
    send_event(
        events,
        ServerMessage::Error {
            code: code.into(),
            message,
            fatal: false,
        },
    );
}

/// `harness_unavailable` for a harness without an adapter, else an internal error.
fn harness_unavailable(error: Error) -> Rejection {
    match error {
        Error::HarnessUnavailable(message) => Rejection::new("harness_unavailable", message),
        error => internal(error),
    }
}

fn invalid_config(error: impl std::fmt::Display) -> Rejection {
    Rejection::new("invalid_config", error.to_string())
}

fn bot_delete_rejection(code: &'static str, message: &str) -> Rejection {
    Rejection::new(code, message)
}

fn invalid_workspace(error: impl std::fmt::Display) -> Rejection {
    Rejection::new("invalid_workspace", error.to_string())
}

fn invalid_session_workspace() -> Rejection {
    Rejection::new(
        "invalid_session_workspace",
        "the requested session belongs to another workspace",
    )
}

fn invalid_session_bot() -> Rejection {
    Rejection::new(
        "invalid_session_bot",
        "the requested session Bot identity does not match its durable owner",
    )
}

fn unknown_session() -> Rejection {
    Rejection::new("unknown_session", "the requested chat does not exist")
}

async fn require_catalog_session(
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

fn invalid_bot(error: impl std::fmt::Display) -> Rejection {
    Rejection::new("invalid_bot", error.to_string())
}

fn invalid_routine(error: impl std::fmt::Display) -> Rejection {
    Rejection::new("invalid_routine", error.to_string())
}

/// The gateway's frontend contributions followed by the memory's current one.
async fn with_memory_contribution(
    memory: &Memory,
    mut contributions: Vec<FrontendContribution>,
) -> mobius::Result<Vec<FrontendContribution>> {
    contributions.push(memory.global_contribution().await?);
    Ok(contributions)
}

fn memory_error(error: mobius::Error) -> Rejection {
    match error {
        mobius::Error::Tool(message) => Rejection::new("invalid_memory", message),
        error => internal(error),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
impl GatewayHost {
    pub(crate) async fn stop_idle_session(&self, session_id: &SessionId) {
        let host = self
            .state
            .lock()
            .await
            .sessions
            .remove(session_id)
            .expect("resident chat");
        assert!(host.stop_if_idle().await);
    }

    pub(crate) async fn is_resident(&self, session_id: &SessionId) -> bool {
        let state = self.state.lock().await;
        state.sessions.contains_key(session_id)
            || mobius::sync::recover_lock(&state.starting_sessions).contains_key(session_id)
    }

    pub(crate) async fn checkpoint_store(&self) -> Arc<dyn CheckpointStore> {
        Arc::clone(&self.state.lock().await.checkpoints)
    }
}

fn reject_persistent_session(summary: &SessionSummary) -> std::result::Result<(), Rejection> {
    if crate::bots::is_persistent(&summary.session_id, &summary.session_context.owner_id) {
        return Err(Rejection::new(
            "persistent_chat",
            "this conversation belongs permanently to its Bot",
        ));
    }
    Ok(())
}

pub(crate) fn session_file_rejection(error: mobius::Error) -> Rejection {
    let code = if matches!(&error, mobius::Error::StorageFull) {
        "storage_full"
    } else {
        "session_file_rejected"
    };
    Rejection {
        code,
        message: match error {
            mobius::Error::Tool(message) => message.into(),
            other => other.to_string().into(),
        },
        fatal: false,
    }
}
