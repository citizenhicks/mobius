use mobius::SessionId;
mod events;
mod runtime;

use super::{
    ActiveRoutineRun, Arc, AtomicBool, BROADCAST_CAPACITY, BotStore, COMMAND_CAPACITY, ChatSpec,
    CheckpointStore, Error, Event, EventMsg, EventPageRequest, ExecutionOutcome, FrontendEvent,
    GatewaySandbox, GitDiffScope, HashSet, HostSnapshot, JournalEvent, MessageAuthor,
    MessageSubmission, ModelStepContentPhase, Mutex, Op, Ordering, Path, PathBuf, ProviderConfig,
    Rejection, ReplayEntry, Result, ReviewDecision, RoutineRunStatus, RunStats, RwLock,
    ServerFrame, ServerMessage, SessionActivities, SessionActivity, SessionActivityState,
    SessionHistoryPage, SessionReadyPayload, SessionWidget, SharedFrame, Submission, Utc, Uuid,
    ValidatedSubmission, VecDeque, WorkActivity, WorkspaceFileScope, WorkspaceFiles, WorkspaceRead,
    active_run_summary, activity_catalog, broadcast, classify_journal_sequence,
    compact_replay_deltas, delete_workspace_file, durable_history_page, event_sequence, git_status,
    internal, invalid_config, invalid_workspace, list_workspace_files, load_replay, mpsc, oneshot,
    project_record, publish_ready_and_pending, read_workspace_file, receive, record_and_publish,
    reject_pending_bot_deletion, stopped, switch_workspace_branch, update_session_activity,
    update_widgets, validate_gateway_event, workspace_git_diff, workspace_git_diff_totals,
    write_workspace_file,
};
use crate::harness::mobius::RealtimeModel;
use crate::harness::{HarnessContext, HarnessSession};
use mobius::backend::model::provider::provider;
use mobius::protocol::MessageTarget;

pub(super) type SessionWidgets = Vec<(Arc<str>, Arc<mobius::protocol::FrontendWidget>)>;

#[derive(Clone)]
pub(crate) struct HostHandle {
    pub(super) inner: Arc<HostInner>,
}

pub(super) struct HostInner {
    pub(super) session_id: SessionId,
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
    storage_reads: Arc<super::telemetry::StorageMeasurements>,
    work_activity: Arc<WorkActivity>,
    harness: Arc<HarnessContext>,
    spec: ChatSpec,
    bots: Arc<BotStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    alive: Arc<AtomicBool>,
    terminated: Arc<AtomicBool>,
    termination: Arc<tokio::sync::Notify>,
    session_mutations: Arc<RwLock<()>>,
    activities: SessionActivities,
    running: Box<dyn HarnessSession>,
    pending_turns: usize,
    pending_messages: HashSet<Arc<str>>,
    admissions: futures_util::stream::FuturesUnordered<
        std::pin::Pin<Box<dyn std::future::Future<Output = SourceAdmission> + Send + Sync>>,
    >,
    approval_active: bool,
    compacted_in_turn: bool,
    idle_turn: Option<Arc<str>>,
    turn_error: Option<Arc<str>>,
    last_assistant_text: Option<String>,
    pending_startup: Vec<SharedFrame>,
    active_routine: Option<ActiveRoutine>,
    sequence: u64,
    pub(super) replay: VecDeque<ReplayEntry>,
    pub(super) replay_bytes: usize,
    pub(super) transcript: super::transcript::TranscriptReducer,
    pub(super) widgets: SessionWidgets,
    pub(super) token_usage: Option<Arc<mobius::protocol::TokenUsageInfo>>,
    commands: mpsc::Receiver<HostCommand>,
    events: broadcast::Sender<SharedFrame>,
    gateway_events: broadcast::Sender<ServerFrame>,
    idle_waiters: Vec<oneshot::Sender<()>>,
}

pub(super) struct LoadedReplay {
    pub(super) latest_sequence: u64,
    pub(super) replay: VecDeque<ReplayEntry>,
    pub(super) replay_bytes: usize,
    pub(super) transcript: super::transcript::TranscriptReducer,
    pub(super) widgets: SessionWidgets,
    pub(super) token_usage: Option<Arc<mobius::protocol::TokenUsageInfo>>,
}

struct SourceAdmission {
    id: Arc<str>,
    reserved: bool,
    result: mobius::Result<crate::harness::MessageAcceptance>,
    reply: oneshot::Sender<std::result::Result<(), Rejection>>,
}

pub(super) struct ProviderCutoverStatus {
    pub(super) idle: bool,
}

pub(super) struct ActiveRoutine {
    pub(super) run: ActiveRoutineRun,
    pub(super) submission_id: Arc<str>,
    pub(super) turn_id: Option<Arc<str>>,
    pub(super) failure: Option<Arc<str>>,
}

/// The run state gateway code reads from a session's durable checkpoint.
pub(crate) struct SessionRuntimeState {
    pub(crate) active_turn: Option<ActiveTurn>,
    /// An approval is durable, whether or not its decision has arrived.
    pub(crate) approval_pending: bool,
    pub(crate) undecided_approval: Option<mobius::protocol::ExecApprovalRequestEvent>,
    pub(crate) has_pending_messages: bool,
    pub(crate) stats: RunStats,
}

pub(crate) struct ActiveTurn {
    pub(crate) turn_id: Arc<str>,
    pub(crate) submission_id: Arc<str>,
    pub(crate) author: Arc<MessageAuthor>,
    pub(crate) started_at_ms: i64,
}

impl SessionRuntimeState {
    pub(crate) async fn load(
        checkpoints: &dyn CheckpointStore,
        session_id: &SessionId,
    ) -> Result<Option<Self>> {
        Ok(checkpoints
            .load(session_id)
            .await?
            .map(Self::from_checkpoint))
    }

    fn from_checkpoint(checkpoint: mobius::backend::checkpoint::Checkpoint) -> Self {
        let stats = RunStats {
            completed: checkpoint.execution_stats,
            active: checkpoint
                .active_execution
                .as_ref()
                .map(|active| active_run_summary(&checkpoint.session_id, active)),
        };
        Self {
            active_turn: checkpoint.active_execution.map(|active| ActiveTurn {
                turn_id: active.turn_id,
                submission_id: active.submission_id,
                author: active.author,
                started_at_ms: active.started_at_ms,
            }),
            approval_pending: checkpoint.pending_approval.is_some(),
            undecided_approval: checkpoint
                .pending_approval
                .filter(|pending| !pending.decision_received)
                .as_ref()
                .map(mobius::backend::checkpoint::PendingApproval::request_event),
            has_pending_messages: !checkpoint.pending_messages.is_empty(),
            stats,
        }
    }

    /// No turn, approval, or queued message remains in the checkpoint.
    pub(super) fn is_settled(&self) -> bool {
        self.active_turn.is_none() && !self.approval_pending && !self.has_pending_messages
    }
}

/// The turn boundaries gateway bookkeeping follows in the agent's event stream.
pub(super) enum TurnLifecycle<'a> {
    Started {
        turn_id: &'a Arc<str>,
    },
    ApprovalRequested {
        turn_id: &'a Arc<str>,
        request_id: &'a Arc<str>,
    },
    /// The turn ended; `abort_reason` is set when it was aborted.
    Finished {
        turn_id: &'a Arc<str>,
        abort_reason: Option<&'a Arc<str>>,
    },
}

pub(super) fn turn_lifecycle(event: &EventMsg) -> Option<TurnLifecycle<'_>> {
    match event {
        EventMsg::TurnStarted(turn) => Some(TurnLifecycle::Started {
            turn_id: &turn.turn_id,
        }),
        EventMsg::ExecApprovalRequest(request) => Some(TurnLifecycle::ApprovalRequested {
            turn_id: &request.turn_id,
            request_id: &request.id,
        }),
        EventMsg::TurnComplete(turn) => Some(TurnLifecycle::Finished {
            turn_id: &turn.turn_id,
            abort_reason: None,
        }),
        EventMsg::TurnAborted(turn) => Some(TurnLifecycle::Finished {
            turn_id: &turn.turn_id,
            abort_reason: Some(&turn.reason),
        }),
        _ => None,
    }
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
        before: Option<crate::wire::HistoryCursor>,
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
    SetMemory {
        memory: Option<bool>,
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
        harness: Arc<HarnessContext>,
        spec: ChatSpec,
        session_mutations: Arc<RwLock<()>>,
        activities: SessionActivities,
        work_activity: Arc<WorkActivity>,
        gateway_events: broadcast::Sender<ServerFrame>,
        session_id: SessionId,
        origin_label: &str,
    ) -> Result<Self> {
        let running = crate::harness::start(&harness, &spec, &session_id, origin_label).await?;
        let alive = Arc::new(AtomicBool::new(true));
        let terminated = Arc::new(AtomicBool::new(false));
        let termination = Arc::new(tokio::sync::Notify::new());
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        let loaded = load_replay(harness.checkpoints.as_ref(), &session_id, &*running).await?;
        let shared_session_id = session_id;
        let mut guard = activities.lock().await;
        let activity = guard
            .activities
            .entry(shared_session_id.share())
            .or_default();
        activity.attention = super::replay::attention_count(&loaded.widgets);
        let awaiting_approval = activity.state == SessionActivityState::AwaitingApproval;
        drop(guard);
        let gateway_host = (harness.host_access)()?;
        let mut state = HostState {
            telemetry: gateway_host.telemetry,
            storage_reads: gateway_host.storage_reads,
            work_activity,
            bots: Arc::clone(&harness.bots),
            checkpoints: Arc::clone(&harness.checkpoints),
            harness,
            spec,
            alive: Arc::clone(&alive),
            terminated: Arc::clone(&terminated),
            termination: Arc::clone(&termination),
            session_mutations: Arc::clone(&session_mutations),
            activities,
            running,
            pending_turns: usize::from(awaiting_approval),
            pending_messages: HashSet::new(),
            admissions: futures_util::stream::FuturesUnordered::new(),
            approval_active: awaiting_approval,
            compacted_in_turn: false,
            idle_turn: None,
            turn_error: None,
            last_assistant_text: None,
            pending_startup: Vec::new(),
            active_routine: None,
            sequence: loaded.latest_sequence,
            replay: loaded.replay,
            replay_bytes: loaded.replay_bytes,
            transcript: loaded.transcript,
            widgets: loaded.widgets,
            token_usage: loaded.token_usage,
            commands: receiver,
            events: events.clone(), // rules-allow: the actor publishes on the broadcast sender the handle subscribes to
            gateway_events,
            idle_waiters: Vec::new(),
        };
        if !state.spec.catalog_visible {
            let runtime =
                SessionRuntimeState::load(state.checkpoints.as_ref(), &shared_session_id).await?;
            if let Some(active) = runtime.and_then(|runtime| runtime.active_turn)
                && let Some(run) = state.bots.history(None)?.into_iter().find(|run| {
                    run.status == RoutineRunStatus::Running
                        && run.session_id.as_ref() == Some(&shared_session_id)
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
    pub(crate) fn session_id(&self) -> &SessionId {
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
        before: Option<crate::wire::HistoryCursor>,
    ) -> std::result::Result<SessionHistoryPage, Rejection> {
        self.request(|reply| HostCommand::HistoryPage { before, reply })
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

    pub(crate) async fn set_memory(
        &self,
        memory: Option<bool>,
    ) -> std::result::Result<(), Rejection> {
        self.request(|reply| HostCommand::SetMemory { memory, reply })
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
        base_url: Option<&'a mobius::backend::model::provider::BaseUrl>,
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
                crate::harness::mobius::provider_catalog::selected_base_url(definition, selection);
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

async fn shutdown_agent(mut agent: Box<dyn HarnessSession>) {
    agent.shutdown();
    while agent.events().recv().await.is_some() {}
}
