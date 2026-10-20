//! Streaming tool execution tests.

use super::*;
use crate::ProviderError;
use crate::backend::model::ToolDefinition;
use crate::middleware::tools::{ExecutionMode, Tool, ToolContext, ToolExposure, Tools};
use crate::protocol::{ModelEvent, ToolCall};

#[derive(Clone, Copy)]
enum Terminal {
    Changed,
    Error,
    HttpError,
    Hold,
    Repaired { streaming: bool },
}

struct StreamingModel {
    calls: AtomicUsize,
    release: Arc<Notify>,
    terminal: Terminal,
    tool_finished: Arc<Notify>,
    tool_started: Arc<Notify>,
    wait_for_tool_start: bool,
}

struct StreamingTool {
    finished: Arc<AtomicBool>,
    release: Option<Arc<Notify>>,
    started: Arc<Notify>,
    tool_finished: Arc<Notify>,
}

#[derive(Default)]
struct StreamingHooks {
    rewrite: bool,
}

struct BarrierStreamingModel {
    barrier: Arc<BarrierState>,
    calls: AtomicUsize,
}

struct BarrierStreamingTool {
    barrier: Arc<BarrierState>,
    name: &'static str,
    definition: Arc<ToolDefinition>,
}

struct BarrierState {
    a_started: Notify,
    b_started: Notify,
    c_started: Notify,
    c_prepared: Notify,
    release_a: Notify,
    release_b: Notify,
    release_stream: Notify,
    order: Mutex<Vec<String>>,
}

impl Middleware for BarrierState {
    fn name(&self) -> &'static str {
        "streaming_barrier_state"
    }

    fn pre_tool_use<'a>(
        &'a self,
        context: &'a mut PreToolUseContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if &*context.call().name == "parallel_c" {
                self.c_prepared.notify_one();
            }
            Ok(())
        })
    }
}

impl Model for BarrierStreamingModel {
    fn respond<'a>(
        &'a self,
        _request: ModelRequest,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
        if attempt > 0 {
            return Box::pin(async { Ok(scripted_message("done")) });
        }
        let barrier = Arc::clone(&self.barrier);
        Box::pin(async move {
            for (call_id, name) in [
                ("barrier-a", "parallel_a"),
                ("barrier-b", "exclusive_b"),
                ("barrier-c", "parallel_c"),
            ] {
                events(ModelEvent::ToolCallReady(ToolCall {
                    call_id: call_id.into(),
                    name: name.into(),
                    arguments: serde_json::json!({}),
                }))
                .await?;
            }
            barrier.c_prepared.notified().await;
            events(ModelEvent::TextDelta("barrier-admitted".into())).await?;
            barrier.release_stream.notified().await;
            ModelOutput::from_output(
                [
                    ("barrier-a", "parallel_a"),
                    ("barrier-b", "exclusive_b"),
                    ("barrier-c", "parallel_c"),
                ]
                .into_iter()
                .map(|(call_id, name)| {
                    serde_json::json!({
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": "{}"
                    })
                })
                .collect(),
                false,
                scripted_usage(),
            )
        })
    }
}

impl Tool for BarrierStreamingTool {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &self.definition
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn execution_mode(&self) -> ExecutionMode {
        if self.name == "exclusive_b" {
            ExecutionMode::Exclusive
        } else {
            ExecutionMode::Parallel
        }
    }

    fn call(
        &self,
        _context: ToolContext,
        _arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        let barrier = Arc::clone(&self.barrier);
        let name = self.name;
        Box::pin(async move {
            barrier
                .order
                .lock()
                .expect("barrier order lock")
                .push(name.into());
            match name {
                "parallel_a" => {
                    barrier.a_started.notify_one();
                    barrier.release_a.notified().await;
                }
                "exclusive_b" => {
                    barrier.b_started.notify_one();
                    barrier.release_b.notified().await;
                }
                "parallel_c" => barrier.c_started.notify_one(),
                _ => unreachable!("unknown barrier tool"),
            }
            Ok(name.into())
        })
    }
}

impl Model for StreamingModel {
    fn respond<'a>(
        &'a self,
        _request: ModelRequest,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
        if attempt > 0 {
            return Box::pin(async { Ok(scripted_message("done")) });
        }
        let release = Arc::clone(&self.release);
        let terminal = self.terminal;
        let tool_finished = Arc::clone(&self.tool_finished);
        let tool_started = Arc::clone(&self.tool_started);
        let wait_for_tool_start = self.wait_for_tool_start;
        Box::pin(async move {
            if !matches!(terminal, Terminal::Repaired { streaming: false }) {
                events(ModelEvent::ToolCallReady(ToolCall {
                    call_id: "stream-call".into(),
                    name: "streaming_tool".into(),
                    arguments: serde_json::from_str(streamed_arguments(terminal))?,
                }))
                .await?;
            }
            if wait_for_tool_start {
                tool_started.notified().await;
            }
            if matches!(
                terminal,
                Terminal::Changed
                    | Terminal::Error
                    | Terminal::HttpError
                    | Terminal::Repaired { streaming: true }
            ) {
                tool_finished.notified().await;
            }
            match terminal {
                Terminal::Changed => Ok(streaming_tool_output_with_arguments(
                    "{\"value\":\"changed\"}",
                    false,
                )),
                Terminal::Error => Err(Error::Provider(ProviderError::stream_interrupted(None))),
                Terminal::HttpError => Err(Error::Provider(ProviderError::http(
                    "busy",
                    503,
                    Some("1".into()),
                ))),
                Terminal::Hold => {
                    release.notified().await;
                    Ok(streaming_tool_output(false))
                }
                Terminal::Repaired { .. } => Ok(streaming_tool_output_with_arguments(
                    streamed_arguments(terminal),
                    false,
                )),
            }
        })
    }
}

impl Tool for StreamingTool {
    fn definition(&self) -> &std::sync::Arc<ToolDefinition> {
        static DEFINITION: std::sync::LazyLock<std::sync::Arc<ToolDefinition>> =
            std::sync::LazyLock::new(|| {
                std::sync::Arc::new(ToolDefinition {
                    name: "streaming_tool".into(),
                    description: "a deterministic streaming test tool".into(),
                    parameters: serde_json::json!({"type": "object", "properties": {
                        "limit": {"type": "integer"}
                    }}),
                })
            });
        &DEFINITION
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn call(
        &self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        if let Some(limit) = arguments.get("limit") {
            assert_eq!(limit, 42);
        }
        let finished = Arc::clone(&self.finished);
        let release = self.release.clone();
        let started = Arc::clone(&self.started);
        let tool_finished = Arc::clone(&self.tool_finished);
        Box::pin(async move {
            started.notify_one();
            if let Some(release) = release {
                release.notified().await;
            }
            finished.store(true, Ordering::SeqCst);
            tool_finished.notify_one();
            Ok("streamed result".into())
        })
    }
}

fn streamed_arguments(terminal: Terminal) -> &'static str {
    if matches!(terminal, Terminal::Repaired { .. }) {
        r#"{"value":"ready","limit":"42"}"#
    } else {
        r#"{"value":"ready"}"#
    }
}

impl Middleware for StreamingHooks {
    fn name(&self) -> &'static str {
        "streaming_hooks"
    }

    fn pre_tool_use<'a>(
        &'a self,
        context: &'a mut PreToolUseContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.rewrite {
                context.replace(
                    "streaming_tool",
                    serde_json::json!({"value": "rewritten", "limit": "42"}),
                )?;
            }
            Ok(())
        })
    }

    fn post_tool_use<'a>(
        &'a self,
        context: &'a mut PostToolUseContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            context.deliver_once("after", || {
                crate::backend::model::internal_user_message("streaming_once_after", "after tools")
            })?;
            Ok(())
        })
    }
}

fn streaming_tool_output(end_turn: bool) -> ModelOutput {
    streaming_tool_output_with_arguments("{\"value\":\"ready\"}", end_turn)
}

fn streaming_tool_output_with_arguments(arguments: &str, end_turn: bool) -> ModelOutput {
    ModelOutput::from_output(
        vec![serde_json::json!({
            "type": "function_call",
            "call_id": "stream-call",
            "name": "streaming_tool",
            "arguments": arguments
        })],
        end_turn,
        scripted_usage(),
    )
    .expect("streaming tool output")
}

fn streaming_config(
    workspace: &Path,
    checkpoints: Arc<dyn CheckpointStore>,
    session_id: &str,
    model: Arc<dyn Model>,
    tool: Arc<dyn Tool>,
    hooks: Arc<dyn Middleware>,
) -> AgentConfig {
    streaming_config_with_tools(workspace, checkpoints, session_id, model, vec![tool], hooks)
}

fn streaming_config_with_tools(
    workspace: &Path,
    checkpoints: Arc<dyn CheckpointStore>,
    session_id: &str,
    model: Arc<dyn Model>,
    tools: Vec<Arc<dyn Tool>>,
    hooks: Arc<dyn Middleware>,
) -> AgentConfig {
    AgentConfig::new(
        Arc::new(ModelRouter::new("streaming-test", model)),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace).expect("local sandbox")),
            ApprovalPolicy::Allow,
        )),
        checkpoints,
        test_middleware(vec![Arc::new(Tools::new(tools)), hooks]),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id(session_id.parse().expect("session ID"))
}

async fn collect_until_turn_end(agent: &mut Agent) -> Vec<EventMsg> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut events = Vec::new();
        while let Some(event) = agent.next_event().await {
            let terminal = matches!(
                event.msg,
                EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
            );
            events.push(event.msg);
            if terminal {
                break;
            }
        }
        events
    })
    .await
    .expect("streaming turn did not reach a terminal event")
}

fn canonical_tool_items(context: &[Arc<Value>]) -> Vec<String> {
    context
        .iter()
        .filter_map(|item| {
            let kind = item.get("type")?.as_str()?;
            let call_id = item.get("call_id")?.as_str()?;
            matches!(kind, "function_call" | "function_call_output")
                .then(|| format!("{kind}:{call_id}"))
        })
        .collect()
}

#[tokio::test]
async fn argument_repair_preserves_the_provider_history_prefix() {
    for (streaming, rewrite) in [(false, false), (true, false), (false, true), (true, true)] {
        let workspace = tempfile::tempdir().expect("workspace");
        let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
            SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3")).expect("store"),
        );
        let finished = Arc::new(Notify::new());
        let model = Arc::new(StreamingModel {
            calls: AtomicUsize::new(0),
            release: Arc::new(Notify::new()),
            terminal: Terminal::Repaired { streaming },
            tool_finished: Arc::clone(&finished),
            tool_started: Arc::new(Notify::new()),
            wait_for_tool_start: false,
        });
        let mut agent = create_agent(streaming_config(
            workspace.path(),
            Arc::clone(&checkpoints),
            "repaired-prefix",
            model,
            Arc::new(StreamingTool {
                finished: Arc::new(AtomicBool::new(false)),
                release: None,
                started: Arc::new(Notify::new()),
                tool_finished: finished,
            }),
            Arc::new(StreamingHooks { rewrite }),
        ))
        .await
        .expect("agent");
        agent
            .sender()
            .submit(user_op("repair limit"))
            .expect("submit");
        let events = collect_until_turn_end(&mut agent).await;
        assert!(events.iter().any(|event| matches!(event,
            EventMsg::ToolCallEnd(result) if &*result.call_id == "stream-call" && !result.is_error
        )));
        let saved = checkpoints
            .load(&"repaired-prefix".parse().expect("session ID"))
            .await
            .expect("load")
            .expect("saved");
        let raw = saved
            .context
            .iter()
            .find(|item| item["type"] == "function_call")
            .expect("raw call");
        let expected = if rewrite {
            serde_json::json!({"value": "rewritten", "limit": 42}).to_string()
        } else {
            streamed_arguments(Terminal::Repaired { streaming }).into()
        };
        assert_eq!(raw["arguments"], expected);
    }
}

#[tokio::test]
async fn streaming_tool_error_or_mismatch_does_not_retry_and_records_unknown_result() {
    for (session_id, terminal, prompt) in [
        (
            "streaming-error",
            Terminal::Error,
            "fail after streaming a tool",
        ),
        (
            "streaming-http-error",
            Terminal::HttpError,
            "fail after streaming a tool over HTTP",
        ),
        (
            "streaming-mismatch",
            Terminal::Changed,
            "change the streamed tool arguments",
        ),
    ] {
        let workspace = tempfile::tempdir().expect("workspace");
        let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
            SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
                .expect("checkpoint store"),
        );
        let tool_started = Arc::new(Notify::new());
        let tool_finished = Arc::new(Notify::new());
        let model = Arc::new(StreamingModel {
            calls: AtomicUsize::new(0),
            release: Arc::new(Notify::new()),
            terminal,
            tool_finished: Arc::clone(&tool_finished),
            tool_started: Arc::clone(&tool_started),
            wait_for_tool_start: true,
        });
        let mut agent = create_agent(streaming_config(
            workspace.path(),
            Arc::clone(&checkpoints) as Arc<dyn CheckpointStore>,
            session_id,
            Arc::<StreamingModel>::clone(&model),
            Arc::new(StreamingTool {
                finished: Arc::new(AtomicBool::new(false)),
                release: None,
                started: tool_started,
                tool_finished,
            }),
            Arc::new(StreamingHooks::default()),
        ))
        .await
        .expect("create agent");
        agent
            .sender()
            .submit(user_op(prompt))
            .expect("submit input");
        collect_until_turn_end(&mut agent).await;

        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        let saved = checkpoints
            .load(&session_id.parse().expect("session ID"))
            .await
            .expect("load checkpoint")
            .expect("saved checkpoint");
        assert_eq!(
            canonical_tool_items(&saved.context),
            [
                "function_call:stream-call".to_owned(),
                "function_call_output:stream-call".to_owned(),
            ]
        );
        let output = saved
            .context
            .iter()
            .find(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
            .and_then(|item| item.get("output"))
            .and_then(|output| output.get(0))
            .and_then(|part| part.get("text"))
            .and_then(Value::as_str)
            .expect("unknown tool output");
        assert!(output.contains("result unknown"));
    }
}

#[tokio::test]
async fn streaming_mixed_parallel_and_exclusive_tools_respect_barriers() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let barrier = Arc::new(BarrierState {
        a_started: Notify::new(),
        b_started: Notify::new(),
        c_started: Notify::new(),
        c_prepared: Notify::new(),
        release_a: Notify::new(),
        release_b: Notify::new(),
        release_stream: Notify::new(),
        order: Mutex::new(Vec::new()),
    });
    let model = Arc::new(BarrierStreamingModel {
        barrier: Arc::clone(&barrier),
        calls: AtomicUsize::new(0),
    });
    let tools = ["parallel_a", "exclusive_b", "parallel_c"]
        .into_iter()
        .map(|name| {
            Arc::new(BarrierStreamingTool {
                barrier: Arc::clone(&barrier),
                name,
                definition: Arc::new(ToolDefinition {
                    name: name.into(),
                    description: "a mixed barrier streaming test tool".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }),
            }) as Arc<dyn Tool>
        })
        .collect();
    let mut agent = create_agent(streaming_config_with_tools(
        workspace.path(),
        Arc::clone(&checkpoints) as Arc<dyn CheckpointStore>,
        "streaming-mixed-barriers",
        Arc::<BarrierStreamingModel>::clone(&model),
        tools,
        Arc::<BarrierState>::clone(&barrier),
    ))
    .await
    .expect("create agent");
    agent
        .sender()
        .submit(user_op("stream mixed barriers"))
        .expect("submit input");

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        drain_until_notified(&mut agent, &barrier.a_started),
    )
    .await
    .expect("parallel A started");
    assert_eq!(
        barrier.order.lock().expect("barrier order lock").as_slice(),
        ["parallel_a"]
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = agent.next_event().await.expect("agent event");
            if matches!(
                event.msg,
                EventMsg::AssistantContentDelta(delta) if delta.delta == "barrier-admitted"
            ) {
                break;
            }
        }
    })
    .await
    .expect("all streamed tools admitted");
    assert_eq!(
        barrier.order.lock().expect("barrier order lock").as_slice(),
        ["parallel_a"]
    );
    barrier.release_a.notify_one();

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        drain_until_notified(&mut agent, &barrier.b_started),
    )
    .await
    .expect("exclusive B started");
    assert_eq!(
        barrier.order.lock().expect("barrier order lock").as_slice(),
        ["parallel_a", "exclusive_b"]
    );
    barrier.release_b.notify_one();

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        drain_until_notified(&mut agent, &barrier.c_started),
    )
    .await
    .expect("parallel C started");
    assert_eq!(
        barrier.order.lock().expect("barrier order lock").as_slice(),
        ["parallel_a", "exclusive_b", "parallel_c"]
    );
    assert_eq!(barrier.order.lock().expect("barrier order lock").len(), 3);
    barrier.release_stream.notify_one();

    let _events = collect_until_turn_end(&mut agent).await;
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);

    let saved = checkpoints
        .load(&"streaming-mixed-barriers".parse().expect("session ID"))
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    assert_eq!(
        canonical_tool_items(&saved.context),
        [
            "function_call:barrier-a".to_owned(),
            "function_call:barrier-b".to_owned(),
            "function_call:barrier-c".to_owned(),
            "function_call_output:barrier-a".to_owned(),
            "function_call_output:barrier-b".to_owned(),
            "function_call_output:barrier-c".to_owned(),
        ]
    );
}

#[tokio::test]
async fn interrupt_drops_a_running_streaming_tool() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let tool_started = Arc::new(Notify::new());
    let tool_finished = Arc::new(Notify::new());
    let tool_done = Arc::new(AtomicBool::new(false));
    let model_release = Arc::new(Notify::new());
    let model = Arc::new(StreamingModel {
        calls: AtomicUsize::new(0),
        release: Arc::clone(&model_release),
        terminal: Terminal::Hold,
        tool_finished: Arc::new(Notify::new()),
        tool_started: Arc::new(Notify::new()),
        wait_for_tool_start: false,
    });
    let mut agent = create_agent(streaming_config(
        workspace.path(),
        Arc::clone(&checkpoints) as Arc<dyn CheckpointStore>,
        "streaming-interrupt",
        Arc::<StreamingModel>::clone(&model),
        Arc::new(StreamingTool {
            finished: Arc::clone(&tool_done),
            release: Some(Arc::clone(&tool_finished)),
            started: Arc::clone(&tool_started),
            tool_finished: Arc::new(Notify::new()),
        }),
        Arc::new(StreamingHooks::default()),
    ))
    .await
    .expect("create agent");
    agent
        .sender()
        .submit(user_op("interrupt the streaming tool"))
        .expect("submit input");
    let turn_id = loop {
        if let EventMsg::ModelStepStarted(step) = agent.next_event().await.expect("agent event").msg
        {
            break Arc::clone(&step.turn_id);
        }
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        drain_until_notified(&mut agent, &tool_started),
    )
    .await
    .expect("tool started");
    agent
        .sender()
        .submit(Op::Interrupt { turn_id })
        .expect("interrupt turn");
    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::TurnAborted(_)
    ) {}
    model_release.notify_waiters();
    tool_finished.notify_waiters();

    assert!(!tool_done.load(Ordering::SeqCst));
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    let saved = checkpoints
        .load(&"streaming-interrupt".parse().expect("session ID"))
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    assert_eq!(
        canonical_tool_items(&saved.context),
        [
            "function_call:stream-call".to_owned(),
            "function_call_output:stream-call".to_owned(),
        ]
    );
}
