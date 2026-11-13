use mobius::SessionId;
use mobius::backend::checkpoint::Checkpoint;
use mobius::protocol::TokenUsage;

use super::session::{HostCommand, HostInner, ProviderCutoverStatus};
use super::*;

mod activity;
mod bots;
mod harnesses;
mod hooks;
mod lifecycle;
mod live_chats;
mod memory;
mod projection;
mod replay;
mod statistics;

impl GatewayHost {
    /// A resident session that reports busy while `busy` holds, like one with an armed idle timer.
    pub(crate) async fn busy_session_for_test(&self, busy: Arc<AtomicBool>) -> JoinHandle<()> {
        let (handle, mut receiver) = probe_handle("busy-probe", 2, false);
        let terminated = Arc::clone(&handle.inner.terminated);
        let termination = Arc::clone(&handle.inner.termination);
        self.state
            .lock()
            .await
            .sessions
            .insert(SessionId::new("busy-probe").unwrap(), handle);
        tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Some(HostCommand::RuntimeIsIdle { reply }) => {
                        if reply.send(Ok(!busy.load(Ordering::Acquire))).is_err() {
                            tracing::debug!("busy probe requester stopped waiting");
                        }
                    }
                    Some(HostCommand::Shutdown) | None => break,
                    Some(_) => {}
                }
            }
            terminated.store(true, Ordering::Release);
            termination.notify_waiters();
        })
    }

    pub(crate) async fn pause_activity_reply_for_test(
        &self,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>, JoinHandle<()>) {
        let (handle, mut receiver) = probe_handle("activity-probe", 2, false);
        let alive = Arc::clone(&handle.inner.alive);
        let terminated = Arc::clone(&handle.inner.terminated);
        let termination = Arc::clone(&handle.inner.termination);
        self.state
            .lock()
            .await
            .sessions
            .insert(SessionId::new("activity-probe").unwrap(), handle);
        let (entered, waiting) = oneshot::channel();
        let (release, mut released) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut entered = Some(entered);
            let mut held_reply = None;
            loop {
                tokio::select! {
                    command = receiver.recv() => match command {
                        Some(HostCommand::RuntimeIsIdle { reply }) => {
                            if let Some(entered) = entered.take() {
                                held_reply = Some(reply);
                                if entered.send(()).is_err() {
                                    tracing::debug!("activity probe observer stopped waiting");
                                }
                            } else if reply.send(Ok(false)).is_err() {
                                tracing::debug!("activity probe requester stopped waiting");
                            }
                        }
                        Some(HostCommand::Shutdown) | None => break,
                        _ => panic!("unexpected activity probe command"),
                    },
                    _ = &mut released => break,
                }
            }
            drop(held_reply);
            alive.store(false, Ordering::Release);
            terminated.store(true, Ordering::Release);
            termination.notify_waiters();
        });
        (waiting, release, task)
    }
}

pub(crate) async fn ensure_test_bot(
    gateway: &GatewayHost,
) -> std::result::Result<crate::wire::BotRecord, Rejection> {
    let state = gateway.state.lock().await;
    if let Some(bot) = state.bots.bots().map_err(internal)?.into_iter().next() {
        return Ok(bot);
    }
    let mut config = state.config()?;
    if config.bot_defaults.is_none() {
        let next = config
            .registering_provider(
                AgentComposition::default().provider,
                "Test".into(),
                Default::default(),
                Vec::new(),
                Vec::new(),
            )
            .and_then(GatewayConfig::validated)
            .map_err(invalid_config)?;
        state.store.save(&next).map_err(internal)?;
        *config = next.into_inner();
    }
    let composition = config
        .bot_defaults
        .as_ref()
        .expect("provider registration installs Bot defaults")
        .config
        .clone();
    drop(config);
    state
        .bots
        .create_bot("Test Bot", "Own gateway test work.", composition.into())
        .map_err(invalid_bot)
}

pub(crate) async fn create_test_session(
    gateway: &GatewayHost,
    workspace: &Path,
) -> std::result::Result<HostHandle, Rejection> {
    let bot = ensure_test_bot(gateway).await?;
    gateway.create_session(workspace, &bot.id).await
}

pub(crate) fn timer_definition(
    workspace: &Path,
    instructions: &str,
    schedule: crate::wire::RoutineSchedule,
    ends_at: Option<i64>,
) -> crate::wire::RoutineDefinition {
    crate::wire::RoutineDefinition {
        workspace: workspace.into(),
        instructions: instructions.into(),
        bindings: vec![crate::wire::RoutineBinding {
            id: Uuid::new_v4().to_string().into(),
            on: crate::wire::HookSelector::Schedule { schedule, ends_at },
            action: crate::wire::RoutineAction::Start,
        }],
    }
}

/// A resident chat handle whose commands the test receives instead of an actor.
pub(crate) fn probe_handle(
    session_id: &str,
    capacity: usize,
    terminated: bool,
) -> (HostHandle, mpsc::Receiver<HostCommand>) {
    let (commands, receiver) = mpsc::channel(capacity);
    let (events, _) = broadcast::channel(1);
    let handle = HostHandle {
        inner: Arc::new(HostInner {
            session_id: SessionId::new(session_id).unwrap(),
            commands,
            events,
            alive: Arc::new(AtomicBool::new(true)),
            terminated: Arc::new(AtomicBool::new(terminated)),
            termination: Arc::new(tokio::sync::Notify::new()),
            session_mutations: Arc::new(RwLock::new(())),
            realtime_voice: Arc::new(Mutex::new(())),
        }),
    };
    (handle, receiver)
}

/// A started gateway with no provider yet, and a workspace for its chats.
pub(crate) struct TestGateway {
    pub(crate) root: tempfile::TempDir,
    pub(crate) workspace: std::path::PathBuf,
    pub(crate) gateway: GatewayHost,
}

pub(crate) async fn test_gateway() -> TestGateway {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    TestGateway {
        root,
        workspace,
        gateway,
    }
}

pub(crate) fn turn_started(turn_id: &str) -> mobius::protocol::EventMsg {
    mobius::protocol::EventMsg::TurnStarted(mobius::protocol::TurnStartedEvent {
        idle: false,
        turn_id: turn_id.into(),
        model_context_window: None,
    })
}

pub(crate) fn turn_complete(turn_id: &str) -> mobius::protocol::EventMsg {
    mobius::protocol::EventMsg::TurnComplete(mobius::protocol::TurnCompleteEvent {
        idle: false,
        turn_id: turn_id.into(),
    })
}
