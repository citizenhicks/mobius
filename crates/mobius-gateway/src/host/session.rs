mod events;
mod runtime;

use super::*;
use mobius::backend::model::provider::provider;
use mobius::middleware::swarm::SwarmBackend;

pub(super) type SessionWidgets = Vec<((String, String), mobius::protocol::FrontendWidget)>;
const CRON_EXECUTION_METADATA_KEY: &str = "mobius_gateway.cron_execution";

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
    pub(super) commands: mpsc::Sender<HostCommand>,
    pub(super) events: broadcast::Sender<ServerFrame>,
    pub(super) accepts_file_attachments: Arc<AtomicBool>,
    pub(super) alive: Arc<AtomicBool>,
}

struct HostState {
    store: ConfigStore,
    gateway: Arc<StdMutex<GatewayConfig>>,
    spec: ChatSpec,
    credentials: Arc<CredentialStore>,
    cron: Arc<CronStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    scratchpad: ScratchpadStore,
    session_files: SessionFileStore,
    swarm: Arc<SwarmStore>,
    accepts_file_attachments: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    catalog_lock: Arc<Mutex<()>>,
    session_mutations: Arc<RwLock<()>>,
    provider_epoch: Arc<AtomicU64>,
    activities: SessionActivities,
    running: RunningAgent,
    pending_turns: usize,
    pending_messages: HashSet<String>,
    approval_active: bool,
    turn_error: Option<String>,
    restart_after_turn: bool,
    pending_startup: Vec<ServerFrame>,
    active_cron: Option<ActiveCron>,
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
    pub(super) selection: ProviderConfig,
    pub(super) provider_epoch: u64,
    pub(super) idle: bool,
}

pub(super) struct ActiveCron {
    pub(super) run: ActiveCronRun,
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
    RenameSession {
        session_id: String,
        title: String,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    SetSessionPinned {
        session_id: String,
        pinned: bool,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    Submit {
        submission: Submission,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    Configure {
        expected_revision: u64,
        config: AgentComposition,
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
    CutOverProvider {
        selection: ProviderConfig,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    ReloadProviderCatalog {
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    },
    RunCron {
        run: ActiveCronRun,
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
    #[expect(
        clippy::too_many_arguments,
        reason = "one chat actor receives each owned gateway dependency explicitly"
    )]
    pub(crate) async fn start(
        store: ConfigStore,
        gateway: Arc<StdMutex<GatewayConfig>>,
        spec: ChatSpec,
        credentials: Arc<CredentialStore>,
        cron: Arc<CronStore>,
        checkpoints: Arc<dyn CheckpointStore>,
        scratchpad: ScratchpadStore,
        session_files: SessionFileStore,
        swarm: Arc<SwarmStore>,
        catalog_lock: Arc<Mutex<()>>,
        session_mutations: Arc<RwLock<()>>,
        provider_epoch: Arc<AtomicU64>,
        activities: SessionActivities,
        gateway_events: broadcast::Sender<ServerFrame>,
        session_id: String,
        origin_label: &str,
    ) -> Result<Self> {
        let (spec, override_saved_model_route) = {
            let config = gateway
                .lock()
                .map_err(|_| Error::Config("gateway configuration lock is poisoned".into()))?
                .clone();
            let normalized =
                spec.normalizing_provider_catalog(&config, store.state_dir(), config.tls.as_ref())?;
            let override_saved_model_route =
                normalized.agent.config.provider != spec.agent.config.provider;
            (normalized, override_saved_model_route)
        };
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
            override_saved_model_route,
            None,
            Arc::clone(&provider_epoch),
        )
        .await?;
        let accepts_file_attachments = Arc::new(AtomicBool::new(runtime_accepts_attachments(
            &running.frontend,
        )));
        let alive = Arc::new(AtomicBool::new(true));
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        let loaded = load_replay(checkpoints.as_ref(), &session_id, &running.frontend).await?;
        activities
            .lock()
            .map_err(|_| Error::Config("session activity lock is poisoned".into()))?
            .entry(session_id.clone())
            .or_default();
        let mut state = HostState {
            store,
            gateway,
            spec,
            credentials,
            cron,
            checkpoints,
            scratchpad,
            session_files,
            swarm,
            accepts_file_attachments: Arc::clone(&accepts_file_attachments),
            alive: Arc::clone(&alive),
            catalog_lock,
            session_mutations,
            provider_epoch,
            activities,
            running,
            pending_turns: 0,
            pending_messages: HashSet::new(),
            approval_active: false,
            turn_error: None,
            restart_after_turn: false,
            pending_startup: Vec::new(),
            active_cron: None,
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
        state.acknowledge_replayed_peer_messages().await?;
        tokio::spawn(state.run());
        Ok(Self {
            inner: Arc::new(HostInner {
                session_id: session_id.into(),
                commands,
                events,
                accepts_file_attachments,
                alive,
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

    pub(crate) async fn rename_session(
        &self,
        session_id: String,
        title: String,
    ) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::RenameSession {
            session_id,
            title,
            reply,
        })
        .await?;
        receive(receiver).await
    }

    pub(crate) async fn set_session_pinned(
        &self,
        session_id: String,
        pinned: bool,
    ) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::SetSessionPinned {
            session_id,
            pinned,
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

    pub(crate) async fn configure(
        &self,
        expected_revision: u64,
        config: AgentComposition,
    ) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::Configure {
            expected_revision,
            config,
            reply,
        })
        .await?;
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

    pub(super) async fn cut_over_provider(
        &self,
        selection: &ProviderConfig,
    ) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::CutOverProvider {
            selection: selection.clone(),
            reply,
        })
        .await?;
        receive(receiver).await
    }

    pub(super) async fn reload_provider_catalog(&self) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        self.send(HostCommand::ReloadProviderCatalog { reply })
            .await?;
        receive(receiver).await
    }

    pub(crate) async fn run_cron(
        &self,
        run: ActiveCronRun,
        input: String,
        cron: &CronStore,
    ) -> std::result::Result<(), Rejection> {
        let (reply, receiver) = oneshot::channel();
        if let Err(error) = self
            .inner
            .commands
            .send(HostCommand::RunCron { run, input, reply })
            .await
        {
            let HostCommand::RunCron { run, .. } = error.0 else {
                unreachable!("only a cron command was sent")
            };
            cron.finish_run(
                run,
                CronRunStatus::Failed,
                Some("the agent stopped before the scheduled run began".into()),
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
        if self.send(HostCommand::StopIfIdle { reply }).await.is_err() {
            return true;
        }
        receiver.await.unwrap_or(true)
    }

    async fn send(&self, command: HostCommand) -> std::result::Result<(), Rejection> {
        self.inner
            .commands
            .send(command)
            .await
            .map_err(|_| stopped())
    }
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
            let selected_base_url = definition
                .configurable_base_url()
                .then(|| {
                    selection
                        .base_url
                        .as_deref()
                        .or_else(|| definition.default_base_url())
                })
                .flatten();
            Ok(selected_base_url == base_url.as_deref())
        }
        ProviderRefresh::Provider(provider) => Ok(selection.provider == *provider),
    }
}

pub(super) fn fail_active_cron(
    cron: &CronStore,
    active: &mut Option<ActiveCron>,
    message: &str,
) -> Result<()> {
    let Some(active) = active.take() else {
        return Ok(());
    };
    cron.finish_run(active.run, CronRunStatus::Failed, Some(message.to_string()))
        .map(|_| ())
}

pub(super) fn setup_agent_config() -> VersionedAgentConfig {
    VersionedAgentConfig {
        revision: 1,
        config: AgentComposition::default(),
    }
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
    let swarm: Arc<dyn SwarmBackend> = swarm;
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

pub(super) fn cron_execution_checkpoint(
    source: &Checkpoint,
    session_id: &str,
    origin_label: &str,
) -> Checkpoint {
    let mut checkpoint = Checkpoint::empty(session_id);
    checkpoint
        .session_context
        .clone_from(&source.session_context);
    checkpoint.session_context.origin_label = Some(origin_label.into());
    checkpoint.catalog_visible = false;
    checkpoint.metadata.clone_from(&source.metadata);
    checkpoint.metadata.insert(
        CRON_EXECUTION_METADATA_KEY.into(),
        serde_json::Value::String(session_id.into()),
    );
    checkpoint.model_route.clone_from(&source.model_route);
    checkpoint
}

pub(super) fn is_cron_execution_checkpoint(checkpoint: &Checkpoint) -> bool {
    !checkpoint.catalog_visible
        && checkpoint
            .metadata
            .get(CRON_EXECUTION_METADATA_KEY)
            .and_then(serde_json::Value::as_str)
            == Some(checkpoint.session_id.as_str())
}

pub(super) async fn hide_checkpoint(
    checkpoints: &Arc<dyn CheckpointStore>,
    session_id: &str,
) -> Result<()> {
    let Some(mut checkpoint) = checkpoints.load(session_id).await? else {
        return Ok(());
    };
    checkpoint.catalog_visible = false;
    checkpoint.sequence = checkpoint
        .sequence
        .checked_add(1)
        .ok_or_else(|| Error::Config("checkpoint sequence overflow".into()))?;
    checkpoints.save(&checkpoint, &[], None).await?;
    Ok(())
}
