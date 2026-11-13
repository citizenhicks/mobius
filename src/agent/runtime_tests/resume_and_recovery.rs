//! Resume And Recovery agent runtime tests.

use super::*;

struct FailFirstMessageSubmit(Arc<AtomicBool>);

impl Middleware for FailFirstMessageSubmit {
    fn name(&self) -> &'static str {
        "fail_first_message_submit"
    }

    fn message_submit<'a>(
        &'a self,
        _context: &'a mut MessageSubmitContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if !self.0.swap(true, Ordering::SeqCst) {
                return Err(Error::Provider("message submit failed".into()));
            }
            Ok(())
        })
    }
}

#[tokio::test]
async fn failed_message_submit_leaves_the_accepted_message_for_restart() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let failed = Arc::new(AtomicBool::new(false));
    let model = Arc::new(HandoffModel::default());
    let make_config = |checkpoints: Arc<dyn CheckpointStore>| {
        AgentConfig::new(
            Arc::new(ModelRouter::new("test", Arc::<HandoffModel>::clone(&model))),
            Arc::new(Sandbox::new(
                Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
                ApprovalPolicy::Ask,
            )),
            checkpoints,
            test_middleware(vec![Arc::new(FailFirstMessageSubmit(Arc::clone(&failed)))]),
            "test prompt",
        )
        .session_context(test_session_context())
        .session_id("message-submit-restart".parse().expect("session ID"))
    };
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::<SqliteCheckpoint>::clone(&checkpoints);
    let mut first = create_agent(make_config(checkpoint_store))
        .await
        .expect("create first agent");
    let submission_id = first
        .sender()
        .submit(user_op("survive restart"))
        .expect("submit message");
    loop {
        let event = first.next_event().await.expect("first agent event");
        if matches!(event.msg, EventMsg::Error(_)) {
            break;
        }
    }
    let saved = checkpoints
        .load(&"message-submit-restart".parse().expect("session ID"))
        .await
        .expect("load failed checkpoint")
        .expect("failed checkpoint");
    assert!(saved.active_execution.is_none());
    assert!(
        saved
            .pending_messages
            .iter()
            .any(|message| message.id() == &*submission_id)
    );
    drop(first);

    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::<SqliteCheckpoint>::clone(&checkpoints);
    let mut restarted = create_agent(make_config(checkpoint_store))
        .await
        .expect("restart agent");
    while !matches!(
        restarted
            .next_event()
            .await
            .expect("restarted agent event")
            .msg,
        EventMsg::TurnComplete(_)
    ) {}
    let saved = checkpoints
        .load(&"message-submit-restart".parse().expect("session ID"))
        .await
        .expect("load completed checkpoint")
        .expect("completed checkpoint");
    assert!(saved.pending_messages.is_empty());
    assert_eq!(model.responses.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn route_change_omits_private_reasoning_but_keeps_visible_history_and_tool_pairs() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let (mut first_output, _, _) = scripted_tool_call().into_parts();
    first_output.insert(
        0,
        serde_json::json!({
            "type": "reasoning",
            "encrypted_content": "provider-private",
            "summary": [{"type": "summary_text", "text": "Private plan"}]
        }),
    );
    let (mut original_reply, _, usage) = scripted_message("Original answer.").into_parts();
    original_reply[0]["_anthropic_content"] = serde_json::json!([
        {"type": "thinking", "thinking": "Signed plan", "signature": "signed-private"},
        {"type": "text", "text": "Original answer."}
    ]);
    original_reply[0][crate::backend::model::REPLAY_REASONING_FIELD] = "Signed plan".into();
    let source = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([
            ModelOutput::from_output(first_output, false, scripted_usage())
                .expect("reasoning and real tool call"),
            ModelOutput::from_output(original_reply, true, usage).expect("signed source reply"),
            scripted_message("Same route answer."),
        ])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let destination = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([scripted_message("Changed route answer.")])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let mut models = ModelRouter::new("source", Arc::clone(&source) as Arc<dyn Model>);
    models
        .register("destination", Arc::clone(&destination) as Arc<dyn Model>)
        .unwrap();
    let mut agent = create_agent(
        AgentConfig::new(
            Arc::new(models),
            Arc::new(Sandbox::new(
                Arc::new(LocalSandbox::new(workspace.path()).unwrap()),
                ApprovalPolicy::Allow,
            )),
            Arc::clone(&checkpoints) as Arc<dyn CheckpointStore>,
            test_middleware(vec![Arc::new(Tools::new(vec![Arc::new(
                ApprovalRequiredTestTool,
            )]))]),
            "test prompt",
        )
        .session_context(test_session_context())
        .session_id("reasoning-route-change".parse().expect("session ID")),
    )
    .await
    .unwrap();
    for message in ["run the tool", "continue on the same route"] {
        agent.sender().submit(user_op(message)).unwrap();
        while !matches!(
            agent.next_event().await.unwrap().msg,
            EventMsg::TurnComplete(_)
        ) {}
    }
    {
        let inputs = source.inputs.lock().unwrap().clone();
        assert!(inputs[2].iter().any(|item| {
            item.get("encrypted_content").and_then(Value::as_str) == Some("provider-private")
        }));
        assert!(
            inputs[2]
                .iter()
                .any(|item| item.get("_anthropic_content").is_some())
        );
    }
    agent
        .sender()
        .submit(Op::SetModel {
            route: "destination".into(),
        })
        .unwrap();
    while !matches!(
        agent.next_event().await.unwrap().msg,
        EventMsg::ModelChanged(_)
    ) {}
    let selected = checkpoints
        .load(&"reasoning-route-change".parse().expect("session ID"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        selected.context_epoch, 0,
        "selection alone keeps the cached prefix"
    );
    agent
        .sender()
        .submit(user_op("continue on the new route"))
        .unwrap();
    while !matches!(
        agent.next_event().await.unwrap().msg,
        EventMsg::TurnComplete(_)
    ) {}

    {
        let inputs = destination.inputs.lock().unwrap().clone();
        assert_eq!(
            inputs.len(),
            1,
            "a fitting history needs no checkpoint preparation"
        );
        let replay = &inputs[0];
        assert!(!replay.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("reasoning")
                || item.get("_anthropic_content").is_some()
                || item
                    .get(crate::backend::model::REPLAY_REASONING_FIELD)
                    .is_some()
        }));
        for kind in ["function_call", "function_call_output"] {
            assert!(replay.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some(kind)
                    && item.get("call_id").and_then(Value::as_str) == Some("reviewed-call")
            }));
        }
        let visible = serde_json::to_string(replay).unwrap();
        for text in [
            "run the tool",
            "Original answer.",
            "Same route answer.",
            "continue on the new route",
        ] {
            assert!(visible.contains(text), "visible history retains {text}");
        }
    }
    let saved = checkpoints
        .load(&"reasoning-route-change".parse().expect("session ID"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.context_epoch, 1);
    assert_eq!(saved.context_model_route.as_deref(), Some("destination"));
    let journal = checkpoints
        .transcript_page(
            &"reasoning-route-change".parse().expect("session ID"),
            TranscriptPageRequest {
                before_sequence: None,
                max_batches: 100,
            },
        )
        .await
        .unwrap()
        .into_positioned_items_chronological();
    assert!(journal.iter().any(
        |(_, item)| item.get("encrypted_content").and_then(Value::as_str)
            == Some("provider-private")
    ));
    assert!(
        journal
            .iter()
            .any(|(_, item)| item.get("_anthropic_content").is_some())
    );
}

#[tokio::test]
async fn stale_save_does_not_leapfrog_winning_checkpoint() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::<SqliteCheckpoint>::clone(&checkpoints);
    let agent = create_agent(config(workspace.path(), checkpoint_store, "target"))
        .await
        .expect("create agent");
    let mut winner = checkpoints
        .load(&"target".parse().expect("session ID"))
        .await
        .expect("load checkpoint")
        .expect("initial checkpoint");
    winner.sequence += 1;
    std::sync::Arc::make_mut(&mut winner.context)
        .push(std::sync::Arc::new(serde_json::json!({"winner": true})));
    checkpoints
        .save(winner.clone(), winner.context.to_vec(), None)
        .await
        .expect("save competing checkpoint");

    let (sender, mut events) = agent.into_parts();
    sender
        .submit(user_op("lose the checkpoint race"))
        .expect("submit turn");
    drop(sender);
    while events.recv().await.is_some() {}

    assert_eq!(
        checkpoints
            .load(&"target".parse().expect("session ID"))
            .await
            .expect("load checkpoint")
            .expect("winning checkpoint"),
        winner
    );
}

#[tokio::test]
async fn restart_recovers_streamed_calls_without_reexecuting_them() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let mut checkpoint = Checkpoint::empty("recover-streamed-call".parse().expect("session ID"));
    checkpoint.session_context = test_session_context().into();
    checkpoint.model_route = Some("test".into());
    checkpoint.active_execution = Some(crate::backend::checkpoint::ActiveExecution {
        author: crate::protocol::MessageAuthor::User.into(),
        submission_id: "submission-1".into(),
        turn_id: "turn-1".into(),
        started_at_ms: 10,
        model_calls: 1,
        tool_calls: 0,
        failed_tool_calls: 0,
        usage: TokenUsage::default(),
        next_model_step: 0,
        stop_hook_active: false,
        phase: crate::backend::checkpoint::ExecutionPhase::Model,
        idle: false,
    });
    checkpoint.active_model_step = Some(crate::backend::checkpoint::ActiveModelStep {
        model_step_id: "step-1".into(),
        step_index: 0,
        started_at_ms: 20,
    });
    checkpoint.pending_tools.push(ToolCall {
        call_id: "streamed-call".into(),
        name: "write_file".into(),
        arguments: serde_json::json!({"path": "never-replay.txt", "content": "side effect"}),
    });
    checkpoints
        .save(checkpoint.clone(), Vec::new(), None)
        .await
        .expect("save interrupted stream");

    let mut agent = create_agent(
        config(
            workspace.path(),
            Arc::<SqliteCheckpoint>::clone(&checkpoints),
            &checkpoint.session_id,
        )
        .initial_replay_batches(0),
    )
    .await
    .expect("recover agent");
    while !matches!(
        agent.next_event().await.expect("recovery event").msg,
        EventMsg::TurnAborted(_)
    ) {}
    let saved = checkpoints
        .load(&checkpoint.session_id)
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    assert!(saved.active_execution.is_none());
    assert!(saved.pending_tools.is_empty());
    assert!(!workspace.path().join("never-replay.txt").exists());
    assert_eq!(
        saved.context.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
        [
            serde_json::json!({
                "type": "function_call",
                "call_id": "streamed-call",
                "name": "write_file",
                "arguments": checkpoint.pending_tools[0].arguments.to_string(),
            }),
            crate::backend::model::tool_output(
                "streamed-call",
                &"execution interrupted; result unknown after restart".into(),
                true
            ),
        ]
        .iter()
        .collect::<Vec<_>>()
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Failure {
    ModelPanic,
    RecoveryPanic,
    TurnEndPanic,
    SubmitError,
    Close,
}

struct SupervisedModel(Failure);

impl Model for SupervisedModel {
    fn respond<'a>(
        &'a self,
        _request: ModelRequest,
        _events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(async move {
            assert!(
                !matches!(self.0, Failure::ModelPanic | Failure::RecoveryPanic),
                "injected model panic"
            );
            Ok(scripted_message("done"))
        })
    }
}

struct CleanupProbe {
    ended: Arc<Mutex<Vec<String>>>,
    turn_ends: Arc<AtomicUsize>,
    failure: Failure,
}

impl Middleware for CleanupProbe {
    fn name(&self) -> &'static str {
        "cleanup_probe"
    }

    fn turn_end<'a>(
        &'a self,
        _context: &'a mut crate::middleware::TurnEndContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.turn_ends.fetch_add(1, Ordering::SeqCst);
            assert!(
                !matches!(self.failure, Failure::TurnEndPanic | Failure::RecoveryPanic),
                "injected turn cleanup panic"
            );
            Ok(())
        })
    }

    fn session_end<'a>(
        &'a self,
        runtime: &'a crate::middleware::RuntimeContext,
    ) -> BoxFuture<'a, Result<()>> {
        self.ended
            .lock()
            .unwrap()
            .push(format!("{}:{}", runtime.model_route, runtime.model));
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn supervisor_seals_failures_once_and_ends_the_session_with_its_latest_model() {
    for failure in [
        Failure::TurnEndPanic,
        Failure::ModelPanic,
        Failure::RecoveryPanic,
        Failure::SubmitError,
        Failure::Close,
    ] {
        let workspace = tempfile::tempdir().expect("workspace");
        let checkpoints =
            Arc::new(SqliteCheckpoint::new(workspace.path().join("state.sqlite3")).unwrap());
        let ended = Arc::new(Mutex::new(Vec::new()));
        let turn_ends = Arc::new(AtomicUsize::new(0));
        let id: crate::SessionId = "supervised-runner".parse().expect("session ID");
        let mut config = config_with_model(
            workspace.path(),
            Arc::<SqliteCheckpoint>::clone(&checkpoints),
            &id,
            "test",
            Arc::new(TestModel),
        );
        Arc::get_mut(&mut config.model)
            .unwrap()
            .register("changed", Arc::new(SupervisedModel(failure)))
            .unwrap();
        let mut middleware: Vec<Arc<dyn Middleware>> = vec![Arc::new(CleanupProbe {
            ended: Arc::clone(&ended),
            turn_ends: Arc::clone(&turn_ends),
            failure,
        })];
        if failure == Failure::SubmitError {
            middleware.push(Arc::new(FailFirstMessageSubmit(Arc::new(AtomicBool::new(
                false,
            )))));
        }
        config.middleware = Arc::new(test_middleware(middleware));
        let mut agent = create_agent(config).await.expect("create agent");
        agent
            .sender()
            .submit(Op::SetModel {
                route: "changed".into(),
            })
            .unwrap();
        while !matches!(
            tokio::time::timeout(std::time::Duration::from_secs(3), agent.next_event())
                .await
                .unwrap()
                .unwrap()
                .msg,
            EventMsg::ModelChanged(_)
        ) {}
        let (sender, mut events) = agent.into_parts();
        let sender = if failure == Failure::Close {
            drop(sender);
            None
        } else {
            sender.submit(user_op("fail once")).expect("submit input");
            Some(sender)
        };
        let received = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut received = Vec::new();
            while let Some(event) = events.recv().await {
                received.push(event.msg);
            }
            received
        })
        .await
        .expect("the event stream closes after cleanup");
        drop(sender);
        let ran_model = matches!(
            failure,
            Failure::ModelPanic | Failure::RecoveryPanic | Failure::TurnEndPanic
        );
        assert_eq!(turn_ends.load(Ordering::SeqCst), usize::from(ran_model));
        assert_eq!(*ended.lock().unwrap(), ["changed:changed"]);
        let saved = checkpoints
            .load(&id)
            .await
            .unwrap()
            .expect("durable checkpoint");
        assert!(saved.active_execution.is_none(), "failed work is terminal");
        assert!(
            saved.active_model_step.is_none(),
            "failed model step is terminal"
        );
        assert!(saved.pending_tools.is_empty(), "no tool work is replayed");
        let completed: Vec<_> = received
            .iter()
            .filter_map(|event| match event {
                EventMsg::ModelStepCompleted(step) => Some(step),
                _ => None,
            })
            .collect();
        let errors: Vec<_> = received
            .iter()
            .filter_map(|event| match event {
                EventMsg::Error(error) => Some(error),
                _ => None,
            })
            .collect();
        assert_eq!(completed.len(), usize::from(ran_model));
        assert_eq!(
            received
                .iter()
                .filter(|event| matches!(event, EventMsg::TurnAborted(_)))
                .count(),
            usize::from(ran_model)
        );
        assert_eq!(errors.len(), usize::from(failure != Failure::Close));
        let expected_error = match failure {
            Failure::TurnEndPanic => Some("injected turn cleanup panic"),
            Failure::ModelPanic | Failure::RecoveryPanic => Some("injected model panic"),
            Failure::SubmitError => Some("message submit failed"),
            Failure::Close => None,
        };
        if let Some(expected) = expected_error {
            assert!(
                errors[0].message.contains(expected),
                "failure cause survives cleanup: {:?}",
                errors[0]
            );
        }
        assert_eq!(saved.execution_stats.failed_run_count, u64::from(ran_model));
        if ran_model {
            if failure == Failure::TurnEndPanic {
                assert!(
                    matches!(completed[0].outcome, ModelStepOutcome::Completed { .. }),
                    "model completed before the cleanup panic"
                );
            } else {
                assert_eq!(completed[0].outcome, ModelStepOutcome::Failed);
            }
            let history = checkpoints
                .execution_page(
                    &id,
                    ExecutionPageRequest {
                        before_sequence: None,
                        limit: 10,
                    },
                )
                .await
                .unwrap();
            assert_eq!(history.executions.len(), 1);
            assert_eq!(history.executions[0].outcome, ExecutionOutcome::Failed);
            assert_eq!(history.executions[0].model_calls, 1);
        }
    }
}
