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
        .load(&"stream-retry".parse().expect("session ID"))
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

#[derive(Default)]
struct RepeatedTool {
    polls: bool,
    parallel: bool,
    runs: Arc<std::sync::atomic::AtomicUsize>,
}

impl crate::middleware::tools::Tool for RepeatedTool {
    fn definition(&self) -> &Arc<ToolDefinition> {
        static DEFINITION: std::sync::LazyLock<Arc<ToolDefinition>> =
            std::sync::LazyLock::new(|| {
                Arc::new(ToolDefinition {
                    name: "repeated".into(),
                    description: "returns the same result".into(),
                    parameters: serde_json::json!({"type": "object", "properties": {}}),
                })
            });
        &DEFINITION
    }

    fn exposure(&self) -> crate::middleware::tools::ToolExposure {
        crate::middleware::tools::ToolExposure::Direct
    }

    fn polls(&self) -> bool {
        self.polls
    }

    fn execution_mode(&self) -> crate::middleware::tools::ExecutionMode {
        if self.parallel {
            crate::middleware::tools::ExecutionMode::Parallel
        } else {
            crate::middleware::tools::ExecutionMode::Exclusive
        }
    }

    fn call(
        &self,
        _context: crate::middleware::tools::ToolContext,
        _arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        Box::pin(async {
            // Lets every parallel call in a batch start before the first one finishes.
            tokio::task::yield_now().await;
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok("same".to_owned().into())
        })
    }
}

/// Runs a turn whose model calls the tool `calls` times, then answers.
async fn run_repeated_tool(polls: bool, calls: usize) -> (Arc<ScriptedModel>, usize, Vec<String>) {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let mut outputs: VecDeque<ModelOutput> = (0..calls)
        .map(|index| {
            ModelOutput::from_output(
                vec![serde_json::json!({
                    "type": "function_call",
                    "call_id": format!("call-{index}"),
                    "name": "repeated",
                    "arguments": "{}"
                })],
                false,
                scripted_usage(),
            )
            .expect("tool output")
        })
        .collect();
    outputs.push_back(scripted_message("done"));
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(outputs),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let limit = |count| std::num::NonZeroUsize::new(count).expect("positive limit");
    let tools = crate::middleware::tools::Tools::new(vec![Arc::new(RepeatedTool {
        polls,
        ..RepeatedTool::default()
    })])
    .repeat_limits(limit(2), limit(3));
    let mut config = config_with_model(
        workspace.path(),
        checkpoints,
        "repeated-tool",
        "test",
        Arc::<ScriptedModel>::clone(&model),
    );
    config.middleware = Arc::new(test_middleware(vec![Arc::new(tools)]));
    let mut agent = create_agent(config).await.expect("create agent");
    agent.sender().submit(user_op("go")).expect("submit input");
    let mut begins = 0;
    let mut warnings = Vec::new();
    while let Some(event) = agent.next_event().await {
        match event.msg {
            EventMsg::ToolCallBegin(_) => begins += 1,
            EventMsg::Warning(warning) => warnings.push(warning.message.to_string()),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    (model, begins, warnings)
}

#[tokio::test]
async fn repeated_tool_results_warn_the_model_then_stop_the_turn() {
    let (model, begins, warnings) = run_repeated_tool(false, 10).await;
    assert_eq!(begins, 3);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("kept repeating"));
    let inputs = std::mem::take(&mut *model.inputs.lock().expect("input lock"));
    assert_eq!(inputs.len(), 3);
    let warned = |input: &Vec<Value>| {
        input
            .iter()
            .any(|item| item.to_string().contains("not making progress"))
    };
    assert!(!warned(&inputs[1]));
    assert!(warned(&inputs[2]));
}

/// Runs a turn whose one response calls the tool five times; returns how many handlers ran,
/// each ended call in order with its output, and how many model requests there were.
async fn run_repeated_batch(parallel: bool) -> (usize, Vec<(String, String)>, usize) {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let calls = (0..5)
        .map(|index| {
            serde_json::json!({
                "type": "function_call",
                "call_id": format!("call-{index}"),
                "name": "repeated",
                "arguments": "{}"
            })
        })
        .collect();
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([
            ModelOutput::from_output(calls, false, scripted_usage()).expect("tool output"),
            scripted_message("done"),
        ])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let limit = |count| std::num::NonZeroUsize::new(count).expect("positive limit");
    let tool = RepeatedTool {
        parallel,
        ..RepeatedTool::default()
    };
    let runs = Arc::clone(&tool.runs);
    let tools = crate::middleware::tools::Tools::new(vec![Arc::new(tool)])
        .repeat_limits(limit(1), limit(2));
    let mut config = config_with_model(
        workspace.path(),
        checkpoints,
        "repeated-batch",
        "test",
        Arc::<ScriptedModel>::clone(&model),
    );
    config.middleware = Arc::new(test_middleware(vec![Arc::new(tools)]));
    let mut agent = create_agent(config).await.expect("create agent");
    agent.sender().submit(user_op("go")).expect("submit input");
    let mut ended = Vec::new();
    while let Some(event) = agent.next_event().await {
        match event.msg {
            EventMsg::ToolCallEnd(end) => {
                ended.push((
                    end.call_id.to_string(),
                    serde_json::to_string(&end.output).expect("output"),
                ));
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    let requests = model.inputs.lock().expect("input lock").len();
    (runs.load(Ordering::SeqCst), ended, requests)
}

#[tokio::test]
async fn the_repeat_limit_skips_the_rest_of_an_exclusive_batch() {
    let (runs, ended, requests) = run_repeated_batch(false).await;
    assert_eq!(runs, 2);
    assert_eq!(requests, 1);
    let ids: Vec<_> = ended.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["call-0", "call-1", "call-2", "call-3", "call-4"]);
    let skipped = ended
        .iter()
        .filter(|(_, output)| output.contains("Not run"))
        .count();
    assert_eq!(skipped, 3);
    assert!(!ended[1].1.contains("Not run") && ended[2].1.contains("Not run"));
}

#[tokio::test]
async fn parallel_calls_that_already_started_finish_before_the_turn_stops() {
    let (runs, ended, requests) = run_repeated_batch(true).await;
    assert_eq!(runs, 5);
    assert_eq!(ended.len(), 5);
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn polling_tools_may_repeat_without_stopping_the_turn() {
    let (model, begins, warnings) = run_repeated_tool(true, 10).await;
    assert_eq!(begins, 10);
    assert!(warnings.is_empty());
    assert_eq!(model.inputs.lock().expect("input lock").len(), 11);
}
