//! Deferred-tool agent-loop tests.

use super::*;

type DiscoveryRequest = (Vec<String>, Vec<String>, Vec<Value>);

struct DiscoveryModel {
    mode: crate::protocol::ToolDiscoveryMode,
    outputs: Mutex<VecDeque<ModelOutput>>,
    compact_outputs: Mutex<VecDeque<CompactOutput>>,
    requests: Mutex<Vec<DiscoveryRequest>>,
}

impl Model for DiscoveryModel {
    fn tool_discovery(&self) -> crate::protocol::ToolDiscoveryMode {
        self.mode
    }

    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        _events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        self.requests.lock().expect("requests").push((
            request.tools.iter().map(|tool| tool.name.clone()).collect(),
            request
                .deferred_tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect(),
            request.input.to_vec(),
        ));
        let output = self
            .outputs
            .lock()
            .expect("outputs")
            .pop_front()
            .ok_or_else(|| Error::Provider("discovery script exhausted".into()));
        Box::pin(async move { output })
    }

    fn compaction_endpoint(&self) -> bool {
        !self
            .compact_outputs
            .lock()
            .expect("compact outputs")
            .is_empty()
    }

    fn compact<'a>(&'a self, _request: CompactRequest<'a>) -> BoxFuture<'a, Result<CompactOutput>> {
        let output = self
            .compact_outputs
            .lock()
            .expect("compact outputs")
            .pop_front()
            .ok_or_else(|| Error::Provider("compaction script exhausted".into()));
        Box::pin(async move { output })
    }
}

struct OptionalTool(Arc<AtomicUsize>);

impl Tool for OptionalTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "optional_work".into(),
            description: "Perform optional work after discovery".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        _arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok("optional work complete".into()) })
    }
}

fn tool_call(call_id: &str, name: &str, arguments: Value) -> ModelOutput {
    ModelOutput::from_output(
        vec![serde_json::json!({
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": serde_json::to_string(&arguments).expect("arguments")
        })],
        false,
        scripted_usage(),
    )
    .expect("tool call")
}

fn discovery_config(
    workspace: &Path,
    checkpoints: Arc<dyn CheckpointStore>,
    model: Arc<DiscoveryModel>,
    executions: Arc<AtomicUsize>,
    session_id: &str,
) -> AgentConfig {
    AgentConfig::new(
        Arc::new(ModelRouter::new("test", model)),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace).expect("sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoints,
        MiddlewareStack::new(vec![Arc::new(Tools::new(vec![Arc::new(OptionalTool(
            executions,
        ))]))])
        .expect("middleware"),
        "test prompt",
    )
    .session_id(session_id)
}

#[tokio::test]
async fn rebuild_discovers_then_exposes_an_optional_tool() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(DiscoveryModel {
        mode: crate::protocol::ToolDiscoveryMode::Rebuild,
        outputs: Mutex::new(VecDeque::from([
            tool_call(
                "search",
                crate::backend::model::TOOLS_SEARCH_NAME,
                serde_json::json!({"query": "optional"}),
            ),
            tool_call("work", "optional_work", serde_json::json!({})),
            scripted_message("done"),
        ])),
        compact_outputs: Mutex::new(VecDeque::new()),
        requests: Mutex::new(Vec::new()),
    });
    let executions = Arc::new(AtomicUsize::new(0));
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let config = discovery_config(
        workspace.path(),
        checkpoint_store,
        Arc::clone(&model),
        Arc::clone(&executions),
        "tool-discovery",
    );
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(Op::UserInput {
            text: "do optional work".into(),
            attachments: Vec::new(),
        })
        .expect("submit input");
    let mut load_event = None;
    loop {
        match agent.next_event().await.expect("agent event").msg {
            EventMsg::ToolLoad(load) => load_event = Some(load),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(
        load_event.expect("tool load event").tools,
        ["optional_work"]
    );
    let requests = model.requests.lock().expect("requests");
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].0, [crate::backend::model::TOOLS_SEARCH_NAME]);
    assert!(requests[0].1.is_empty());
    assert_eq!(
        requests[1].0,
        [crate::backend::model::TOOLS_SEARCH_NAME, "optional_work"]
    );
    assert!(requests[1].1.is_empty());
    let load = requests[1]
        .2
        .iter()
        .find_map(|item| crate::backend::model::ToolLoad::from_input(item).expect("valid load"))
        .expect("tool load");
    assert_eq!(load.tools, ["optional_work"]);
}

#[tokio::test]
async fn native_materialization_can_call_a_deferred_tool_in_the_same_step() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let mut call = tool_call("work", "optional_work", serde_json::json!({}));
    call.materialized_tools.insert("optional_work".into());
    let model = Arc::new(DiscoveryModel {
        mode: crate::protocol::ToolDiscoveryMode::Native,
        outputs: Mutex::new(VecDeque::from([call, scripted_message("done")])),
        compact_outputs: Mutex::new(VecDeque::new()),
        requests: Mutex::new(Vec::new()),
    });
    let executions = Arc::new(AtomicUsize::new(0));
    let config = discovery_config(
        workspace.path(),
        checkpoints,
        Arc::clone(&model),
        Arc::clone(&executions),
        "native-tool-discovery",
    );
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(Op::UserInput {
            text: "do optional work".into(),
            attachments: Vec::new(),
        })
        .expect("submit input");
    let mut load_event = None;
    loop {
        match agent.next_event().await.expect("agent event").msg {
            EventMsg::ToolLoad(load) => load_event = Some(load),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(
        load_event.expect("tool load event").tools,
        ["optional_work"]
    );
    let requests = model.requests.lock().expect("requests");
    assert_eq!(requests[0].0, [crate::backend::model::TOOLS_SEARCH_NAME]);
    assert_eq!(requests[0].1, ["optional_work"]);
    assert!(requests[1].2.iter().any(|item| {
        crate::backend::model::ToolLoad::from_input(item)
            .expect("valid load")
            .is_some()
    }));
}

#[tokio::test]
async fn compaction_preserves_loaded_deferred_tools() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let loaded = ModelOutput::from_output(
        vec![serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "loaded"}]
        })],
        true,
        TokenUsage {
            input_tokens: 2_000,
            total_tokens: 2_000,
            ..TokenUsage::default()
        },
    )
    .expect("loaded response");
    let model = Arc::new(DiscoveryModel {
        mode: crate::protocol::ToolDiscoveryMode::Rebuild,
        outputs: Mutex::new(VecDeque::from([
            tool_call(
                "search",
                crate::backend::model::TOOLS_SEARCH_NAME,
                serde_json::json!({"query": "optional"}),
            ),
            loaded,
            tool_call("work", "optional_work", serde_json::json!({})),
            scripted_message("done"),
        ])),
        compact_outputs: Mutex::new(VecDeque::from([CompactOutput::from_output(
            vec![serde_json::json!({
                "type": "compaction",
                "encrypted_content": "opaque"
            })],
            scripted_usage(),
        )
        .expect("compaction output")])),
        requests: Mutex::new(Vec::new()),
    });
    let executions = Arc::new(AtomicUsize::new(0));
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("test", model.clone())),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoints,
        MiddlewareStack::new(vec![
            Arc::new(Tools::new(vec![Arc::new(OptionalTool(Arc::clone(
                &executions,
            )))])),
            Arc::new(Compaction::new(1_000).expect("compaction")),
        ])
        .expect("middleware"),
        "test prompt",
    )
    .session_id("compacted-tool-discovery");
    let mut agent = create_agent(config).await.expect("create agent");

    for text in ["load optional work", "use optional work"] {
        agent
            .sender()
            .submit(Op::UserInput {
                text: text.into(),
                attachments: Vec::new(),
            })
            .expect("submit input");
        while !matches!(
            agent.next_event().await.expect("agent event").msg,
            EventMsg::TurnComplete(_)
        ) {}
    }

    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let requests = model.requests.lock().expect("requests");
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests[2].0,
        [crate::backend::model::TOOLS_SEARCH_NAME, "optional_work"]
    );
    let loads = requests[2]
        .2
        .iter()
        .filter_map(|item| crate::backend::model::ToolLoad::from_input(item).expect("valid load"))
        .collect::<Vec<_>>();
    assert_eq!(loads.len(), 1);
    assert_eq!(loads[0].tools, ["optional_work"]);
}
