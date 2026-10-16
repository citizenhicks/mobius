//! Deferred-tool agent-loop tests.

use super::*;

type DiscoveryRequest = (Vec<String>, Vec<String>, Vec<Value>);

struct DiscoveryModel {
    mode: crate::protocol::ToolDiscoveryMode,
    outputs: Mutex<VecDeque<ModelOutput>>,
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
            request.input.iter().cloned().collect(),
        ));
        if request.tools.len() == 1 && request.tools[0].name == "write_handoff" {
            return Box::pin(async { Ok(scripted_handoff("discovery-checkpoint")) });
        }
        let output = self
            .outputs
            .lock()
            .expect("outputs")
            .pop_front()
            .ok_or_else(|| Error::Provider("discovery script exhausted".into()));
        Box::pin(async move { output })
    }
}

struct ApprovalTool(Arc<AtomicUsize>);

struct HideAfterModel(AtomicUsize);

impl Tool for ApprovalTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "approval_work".into(),
            description: "Perform approval work".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        _arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok("approval work complete".into()) })
    }
}

impl Middleware for HideAfterModel {
    fn name(&self) -> &'static str {
        "hide_after_model"
    }

    fn tool_exposure<'a>(
        &'a self,
        context: &'a mut crate::middleware::ToolExposureContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.0.fetch_add(1, Ordering::SeqCst) > 0 {
                context.hide(&["approval_work"]);
            }
            Ok(())
        })
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

#[tokio::test]
async fn tool_hidden_after_inference_never_reaches_approval() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(DiscoveryModel {
        mode: crate::protocol::ToolDiscoveryMode::Rebuild,
        outputs: Mutex::new(VecDeque::from([
            tool_call("approval", "approval_work", serde_json::json!({})),
            scripted_message("done"),
        ])),
        requests: Mutex::new(Vec::new()),
    });
    let executions = Arc::new(AtomicUsize::new(0));
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("test", model)),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoints,
        test_middleware(vec![
            Arc::new(Tools::new(vec![Arc::new(ApprovalTool(Arc::clone(
                &executions,
            )))])),
            Arc::new(HideAfterModel(AtomicUsize::new(0))),
        ]),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id("live-tool-exposure");
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(user_op("do approval work"))
        .expect("submit input");
    let mut approval_requested = false;
    let mut unavailable = false;
    loop {
        match agent.next_event().await.expect("agent event").msg {
            EventMsg::ExecApprovalRequest(_) => approval_requested = true,
            EventMsg::ToolCallEnd(result) if result.call_id == "approval" => {
                unavailable = result.is_error && result.output.text().contains("unavailable");
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert!(!approval_requested);
    assert!(unavailable);
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}
