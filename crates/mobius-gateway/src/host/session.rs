mod events;
mod runtime;

use super::*;
use mobius::backend::model::provider::provider;
use mobius::middleware::bots::BotsBackend;
#[cfg(test)]
pub(in crate::host) use runtime::fail_queued_routine_commands;

pub(super) type SessionWidgets = Vec<((String, String), mobius::protocol::FrontendWidget)>;

#[derive(Clone)]
pub(crate) struct HostHandle {
    pub(super) inner: Arc<HostInner>,
}

impl Drop for HostHandle {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 2 {
            let _ = self.inner.commands.try_send(HostCommand::CapacityChanged);
        }
    }
}

pub(super) struct HostInner {
    pub(super) session_id: Arc<str>,
    pub(super) bot_id: Arc<str>,
    pub(super) commands: mpsc::Sender<HostCommand>,
    pub(super) events: broadcast::Sender<ServerFrame>,
    pub(super) accepts_file_attachments: Arc<AtomicBool>,
    pub(super) alive: Arc<AtomicBool>,
    pub(super) terminated: Arc<AtomicBool>,
    pub(super) termination: Arc<tokio::sync::Notify>,
    pub(super) session_mutations: Arc<RwLock<()>>,
}

struct HostState {
    store: ConfigStore,
    gateway: Arc<StdMutex<GatewayConfig>>,
    spec: ChatSpec,
    credentials: Arc<CredentialStore>,
    bots: Arc<BotStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    scratchpad: ScratchpadStore,
    session_files: SessionFileStore,
    swarm: Arc<SwarmStore>,
    accepts_file_attachments: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    terminated: Arc<AtomicBool>,
    termination: Arc<tokio::sync::Notify>,
    session_mutations: Arc<RwLock<()>>,
    provider_epoch: Arc<AtomicU64>,
    activities: SessionActivities,
    running: RunningAgent,
    pending_turns: usize,
    pending_messages: HashSet<String>,
    approval_active: bool,
    turn_error: Option<String>,
    last_assistant_text: Option<String>,
    restart_after_turn: bool,
    pending_startup: Vec<ServerFrame>,
    active_routine: Option<ActiveRoutine>,
    sequence: u64,
    pub(super) replay: VecDeque<ServerFrame>,
    pub(super) replay_bytes: usize,
    pub(super) next_before_sequence: Option<u64>,
    pub(super) widgets: SessionWidgets,
    commands: mpsc::Receiver<HostCommand>,
    events: broadcast::Sender<ServerFrame>,
    gateway_events: broadcast::Sender<ServerFrame>,
    idle_waiters: Vec<oneshot::Sender<()>>,
}

pub(super) struct LoadedReplay {
    pub(super) latest_sequence: u64,
    pub(super) replay: VecDeque<ServerFrame>,
    pub(super) replay_bytes: usize,
    pub(super) next_before_sequence: Option<u64>,
    pub(super) widgets: SessionWidgets,
}

struct RunningAgent {
    session_id: String,
    sender: Option<AgentSender>,
    events: mpsc::Receiver<JournalEvent>,
    model_router: Arc<ModelRouter>,
    frontend: FrontendExtensions,
    session: mobius::protocol::SessionConfiguredEvent,
    gateway_sandbox: Arc<GatewaySandbox>,
    subagent_template: Option<Arc<OnceLock<AgentConfig>>>,
    tool_count: usize,
    provider_epoch: u64,
}

struct ReusableModelRouter {
    router: Arc<ModelRouter>,
    provider_epoch: u64,
}

pub(super) struct ProviderCutoverStatus {
    pub(super) idle: bool,
}

pub(super) struct ActiveRoutine {
    pub(super) run: ActiveRoutineRun,
    pub(super) submission_id: String,
    pub(super) turn_id: Option<String>,
    pub(super) failure: Option<String>,
}

pub(super) enum HostCommand {
    Snapshot {
        last_sequence: Option<u64>,
        reply: oneshot::Sender<std::result::Result<HostSnapshot, Rejection>>,
    },
    HistoryPage {
        before_sequence: Option<u64>,
        reply: oneshot::Sender<std::result::Result<SessionHistoryPage, Rejection>>,
    },
    Submit {
        submission: Submission,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    ReloadBot {
        bot: crate::wire::BotRecord,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    GitDiff {
        scope: GitDiffScope,
        reply: oneshot::Sender<std::result::Result<String, Rejection>>,
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
    WriteWorkspaceFile {
        path: String,
        content: String,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    SwitchGitBranch {
        branch: String,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    RefreshProvider {
        scope: ProviderRefresh,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    RefreshExtension {
        id: String,
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
    CapacityChanged,
    StopIfIdle {
        reply: oneshot::Sender<bool>,
    },
}

enum Next {
    Command(Option<HostCommand>),
    Event(Option<JournalEvent>),
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
    pub(crate) fn bot_id(&self) -> &str {
        &self.inner.bot_id
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one chat actor receives each owned gateway dependency explicitly"
    )]
    pub(crate) async fn start(
        store: ConfigStore,
        gateway: Arc<StdMutex<GatewayConfig>>,
        spec: ChatSpec,
        credentials: Arc<CredentialStore>,
        bots: Arc<BotStore>,
        checkpoints: Arc<dyn CheckpointStore>,
        scratchpad: ScratchpadStore,
        session_files: SessionFileStore,
        swarm: Arc<SwarmStore>,
        session_mutations: Arc<RwLock<()>>,
        provider_epoch: Arc<AtomicU64>,
        activities: SessionActivities,
        gateway_events: broadcast::Sender<ServerFrame>,
        session_id: String,
        origin_label: &str,
    ) -> Result<Self> {
        let running = start_agent(
            Arc::clone(&gateway),
            &spec,
            &store,
            Arc::clone(&credentials),
            Arc::clone(&checkpoints),
            scratchpad.clone(),
            session_files.clone(),
            Arc::clone(&swarm),
            session_id.clone(),
            origin_label,
            true,
            None,
            Arc::clone(&provider_epoch),
        )
        .await?;
        let accepts_file_attachments = Arc::new(AtomicBool::new(runtime_accepts_attachments(
            &running.frontend,
        )));
        let alive = Arc::new(AtomicBool::new(true));
        let terminated = Arc::new(AtomicBool::new(false));
        let termination = Arc::new(tokio::sync::Notify::new());
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        let loaded = load_replay(checkpoints.as_ref(), &session_id, &running.frontend).await?;
        let awaiting_approval = activities
            .lock()
            .map_err(|_| Error::Config("session activity lock is poisoned".into()))?
            .entry(session_id.clone())
            .or_default()
            .state
            == SessionActivityState::AwaitingApproval;
        let bot_id = spec.bot_id.clone();
        let mut state = HostState {
            store,
            gateway,
            spec,
            credentials,
            bots,
            checkpoints,
            scratchpad,
            session_files,
            swarm,
            accepts_file_attachments: Arc::clone(&accepts_file_attachments),
            alive: Arc::clone(&alive),
            terminated: Arc::clone(&terminated),
            termination: Arc::clone(&termination),
            session_mutations: Arc::clone(&session_mutations),
            provider_epoch,
            activities,
            running,
            pending_turns: usize::from(awaiting_approval),
            pending_messages: HashSet::new(),
            approval_active: awaiting_approval,
            turn_error: None,
            last_assistant_text: None,
            restart_after_turn: false,
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
        state.reconcile_loaded_startup().await?;
        state.reconcile_replayed_swarm_work().await?;
        tokio::spawn(state.run());
        Ok(Self {
            inner: Arc::new(HostInner {
                session_id: session_id.into(),
                bot_id: bot_id.into(),
                commands,
                events,
                accepts_file_attachments,
                alive,
                terminated,
                termination,
                session_mutations,
            }),
        })
    }

    #[must_use]
    pub(crate) fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ServerFrame> {
        self.inner.events.subscribe()
    }

    #[must_use]
    pub(crate) fn accepts_file_attachments(&self) -> bool {
        self.inner.accepts_file_attachments.load(Ordering::Relaxed)
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

    pub(crate) async fn reload_bot(
        &self,
        bot: crate::wire::BotRecord,
    ) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::ReloadBot { bot, reply }).await?;
        receive(receiver).await
    }

    pub(crate) async fn git_diff(
        &self,
        scope: GitDiffScope,
    ) -> std::result::Result<String, Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::GitDiff { scope, reply }).await?;
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

    pub(super) async fn refresh_provider(
        &self,
        scope: ProviderRefresh,
    ) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::RefreshProvider { scope, reply })
            .await?;
        receive(receiver).await
    }

    pub(super) async fn refresh_extension(&self, id: String) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::RefreshExtension { id, reply })
            .await?;
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

    async fn wait_terminated(&self) {
        while !self.inner.terminated.load(Ordering::Acquire) {
            let terminated = self.inner.termination.notified();
            if self.inner.terminated.load(Ordering::Acquire) {
                return;
            }
            terminated.await;
        }
    }

    async fn send(&self, command: HostCommand) -> std::result::Result<(), Rejection> {
        self.inner
            .commands
            .send(command)
            .await
            .map_err(|_| stopped())
    }
}

fn try_begin_session_mutation(
    mutations: &Arc<RwLock<()>>,
    bots: &BotStore,
) -> std::result::Result<tokio::sync::OwnedRwLockReadGuard<()>, Rejection> {
    let mutation = Arc::clone(mutations)
        .try_read_owned()
        .map_err(|_| Rejection {
            code: "gateway_busy",
            message: "retry after the gateway update finishes".into(),
            fatal: false,
        })?;
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
    checkpoints: Arc<dyn CheckpointStore>,
    scratchpad: ScratchpadStore,
    session_files: SessionFileStore,
    swarm: Arc<SwarmStore>,
    session_id: String,
    origin_label: &str,
    override_saved_model_route: bool,
    reusable_model_router: Option<ReusableModelRouter>,
    provider_epoch: Arc<AtomicU64>,
) -> Result<RunningAgent> {
    let swarm: Arc<dyn BotsBackend> = swarm;
    let reusable_provider_epoch = reusable_model_router
        .as_ref()
        .map(|reusable| reusable.provider_epoch);
    let BuiltAgent {
        agent,
        model_router,
        gateway_sandbox,
        subagent_template,
    } = assemble(
        gateway,
        spec,
        store,
        credentials,
        checkpoints,
        scratchpad,
        session_files,
        swarm,
        Some(session_id),
        origin_label,
        override_saved_model_route,
        reusable_model_router.map(|reusable| reusable.router),
    )
    .await?;
    let session = agent.session().clone();
    let frontend = agent.frontend().clone();
    let tool_count = agent.tool_count();
    let session_id = session.session_id.clone();
    let (sender, events) = agent.into_recorded_parts();
    Ok(RunningAgent {
        session_id,
        sender: Some(sender),
        events,
        model_router,
        frontend,
        session,
        gateway_sandbox,
        subagent_template,
        tool_count,
        provider_epoch: reusable_provider_epoch
            .unwrap_or_else(|| provider_epoch.load(Ordering::Acquire)),
    })
}

fn reusable_model_router(
    old_spec: &ChatSpec,
    next_spec: &ChatSpec,
    running: &RunningAgent,
) -> Option<ReusableModelRouter> {
    provider_config_unchanged(old_spec, next_spec).then(|| ReusableModelRouter {
        router: Arc::clone(&running.model_router),
        provider_epoch: running.provider_epoch,
    })
}

pub(super) fn provider_config_unchanged(old_spec: &ChatSpec, next_spec: &ChatSpec) -> bool {
    old_spec.agent.config.provider == next_spec.agent.config.provider
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
