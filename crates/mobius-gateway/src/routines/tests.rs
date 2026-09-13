use super::*;

use mobius::agent::{AgentConfig, create_agent};
use mobius::backend::checkpoint::sqlite::SqliteCheckpoint;
use mobius::backend::model::{Model, ModelEventSink, ModelOutput, ModelRequest, ModelRouter};
use mobius::backend::sandbox::{ApprovalPolicy, Sandbox, local::LocalSandbox};
use mobius::middleware::{MiddlewareStack, messages::Messages, tools::Tools};

struct NoModel;

impl Model for NoModel {
    fn tool_discovery(&self) -> mobius::protocol::ToolDiscoveryMode {
        mobius::protocol::ToolDiscoveryMode::Native
    }
    fn respond<'a>(
        &'a self,
        _request: ModelRequest<'a>,
        _events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(async { unreachable!("routine registration does not call the model") })
    }
}

#[tokio::test]
async fn routines_are_main_only_approved_and_bound_to_the_current_bot() {
    let root = tempfile::tempdir().expect("temporary directory");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let bots = Arc::new(BotStore::open(root.path()).expect("Bot store"));
    let bot = bots
        .seed_default(&crate::wire::VersionedAgentConfig {
            revision: 1,
            config: crate::wire::AgentComposition::default(),
        })
        .expect("seed Bot")
        .expect("new Bot");
    let routines = Routines::new(Arc::clone(&bots), bot.id.clone(), workspace.clone());
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(LocalSandbox::new(&workspace).expect("sandbox")),
        ApprovalPolicy::Ask,
    ));
    for (role, expected_tools) in [
        (AgentRole::Main, 2),
        (
            AgentRole::Subagent {
                parent_session_id: "parent".into(),
                parent_turn_id: "turn".into(),
            },
            0,
        ),
    ] {
        let agent = create_agent(
            AgentConfig::new(
                Arc::new(ModelRouter::new("model", Arc::new(NoModel))),
                Arc::clone(&sandbox),
                Arc::new(
                    SqliteCheckpoint::new(root.path().join("checkpoints.sqlite3"))
                        .expect("checkpoints"),
                ),
                MiddlewareStack::new(vec![
                    Arc::new(Messages::default()),
                    Arc::new(Tools::new(Vec::new())),
                    Arc::new(routines.clone()),
                ])
                .expect("middleware"),
                "Test routine ownership.",
            )
            .role(role),
        )
        .await
        .expect("agent without Bot session metadata");
        assert_eq!(agent.tool_count(), expected_tools);
        let (sender, mut events) = agent.into_parts();
        drop(sender);
        while events.recv().await.is_some() {}
    }
    let tool = CreateRoutine(routines);
    assert_eq!(tool.approval(), ApprovalRequirement::Always);
    let arguments = serde_json::json!({
        "instructions": "Review the build",
        "schedule": {"kind": "interval", "every_seconds": 60},
    });
    let mut targeted = arguments.clone();
    targeted["bot_id"] = "another-bot".into();
    let mut invalid = arguments.clone();
    invalid["schedule"]["every_seconds"] = 0.into();
    let model = RoutineModel(std::sync::Mutex::new(Some(vec![
        targeted, invalid, arguments,
    ])));
    let mut agent = create_agent(AgentConfig::new(
        Arc::new(ModelRouter::new("model", Arc::new(model))),
        sandbox,
        Arc::new(SqliteCheckpoint::new(root.path().join("checkpoints.sqlite3")).unwrap()),
        MiddlewareStack::new(vec![
            Arc::new(Messages::default()),
            Arc::new(Tools::new(Vec::new())),
            Arc::new(tool.0),
        ])
        .unwrap(),
        "Test routine creation.",
    ))
    .await
    .unwrap();
    agent
        .sender()
        .submit(mobius::protocol::Op::Message {
            message: mobius::protocol::MessageSubmission {
                author: mobius::protocol::MessageAuthor::User,
                text: "Create a routine".into(),
                attachments: Vec::new(),
                reply: None,
                requested_delivery: None,
                target_turn_id: None,
            },
        })
        .unwrap();
    let mut rejected_calls = 0;
    let mut approved = false;
    while let Some(event) = agent.next_event().await {
        match event.msg {
            EventMsg::ExecApprovalRequest(request) => {
                assert!(
                    bots.routine_records(None, chrono::Utc::now().timestamp())
                        .unwrap()
                        .is_empty()
                );
                agent
                    .sender()
                    .submit(mobius::protocol::Op::ExecApproval {
                        id: request.id,
                        decision: mobius::protocol::ReviewDecision::Approved,
                    })
                    .unwrap();
                approved = true;
            }
            EventMsg::ToolCallEnd(call) => rejected_calls += usize::from(call.is_error),
            EventMsg::TurnComplete(_) => break,
            EventMsg::Error(error) => panic!("{error:?}"),
            _ => {}
        }
    }
    assert!(approved);
    assert_eq!(rejected_calls, 2);
    let records = bots
        .routine_records(None, chrono::Utc::now().timestamp())
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].bot_id, bot.id);
    let (sender, mut events) = agent.into_parts();
    drop(sender);
    while events.recv().await.is_some() {}
}

struct RoutineModel(std::sync::Mutex<Option<Vec<Value>>>);

impl Model for RoutineModel {
    fn tool_discovery(&self) -> mobius::protocol::ToolDiscoveryMode {
        mobius::protocol::ToolDiscoveryMode::Rebuild
    }

    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        _events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        if !request
            .tools
            .iter()
            .any(|tool| tool.name == "create_routine")
        {
            return Box::pin(async {
                ModelOutput::from_output(
                    vec![serde_json::json!({
                        "type": "function_call", "call_id": "discover-routines",
                        "name": "tools_search", "arguments": "{\"query\":\"create_routine\"}"
                    })],
                    false,
                    Default::default(),
                )
            });
        }
        let calls = self.0.lock().unwrap().take();
        let end_turn = calls.is_none();
        Box::pin(async move {
            let output = calls.map_or_else(
                || {
                    vec![serde_json::json!({
                        "type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "Done"}]
                    })]
                },
                |calls| {
                    calls
                        .into_iter()
                        .enumerate()
                        .map(|(index, arguments)| {
                            serde_json::json!({
                                "type": "function_call", "call_id": format!("call-{index}"),
                                "name": "create_routine", "arguments": arguments.to_string()
                            })
                        })
                        .collect()
                },
            );
            ModelOutput::from_output(output, end_turn, Default::default())
        })
    }
}
