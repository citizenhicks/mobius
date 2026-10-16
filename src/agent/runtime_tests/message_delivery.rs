//! Message-delivery runtime tests.

use super::*;
use crate::agent::AgentSender;
use crate::middleware::{ActiveCommandContext, SubmissionResult};

struct PartiallyBlockingBeforeModel {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl Middleware for PartiallyBlockingBeforeModel {
    fn name(&self) -> &'static str {
        "partially_blocking_before_model"
    }

    fn pre_model<'a>(&'a self, context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            context.push_input(crate::backend::model::internal_user_message(
                "partial_hook",
                "must be discarded",
            ))?;
            self.started.notify_one();
            self.release.notified().await;
            Ok(())
        })
    }
}

struct RefillMiddleware {
    count: AtomicUsize,
    finished: Arc<AtomicBool>,
    limit: usize,
    release: Arc<Notify>,
    sender: Mutex<Option<AgentSender>>,
}

struct ReleasingModel {
    finished: Arc<AtomicBool>,
    release: Arc<Notify>,
    started: Arc<Notify>,
}

impl RefillMiddleware {
    fn op() -> Op {
        Op::command("refill", "refill", String::new())
    }
}

impl Middleware for RefillMiddleware {
    fn name(&self) -> &'static str {
        "refill"
    }

    fn active_command<'a>(
        &'a self,
        context: &'a mut ActiveCommandContext<'_>,
    ) -> BoxFuture<'a, Result<Option<SubmissionResult>>> {
        Box::pin(async move {
            if context.command != "refill" {
                return Ok(None);
            }
            let count = self.count.fetch_add(1, Ordering::SeqCst) + 1;
            if count == 1 {
                self.release.notify_one();
            }
            if count < self.limit && !self.finished.load(Ordering::SeqCst) {
                self.sender
                    .lock()
                    .expect("refill sender lock")
                    .as_ref()
                    .expect("refill sender")
                    .submit(Self::op())?;
            }
            Ok(Some(SubmissionResult::Rejected("refilled".into())))
        })
    }
}

impl Model for ReleasingModel {
    fn respond<'a>(
        &'a self,
        _request: ModelRequest,
        _events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        let finished = Arc::clone(&self.finished);
        let release = Arc::clone(&self.release);
        let started = Arc::clone(&self.started);
        Box::pin(async move {
            started.notify_one();
            release.notified().await;
            finished.store(true, Ordering::SeqCst);
            Ok(scripted_message("done"))
        })
    }
}

fn blocking_model(started: &Arc<Notify>, release: &Arc<Notify>) -> Arc<BlockingModel> {
    Arc::new(BlockingModel {
        started: Arc::clone(started),
        release: Arc::clone(release),
        calls: AtomicUsize::new(0),
    })
}

async fn active_turn(agent: &mut Agent, started: &Notify) -> String {
    let turn_id = loop {
        let event = agent.next_event().await.expect("turn event");
        if let EventMsg::TurnStarted(started) = event.msg {
            break started.turn_id;
        }
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        drain_until_notified(agent, started),
    )
    .await
    .expect("active boundary started");
    turn_id
}

#[tokio::test]
async fn rejected_input_refill_cannot_starve_model_completion() {
    let workspace = tempfile::tempdir().expect("workspace");
    let release = Arc::new(Notify::new());
    let started = Arc::new(Notify::new());
    let finished = Arc::new(AtomicBool::new(false));
    let refill = Arc::new(RefillMiddleware {
        count: AtomicUsize::new(0),
        finished: Arc::clone(&finished),
        limit: 8,
        release: Arc::clone(&release),
        sender: Mutex::new(None),
    });
    let model = Arc::new(ReleasingModel {
        finished: Arc::clone(&finished),
        release,
        started: Arc::clone(&started),
    });
    let mut agent = create_agent(
        AgentConfig::new(
            Arc::new(ModelRouter::new("refill-model", model)),
            Arc::new(Sandbox::new(
                Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
                ApprovalPolicy::Allow,
            )),
            Arc::new(
                SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
                    .expect("checkpoint store"),
            ),
            test_middleware(vec![refill.clone()]),
            "test prompt",
        )
        .session_context(test_session_context())
        .session_id("refill-model"),
    )
    .await
    .expect("create agent");
    *refill.sender.lock().expect("refill sender lock") = Some(agent.sender());
    agent.sender().submit(user_op("start")).expect("start turn");
    active_turn(&mut agent, &started).await;
    agent
        .sender()
        .submit(RefillMiddleware::op())
        .expect("submit refill");

    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::TurnComplete(_)
    ) {}

    assert!(finished.load(Ordering::SeqCst));
    assert!(refill.count.load(Ordering::SeqCst) < refill.limit);
}

#[tokio::test]
async fn steer_is_durable_before_a_blocked_model_completes() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let model = blocking_model(&started, &release);
    let mut agent = create_agent(
        AgentConfig::new(
            Arc::new(ModelRouter::new("blocking", model)),
            Arc::new(Sandbox::new(
                Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
                ApprovalPolicy::Ask,
            )),
            checkpoints.clone(),
            test_middleware(Vec::new()),
            "test prompt",
        )
        .session_context(test_session_context())
        .session_id("blocked-model"),
    )
    .await
    .expect("create agent");
    agent.sender().submit(user_op("start")).expect("start turn");
    let turn_id = active_turn(&mut agent, &started).await;

    let submission_id = agent
        .sender()
        .submit(active_user_op(
            "persist me",
            &turn_id,
            ActiveMessageDelivery::Steer,
        ))
        .expect("submit steer");
    loop {
        let event = agent.next_event().await.expect("steer event");
        if event.submission_id.as_deref() == Some(&submission_id) {
            break;
        }
    }
    let saved = checkpoints
        .load("blocked-model")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    release.notify_one();
    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::TurnComplete(_)
    ) {}

    assert!(saved.pending_messages.iter().any(|item| {
        item.owner() == "messages"
            && item.id() == submission_id
            && matches!(
                item.boundary(),
                crate::backend::checkpoint::QueuedMessageBoundary::Steer {
                    turn_id: target
                } if target == &turn_id
            )
            && matches!(
                item.event(),
                MessageEvent {
                    delivery: MessageDelivery::Steer,
                    text,
                    ..
                } if text == "persist me"
            )
    }));
}

#[tokio::test]
async fn interrupt_discards_partial_pre_model_context() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let middleware = PartiallyBlockingBeforeModel {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    };
    let mut agent = create_agent(
        AgentConfig::new(
            Arc::new(ModelRouter::new("test", Arc::new(TestModel))),
            Arc::new(Sandbox::new(
                Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
                ApprovalPolicy::Ask,
            )),
            checkpoints.clone(),
            test_middleware(vec![Arc::new(middleware)]),
            "test prompt",
        )
        .session_context(test_session_context())
        .session_id("interrupt-pre-model"),
    )
    .await
    .expect("create agent");
    agent.sender().submit(user_op("start")).expect("start turn");
    let turn_id = active_turn(&mut agent, &started).await;
    agent
        .sender()
        .submit(Op::Interrupt { turn_id })
        .expect("interrupt turn");
    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::TurnAborted(_)
    ) {}
    release.notify_one();
    let saved = checkpoints
        .load("interrupt-pre-model")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");

    assert!(
        saved
            .context
            .iter()
            .all(|item| internal_message_kind(item) != Some("partial_hook"))
    );
}
