//! Model Steps agent runtime tests.

use super::*;

#[tokio::test]
async fn model_step_lifecycle_preserves_correlation_usage_and_content() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([scripted_message("Done.")])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        checkpoints,
        "step-lifecycle",
        "test",
        model,
    ))
    .await
    .expect("create agent");
    agent
        .sender()
        .submit(user_op("hello"))
        .expect("submit input");

    let mut started = None;
    let mut completed = None;
    let mut message = None;
    while let Some(event) = agent.next_event().await {
        match event.msg {
            EventMsg::ModelStepStarted(event) => started = Some(event),
            EventMsg::ModelStepCompleted(event) => completed = Some(event),
            EventMsg::AssistantMessage(event) => message = Some(event),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    let started = started.expect("model step started");
    let completed = completed.expect("model step completed");
    let message = message.expect("agent message");
    assert_eq!(started.session_id, "step-lifecycle");
    assert_eq!(started.step_index, 0);
    assert!(started.started_at_ms >= 0);
    assert_eq!(completed.session_id, started.session_id);
    assert_eq!(completed.turn_id, started.turn_id);
    assert_eq!(completed.model_step_id, started.model_step_id);
    assert_eq!(completed.started_at_ms, started.started_at_ms);
    assert!(completed.completed_at_ms >= completed.started_at_ms);
    let diagnostics = completed.diagnostics.as_ref().expect("step diagnostics");
    assert_eq!(diagnostics.provider, "test");
    assert_eq!(
        diagnostics.prompt_cache.capability,
        crate::protocol::PromptCacheMode::Unsupported
    );
    assert_eq!(
        diagnostics.prompt_cache.outcome,
        crate::protocol::PromptCacheOutcome::Unsupported
    );
    assert_eq!(diagnostics.prompt_cache.context_epoch, 0);
    assert!(diagnostics.prompt_cache.rewrite_reasons.is_empty());
    assert_eq!(diagnostics.estimated_cost_microusd, None);
    assert_eq!(
        completed.outcome,
        ModelStepOutcome::Completed {
            end_turn: true,
            tool_call_ids: Vec::new(),
            usage: scripted_usage(),
        }
    );
    assert_eq!(
        message.content,
        vec![ModelStepContent {
            output_index: 0,
            part_index: 0,
            phase: ModelStepContentPhase::FinalAnswer,
            text: "Done.".into(),
            annotations: Vec::new(),
        }]
    );
    assert_eq!(message.session_id, started.session_id);
    assert_eq!(message.turn_id, started.turn_id);
    assert_eq!(message.model_step_id, started.model_step_id);
}

#[tokio::test(start_paused = true)]
async fn interrupted_stream_retries_in_a_fresh_model_step() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(RecoveringStreamModel {
        calls: AtomicUsize::new(0),
        inputs: Mutex::new(Vec::new()),
    });
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        checkpoint_store,
        "stream-retry",
        "test",
        model.clone(),
    ))
    .await
    .expect("create agent");
    agent
        .sender()
        .submit(user_op("hello"))
        .expect("submit input");

    let mut started = Vec::new();
    let mut completed = Vec::new();
    let mut delta = None;
    let mut message = None;
    let mut tool_begins = 0;
    let mut web_search = None;
    let mut web_search_ends = Vec::new();
    while let Some(event) = agent.next_event().await {
        match event.msg {
            EventMsg::ModelStepStarted(event) => started.push(event),
            EventMsg::ModelStepCompleted(event) => completed.push(event),
            EventMsg::AssistantContentDelta(event) => delta = Some(event),
            EventMsg::AssistantMessage(event) => message = Some(event),
            EventMsg::ToolCallBegin(_) => tool_begins += 1,
            EventMsg::WebSearchBegin(event) => web_search = Some(event),
            EventMsg::WebSearchEnd(event) => web_search_ends.push(event),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    let saved = checkpoints
        .load("stream-retry")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");

    assert_eq!(started.len(), 2);
    assert_ne!(started[0].model_step_id, started[1].model_step_id);
    assert_eq!((started[0].step_index, started[1].step_index), (0, 0));
    assert_eq!(completed.len(), 2);
    assert_eq!(completed[0].model_step_id, started[0].model_step_id);
    assert_eq!(completed[0].outcome, ModelStepOutcome::Retrying);
    assert_eq!(completed[1].model_step_id, started[1].model_step_id);
    assert!(matches!(
        &completed[1].outcome,
        ModelStepOutcome::Completed { .. }
    ));
    assert_eq!(
        delta.expect("partial delta").model_step_id,
        started[0].model_step_id
    );
    assert_eq!(
        message.expect("recovered message").model_step_id,
        started[1].model_step_id
    );
    assert_eq!(tool_begins, 0);
    assert_eq!(
        web_search.expect("interrupted web search").model_step_id,
        started[0].model_step_id
    );
    // The failed attempt started a search it could never finish, so the backend
    // closes it out instead of leaving frontends to infer the interruption.
    let [search_end] = web_search_ends.as_slice() else {
        panic!("expected exactly one web search end, got {web_search_ends:?}");
    };
    assert_eq!(search_end.call_id, "search-1");
    assert_eq!(search_end.model_step_id, started[0].model_step_id);
    assert!(matches!(search_end.action, WebSearchAction::Interrupted));
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    let inputs = model.inputs.lock().expect("stream input lock");
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0], inputs[1]);
    let context = serde_json::to_string(&saved.context).expect("serialize context");
    assert!(!context.contains("partial"));
    assert!(context.contains("Recovered."));
}

#[tokio::test(start_paused = true)]
async fn steer_during_stream_retry_rebuilds_the_request_input() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(RecoveringStreamModel {
        calls: AtomicUsize::new(0),
        inputs: Mutex::new(Vec::new()),
    });
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        checkpoints,
        "stream-retry-steer",
        "test",
        model.clone(),
    ))
    .await
    .expect("create agent");
    agent
        .sender()
        .submit(user_op("hello"))
        .expect("submit input");
    let turn_id = loop {
        if let EventMsg::ModelStepCompleted(completed) =
            agent.next_event().await.expect("retry event").msg
            && completed.outcome == ModelStepOutcome::Retrying
        {
            break completed.turn_id;
        }
    };
    let steer_id = agent
        .sender()
        .submit(active_user_op(
            "new direction",
            turn_id,
            ActiveMessageDelivery::Steer,
        ))
        .expect("submit steer");
    loop {
        if let EventMsg::Frontend(FrontendEvent::Widget { item, .. }) =
            agent.next_event().await.expect("steer acknowledgement").msg
            && item.id == steer_id
        {
            break;
        }
    }
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    while !matches!(
        agent.next_event().await.expect("completed turn").msg,
        EventMsg::TurnComplete(_)
    ) {}

    let inputs = model.inputs.lock().expect("stream input lock");
    assert_eq!(inputs.len(), 2);
    let first = serde_json::to_string(&inputs[0]).expect("first input");
    let retried = serde_json::to_string(&inputs[1]).expect("retried input");
    assert!(!first.contains("new direction"));
    assert!(retried.contains("new direction"));
}

#[tokio::test(start_paused = true)]
async fn interrupted_stream_exhaustion_surfaces_only_the_safe_provider_error() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(InterruptedStreamModel {
        calls: AtomicUsize::new(0),
        retry_after: None,
    });
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        checkpoints,
        "stream-exhaustion",
        "test",
        model.clone(),
    ))
    .await
    .expect("create agent");
    agent
        .sender()
        .submit(user_op("hello"))
        .expect("submit input");

    let mut started = Vec::new();
    let mut outcomes = Vec::new();
    let mut failure = None;
    while let Some(event) = agent.next_event().await {
        match event.msg {
            EventMsg::ModelStepStarted(event) => started.push(event),
            EventMsg::ModelStepCompleted(event) => outcomes.push(event.outcome),
            EventMsg::Error(event) => failure = Some(event),
            EventMsg::TurnAborted(_) => break,
            _ => {}
        }
    }

    assert_eq!(model.calls.load(Ordering::SeqCst), STREAM_RETRY_LIMIT + 1);
    assert_eq!(started.len(), STREAM_RETRY_LIMIT + 1);
    assert!(started.windows(2).all(|steps| {
        steps[0].model_step_id != steps[1].model_step_id
            && steps[0].step_index == steps[1].step_index
    }));
    assert_eq!(outcomes.len(), STREAM_RETRY_LIMIT + 1);
    assert!(
        outcomes[..STREAM_RETRY_LIMIT]
            .iter()
            .all(|outcome| *outcome == ModelStepOutcome::Retrying)
    );
    assert_eq!(outcomes[STREAM_RETRY_LIMIT], ModelStepOutcome::Failed);
    let failure = failure.expect("terminal provider error");
    assert_eq!(failure.kind, ErrorKind::Provider);
    assert!(failure.retryable);
    assert_eq!(
        failure.message,
        "provider error: model response stream was interrupted"
    );
}

#[tokio::test]
async fn interrupt_during_stream_retry_backoff_cancels_the_retry() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(InterruptedStreamModel {
        calls: AtomicUsize::new(0),
        retry_after: Some("30".into()),
    });
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        checkpoints,
        "stream-backoff-interrupt",
        "test",
        model.clone(),
    ))
    .await
    .expect("create agent");
    agent
        .sender()
        .submit(user_op("hello"))
        .expect("submit input");
    let turn_id = loop {
        if let EventMsg::ModelStepCompleted(completed) =
            agent.next_event().await.expect("model step completion").msg
            && completed.outcome == ModelStepOutcome::Retrying
        {
            break completed.turn_id;
        }
    };
    agent
        .sender()
        .submit(Op::Interrupt { turn_id })
        .expect("interrupt turn");

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !matches!(
            agent.next_event().await.expect("agent event").msg,
            EventMsg::TurnAborted(_)
        ) {}
    })
    .await
    .expect("backoff cancellation");
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_model_step_retains_provider_retry_metadata() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        checkpoints,
        "failed-step",
        "test",
        Arc::new(RetryableModel),
    ))
    .await
    .expect("create agent");
    agent
        .sender()
        .submit(user_op("hello"))
        .expect("submit input");

    let mut terminal = None;
    let mut failure = None;
    while let Some(event) = agent.next_event().await {
        match event.msg {
            EventMsg::ModelStepCompleted(event) => terminal = Some(event),
            EventMsg::Error(event) => failure = Some(event),
            EventMsg::TurnAborted(_) => break,
            _ => {}
        }
    }

    assert_eq!(
        terminal.expect("terminal step").outcome,
        ModelStepOutcome::Failed
    );
    let failure = failure.expect("structured provider error");
    assert_eq!(failure.kind, ErrorKind::Provider);
    assert!(failure.retryable);
    assert_eq!(failure.status, Some(429));
    assert_eq!(failure.retry_after.as_deref(), Some("5"));
    assert!(failure.message.contains("quota exceeded"));
}

#[tokio::test]
async fn interrupted_model_request_emits_one_terminal_step() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let entered = Arc::new(Notify::new());
    let model = Arc::new(BlockingModel {
        started: Arc::clone(&entered),
        release: Arc::new(Notify::new()),
        calls: AtomicUsize::new(0),
    });
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        checkpoints,
        "interrupted-step",
        "test",
        model,
    ))
    .await
    .expect("create agent");
    agent
        .sender()
        .submit(user_op("hello"))
        .expect("submit input");
    let started = loop {
        if let EventMsg::ModelStepStarted(started) =
            agent.next_event().await.expect("model step started").msg
        {
            break started;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
        .await
        .expect("model entered");
    agent
        .sender()
        .submit(Op::Interrupt {
            turn_id: started.turn_id.clone(),
        })
        .expect("interrupt turn");

    let mut terminal = Vec::new();
    while let Some(event) = agent.next_event().await {
        match event.msg {
            EventMsg::ModelStepCompleted(event) => terminal.push(event),
            EventMsg::TurnAborted(_) => break,
            _ => {}
        }
    }

    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0].model_step_id, started.model_step_id);
    assert_eq!(terminal[0].outcome, ModelStepOutcome::Interrupted);
}
