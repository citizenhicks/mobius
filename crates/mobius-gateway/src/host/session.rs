mod events;
mod runtime;

use super::*;
use mobius::backend::model::provider::provider;
#[cfg(test)]
pub(in crate::host) use runtime::fail_queued_routine_commands;

pub(super) type SessionWidgets = Vec<((String, String), mobius::protocol::FrontendWidget)>;

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
    #[cfg(test)]
    pub(super) gateway_sandbox: std::sync::Weak<GatewaySandbox>,
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
    scratchpad: ScratchpadStore,
    session_files: SessionFileStore,
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
    pending_messages: HashSet<String>,
    admissions: futures_util::stream::FuturesUnordered<
        std::pin::Pin<Box<dyn std::future::Future<Output = SourceAdmission> + Send + Sync>>,
    >,
    approval_active: bool,
    turn_error: Option<String>,
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
    id: String,
    reserved: bool,
    result: mobius::Result<mobius::agent::MessageAcceptance>,
    reply: oneshot::Sender<std::result::Result<(), Rejection>>,
}

struct RunningAgent {
    session_id: String,
    sender: Option<AgentSender>,
    events: mpsc::Receiver<JournalEvent>,
    model_router: Arc<ModelRouter>,
    sandbox: Arc<mobius::backend::sandbox::Sandbox>,
    frontend: FrontendExtensions,
    frontend_sink: mobius::middleware::FrontendEventSink,
    session: mobius::protocol::SessionConfiguredEvent,
    gateway_sandbox: Arc<GatewaySandbox>,
    subagents: Option<Arc<mobius::middleware::subagents::Subagents>>,
    subagent_template: Option<Arc<OnceLock<AgentConfig>>>,
    tool_count: usize,
    prepared: Arc<crate::assembly::PreparedBot>,
}

pub(super) struct ProviderCutoverStatus {
    pub(super) idle: bool,
}

pub(crate) struct RealtimeModel {
    pub(crate) bot_name: String,
    pub(crate) bot_instructions: String,
    pub(crate) router: Arc<ModelRouter>,
    /// The selected voice route.
    pub(crate) voice: String,
    /// The chat route whose transport serves the voice.
    pub(crate) route: String,
    pub(crate) provider_instance: String,
    pub(crate) active_turn_id: Option<String>,
    pub(crate) checkpoints: Arc<dyn CheckpointStore>,
    pub(crate) frontend: mobius::middleware::FrontendEventSink,
}

pub(super) struct ActiveRoutine {
    pub(super) run: ActiveRoutineRun,
    pub(super) submission_id: String,
    pub(super) turn_id: Option<String>,
    pub(super) failure: Option<String>,
}

pub(super) enum HostCommand {
    RuntimeIsIdle {
        reply: oneshot::Sender<std::result::Result<bool, Rejection>>,
    },
    #[cfg(test)]
    BotId {
        reply: oneshot::Sender<String>,
    },
    AcceptsFileAttachments {
        reply: oneshot::Sender<std::result::Result<bool, Rejection>>,
    },
    RealtimeModel {
        reply: oneshot::Sender<std::result::Result<RealtimeModel, Rejection>>,
    },
    ObserveVoiceUsage {
        provider_instance: String,
        usage: mobius::protocol::TokenUsage,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
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
        submission: Submission,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    DeliverSource {
        submission: Submission,
        bot_id: String,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    ReassignBot {
        bot_id: String,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    AttachFolder {
        folder: PathBuf,
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
    pub(crate) async fn bot_id(&self) -> std::result::Result<String, Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::BotId { reply }).await?;
        receiver.await.map_err(|_| stopped())
    }

    pub(super) async fn reassign_bot(&self, bot_id: String) -> std::result::Result<(), Rejection> {
        let _voice = self.claim_realtime_voice()?;
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::ReassignBot { bot_id, reply })
            .await?;
        receive(receiver).await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one chat actor receives each owned gateway dependency explicitly"
    )]
    pub(super) async fn start(
        store: ConfigStore,
        gateway: Arc<StdMutex<GatewayConfig>>,
        spec: ChatSpec,
        credentials: Arc<CredentialStore>,
        bots: Arc<BotStore>,
        checkpoints: Arc<dyn CheckpointStore>,
        scratchpad: ScratchpadStore,
        session_files: SessionFileStore,
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
            scratchpad.clone(),
            session_files.clone(),
            Arc::clone(&discovery_gate),
            Arc::clone(&desktop),
            Arc::clone(&remote_desktop),
            session_id.clone(),
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
        let awaiting_approval = {
            let mut activities = activities.lock().await;
            let activity = activities.activities.entry(session_id.clone()).or_default();
            activity.attention = super::replay::attention_count(&loaded.widgets);
            activity.state == SessionActivityState::AwaitingApproval
        };
        let mut state = HostState {
            telemetry: Arc::clone(&(host_access)()?.telemetry),
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
            events: events.clone(),
            gateway_events,
            idle_waiters: Vec::new(),
        };
        if !state.spec.catalog_visible {
            let checkpoint = state.checkpoints.load(&session_id).await?;
            if let Some(active) = checkpoint.and_then(|checkpoint| checkpoint.active_execution)
                && let Some(run) = state.bots.history(None)?.into_iter().find(|run| {
                    run.status == RoutineRunStatus::Running
                        && run.session_id.as_deref() == Some(session_id.as_str())
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
        #[cfg(test)]
        let gateway_sandbox = Arc::downgrade(&state.running.gateway_sandbox);
        tokio::spawn(state.run());
        Ok(Self {
            inner: Arc::new(HostInner {
                session_id: session_id.into(),
                commands,
                events,
                alive,
                terminated,
                termination,
                session_mutations,
                realtime_voice: Arc::new(Mutex::new(())),
                #[cfg(test)]
                gateway_sandbox,
            }),
        })
    }

    #[must_use]
    pub(crate) fn session_id(&self) -> &str {
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
            .map_err(|_| {
                Rejection::new(
                    "realtime_voice",
                    "this chat already has an active voice call",
                )
            })
    }

    pub(crate) async fn realtime_model(&self) -> std::result::Result<RealtimeModel, Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::RealtimeModel { reply }).await?;
        receive(receiver).await
    }

    pub(crate) async fn observe_voice_usage(
        &self,
        provider_instance: String,
        usage: mobius::protocol::TokenUsage,
    ) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::ObserveVoiceUsage {
            provider_instance,
            usage,
            reply,
        })
        .await?;
        receive(receiver).await
    }

    pub(crate) async fn accepts_file_attachments(&self) -> std::result::Result<bool, Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::AcceptsFileAttachments { reply })
            .await?;
        receive(receiver).await
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
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::Snapshot {
            last_sequence,
            reply,
        })
        .await?;
        receive(receiver).await
    }

    /// The chat's current state, without replay.
    pub(crate) async fn ready(&self) -> std::result::Result<SessionReadyPayload, Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::Ready { reply }).await?;
        receive(receiver).await
    }

    pub(crate) async fn history_page(
        &self,
        before_sequence: Option<u64>,
    ) -> std::result::Result<SessionHistoryPage, Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::HistoryPage {
            before_sequence,
            reply,
        })
        .await?;
        receive(receiver).await
    }

    pub(crate) async fn submit(
        &self,
        submission: Submission,
    ) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::Submit { submission, reply }).await?;
        receive(receiver).await
    }

    /// Delivers a source message only while this chat belongs to `bot_id`, and
    /// answers once the agent has accepted or rejected it.
    pub(super) async fn deliver_source(
        &self,
        submission: Submission,
        bot_id: String,
    ) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::DeliverSource {
            submission,
            bot_id,
            reply,
        })
        .await?;
        receive(receiver).await
    }

    pub(crate) async fn attach_folder(
        &self,
        folder: PathBuf,
    ) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::AttachFolder { folder, reply })
            .await?;
        receive(receiver).await
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
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::GitWorkspace { reply }).await?;
        receiver.await.map_err(|_| stopped())?
    }

    pub(crate) async fn workspace_files(
        &self,
        scope: WorkspaceFileScope,
    ) -> std::result::Result<WorkspaceFiles, Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::WorkspaceFiles { scope, reply })
            .await?;
        receiver.await.map_err(|_| stopped())?
    }

    pub(crate) async fn read_workspace_file(
        &self,
        path: String,
        offset: u64,
        max_bytes: usize,
    ) -> std::result::Result<WorkspaceRead, Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::ReadWorkspaceFile {
            path,
            offset,
            max_bytes,
            reply,
        })
        .await?;
        receiver.await.map_err(|_| stopped())?
    }

    pub(crate) async fn delete_workspace_file(
        &self,
        path: String,
    ) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::DeleteWorkspaceFile { path, reply })
            .await?;
        receive(receiver).await
    }

    pub(crate) async fn write_workspace_file(
        &self,
        path: String,
        content: String,
    ) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::WriteWorkspaceFile {
            path,
            content,
            reply,
        })
        .await?;
        receive(receiver).await
    }

    pub(crate) async fn switch_git_branch(
        &self,
        branch: String,
    ) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::SwitchGitBranch { branch, reply })
            .await?;
        receive(receiver).await
    }

    pub(super) async fn runtime_is_idle(&self) -> std::result::Result<bool, Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::RuntimeIsIdle { reply }).await?;
        receive(receiver).await
    }

    pub(super) async fn provider_cutover_status(
        &self,
    ) -> std::result::Result<ProviderCutoverStatus, Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::ProviderCutoverStatus { reply })
            .await?;
        receiver.await.map_err(|_| stopped())
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
        if self.send(HostCommand::WaitIdle { reply }).await.is_ok() {
            let _ = receiver.await;
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
        let _ = self.send(HostCommand::Shutdown).await;
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

    async fn send(&self, command: HostCommand) -> std::result::Result<(), Rejection> {
        if matches!(
            command,
            HostCommand::Shutdown
                | HostCommand::WaitIdle { .. }
                | HostCommand::StopIfIdle { .. }
                | HostCommand::ObserveVoiceUsage { .. }
                | HostCommand::ProviderCutoverStatus { .. }
        ) {
            return self
                .inner
                .commands
                .send(command)
                .await
                .map_err(|_| stopped());
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
    let mutation = Arc::clone(mutations)
        .try_read_owned()
        .map_err(|_| Rejection::new("gateway_busy", "retry after the gateway update finishes"))?;
    reject_pending_bot_deletion(bots)?;
    Ok(mutation)
}

/// Which configured setups one credential change invalidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ProviderRefresh {
    /// One API-key credential, stored against a single instance.
    Instance {
        instance: String,
        base_url: Option<String>,
    },
    /// One browser login, shared by every instance of that provider.
    Provider(String),
}

pub(super) fn provider_refresh_matches(
    selection: &ProviderConfig,
    scope: &ProviderRefresh,
) -> Result<bool> {
    match scope {
        ProviderRefresh::Instance { instance, base_url } => {
            if selection.instance != *instance {
                return Ok(false);
            }
            let definition = provider(&selection.provider)?;
            let selected_base_url =
                crate::provider_catalog::selected_base_url(definition, selection);
            Ok(selected_base_url == base_url.as_deref())
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
    bots.finish_run(
        active.run,
        RoutineRunStatus::Failed,
        Some(message.to_string()),
    )
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
    scratchpad: ScratchpadStore,
    session_files: SessionFileStore,
    discovery_gate: Arc<Mutex<()>>,
    desktop: Arc<DesktopControl>,
    remote_desktop: Arc<RemoteDesktop>,
    session_id: String,
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
            let bot = bots.bot(&spec.bot_id)?;
            let epoch = provider_epoch.load(Ordering::Acquire);
            let cached = bots
                .prepared
                .lock()
                .await
                .get(&bot.id)
                .filter(|prepared| prepared.matches_runtime(&bot) && prepared.epoch == epoch)
                .cloned();
            if let Some(prepared) = cached {
                break prepared;
            }
            let config = gateway
                .lock()
                .map_err(|_| Error::Config("gateway configuration lock is poisoned".into()))?
                .clone();
            let prepared = crate::assembly::prepare_bot(
                &config,
                bot.clone(),
                store,
                &credentials,
                session_files.clone(),
                epoch,
                Arc::clone(remote_desktop.configuration()),
            )
            .await;
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
                .get(&bot.id)
                .filter(|cached| cached.matches_runtime(&bot) && cached.epoch == epoch)
            {
                break Arc::clone(cached);
            }
            let prepared = Arc::new(prepared?);
            if let Some(previous) = cache.insert(spec.bot_id.clone(), Arc::clone(&prepared)) {
                previous.invalidate();
            }
            break prepared;
        }
    };
    if let Some(mut checkpoint) = checkpoints.load(&session_id).await?
        && checkpoint.session_context.owner_id != spec.bot_id
    {
        checkpoint.sequence = checkpoint
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::Config("checkpoint sequence overflow".into()))?;
        checkpoint.session_context.owner_id.clone_from(&spec.bot_id);
        checkpoint.metadata.extend(spec.metadata()?);
        checkpoints.save(&checkpoint, &[], None).await?;
    }
    let BuiltAgent {
        agent,
        model_router,
        sandbox,
        gateway_sandbox,
        subagents,
        subagent_template,
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
        Some(session_id),
        origin_label,
        Arc::clone(&prepared),
        Some(live_chats),
        Some(host_access),
    )
    .await?;
    let session = agent.session().clone();
    let frontend = agent.frontend().clone();
    let frontend_sink = agent.frontend_sink();
    let tool_count = agent.tool_count();
    let session_id = session.session_id.clone();
    let (sender, events) = agent.into_recorded_parts();
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
        subagents,
        subagent_template,
        tool_count,
        prepared,
    })
}

pub(super) fn runtime_accepts_attachments(frontend: &FrontendExtensions) -> bool {
    frontend
        .contributions()
        .iter()
        .any(|contribution| contribution.accepts_file_attachments)
}

async fn shutdown_agent(agent: RunningAgent) {
    let RunningAgent {
        sender,
        mut events,
        subagent_template,
        ..
    } = agent;
    drop(sender);
    while events.recv().await.is_some() {}
    drop(subagent_template);
}
