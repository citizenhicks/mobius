use super::*;

#[tokio::test]
async fn external_skill_resources_use_the_generic_read_tool() {
    let workspace = TempDir::new().expect("create workspace");
    let skill_root = TempDir::new().expect("create skill root");
    let skill_dir = skill_root.path().join("review");
    std::fs::create_dir_all(skill_dir.join("references")).expect("create skill");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: review\ndescription: Review code.\n---\nRead references/details.md.",
    )
    .expect("write skill");
    std::fs::write(
        skill_dir.join("references/details.md"),
        "Always inspect every caller.",
    )
    .expect("write skill reference");
    let skill_file = std::fs::canonicalize(skill_dir.join("SKILL.md")).expect("canonical skill");
    let details = std::fs::canonicalize(skill_dir.join("references/details.md"))
        .expect("canonical skill reference");
    let model = Arc::new(ScriptedModel::new(vec![
        tool_response(
            "call-1",
            "read_file",
            serde_json::json!({"path": skill_file}),
        ),
        tool_response("call-2", "read_file", serde_json::json!({"path": details})),
        text_response("review loaded"),
    ]));
    let extensions = Extensions::discover([skill_root.path().to_path_buf()])
        .expect("discover skill")
        .prompt("Load the relevant skill before following its instructions.")
        .expect("custom skill prompt");
    let route: Arc<dyn Model> = model.clone();
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(
            LocalSandbox::new(workspace.path())
                .expect("local sandbox")
                .allow_read_root(&skill_dir)
                .expect("skill read root"),
        ),
        ApprovalPolicy::Ask,
    ));
    let mut agent = create_agent(AgentConfig::new(
        Arc::new(ModelRouter::new("test", route)),
        sandbox,
        Arc::new(MemoryCheckpoints::default()),
        MiddlewareStack::new(vec![
            Arc::new(Messages::default()),
            Arc::new(Tools::coding()),
            Arc::new(extensions),
        ])
        .expect("middleware"),
        "test system prompt",
    ))
    .await
    .expect("create agent");

    agent
        .sender()
        .submit(user_message("review this"))
        .expect("submit turn");
    final_message(&mut agent).await;

    let requests = model.requests.lock().expect("requests");
    assert!(
        requests[0]
            .instructions
            .contains("Load the relevant skill before following its instructions.")
    );
    assert!(
        requests[0]
            .instructions
            .contains(skill_file.to_str().expect("skill path is valid UTF-8"))
    );
    assert!(
        requests[0]
            .tools
            .iter()
            .all(|definition| definition.name != "load_skill")
    );
    assert!(
        requests[1]
            .input
            .iter()
            .filter_map(|item| item.get("output").and_then(Value::as_str))
            .any(|output| output.contains("Read references/details.md."))
    );
    assert!(
        requests[2]
            .input
            .iter()
            .filter_map(|item| item.get("output").and_then(Value::as_str))
            .any(|output| output.contains("Always inspect every caller."))
    );
}

#[tokio::test]
async fn async_subagent_uses_configured_model_reasoning_and_durable_fork() {
    let workspace = TempDir::new().expect("create workspace");
    let root_model = Arc::new(ScriptedModel::new(vec![
        tool_response(
            "call-search",
            mobius::backend::model::TOOLS_SEARCH_NAME,
            serde_json::json!({"query": "agent"}),
        ),
        tool_response(
            "call-spawn",
            "spawn_agent",
            serde_json::json!({
                "task_name": "cheap",
                "text": "solve child task",
                "fork_turns": "none"
            }),
        ),
        tool_response(
            "call-wait",
            "wait_agent",
            serde_json::json!({"timeout_ms": 10_000}),
        ),
        text_response("root complete"),
    ]));
    let unused_child_model = Arc::new(ScriptedModel::new(Vec::new()));
    let child_model = Arc::new(ScriptedModel::new(vec![text_response("child complete")]));
    let root_route: Arc<dyn Model> = root_model.clone();
    let child_route: Arc<dyn Model> = unused_child_model;
    let child_high_route: Arc<dyn Model> = child_model.clone();
    let mut routes = ModelRouter::new("root", root_route);
    routes
        .register("child", child_route)
        .expect("register child route");
    routes
        .register("child-high", child_high_route)
        .expect("register child reasoning route");
    for choice in [
        ModelChoice {
            route: "root".into(),
            group: "root".into(),
            model: "root".into(),
            reasoning_effort: None,
            context_window: None,
            supports_image_input: true,
            tool_discovery: ToolDiscoveryMode::Rebuild,
        },
        ModelChoice {
            route: "child".into(),
            group: "child".into(),
            model: "child".into(),
            reasoning_effort: Some("low".into()),
            context_window: None,
            supports_image_input: true,
            tool_discovery: ToolDiscoveryMode::Rebuild,
        },
        ModelChoice {
            route: "child-high".into(),
            group: "child".into(),
            model: "child".into(),
            reasoning_effort: Some("high".into()),
            context_window: None,
            supports_image_input: true,
            tool_discovery: ToolDiscoveryMode::Rebuild,
        },
    ] {
        routes
            .configure_choice(choice)
            .expect("configure model choice");
    }
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
        ApprovalPolicy::Ask,
    ));
    let checkpoint_store = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("subagents.sqlite3"))
            .expect("open checkpoint database"),
    );
    let checkpoints: Arc<dyn CheckpointStore> = checkpoint_store.clone();
    let template = Arc::new(OnceLock::<AgentConfig>::new());
    let child_template = Arc::downgrade(&template);
    let launcher: SubagentLauncher = Arc::new(move |launch: SubagentLaunch| {
        let child_template = child_template.clone();
        Box::pin(async move {
            let config = child_template
                .upgrade()
                .expect("subagent template owner")
                .get()
                .expect("subagent template")
                .clone()
                .session_id(launch.session_id)
                .metadata(launch.metadata)
                .model_route(&launch.model, launch.reasoning_effort.as_deref())?;
            create_agent(config).await
        })
    });
    let config = AgentConfig::new(
        Arc::new(routes),
        sandbox,
        checkpoints,
        MiddlewareStack::new(vec![
            Arc::new(Messages::default()),
            Arc::new(Tools::new(Vec::new())),
            Arc::new(
                Subagents::new(1, 21, 64, launcher)
                    .expect("subagents")
                    .default_model("child")
                    .default_reasoning("high")
                    .expect("subagent reasoning"),
            ),
        ])
        .expect("middleware"),
        "test system prompt",
    )
    .session_id("root");
    assert!(template.set(config.clone()).is_ok());
    let mut agent = create_agent(config).await.expect("create agent");

    agent
        .sender()
        .submit(user_message("delegate cheaply"))
        .expect("submit turn");

    let message = final_message(&mut agent).await;
    let sessions = checkpoint_store
        .list_sessions_page(SessionPageRequest {
            cursor: None,
            limit: 10,
        })
        .await
        .expect("list sessions");
    let child = sessions
        .sessions
        .iter()
        .find(|session| session.session_id != "root")
        .expect("child session");

    assert_eq!(
        (
            message,
            child_model.requests.lock().expect("child requests").len(),
            child.parent_session_id.as_deref(),
        ),
        ("root complete".to_string(), 1, Some("root"))
    );
}
