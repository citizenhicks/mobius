use super::*;

#[tokio::test]
async fn native_compaction_survives_recreation_with_current_prompt_and_tools() {
    let workspace = TempDir::new().expect("create workspace");
    let model = Arc::new(ScriptedModel::with_compaction(
        vec![
            text_response_with_usage("first done", usage(2_000)),
            text_response("compacted done"),
            text_response("second done"),
        ],
        vec![
            CompactOutput::from_output(
                vec![serde_json::json!({
                    "type": "compaction",
                    "encrypted_content": "opaque"
                })],
                usage(10),
            )
            .expect("compaction output"),
        ],
    ));
    let route: Arc<dyn Model> = model.clone();
    let router = Arc::new(ModelRouter::new("test", route));
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
        ApprovalPolicy::Ask,
    ));
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(MemoryCheckpoints::default());
    let config = |base: &str, section: &'static str, coding_tools: bool| {
        let mut middleware: Vec<Arc<dyn Middleware>> = vec![
            Arc::new(Messages::default()),
            Arc::new(StaticPrompt(section)),
        ];
        if coding_tools {
            middleware.push(Arc::new(Tools::coding()));
        }
        middleware.push(Arc::new(
            Compaction::new(1_000).expect("compaction middleware"),
        ));
        AgentConfig::new(
            Arc::clone(&router),
            Arc::clone(&sandbox),
            Arc::clone(&checkpoints),
            MiddlewareStack::new(middleware).expect("middleware"),
            base,
        )
        .session_id("prompt-refresh")
    };

    let mut first = create_agent(config("old base marker", "old section marker", true))
        .await
        .expect("first agent");
    first
        .sender()
        .submit(user_message("first turn"))
        .expect("first turn");
    assert_eq!(final_message(&mut first).await, "first done");
    first
        .sender()
        .submit(user_message("compact turn"))
        .expect("compaction turn");
    assert_eq!(final_message(&mut first).await, "compacted done");
    let (sender, mut events) = first.into_parts();
    drop(sender);
    while events.recv().await.is_some() {}

    let mut second = create_agent(config("new base marker", "new section marker", false))
        .await
        .expect("replacement agent");
    second
        .sender()
        .submit(user_message("second turn"))
        .expect("second turn");
    assert_eq!(final_message(&mut second).await, "second done");

    let requests = model.requests.lock().expect("requests");
    assert_eq!(requests.len(), 3);
    assert!(!requests[0].tools.is_empty());
    assert!(requests[0].instructions.contains("**tools**"));
    assert_eq!(requests[1].tools, requests[0].tools);
    let replacement = &requests[2];
    assert!(replacement.tools.is_empty());
    assert!(!replacement.instructions.contains("**tools**"));
    assert_eq!(
        replacement.instructions.matches("new base marker").count(),
        1
    );
    assert_eq!(
        replacement
            .instructions
            .matches("new section marker")
            .count(),
        1
    );
    assert!(!replacement.instructions.contains("old base marker"));
    assert!(!replacement.instructions.contains("old section marker"));
    let history = serde_json::to_string(&replacement.input).expect("serialize history");
    assert!(history.contains("opaque"));
    assert!(history.contains("second turn"));
    assert!(!history.contains("base marker"));
    assert!(!history.contains("section marker"));
    let first_tools = requests[0].tools.clone();
    drop(requests);

    let compact_requests = model.compact_requests.lock().expect("compact requests");
    assert_eq!(compact_requests.len(), 1);
    let compact_request = &compact_requests[0];
    assert_eq!(compact_request.session_id, "prompt-refresh");
    assert!(compact_request.instructions.contains("old base marker"));
    assert!(compact_request.instructions.contains("old section marker"));
    assert!(compact_request.instructions.contains("**tools**"));
    assert_eq!(compact_request.tools, first_tools);
    let compact_input = serde_json::to_string(&compact_request.input).expect("compact input");
    assert!(compact_input.contains("first turn"));
    assert!(compact_input.contains("compact turn"));
    assert!(!compact_input.contains("old base marker"));
    assert!(!compact_input.contains("old section marker"));
    assert!(!compact_input.contains("read_file"));
}

#[tokio::test]
async fn steering_is_injected_before_native_compaction() {
    let workspace = TempDir::new().expect("create workspace");
    let first = text_response_with_usage("draft", usage(1_000));
    let scripted = Arc::new(ScriptedModel::with_compaction(
        vec![first, text_response("done")],
        vec![
            CompactOutput::from_output(
                vec![serde_json::json!({
                    "type": "compaction",
                    "encrypted_content": "opaque"
                })],
                usage(100),
            )
            .expect("compaction output"),
        ],
    ));
    let model = Arc::new(GatedModel {
        inner: Arc::clone(&scripted),
        first: AtomicBool::new(true),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let mut agent = create_agent(test_config(
        workspace.path(),
        Arc::clone(&model),
        vec![Arc::new(Compaction::new(500).expect("compaction"))],
    ))
    .await
    .expect("create agent");
    let sender = agent.sender();
    sender.submit(user_message("start")).expect("submit turn");

    let turn_id = loop {
        match agent.next_event().await.expect("turn event").msg {
            EventMsg::TurnStarted(turn) => break turn.turn_id,
            EventMsg::Error(error) => panic!("{}", error.message),
            _ => {}
        }
    };
    model.entered.notified().await;
    sender
        .submit(steer_message(turn_id, "steered"))
        .expect("steer active turn");
    model.release.notify_one();

    let mut message = String::new();
    let mut steered_target = None;
    while let Some(event) = agent.next_event().await {
        match event.msg {
            EventMsg::Message(event)
                if event.text == "steered" && event.delivery == MessageDelivery::Steer =>
            {
                steered_target = event.message_target;
            }
            EventMsg::AssistantMessage(event) => message = assistant_final_text(event),
            EventMsg::TurnComplete(_) => break,
            EventMsg::Error(error) => panic!("{}", error.message),
            _ => {}
        }
    }
    assert_eq!(message, "done");
    assert_eq!(
        steered_target,
        Some(MessageTarget {
            checkpoint_sequence: 6,
            batch_item_count: 1,
        })
    );
    let requests = scripted.compact_requests.lock().expect("compact requests");
    assert_eq!(requests.len(), 1);
    assert!(
        serde_json::to_string(&requests[0].input)
            .expect("serialize compact input")
            .contains("steered")
    );
}

#[tokio::test]
async fn compaction_uses_the_context_window_of_a_new_model_route() {
    let workspace = TempDir::new().expect("create workspace");
    let large = Arc::new(ScriptedModel::new(vec![text_response("draft")]));
    let small = Arc::new(ScriptedModel::with_compaction(
        vec![text_response("done")],
        vec![
            CompactOutput::from_output(
                vec![serde_json::json!({
                    "type": "compaction",
                    "encrypted_content": "opaque"
                })],
                usage(10),
            )
            .expect("compaction output"),
        ],
    ));
    let large_model: Arc<dyn Model> = large.clone();
    let small_model: Arc<dyn Model> = small.clone();
    let mut router = ModelRouter::new("large", large_model);
    router.register("small", small_model).expect("small route");
    for (route, context_window) in [("large", 300_000), ("small", 8_000)] {
        router
            .configure_choice(ModelChoice {
                route: route.into(),
                group: route.into(),
                model: route.into(),
                reasoning_effort: None,
                context_window: Some(context_window),
                supports_image_input: true,
                tool_discovery: ToolDiscoveryMode::Rebuild,
            })
            .expect("route metadata");
    }
    let mut agent = create_agent(test_config_with_router(
        workspace.path(),
        router,
        vec![Arc::new(Compaction::default())],
    ))
    .await
    .expect("create agent");

    agent
        .sender()
        .submit(user_message("first"))
        .expect("submit first turn");
    assert_eq!(final_message(&mut agent).await, "draft");
    agent
        .sender()
        .submit(Op::SetModel {
            route: "small".into(),
        })
        .expect("select small route");
    agent
        .sender()
        .submit(user_message("second"))
        .expect("submit second turn");

    assert_eq!(final_message(&mut agent).await, "done");
    assert!(
        large
            .compact_requests
            .lock()
            .expect("large compact")
            .is_empty()
    );
    assert_eq!(
        small.compact_requests.lock().expect("small compact").len(),
        1
    );
}

#[tokio::test]
async fn native_compaction_ignores_stale_usage_after_a_retained_user() {
    let workspace = TempDir::new().expect("create workspace");
    let model = Arc::new(ScriptedModel::with_compaction(
        vec![
            text_response_with_usage("first done", usage(2_000)),
            text_response_with_usage("second done", usage(2_000)),
            text_response("third done"),
        ],
        vec![
            CompactOutput::from_output(
                vec![
                    serde_json::json!({
                        "type": "message",
                        "id": "message-2",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "second"}]
                    }),
                    serde_json::json!({
                        "type": "compaction",
                        "encrypted_content": "opaque"
                    }),
                ],
                usage(10),
            )
            .expect("compaction output"),
        ],
    ));
    let mut agent = create_agent(test_config(
        workspace.path(),
        Arc::clone(&model),
        vec![Arc::new(Compaction::new(1_000).expect("compaction"))],
    ))
    .await
    .expect("create agent");

    for (prompt, expected) in [
        ("first", "first done"),
        ("second", "second done"),
        ("third", "third done"),
    ] {
        agent
            .sender()
            .submit(user_message(prompt))
            .expect("submit turn");
        assert_eq!(final_message(&mut agent).await, expected);
    }

    assert_eq!(
        model
            .compact_requests
            .lock()
            .expect("compact requests")
            .len(),
        1
    );
    let requests = model.requests.lock().expect("requests");
    let second_users = requests[2]
        .input
        .iter()
        .filter(|item| {
            item.get("role").and_then(Value::as_str) == Some("user")
                && item.pointer("/content/0/text").and_then(Value::as_str) == Some("second")
        })
        .count();
    let rebuilt = serde_json::to_string(&requests[2].input).expect("serialize rebuilt context");
    assert_eq!(second_users, 1);
    assert!(rebuilt.contains("opaque"));
}

#[tokio::test]
async fn compaction_falls_back_to_a_model_summary_and_keeps_recent_context() {
    let workspace = TempDir::new().expect("create workspace");
    let first = text_response_with_usage("draft", usage(40_000));
    let model = Arc::new(ScriptedModel::new(vec![
        first,
        text_response("## Goal\nContinue the task."),
        text_response("done"),
    ]));
    let mut agent = create_agent(test_config(
        workspace.path(),
        Arc::clone(&model),
        vec![Arc::new(
            Compaction::new(30_000).expect("compaction middleware"),
        )],
    ))
    .await
    .expect("create agent");

    agent
        .sender()
        .submit(user_message("x".repeat(40_000)))
        .expect("submit first turn");
    assert_eq!(final_message(&mut agent).await, "draft");
    agent
        .sender()
        .submit(user_message("continue"))
        .expect("submit second turn");
    assert_eq!(final_message(&mut agent).await, "done");

    let requests = model.requests.lock().expect("requests");
    assert_eq!(requests.len(), 3);
    assert!(requests[1].instructions.contains("Summarize coding-agent"));
    assert_eq!(requests[2].instructions, requests[0].instructions);
    assert_eq!(
        requests[2].instructions.matches("**instructions**").count(),
        1
    );
    let rebuilt = serde_json::to_string(&requests[2].input).expect("serialize rebuilt context");
    assert!(rebuilt.contains("<compacted_context>"));
    assert!(rebuilt.contains("continue"));
    assert_eq!(
        requests[2]
            .input
            .iter()
            .filter(|item| {
                item.get("role").and_then(Value::as_str) == Some("user")
                    && item.pointer("/content/0/text").and_then(Value::as_str) == Some("continue")
            })
            .count(),
        1
    );
    assert!(
        model
            .compact_requests
            .lock()
            .expect("compact requests")
            .is_empty()
    );
}
