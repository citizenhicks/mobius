//! Configuration agent runtime tests.

use super::*;

struct FailFirstTurnEnd(Arc<Mutex<Vec<ExecutionOutcome>>>);

impl Middleware for FailFirstTurnEnd {
    fn name(&self) -> &'static str {
        "fail_first_turn_end"
    }

    fn turn_end<'a>(
        &'a self,
        context: &'a mut crate::middleware::TurnEndContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut outcomes = self.0.lock().expect("turn-end outcomes lock");
            let first = outcomes.is_empty();
            outcomes.push(context.outcome());
            drop(outcomes);
            if first {
                Err(Error::Stopped("turn-end hook failed".into()))
            } else {
                Ok(())
            }
        })
    }
}

#[tokio::test]
async fn failing_turn_end_hook_runs_once_with_the_original_outcome() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([scripted_message("done")])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let mut agent_config = config_with_model(
        workspace.path(),
        checkpoints,
        "turn-end-at-most-once",
        "main",
        model,
    );
    agent_config.middleware =
        test_middleware(vec![Arc::new(FailFirstTurnEnd(Arc::clone(&outcomes)))]);
    let mut agent = create_agent(agent_config).await.expect("create agent");
    agent
        .sender()
        .submit(user_op("hello"))
        .expect("submit input");
    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::TurnAborted(_)
    ) {}

    assert_eq!(
        outcomes.lock().expect("turn-end outcomes lock").as_slice(),
        [ExecutionOutcome::Completed]
    );
}

#[tokio::test]
async fn middleware_event_saturation_fails_agent_creation_instead_of_dropping_updates() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let mut agent_config = config(workspace.path(), checkpoint_store, "saturating-events");
    agent_config.middleware = test_middleware(vec![Arc::new(SaturatingMiddleware)]);

    let Err(error) = create_agent(agent_config).await else {
        panic!("agent creation should report the full event queue");
    };

    assert_eq!(
        error.to_string(),
        "agent stopped: event recorder queue is full"
    );
    assert!(
        checkpoints
            .load("saturating-events")
            .await
            .expect("load failed session")
            .is_none()
    );
}

#[tokio::test]
async fn configured_approval_policy_ignores_checkpoint_middleware_state() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    checkpoints
        .save_state(
            "policy-authority",
            "sandbox.approval_policy",
            &serde_json::json!("allow_network"),
        )
        .await
        .expect("seed stale policy");
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints;
    let mut agent = create_agent(config(
        workspace.path(),
        checkpoint_store,
        "policy-authority",
    ))
    .await
    .expect("create agent");
    agent.next_event().await.expect("configured event");
    let EventMsg::Frontend(FrontendEvent::Widget { item, .. }) =
        agent.next_event().await.expect("sandbox widget").msg
    else {
        panic!("expected sandbox widget");
    };

    assert_eq!(item.text, "approval ASK");
}

#[tokio::test]
async fn request_only_input_reaches_the_model_without_entering_the_checkpoint() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([scripted_message("done")])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("main", model.clone())),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoint_store,
        test_middleware(vec![Arc::new(RequestOnlyMiddleware)]),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id("request-only");
    let mut agent = create_agent(config).await.expect("create agent");
    agent.next_event().await.expect("configured event");
    agent.next_event().await.expect("sandbox widget");
    agent
        .sender()
        .submit(user_op("hello"))
        .expect("submit input");
    loop {
        if matches!(
            agent.next_event().await.expect("agent event").msg,
            EventMsg::TurnComplete(_)
        ) {
            break;
        }
    }
    let saved = checkpoints
        .load("request-only")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");

    assert!(
        model.inputs.lock().expect("input lock")[0]
            .iter()
            .any(|item| internal_message_kind(item) == Some("request_only"))
    );
    assert!(
        saved
            .context
            .iter()
            .all(|item| internal_message_kind(item) != Some("request_only"))
    );
}

#[tokio::test]
async fn configured_model_step_limit_stops_after_primary_model_calls() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([
            scripted_continuation("one"),
            scripted_continuation("two"),
            scripted_message("unexpected"),
        ])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints;
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("main", model.clone())),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoint_store,
        test_middleware(Vec::new()),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id("model-step-limit")
    .max_model_steps(2);
    let mut agent = create_agent(config).await.expect("create agent");
    agent.next_event().await.expect("configured event");
    agent.next_event().await.expect("sandbox widget");
    agent
        .sender()
        .submit(user_op("continue"))
        .expect("submit input");
    let message = loop {
        if let EventMsg::Error(error) = agent.next_event().await.expect("agent event").msg {
            break error.message;
        }
    };

    assert_eq!(model.inputs.lock().expect("input lock").len(), 2);
    assert_eq!(
        message,
        "agent stopped: turn reached the configured limit of 2 model steps"
    );
}

#[tokio::test]
async fn zero_model_step_limit_is_rejected_at_agent_creation() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let result = create_agent(
        config(workspace.path(), checkpoints, "zero-model-step-limit").max_model_steps(0),
    )
    .await;
    let Err(error) = result else {
        panic!("zero model-step limit must fail");
    };

    assert_eq!(
        error.to_string(),
        "configuration error: maximum model steps must be positive"
    );
}

#[tokio::test]
async fn completed_pre_model_effects_are_settled_when_a_later_hook_fails() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let observed_usage = Arc::new(Mutex::new(Vec::new()));
    let usage_observer = Arc::clone(&observed_usage);
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("main", Arc::new(TestModel))),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoint_store,
        test_middleware(vec![
            Arc::new(DurableBeforeModel),
            Arc::new(FailingBeforeModel),
        ]),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id("settled-hooks")
    .usage_observer(move |route, usage| {
        usage_observer
            .lock()
            .expect("usage observer lock")
            .push((route.to_owned(), usage.total_tokens));
        Ok(())
    });
    let mut agent = create_agent(config).await.expect("create agent");
    agent.next_event().await.expect("configured event");
    agent.next_event().await.expect("sandbox widget");
    agent
        .sender()
        .submit(user_op("hello"))
        .expect("submit input");
    let mut saw_effect = false;
    loop {
        match agent.next_event().await.expect("agent event").msg {
            EventMsg::ContextCompacted => saw_effect = true,
            EventMsg::TurnAborted(_) => break,
            _ => {}
        }
    }
    let saved = checkpoints
        .load("settled-hooks")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    let execution = checkpoints
        .execution_page(
            "settled-hooks",
            ExecutionPageRequest {
                before_sequence: None,
                limit: 1,
            },
        )
        .await
        .expect("execution page")
        .executions
        .pop()
        .expect("failed execution");

    assert!(saw_effect);
    assert_eq!(
        observed_usage
            .lock()
            .expect("observed usage lock")
            .as_slice(),
        [("main".into(), 1)]
    );
    assert_eq!(saved.total_usage.total_tokens, 1);
    assert_eq!(
        (
            execution.outcome,
            execution.model_calls,
            execution.usage.total_tokens,
            saved.execution_stats.failed_run_count,
        ),
        (ExecutionOutcome::Failed, 0, 1, 1)
    );
    assert!(
        saved
            .context
            .iter()
            .any(|item| internal_message_kind(item) == Some("settled"))
    );
}
