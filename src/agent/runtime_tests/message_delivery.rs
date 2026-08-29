//! Message-delivery runtime tests.

use super::*;

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

struct BlockingPreTool {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl Middleware for BlockingPreTool {
    fn name(&self) -> &'static str {
        "blocking_pre_tool"
    }

    fn pre_tool_use<'a>(
        &'a self,
        _context: &'a mut PreToolUseContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.started.notify_one();
            self.release.notified().await;
            Ok(())
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
async fn steer_is_durable_while_pre_model_is_blocked() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let middleware = BlockingTailMiddleware {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
        blocked: AtomicBool::new(false),
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
        .session_id("blocked-pre-model"),
    )
    .await
    .expect("create agent");
    agent.sender().submit(user_op("start")).expect("start turn");
    let turn_id = active_turn(&mut agent, &started).await;

    let submission_id = agent
        .sender()
        .submit(active_user_op(
            "survive the hook",
            turn_id,
            ActiveMessageDelivery::Steer,
        ))
        .expect("submit steer");
    let saved = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let saved = checkpoints
                .load("blocked-pre-model")
                .await
                .expect("load checkpoint")
                .expect("saved checkpoint");
            if saved
                .pending_messages
                .iter()
                .any(|item| item.id() == submission_id)
            {
                break saved;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("steer persisted while hook was blocked");
    release.notify_one();
    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::TurnAborted(_)
    ) {}

    assert!(saved.pending_messages.iter().any(|item| {
        matches!(
            item.event(),
            MessageEvent {
                delivery: MessageDelivery::Steer,
                text,
                ..
            } if text == "survive the hook"
        )
    }));
}

#[tokio::test]
async fn queued_message_starts_once_after_the_active_turn_finishes() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let model = blocking_model(&started, &release);
    let mut agent = create_agent(
        AgentConfig::new(
            Arc::new(ModelRouter::new("blocking", model.clone())),
            Arc::new(Sandbox::new(
                Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
                ApprovalPolicy::Ask,
            )),
            checkpoints,
            test_middleware(Vec::new()),
            "test prompt",
        )
        .session_id("queued-after-turn"),
    )
    .await
    .expect("create agent");
    agent.sender().submit(user_op("start")).expect("start turn");
    let turn_id = active_turn(&mut agent, &started).await;
    agent
        .sender()
        .submit(active_user_op(
            "next turn",
            turn_id,
            ActiveMessageDelivery::Queue,
        ))
        .expect("submit queued message");
    release.notify_one();

    let mut completed_turns = 0;
    let mut queued_messages = 0;
    let mut queued_after_completion = false;
    while completed_turns < 2 {
        match agent.next_event().await.expect("agent event").msg {
            EventMsg::Message(MessageEvent {
                delivery: MessageDelivery::Queue,
                text,
                ..
            }) if text == "next turn" => {
                queued_messages += 1;
                queued_after_completion = completed_turns == 1;
            }
            EventMsg::TurnComplete(_) => completed_turns += 1,
            _ => {}
        }
    }

    assert_eq!((queued_messages, queued_after_completion), (1, true));
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
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

#[tokio::test]
async fn ready_steer_wins_the_model_completion_barrier() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
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
            checkpoints,
            test_middleware(Vec::new()),
            "test prompt",
        )
        .session_id("model-steer-barrier"),
    )
    .await
    .expect("create agent");
    agent.sender().submit(user_op("start")).expect("start turn");
    let turn_id = active_turn(&mut agent, &started).await;
    let steer_id = agent
        .sender()
        .submit(active_user_op(
            "ready steer",
            turn_id,
            ActiveMessageDelivery::Steer,
        ))
        .expect("submit steer");
    release.notify_one();

    let mut widget_position = None;
    let mut assistant_message_position = None;
    let mut position = 0;
    while widget_position.is_none() || assistant_message_position.is_none() {
        let event = agent.next_event().await.expect("agent event");
        match event.msg {
            EventMsg::Frontend(FrontendEvent::Widget { item, .. }) if item.id == steer_id => {
                widget_position = Some(position);
            }
            EventMsg::AssistantMessage(message)
                if message.content.iter().any(|content| content.text == "done") =>
            {
                assistant_message_position = Some(position);
            }
            _ => {}
        }
        position += 1;
    }
    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::TurnComplete(_)
    ) {}

    assert!(widget_position < assistant_message_position);
}

#[tokio::test]
async fn steer_during_pre_tool_hook_is_persisted_before_the_assistant_message() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let first = ModelOutput::from_output(
        vec![
            serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "calling"}]
            }),
            serde_json::json!({
                "type": "function_call",
                "call_id": "call-1",
                "name": "approval_required",
                "arguments": "{}"
            }),
        ],
        false,
        scripted_usage(),
    )
    .expect("model output");
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([first, scripted_message("done")])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let mut agent = create_agent(
        AgentConfig::new(
            Arc::new(ModelRouter::new("test", model)),
            Arc::new(Sandbox::new(
                Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
                ApprovalPolicy::Allow,
            )),
            checkpoints,
            test_middleware(vec![
                Arc::new(Tools::new(vec![Arc::new(ApprovalRequiredTestTool)])),
                Arc::new(BlockingPreTool {
                    started: Arc::clone(&started),
                    release: Arc::clone(&release),
                }),
            ]),
            "test prompt",
        )
        .session_id("pre-tool-steer"),
    )
    .await
    .expect("create agent");
    agent.sender().submit(user_op("start")).expect("start turn");
    let turn_id = active_turn(&mut agent, &started).await;
    let steer_id = agent
        .sender()
        .submit(active_user_op(
            "inspect first",
            turn_id,
            ActiveMessageDelivery::Steer,
        ))
        .expect("submit steer");
    release.notify_one();

    let mut widget_position = None;
    let mut assistant_message_position = None;
    let mut position = 0;
    while widget_position.is_none() || assistant_message_position.is_none() {
        let event = agent.next_event().await.expect("agent event");
        match event.msg {
            EventMsg::Frontend(FrontendEvent::Widget { item, .. }) if item.id == steer_id => {
                widget_position = Some(position);
            }
            EventMsg::AssistantMessage(message)
                if message
                    .content
                    .iter()
                    .any(|content| content.text == "calling") =>
            {
                assistant_message_position = Some(position);
            }
            _ => {}
        }
        position += 1;
    }
    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::TurnComplete(_)
    ) {}

    assert!(widget_position < assistant_message_position);
}
