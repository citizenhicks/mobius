//! Model Steps agent runtime tests.

use super::*;

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
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::<SqliteCheckpoint>::clone(&checkpoints);
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        checkpoint_store,
        "stream-retry",
        "test",
        Arc::<RecoveringStreamModel>::clone(&model),
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
    assert_eq!(&*search_end.call_id, "search-1");
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
async fn interrupted_stream_exhaustion_surfaces_only_the_safe_provider_error() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(InterruptedStreamModel {
        transport: ModelTransportSettings::default(),
        calls: AtomicUsize::new(0),
        retry_after: None,
    });
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        checkpoints,
        "stream-exhaustion",
        "test",
        Arc::<InterruptedStreamModel>::clone(&model),
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

    assert_eq!(model.calls.load(Ordering::SeqCst), stream_retry_limit() + 1);
    assert_eq!(started.len(), stream_retry_limit() + 1);
    assert!(started.windows(2).all(|steps| {
        steps[0].model_step_id != steps[1].model_step_id
            && steps[0].step_index == steps[1].step_index
    }));
    assert_eq!(outcomes.len(), stream_retry_limit() + 1);
    assert!(
        outcomes[..stream_retry_limit()]
            .iter()
            .all(|outcome| *outcome == ModelStepOutcome::Retrying)
    );
    assert_eq!(outcomes[stream_retry_limit()], ModelStepOutcome::Failed);
    let failure = failure.expect("terminal provider error");
    assert_eq!(failure.kind, ErrorKind::Provider);
    assert!(failure.retryable);
    assert_eq!(
        &*failure.message,
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
        transport: ModelTransportSettings::default(),
        calls: AtomicUsize::new(0),
        retry_after: Some("3".into()),
    });
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        checkpoints,
        "stream-backoff-interrupt",
        "test",
        Arc::<InterruptedStreamModel>::clone(&model),
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
