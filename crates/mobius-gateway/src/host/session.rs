mod events;
mod runtime;

use super::{
    ActiveRoutineRun, AgentSender, Arc, AtomicBool, AtomicU64, BROADCAST_CAPACITY, BotStore,
    BuiltAgent, COMMAND_CAPACITY, ChatSpec, CheckpointStore, ConfigStore, CredentialStore,
    DesktopControl, Error, Event, EventMsg, EventPageRequest, ExecutionOutcome, FrontendEvent,
    FrontendExtensions, GatewayConfig, GatewaySandbox, GitDiffScope, HashSet, HostAccess,
    HostSnapshot, JournalEvent, LiveChats, MessageAuthor, MessageSubmission, ModelRouter,
    ModelStepContentPhase, Mutex, Op, Ordering, Path, PathBuf, ProviderConfig, Rejection,
    RemoteDesktop, ReplayEntry, Result, ReviewDecision, RoutineRunStatus, RunStats, RwLock,
    Scratchpad, ServerFrame, ServerMessage, SessionActivities, SessionActivity,
    SessionActivityState, SessionFileStore, SessionHistoryPage, SessionReadyPayload, SessionWidget,
    SharedFrame, StdMutex, Submission, Utc, Uuid, ValidatedSubmission, VecDeque, WorkActivity,
    WorkspaceFileScope, WorkspaceFiles, WorkspaceRead, active_run_summary, activity_catalog,
    assemble, broadcast, classify_journal_sequence, compact_replay_deltas, delete_workspace_file,
    durable_history_page, event_sequence, event_turn_page, git_status, internal, invalid_config,
    invalid_workspace, list_workspace_files, load_replay, mpsc, oneshot, project_record,
    publish_ready_and_pending, read_workspace_file, receive, record_and_publish,
    reject_pending_bot_deletion, replayable, stopped, switch_workspace_branch,
    update_session_activity, update_widgets, validate_gateway_event, workspace_git_diff,
    workspace_git_diff_totals, write_workspace_file,
};
use mobius::backend::model::provider::provider;
use mobius::protocol::MessageTarget;

pub(super) type SessionWidgets = Vec<(Arc<str>, Arc<mobius::protocol::FrontendWidget>)>;

#[derive(Clone)]
pub(crate) struct HostHandle {
    pub(super) inner: Arc<HostInner>,
}

pub(super) struct HostInner {
    pub(super) session_id: Arc<str>,
    pub(super) commands: mpsc::Sender<HostCommand>,
    pub(super) events: broadcast::Sender<SharedFrame>,
    pub(super) alive: Arc<AtomicBool>,
    pub(super) terminated: Arc<AtomicBool>,
    pub(super) termination: Arc<tokio::sync::Notify>,
    pub(super) session_mutations: Arc<RwLock<()>>,
    pub(super) realtime_voice: Arc<Mutex<()>>,
}

struct HostState {
    telemetry: Arc<crate::telemetry::Telemetry>,
    work_activity: Arc<WorkActivity>,
    store: ConfigStore,
    gateway: Arc<StdMutex<GatewayConfig>>,
    spec: ChatSpec,
    credentials: Arc<CredentialStore>,
    bots: Arc<BotStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    scratchpad: Arc<Scratchpad>,
    session_files: Arc<SessionFileStore>,
    alive: Arc<AtomicBool>,
    terminated: Arc<AtomicBool>,
    termination: Arc<tokio::sync::Notify>,
    session_mutations: Arc<RwLock<()>>,
    discovery_gate: Arc<Mutex<()>>,
    desktop: Arc<DesktopControl>,
    remote_desktop: Arc<RemoteDesktop>,
    live_chats: Arc<dyn LiveChats>,
    host_access: HostAccess,
    provider_epoch: Arc<AtomicU64>,
    activities: SessionActivities,
    running: RunningAgent,
    pending_turns: usize,
    pending_messages: HashSet<Arc<str>>,
    admissions: futures_util::stream::FuturesUnordered<
        std::pin::Pin<Box<dyn std::future::Future<Output = SourceAdmission> + Send + Sync>>,
    >,
    approval_active: bool,
    turn_error: Option<Arc<str>>,
    last_assistant_text: Option<String>,
    pending_startup: Vec<SharedFrame>,
    active_routine: Option<ActiveRoutine>,
    sequence: u64,
    pub(super) replay: VecDeque<ReplayEntry>,
    pub(super) replay_bytes: usize,
    pub(super) next_before_sequence: Option<u64>,
    pub(super) widgets: SessionWidgets,
    commands: mpsc::Receiver<HostCommand>,
    events: broadcast::Sender<SharedFrame>,
    gateway_events: broadcast::Sender<ServerFrame>,
    idle_waiters: Vec<oneshot::Sender<()>>,
}

pub(super) struct LoadedReplay {
    pub(super) latest_sequence: u64,
    pub(super) replay: VecDeque<ReplayEntry>,
    pub(super) replay_bytes: usize,
    pub(super) next_before_sequence: Option<u64>,
    pub(super) widgets: SessionWidgets,
}

struct SourceAdmission {
    id: Arc<str>,
    reserved: bool,
    result: mobius::Result<mobius::agent::MessageAcceptance>,
    reply: oneshot::Sender<std::result::Result<(), Rejection>>,
}

struct RunningAgent {
    session_id: Arc<str>,
    sender: Option<AgentSender>,
    events: mpsc::Receiver<JournalEvent>,
    model_router: Arc<ModelRouter>,
    sandbox: Arc<mobius::backend::sandbox::Sandbox>,
    frontend: FrontendExtensions,
    frontend_sink: mobius::middleware::FrontendEventSink,
    session: Arc<mobius::protocol::SessionConfiguredEvent>,
    gateway_sandbox: Arc<GatewaySandbox>,
    prepared: Arc<crate::assembly::PreparedBot>,
}

pub(super) struct ProviderCutoverStatus {
    pub(super) idle: bool,
}

pub(crate) struct RealtimeModel {
    /// The chat route used to prepare this call's instructions.
    pub(crate) route: String,
    pub(crate) call: mobius::middleware::voice::VoiceCallContext,
}

pub(super) struct ActiveRoutine {
    pub(super) run: ActiveRoutineRun,
    pub(super) submission_id: Arc<str>,
    pub(super) turn_id: Option<Arc<str>>,
    pub(super) failure: Option<Arc<str>>,
}

pub(super) enum HostCommand {
    RuntimeIsIdle {
        reply: oneshot::Sender<std::result::Result<bool, Rejection>>,
    },
    #[cfg(test)]
    BotId {
        reply: oneshot::Sender<Arc<str>>,
    },
    AcceptsFileAttachments {
        reply: oneshot::Sender<std::result::Result<bool, Rejection>>,
    },
    RealtimeModel {
        reply: oneshot::Sender<std::result::Result<RealtimeModel, Rejection>>,
    },
    Snapshot {
        last_sequence: Option<u64>,
        reply: oneshot::Sender<std::result::Result<HostSnapshot, Rejection>>,
    },
    Ready {
        reply: oneshot::Sender<std::result::Result<SessionReadyPayload, Rejection>>,
    },
    HistoryPage {
        before_sequence: Option<u64>,
        reply: oneshot::Sender<std::result::Result<SessionHistoryPage, Rejection>>,
    },
    Submit {
        submission: ValidatedSubmission,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    DeliverSource {
        submission: ValidatedSubmission,
        bot_id: Arc<str>,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    ReassignBot {
        bot_id: Arc<str>,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    AttachFolder {
        folder: PathBuf,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    SetMessagePinned {
        target: MessageTarget,
        pinned: bool,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    GitWorkspace {
        reply: oneshot::Sender<std::result::Result<(Arc<GatewaySandbox>, PathBuf), Rejection>>,
    },
    WorkspaceFiles {
        scope: WorkspaceFileScope,
        reply: oneshot::Sender<std::result::Result<WorkspaceFiles, Rejection>>,
    },
    ReadWorkspaceFile {
        path: String,
        offset: u64,
        max_bytes: usize,
        reply: oneshot::Sender<std::result::Result<WorkspaceRead, Rejection>>,
    },
    DeleteWorkspaceFile {
        path: String,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    WriteWorkspaceFile {
        path: String,
        content: String,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    SwitchGitBranch {
        branch: String,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    ProviderCutoverStatus {
        reply: oneshot::Sender<ProviderCutoverStatus>,
    },
    RunRoutine {
        run: ActiveRoutineRun,
        input: String,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    WaitIdle {
        reply: oneshot::Sender<()>,
    },
    StopIfIdle {
        reply: oneshot::Sender<bool>,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JournalSequence {
    AlreadyLoaded,
    Next,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JournalDelivery {
    Live,
    LoadedStartup,
    ReplacementStartup,
}

impl HostHandle {
    #[cfg(test)]
    pub(crate) async fn bot_id(&self) -> std::result::Result<Arc<str>, Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::BotId { reply }).await?;
        receiver.await.map_err(super::stopped_by)
    }

    pub(super) async fn reassign_bot(
        &self,
        bot_id: Arc<str>,
    ) -> std::result::Result<(), Rejection> {
        let _voice = self.claim_realtime_voice()?;
        self.request(|reply| HostCommand::ReassignBot { bot_id, reply })
            .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one chat actor receives each owned gateway dependency explicitly"
    )]
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the running agent and host state move into the spawned actor"
    )]
    pub(super) async fn start(
        store: ConfigStore,
        gateway: Arc<StdMutex<GatewayConfig>>,
        spec: ChatSpec,
        credentials: Arc<CredentialStore>,
        bots: Arc<BotStore>,
        checkpoints: Arc<dyn CheckpointStore>,
        scratchpad: Arc<Scratchpad>,
        session_files: Arc<SessionFileStore>,
        session_mutations: Arc<RwLock<()>>,
        discovery_gate: Arc<Mutex<()>>,
        desktop: Arc<DesktopControl>,
        remote_desktop: Arc<RemoteDesktop>,
        provider_epoch: Arc<AtomicU64>,
        activities: SessionActivities,
        work_activity: Arc<WorkActivity>,
        gateway_events: broadcast::Sender<ServerFrame>,
        session_id: String,
        origin_label: &str,
        live_chats: Arc<dyn LiveChats>,
        host_access: HostAccess,
    ) -> Result<Self> {
        let running = start_agent(
            Arc::clone(&gateway),
            &spec,
            &store,
            Arc::clone(&credentials),
            Arc::clone(&bots),
            Arc::clone(&checkpoints),
            Arc::clone(&scratchpad),
            Arc::clone(&session_files),
            Arc::clone(&discovery_gate),
            Arc::clone(&desktop),
            Arc::clone(&remote_desktop),
            &session_id,
            origin_label,
            None,
            Arc::clone(&provider_epoch),
            Arc::clone(&live_chats),
            Arc::clone(&host_access),
        )
        .await?;
        let alive = Arc::new(AtomicBool::new(true));
        let terminated = Arc::new(AtomicBool::new(false));
        let termination = Arc::new(tokio::sync::Notify::new());
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        let loaded = load_replay(checkpoints.as_ref(), &session_id, &running.frontend).await?;
        let shared_session_id = Arc::<str>::from(session_id);
        let mut guard = activities.lock().await;
        let activity = guard
            .activities
            .entry(Arc::clone(&shared_session_id))
            .or_default();
        activity.attention = super::replay::attention_count(&loaded.widgets);
        let awaiting_approval = activity.state == SessionActivityState::AwaitingApproval;
        drop(guard);
        let mut state = HostState {
            telemetry: (host_access)()?.telemetry,
            work_activity,
            store,
            gateway,
            spec,
            credentials,
            bots,
            checkpoints,
            scratchpad,
            session_files,
            discovery_gate,
            desktop,
            remote_desktop,
            live_chats,
            host_access,
            alive: Arc::clone(&alive),
            terminated: Arc::clone(&terminated),
            termination: Arc::clone(&termination),
            session_mutations: Arc::clone(&session_mutations),
            provider_epoch,
            activities,
            running,
            pending_turns: usize::from(awaiting_approval),
            pending_messages: HashSet::new(),
            admissions: futures_util::stream::FuturesUnordered::new(),
            approval_active: awaiting_approval,
            turn_error: None,
            last_assistant_text: None,
            pending_startup: Vec::new(),
            active_routine: None,
            sequence: loaded.latest_sequence,
            replay: loaded.replay,
            replay_bytes: loaded.replay_bytes,
            next_before_sequence: loaded.next_before_sequence,
            widgets: loaded.widgets,
            commands: receiver,
            events: events.clone(), // rules-allow: the actor publishes on the broadcast sender the handle subscribes to
            gateway_events,
            idle_waiters: Vec::new(),
        };
        if !state.spec.catalog_visible {
            let checkpoint = state.checkpoints.load(&shared_session_id).await?;
            if let Some(active) = checkpoint.and_then(|checkpoint| checkpoint.active_execution)
                && let Some(run) = state.bots.history(None)?.into_iter().find(|run| {
                    run.status == RoutineRunStatus::Running
                        && run.session_id.as_deref() == Some(&*shared_session_id)
                })
            {
                state.active_routine = Some(ActiveRoutine {
                    run: state.bots.resume_run(&run.id)?,
                    submission_id: active.submission_id,
                    turn_id: Some(active.turn_id),
                    failure: None,
                });
            }
        }
        state.reconcile_loaded_startup().await?;
        tokio::spawn(state.run());
        Ok(Self {
            inner: Arc::new(HostInner {
                session_id: shared_session_id,
                commands,
                events,
                alive,
                terminated,
                termination,
                session_mutations,
                realtime_voice: Arc::new(Mutex::new(())),
            }),
        })
    }

    #[must_use]
    pub(crate) fn session_id(&self) -> &Arc<str> {
        &self.inner.session_id
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<SharedFrame> {
        self.inner.events.subscribe()
    }

    pub(crate) fn claim_realtime_voice(
        &self,
    ) -> std::result::Result<tokio::sync::OwnedMutexGuard<()>, Rejection> {
        Arc::clone(&self.inner.realtime_voice)
            .try_lock_owned()
            .map_err(|error| {
                Rejection::new(
                    "realtime_voice",
                    format!("this chat already has an active voice call: {error}"),
                )
            })
    }

    pub(crate) async fn realtime_model(&self) -> std::result::Result<RealtimeModel, Rejection> {
        self.request(|reply| HostCommand::RealtimeModel { reply })
            .await
    }

    pub(crate) async fn accepts_file_attachments(&self) -> std::result::Result<bool, Rejection> {
        self.request(|reply| HostCommand::AcceptsFileAttachments { reply })
            .await
    }

    pub(super) fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::Acquire)
    }

    pub(crate) fn begin_session_file_mutation(
        &self,
        bots: &BotStore,
    ) -> std::result::Result<tokio::sync::OwnedRwLockReadGuard<()>, Rejection> {
        let mutation = try_begin_session_mutation(&self.inner.session_mutations, bots)?;
        if !self.is_alive() {
            return Err(stopped());
        }
        Ok(mutation)
    }

    pub(crate) async fn snapshot(
        &self,
        last_sequence: Option<u64>,
    ) -> std::result::Result<HostSnapshot, Rejection> {
        self.request(|reply| HostCommand::Snapshot {
            last_sequence,
            reply,
        })
        .await
    }

    /// The chat's current state, without replay.
    pub(crate) async fn ready(&self) -> std::result::Result<SessionReadyPayload, Rejection> {
        self.request(|reply| HostCommand::Ready { reply }).await
    }

    pub(crate) async fn history_page(
        &self,
        before_sequence: Option<u64>,
    ) -> std::result::Result<SessionHistoryPage, Rejection> {
        self.request(|reply| HostCommand::HistoryPage {
            before_sequence,
            reply,
        })
        .await
    }

    pub(crate) async fn submit(
        &self,
        submission: Submission,
    ) -> std::result::Result<(), Rejection> {
        self.submit_validated(
            ValidatedSubmission::new(submission)
                .map_err(|error| Rejection::new("invalid_submission", error.to_string()))?,
        )
        .await
    }

    pub(crate) async fn submit_validated(
        &self,
        submission: ValidatedSubmission,
    ) -> std::result::Result<(), Rejection> {
        self.request(|reply| HostCommand::Submit { submission, reply })
            .await
    }

    /// Delivers a source message only while this chat belongs to `bot_id`, and
    /// answers once the agent has accepted or rejected it.
    pub(super) async fn deliver_source(
        &self,
        submission: Submission,
        bot_id: Arc<str>,
    ) -> std::result::Result<(), Rejection> {
        self.deliver_validated_source(
            ValidatedSubmission::new(submission)
                .map_err(|error| Rejection::new("invalid_submission", error.to_string()))?,
            bot_id,
        )
        .await
    }

    pub(super) async fn deliver_validated_source(
        &self,
        submission: ValidatedSubmission,
        bot_id: Arc<str>,
    ) -> std::result::Result<(), Rejection> {
        self.request(|reply| HostCommand::DeliverSource {
            submission,
            bot_id,
            reply,
        })
        .await
    }

    pub(crate) async fn attach_folder(
        &self,
        folder: PathBuf,
    ) -> std::result::Result<(), Rejection> {
        self.request(|reply| HostCommand::AttachFolder { folder, reply })
            .await
    }

    pub(crate) async fn set_message_pinned(
        &self,
        target: MessageTarget,
        pinned: bool,
    ) -> std::result::Result<(), Rejection> {
        self.request(|reply| HostCommand::SetMessagePinned {
            target,
            pinned,
            reply,
        })
        .await
    }

    pub(crate) async fn git_diff(
        &self,
        scope: GitDiffScope,
    ) -> std::result::Result<String, Rejection> {
        let (sandbox, workspace) = self.git_workspace().await?;
        workspace_git_diff(&sandbox, &workspace, scope).await
    }

    pub(crate) async fn git_diff_totals(
        &self,
        scope: GitDiffScope,
    ) -> std::result::Result<crate::wire::DiffTotals, Rejection> {
        let (sandbox, workspace) = self.git_workspace().await?;
        workspace_git_diff_totals(&sandbox, &workspace, scope).await
    }

    async fn git_workspace(
        &self,
    ) -> std::result::Result<(Arc<GatewaySandbox>, PathBuf), Rejection> {
        self.request(|reply| HostCommand::GitWorkspace { reply })
            .await
    }

    pub(crate) async fn workspace_files(
        &self,
        scope: WorkspaceFileScope,
    ) -> std::result::Result<WorkspaceFiles, Rejection> {
        self.request(|reply| HostCommand::WorkspaceFiles { scope, reply })
            .await
    }

    pub(crate) async fn read_workspace_file(
        &self,
        path: String,
        offset: u64,
        max_bytes: usize,
    ) -> std::result::Result<WorkspaceRead, Rejection> {
        self.request(|reply| HostCommand::ReadWorkspaceFile {
            path,
            offset,
            max_bytes,
            reply,
        })
        .await
    }

    pub(crate) async fn delete_workspace_file(
        &self,
        path: String,
    ) -> std::result::Result<(), Rejection> {
        self.request(|reply| HostCommand::DeleteWorkspaceFile { path, reply })
            .await
    }

    pub(crate) async fn write_workspace_file(
        &self,
        path: String,
        content: String,
    ) -> std::result::Result<(), Rejection> {
        self.request(|reply| HostCommand::WriteWorkspaceFile {
            path,
            content,
            reply,
        })
        .await
    }

    pub(crate) async fn switch_git_branch(
        &self,
        branch: String,
    ) -> std::result::Result<(), Rejection> {
        self.request(|reply| HostCommand::SwitchGitBranch { branch, reply })
            .await
    }

    pub(super) async fn runtime_is_idle(&self) -> std::result::Result<bool, Rejection> {
        self.request(|reply| HostCommand::RuntimeIsIdle { reply })
            .await
    }

    pub(super) async fn provider_cutover_status(
        &self,
    ) -> std::result::Result<ProviderCutoverStatus, Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::ProviderCutoverStatus { reply })
            .await?;
        receiver.await.map_err(super::stopped_by)
    }

    pub(crate) async fn run_routine(
        &self,
        run: ActiveRoutineRun,
        input: String,
        bots: &BotStore,
    ) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        if let Err(error) = self
            .inner
            .commands
            .send(HostCommand::RunRoutine { run, input, reply })
            .await
        {
            let HostCommand::RunRoutine { run, .. } = error.0 else {
                unreachable!("only a routine command was sent")
            };
            bots.finish_run(
                run,
                RoutineRunStatus::Failed,
                Some("the agent stopped before the Bot routine began".into()),
            )
            .map_err(internal)?;
            return Err(stopped());
        }
        receive(receiver).await
    }

    pub(super) async fn wait_idle(&self) {
        let (reply, receiver) = oneshot::channel();
        if self.send(HostCommand::WaitIdle { reply }).await.is_ok()
            && let Err(error) = receiver.await
        {
            tracing::debug!(%error, "chat host stopped before reporting idle");
        }
    }

    pub(super) fn is_unreferenced(&self) -> bool {
        Arc::strong_count(&self.inner) == 1
    }

    pub(super) async fn stop_if_idle(&self) -> bool {
        let (reply, receiver) = oneshot::channel();
        let stopped = if self.send(HostCommand::StopIfIdle { reply }).await.is_err() {
            true
        } else {
            receiver.await.unwrap_or(true)
        };
        if stopped {
            self.wait_terminated().await;
        }
        stopped
    }

    pub(super) async fn shutdown(&self) {
        if let Err(rejection) = self.send(HostCommand::Shutdown).await {
            tracing::debug!(
                code = rejection.code,
                message = %rejection.message,
                "chat host stopped before shutdown"
            );
        }
        self.wait_terminated().await;
    }

    pub(crate) async fn wait_terminated(&self) {
        while !self.inner.terminated.load(Ordering::Acquire) {
            let terminated = self.inner.termination.notified();
            if self.inner.terminated.load(Ordering::Acquire) {
                return;
            }
            terminated.await;
        }
    }

    /// Sends the command built around a fresh reply channel and awaits the chat's answer.
    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<std::result::Result<T, Rejection>>) -> HostCommand,
    ) -> std::result::Result<T, Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(command(reply)).await?;
        receive(receiver).await
    }

    async fn send(&self, command: HostCommand) -> std::result::Result<(), Rejection> {
        if matches!(
            command,
            HostCommand::Shutdown
                | HostCommand::WaitIdle { .. }
                | HostCommand::StopIfIdle { .. }
                | HostCommand::ProviderCutoverStatus { .. }
        ) {
            return self
                .inner
                .commands
                .send(command)
                .await
                .map_err(super::stopped_by);
        }
        self.inner
            .commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Rejection::new(
                    "server_busy",
                    "the session request queue is full; retry later",
                ),
                mpsc::error::TrySendError::Closed(_) => stopped(),
            })
    }
}

fn try_begin_session_mutation(
    mutations: &Arc<RwLock<()>>,
    bots: &BotStore,
) -> std::result::Result<tokio::sync::OwnedRwLockReadGuard<()>, Rejection> {
    let mutation = Arc::clone(mutations).try_read_owned().map_err(|error| {
        Rejection::new(
            "gateway_busy",
            format!("retry after the gateway update finishes: {error}"),
        )
    })?;
    reject_pending_bot_deletion(bots)?;
    Ok(mutation)
}

/// Which configured setups one credential change invalidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ProviderRefresh<'a> {
    /// One API-key credential, stored against a single instance.
    Instance {
        instance: &'a str,
        base_url: Option<&'a str>,
    },
    /// One browser login, shared by every instance of that provider.
    Provider(&'a str),
}

pub(super) fn provider_refresh_matches(
    selection: &ProviderConfig,
    scope: &ProviderRefresh<'_>,
) -> Result<bool> {
    match scope {
        ProviderRefresh::Instance { instance, base_url } => {
            if selection.instance != *instance {
                return Ok(false);
            }
            let definition = provider(&selection.provider)?;
            let selected_base_url =
                crate::provider_catalog::selected_base_url(definition, selection);
            Ok(selected_base_url == *base_url)
        }
        ProviderRefresh::Provider(provider) => Ok(selection.provider == *provider),
    }
}

pub(super) fn fail_active_routine(
    bots: &BotStore,
    active: &mut Option<ActiveRoutine>,
    message: &str,
) -> Result<()> {
    let Some(active) = active.take() else {
        return Ok(());
    };
    bots.finish_run(active.run, RoutineRunStatus::Failed, Some(message.into()))
        .map(|_| ())
}

#[expect(
    clippy::too_many_arguments,
    reason = "agent assembly keeps chat and gateway dependencies explicit"
)]
async fn start_agent(
    gateway: Arc<StdMutex<GatewayConfig>>,
    spec: &ChatSpec,
    store: &ConfigStore,
    credentials: Arc<CredentialStore>,
    bots: Arc<BotStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    scratchpad: Arc<Scratchpad>,
    session_files: Arc<SessionFileStore>,
    discovery_gate: Arc<Mutex<()>>,
    desktop: Arc<DesktopControl>,
    remote_desktop: Arc<RemoteDesktop>,
    session_id: &str,
    origin_label: &str,
    prepared: Option<Arc<crate::assembly::PreparedBot>>,
    provider_epoch: Arc<AtomicU64>,
    live_chats: Arc<dyn LiveChats>,
    host_access: HostAccess,
) -> Result<RunningAgent> {
    let prepared = if let Some(prepared) = prepared {
        prepared
    } else {
        loop {
            let generation = bots.preparation_generation.load(Ordering::Acquire);
            let bot = Arc::new(bots.bot(&spec.bot_id)?);
            let epoch = provider_epoch.load(Ordering::Acquire);
            let cached = bots
                .prepared
                .lock()
                .await
                .get(&*bot.id)
                .filter(|prepared| prepared.matches_runtime(&bot) && prepared.epoch == epoch)
                .map(Arc::clone);
            if let Some(prepared) = cached {
                break prepared;
            }
            let preparation = {
                let config = crate::config::lock(&gateway)?;
                crate::assembly::prepare_bot(
                    &config,
                    Arc::clone(&bot),
                    store,
                    &credentials,
                    Arc::clone(&session_files),
                    epoch,
                    Arc::clone(remote_desktop.configuration()),
                )
            };
            let prepared = preparation.await;
            let mut cache = bots.prepared.lock().await;
            let current_bot = bots.bot(&spec.bot_id)?;
            // A refresh can finish while preparation waits for external resources.
            if generation != bots.preparation_generation.load(Ordering::Acquire)
                || epoch != provider_epoch.load(Ordering::Acquire)
                || bot.description != current_bot.description
                || bot.config.config != current_bot.config.config
            {
                continue;
            }
            if let Some(cached) = cache
                .get(&*bot.id)
                .filter(|cached| cached.matches_runtime(&bot) && cached.epoch == epoch)
            {
                break Arc::clone(cached);
            }
            let prepared = Arc::new(prepared?);
            if let Some(previous) = cache.insert(Arc::clone(&bot.id), Arc::clone(&prepared)) {
                previous.invalidate();
            }
            break prepared;
        }
    };
    if let Some(mut checkpoint) = checkpoints.load(session_id).await?
        && checkpoint.session_context.owner_id != spec.bot_id
    {
        checkpoint.sequence = checkpoint
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::Config("checkpoint sequence overflow".into()))?;
        Arc::make_mut(&mut checkpoint.session_context).owner_id = Arc::clone(&spec.bot_id);
        checkpoint.metadata.extend(spec.metadata()?);
        checkpoints.save(checkpoint, Vec::new(), None).await?;
    }
    let BuiltAgent {
        agent,
        model_router,
        sandbox,
        gateway_sandbox,
    } = assemble(
        gateway,
        spec,
        store,
        checkpoints,
        scratchpad,
        session_files,
        discovery_gate,
        desktop,
        remote_desktop,
        Some(session_id.into()),
        origin_label,
        Arc::clone(&prepared),
        Some(live_chats),
        Some(host_access),
    )
    .await?;
    let frontend_sink = agent.frontend_sink();
    let (sender, events, session, frontend) = agent.into_recorded_parts();
    let session_id = Arc::clone(&session.session_id);
    Ok(RunningAgent {
        session_id,
        sender: Some(sender),
        events,
        model_router,
        sandbox,
        frontend,
        frontend_sink,
        session,
        gateway_sandbox,
        prepared,
    })
}

async fn shutdown_agent(agent: RunningAgent) {
    let RunningAgent {
        sender, mut events, ..
    } = agent;
    drop(sender);
    while events.recv().await.is_some() {}
}
