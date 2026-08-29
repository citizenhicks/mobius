use super::*;

#[tokio::test]
async fn loop_executes_tool_and_returns_result_to_model() {
    let workspace = TempDir::new().expect("create workspace");
    std::fs::write(workspace.path().join("note.txt"), "hello").expect("write fixture");
    let model = Arc::new(ScriptedModel::new(vec![
        tool_response(
            "call-1",
            "read_file",
            serde_json::json!({"path": "note.txt"}),
        ),
        text_response("read hello"),
    ]));
    let mut agent = create_agent(test_config(
        workspace.path(),
        Arc::clone(&model),
        vec![Arc::new(Tools::coding())],
    ))
    .await
    .expect("create agent");

    agent
        .sender()
        .submit(user_message("read note.txt"))
        .expect("submit turn");

    assert_eq!(final_message(&mut agent).await, "read hello");
    assert!(
        model.requests.lock().expect("requests")[1]
            .input
            .iter()
            .any(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call_output")
                    && item.get("output").and_then(Value::as_str) == Some("hello")
            })
    );
}

#[tokio::test]
async fn middleware_prompt_is_composed_once_per_agent() {
    let workspace = TempDir::new().expect("create workspace");
    let model = Arc::new(ScriptedModel::new(vec![
        text_response("first"),
        text_response("second"),
    ]));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = create_agent(test_config(
        workspace.path(),
        Arc::clone(&model),
        vec![Arc::new(PromptExtension(Arc::clone(&calls)))],
    ))
    .await
    .expect("create agent");

    for message in ["one", "two"] {
        agent
            .sender()
            .submit(user_message(message))
            .expect("submit turn");
        final_message(&mut agent).await;
    }

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let platform = if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "an unsupported operating system"
    };
    let expected = format!(
        "**instructions**\n\ntest system prompt\n\n**sandbox**\n\nmöbius is running on {platform}.\n\n**prompt extension**\n\ncapability prompt"
    );
    assert!(
        model
            .requests
            .lock()
            .expect("requests")
            .iter()
            .all(|request| request.instructions == expected)
    );
}

#[tokio::test]
async fn live_messages_expose_their_durable_transcript_boundaries() {
    let workspace = TempDir::new().expect("create workspace");
    let model = Arc::new(ScriptedModel::new(vec![text_response("answer")]));
    let mut agent = create_agent(test_config(workspace.path(), model, Vec::new()))
        .await
        .expect("create agent");
    agent
        .sender()
        .submit(user_message("question"))
        .expect("submit turn");

    let mut targets = Vec::new();
    while let Some(event) = agent.next_event().await {
        match event.msg {
            EventMsg::Message(message) => targets.push(message.message_target),
            EventMsg::AssistantMessage(message) => targets.push(message.message_target),
            EventMsg::TurnComplete(_) => break,
            EventMsg::Error(error) => panic!("{}", error.message),
            _ => {}
        }
    }

    assert_eq!(
        targets,
        [
            Some(MessageTarget {
                checkpoint_sequence: 2,
                batch_item_count: 1,
            }),
            Some(MessageTarget {
                checkpoint_sequence: 4,
                batch_item_count: 1,
            }),
        ]
    );
}

#[tokio::test]
async fn approval_allows_an_explicitly_approved_write() {
    let workspace = TempDir::new().expect("create workspace");
    let output_path = workspace.path().join("result.txt");
    let model = Arc::new(ScriptedModel::new(vec![
        tool_response(
            "call-1",
            "write_file",
            serde_json::json!({"path": "result.txt", "content": "approved"}),
        ),
        text_response("done"),
    ]));
    let mut agent = create_agent(test_config(
        workspace.path(),
        model,
        vec![Arc::new(Tools::coding())],
    ))
    .await
    .expect("create agent");
    let sender = agent.sender();
    sender
        .submit(user_message("write the result"))
        .expect("submit turn");

    while let Some(event) = agent.next_event().await {
        match event.msg {
            EventMsg::ExecApprovalRequest(request) => {
                assert!(!output_path.exists());
                sender
                    .submit(Op::ExecApproval {
                        id: request.id,
                        decision: ReviewDecision::Approved,
                    })
                    .expect("approve write");
            }
            EventMsg::TurnComplete(_) => break,
            EventMsg::Error(error) => panic!("{}", error.message),
            _ => {}
        }
    }

    assert_eq!(
        std::fs::read_to_string(output_path).expect("read result"),
        "approved"
    );
}

#[tokio::test]
async fn approval_denial_prevents_command_execution() {
    let workspace = TempDir::new().expect("create workspace");
    let output_path = workspace.path().join("denied.txt");
    let model = Arc::new(ScriptedModel::new(vec![
        tool_response(
            "call-1",
            "bash",
            serde_json::json!({"command": "printf unsafe > denied.txt"}),
        ),
        text_response("denied"),
    ]));
    let mut agent = create_agent(test_config(
        workspace.path(),
        model,
        vec![Arc::new(Tools::coding())],
    ))
    .await
    .expect("create agent");
    let sender = agent.sender();
    sender
        .submit(user_message("run a command"))
        .expect("submit turn");

    while let Some(event) = agent.next_event().await {
        match event.msg {
            EventMsg::ExecApprovalRequest(request) => {
                sender
                    .submit(Op::ExecApproval {
                        id: request.id,
                        decision: ReviewDecision::Denied {
                            rejection: "test denial".into(),
                        },
                    })
                    .expect("deny command");
            }
            EventMsg::TurnComplete(_) => break,
            EventMsg::Error(error) => panic!("{}", error.message),
            _ => {}
        }
    }

    assert!(!output_path.exists());
}

#[tokio::test]
async fn interrupt_only_aborts_its_target_turn() {
    let workspace = TempDir::new().expect("create workspace");
    let scripted = Arc::new(ScriptedModel::new(vec![text_response("unused")]));
    let model = Arc::new(GatedModel {
        inner: scripted,
        first: AtomicBool::new(true),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let mut agent = create_agent(test_config(
        workspace.path(),
        Arc::clone(&model),
        Vec::new(),
    ))
    .await
    .expect("create agent");
    let sender = agent.sender();
    sender.submit(user_message("start")).expect("submit turn");

    let configured = agent.next_event().await.expect("session event");
    assert!(configured.submission_id.is_none());
    let turn_id = loop {
        let event = agent.next_event().await.expect("turn event");
        if let EventMsg::TurnStarted(turn) = event.msg {
            break turn.turn_id;
        }
    };
    model.entered.notified().await;

    let stale_submission = sender
        .submit(Op::Interrupt {
            turn_id: "stale-turn".into(),
        })
        .expect("submit stale interrupt");
    loop {
        let event = agent.next_event().await.expect("stale interrupt event");
        if let EventMsg::Warning(warning) = event.msg {
            assert_eq!(
                (event.submission_id, warning.message),
                (
                    Some(stale_submission),
                    "interrupt targeted a stale turn".to_string()
                )
            );
            break;
        }
    }

    let interrupt_submission = sender
        .submit(Op::Interrupt {
            turn_id: turn_id.clone(),
        })
        .expect("submit targeted interrupt");
    loop {
        let event = agent.next_event().await.expect("interrupt event");
        if let EventMsg::TurnAborted(turn) = event.msg {
            assert_eq!(
                (event.submission_id, turn.turn_id),
                (Some(interrupt_submission), turn_id)
            );
            break;
        }
    }
}
