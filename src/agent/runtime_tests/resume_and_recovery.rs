//! Resume And Recovery agent runtime tests.

use super::*;

struct FailFirstMessageSubmit(Arc<AtomicBool>);

impl Middleware for FailFirstMessageSubmit {
    fn name(&self) -> &'static str {
        "fail_first_message_submit"
    }

    fn message_submit<'a>(
        &'a self,
        _context: &'a mut MessageSubmitContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if !self.0.swap(true, Ordering::SeqCst) {
                return Err(Error::Provider("message submit failed".into()));
            }
            Ok(())
        })
    }
}

#[tokio::test]
async fn failed_message_submit_leaves_the_accepted_message_for_restart() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let failed = Arc::new(AtomicBool::new(false));
    let model = Arc::new(NativeCompactionModel::default());
    let make_config = |checkpoints: Arc<dyn CheckpointStore>| {
        AgentConfig::new(
            Arc::new(ModelRouter::new("test", model.clone())),
            Arc::new(Sandbox::new(
                Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
                ApprovalPolicy::Ask,
            )),
            checkpoints,
            test_middleware(vec![Arc::new(FailFirstMessageSubmit(failed.clone()))]),
            "test prompt",
        )
        .session_id("message-submit-restart")
    };
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let mut first = create_agent(make_config(checkpoint_store))
        .await
        .expect("create first agent");
    let submission_id = first
        .sender()
        .submit(user_op("survive restart"))
        .expect("submit message");
    loop {
        let event = first.next_event().await.expect("first agent event");
        if matches!(event.msg, EventMsg::Error(_)) {
            break;
        }
    }
    let saved = checkpoints
        .load("message-submit-restart")
        .await
        .expect("load failed checkpoint")
        .expect("failed checkpoint");
    assert!(saved.active_execution.is_none());
    assert!(
        saved
            .pending_messages
            .iter()
            .any(|message| message.id() == submission_id)
    );
    drop(first);

    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let mut restarted = create_agent(make_config(checkpoint_store))
        .await
        .expect("restart agent");
    while !matches!(
        restarted
            .next_event()
            .await
            .expect("restarted agent event")
            .msg,
        EventMsg::TurnComplete(_)
    ) {}
    let saved = checkpoints
        .load("message-submit-restart")
        .await
        .expect("load completed checkpoint")
        .expect("completed checkpoint");
    assert!(saved.pending_messages.is_empty());
    assert_eq!(model.responses.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn interrupted_approval_is_one_durable_terminal_transition() {
    let workspace = tempfile::tempdir().expect("workspace");
    let database = workspace.path().join("checkpoints.sqlite3");
    let checkpoints = Arc::new(SqliteCheckpoint::new(&database).expect("checkpoint store"));
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([scripted_tool_call()])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let make_config = |checkpoints: Arc<dyn CheckpointStore>| {
        AgentConfig::new(
            Arc::new(ModelRouter::new("test", model.clone())),
            Arc::new(Sandbox::new(
                Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
                ApprovalPolicy::Ask,
            )),
            checkpoints,
            test_middleware(vec![Arc::new(Tools::new(vec![Arc::new(
                ApprovalRequiredTestTool,
            )]))]),
            "test prompt",
        )
        .session_id("atomic-approval-interrupt")
    };
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let mut first = create_agent(make_config(checkpoint_store))
        .await
        .expect("create first agent");
    first
        .sender()
        .submit(user_op("run it"))
        .expect("submit input");
    let turn_id = loop {
        if let EventMsg::ExecApprovalRequest(request) =
            first.next_event().await.expect("approval request").msg
        {
            break request.turn_id;
        }
    };

    rusqlite::Connection::open(&database)
        .expect("open checkpoint database")
        .execute_batch(
            "CREATE TRIGGER reject_turn_abort
             BEFORE INSERT ON event_journal
             WHEN NEW.event_kind = 'turn_aborted'
             BEGIN
                 SELECT RAISE(ABORT, 'forced turn abort failure');
             END;",
        )
        .expect("install abort failure");
    first
        .sender()
        .submit(Op::Interrupt {
            turn_id: turn_id.clone(),
        })
        .expect("interrupt approval");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while first.next_event().await.is_some() {}
    })
    .await
    .expect("failed terminal transition stops the first agent");
    drop(first);

    let failed = checkpoints
        .load("atomic-approval-interrupt")
        .await
        .expect("load failed transition")
        .expect("failed transition checkpoint");
    assert!(failed.active_execution.is_some());
    assert!(failed.pending_approval.is_some());
    assert_eq!(failed.pending_tools.len(), 1);

    rusqlite::Connection::open(&database)
        .expect("open checkpoint database")
        .execute_batch("DROP TRIGGER reject_turn_abort;")
        .expect("remove abort failure");
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let mut restarted = create_agent(make_config(checkpoint_store))
        .await
        .expect("restart pending approval");
    let resumed_turn_id = loop {
        if let EventMsg::ExecApprovalRequest(request) = restarted
            .next_event()
            .await
            .expect("resumed approval request")
            .msg
        {
            break request.turn_id;
        }
    };
    assert_eq!(resumed_turn_id, turn_id);
    restarted
        .sender()
        .submit(Op::Interrupt {
            turn_id: resumed_turn_id,
        })
        .expect("retry interrupt");
    while !matches!(
        restarted.next_event().await.expect("terminal event").msg,
        EventMsg::TurnAborted(_)
    ) {}
    drop(restarted);

    let terminal = checkpoints
        .load("atomic-approval-interrupt")
        .await
        .expect("load terminal checkpoint")
        .expect("terminal checkpoint");
    assert!(terminal.active_execution.is_none());
    assert!(terminal.active_model_step.is_none());
    assert!(terminal.pending_approval.is_none());
    assert!(terminal.pending_tools.is_empty());

    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let mut final_restart = create_agent(make_config(checkpoint_store))
        .await
        .expect("restart terminal session");
    let resurrected = tokio::time::timeout(std::time::Duration::from_millis(100), async {
        loop {
            let Some(event) = final_restart.next_event().await else {
                return false;
            };
            if matches!(event.msg, EventMsg::ExecApprovalRequest(_)) {
                return true;
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(!resurrected);
}

#[tokio::test]
async fn resumed_agent_rejects_a_checkpoint_without_its_model_route() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    checkpoints
        .save(&Checkpoint::empty("missing-route"), &[], None)
        .await
        .expect("save checkpoint");
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints;

    let error =
        match create_agent(config(workspace.path(), checkpoint_store, "missing-route")).await {
            Ok(_) => panic!("resume must not substitute the configured default route"),
            Err(error) => error,
        };

    assert!(matches!(
        error,
        Error::Checkpoint(message) if message == "saved session has no model route"
    ));
}

#[tokio::test]
async fn explicit_model_route_replaces_a_saved_route_that_is_still_registered() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let mut original = create_agent(config_with_two_routes(
        workspace.path(),
        checkpoint_store,
        "target",
        "kimi-k3",
        "kimi-k2.7",
    ))
    .await
    .expect("create original agent");
    original.next_event().await.expect("configured event");
    drop(original);

    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let mut restarted = create_agent(
        config_with_two_routes(
            workspace.path(),
            checkpoint_store,
            "target",
            "kimi-k2.7",
            "kimi-k3",
        )
        .override_saved_model_route(),
    )
    .await
    .expect("restart with explicit route");
    let EventMsg::SessionConfigured(configured) =
        restarted.next_event().await.expect("configured event").msg
    else {
        panic!("expected configured event");
    };
    let saved = checkpoints
        .load("target")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");

    assert_eq!(configured.model.route, "kimi-k2.7");
    assert_eq!(saved.model_route.as_deref(), Some("kimi-k2.7"));
}

#[tokio::test]
async fn model_route_change_is_recorded_with_its_checkpoint() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let mut agent = create_agent(config_with_two_routes(
        workspace.path(),
        checkpoint_store,
        "route-change",
        "kimi-k3",
        "kimi-k2.7",
    ))
    .await
    .expect("create agent");
    agent.next_event().await.expect("configured event");
    let submission_id = agent
        .sender()
        .submit(Op::SetModel {
            route: "kimi-k2.7".into(),
        })
        .expect("change route");

    let changed = loop {
        let event = agent.next_event().await.expect("model changed event");
        if event.submission_id.as_deref() == Some(&submission_id) {
            break event;
        }
    };
    let saved = checkpoints
        .load("route-change")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    let recorded = checkpoints
        .event_page(
            "route-change",
            EventPageRequest {
                before_sequence: None,
                limit: 1,
            },
        )
        .await
        .expect("event journal")
        .events
        .pop()
        .expect("recorded model change");

    assert!(matches!(
        changed.msg,
        EventMsg::ModelChanged(event) if event.route == "kimi-k2.7"
    ));
    assert_eq!(saved.model_route.as_deref(), Some("kimi-k2.7"));
    assert_eq!(recorded.event.submission_id, Some(submission_id));
    assert!(matches!(
        recorded.event.msg,
        EventMsg::ModelChanged(event) if event.route == "kimi-k2.7"
    ));
}

#[tokio::test]
async fn new_agent_uses_its_configured_model_instead_of_global_state() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    checkpoints
        .save_state(
            "agent",
            "model_route",
            &serde_json::Value::String("other".into()),
        )
        .await
        .expect("save unrelated global state");
    let mut models = ModelRouter::new("default", Arc::new(TestModel));
    models
        .register("other", Arc::new(TestModel))
        .expect("alternate route");
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let mut agent = create_agent(
        AgentConfig::new(
            Arc::new(models),
            Arc::new(Sandbox::new(
                Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
                ApprovalPolicy::Ask,
            )),
            checkpoint_store,
            test_middleware(Vec::new()),
            "test prompt",
        )
        .session_id("fresh"),
    )
    .await
    .expect("create agent");

    let EventMsg::SessionConfigured(configured) =
        agent.next_event().await.expect("configured event").msg
    else {
        panic!("expected configured event");
    };

    assert_eq!(configured.model.route, "default");
}

#[tokio::test]
async fn stale_save_does_not_leapfrog_winning_checkpoint() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let agent = create_agent(config(workspace.path(), checkpoint_store, "target"))
        .await
        .expect("create agent");
    let mut winner = checkpoints
        .load("target")
        .await
        .expect("load checkpoint")
        .expect("initial checkpoint");
    winner.sequence += 1;
    winner.context.push(serde_json::json!({"winner": true}));
    checkpoints
        .save(&winner, &winner.context, None)
        .await
        .expect("save competing checkpoint");

    let (sender, mut events) = agent.into_parts();
    sender
        .submit(user_op("lose the checkpoint race"))
        .expect("submit turn");
    drop(sender);
    while events.recv().await.is_some() {}

    assert_eq!(
        checkpoints
            .load("target")
            .await
            .expect("load checkpoint")
            .expect("winning checkpoint"),
        winner
    );
}

#[tokio::test]
async fn resumed_agent_uses_the_durable_session_context() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let durable_context = SessionContext {
        workspace_label: Some("Project One".into()),
        origin_label: Some("cron".into()),
        ..SessionContext::default()
    };
    let mut agent = create_agent(
        config(workspace.path(), checkpoint_store, "target")
            .session_context(durable_context.clone()),
    )
    .await
    .expect("create agent");
    let EventMsg::SessionConfigured(created) =
        agent.next_event().await.expect("created session event").msg
    else {
        panic!("expected configured session");
    };
    drop(agent);
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints;
    let mut resumed = create_agent(
        config(workspace.path(), checkpoint_store, "target").session_context(SessionContext {
            workspace_label: Some("wrong workspace".into()),
            ..SessionContext::default()
        }),
    )
    .await
    .expect("resume agent");
    let EventMsg::SessionConfigured(restored) = resumed
        .next_event()
        .await
        .expect("resumed session event")
        .msg
    else {
        panic!("expected configured session");
    };

    assert_eq!(
        (created.context, restored.context),
        (durable_context.clone(), durable_context)
    );
}

#[tokio::test]
async fn resumed_agent_preserves_or_explicitly_replaces_durable_metadata() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let durable_metadata = std::collections::BTreeMap::from([(
        "gateway.chat".into(),
        serde_json::json!({"workspace": "/srv/project"}),
    )]);
    let created_metadata = Arc::new(Mutex::new(None));
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let agent = create_agent(config_with_metadata_probe(
        workspace.path(),
        checkpoint_store,
        "target",
        Some(durable_metadata.clone()),
        Arc::clone(&created_metadata),
    ))
    .await
    .expect("create agent");
    drop(agent);
    let resumed_metadata = Arc::new(Mutex::new(None));
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let resumed = create_agent(config_with_metadata_probe(
        workspace.path(),
        checkpoint_store,
        "target",
        None,
        Arc::clone(&resumed_metadata),
    ))
    .await
    .expect("resume agent");
    drop(resumed);
    let replacement_metadata = std::collections::BTreeMap::from([(
        "gateway.chat".into(),
        serde_json::json!({"workspace": "/srv/replacement"}),
    )]);
    let replaced_metadata = Arc::new(Mutex::new(None));
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let replaced = create_agent(config_with_metadata_probe(
        workspace.path(),
        checkpoint_store,
        "target",
        Some(replacement_metadata.clone()),
        Arc::clone(&replaced_metadata),
    ))
    .await
    .expect("replace metadata");
    drop(replaced);

    assert_eq!(
        created_metadata.lock().expect("created metadata").as_ref(),
        Some(&durable_metadata)
    );
    assert_eq!(
        resumed_metadata.lock().expect("resumed metadata").as_ref(),
        Some(&durable_metadata)
    );
    assert_eq!(
        replaced_metadata
            .lock()
            .expect("replaced metadata")
            .as_ref(),
        Some(&replacement_metadata)
    );
    assert_eq!(
        checkpoints
            .load("target")
            .await
            .expect("load checkpoint")
            .expect("saved checkpoint")
            .metadata,
        replacement_metadata
    );
}

#[tokio::test]
async fn resume_request_carries_the_target_session_context() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let target_context = SessionContext {
        workspace_id: Some("workspace-two".into()),
        workspace_label: Some("Project Two".into()),
        origin_label: Some("cron".into()),
        ..SessionContext::default()
    };
    let mut target = Checkpoint::empty("target");
    target.session_context.clone_from(&target_context);
    target.model_route = Some("foreign-workspace-route".into());
    checkpoints
        .save(&target, &[], None)
        .await
        .expect("save target session");
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints;
    let agent = create_agent(config(workspace.path(), checkpoint_store, "current"))
        .await
        .expect("create current agent");
    let (sender, mut events) = agent.into_parts();
    events.recv().await.expect("configured session event");
    let submission_id = sender
        .submit(Op::ResumeSession {
            session_id: "target".into(),
        })
        .expect("request resume");

    let event = loop {
        let event = events.recv().await.expect("resume requested event");
        if event.submission_id.as_deref() == Some(&submission_id) {
            break event;
        }
    };
    let EventMsg::SessionResumeRequested(request) = event.msg else {
        panic!("expected resume request");
    };

    assert_eq!(
        (event.submission_id, request.session_id, request.context),
        (Some(submission_id), "target".into(), target_context)
    );
}

#[tokio::test]
async fn zero_replay_mode_emits_uncertain_tool_recovery_as_individual_events() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let call = ToolCall {
        call_id: "call-1".into(),
        name: "write_file".into(),
        arguments: serde_json::json!({"path": "note.txt", "content": "hello"}),
    };
    let mut target = Checkpoint::empty("target");
    target.model_route = Some("test".into());
    target.active_execution = Some(crate::backend::checkpoint::ActiveExecution {
        submission_id: "submission-1".into(),
        turn_id: "turn-1".into(),
        started_at_ms: 1,
        model_calls: 0,
        tool_calls: 0,
        failed_tool_calls: 0,
        usage: TokenUsage::default(),
        next_model_step: 0,
        stop_hook_active: false,
        phase: crate::backend::checkpoint::ExecutionPhase::Model,
    });
    target.context.push(serde_json::json!({
        "type": "function_call",
        "call_id": call.call_id.clone(),
        "name": call.name.clone(),
        "arguments": call.arguments.to_string()
    }));
    target.pending_tools.push(call);
    target.pending_messages.push(queued_user_message(
        "message-1",
        "queued after restart",
        QueuedMessageBoundary::Steer {
            turn_id: "turn-1".into(),
        },
    ));
    checkpoints
        .save(&target, &target.context, None)
        .await
        .expect("save target");
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();

    let mut agent = create_agent(
        config(workspace.path(), checkpoint_store, "target").initial_replay_batches(0),
    )
    .await
    .expect("resume agent");
    assert!(matches!(
        agent.next_event().await.expect("session event").msg,
        EventMsg::SessionConfigured(_)
    ));
    let mut history = Vec::new();
    while history.len() < 2 {
        let event = agent.next_event().await.expect("recovery event").msg;
        if matches!(event, EventMsg::ToolCallEnd(_) | EventMsg::Message(_)) {
            history.push(event);
        }
    }
    assert!(matches!(
        history.as_slice(),
        [
            EventMsg::ToolCallEnd(ToolCallEndEvent {
                turn_id,
                call_id,
                output,
                is_error: true,
                ..
            }),
            EventMsg::Message(user)
        ] if turn_id == "turn-1"
            && call_id == "call-1"
            && output == "execution interrupted; result unknown after restart"
            && user.text == "queued after restart"
            && user.delivery == MessageDelivery::Queue
    ));
    while !matches!(
        agent.next_event().await.expect("queued turn event").msg,
        EventMsg::TurnAborted(_)
    ) {}

    let recovered = checkpoints
        .load("target")
        .await
        .expect("load checkpoint")
        .expect("recovered checkpoint");
    let execution = checkpoints
        .execution_page(
            "target",
            ExecutionPageRequest {
                before_sequence: None,
                limit: 2,
            },
        )
        .await
        .expect("execution page")
        .executions
        .into_iter()
        .find(|execution| execution.turn_id == "turn-1")
        .expect("recovered execution");
    assert!(recovered.active_execution.is_none());
    assert!(recovered.pending_tools.is_empty());
    assert_eq!(
        recovered.context.iter().find_map(|item| {
            item.get(TOOL_ERROR_FIELD)
                .and_then(serde_json::Value::as_bool)
        }),
        Some(true)
    );
    assert_eq!(
        (
            execution.outcome,
            execution.tool_calls,
            execution.failed_tool_calls,
            recovered.execution_stats.aborted_run_count,
        ),
        (ExecutionOutcome::Aborted, 1, 1, 1)
    );
}

#[tokio::test]
async fn restart_resolves_a_durable_turn_completion_instead_of_aborting_it() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let mut checkpoint = Checkpoint::empty("resume-completion");
    checkpoint.model_route = Some("test".into());
    checkpoint.active_execution = Some(crate::backend::checkpoint::ActiveExecution {
        submission_id: "submission-1".into(),
        turn_id: "turn-1".into(),
        started_at_ms: 1,
        model_calls: 1,
        tool_calls: 0,
        failed_tool_calls: 0,
        usage: TokenUsage::default(),
        next_model_step: 1,
        stop_hook_active: false,
        phase: crate::backend::checkpoint::ExecutionPhase::Completion {
            last_assistant_message: Some("finished".into()),
        },
    });
    checkpoints
        .save(&checkpoint, &[], None)
        .await
        .expect("save turn completion");
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();

    let mut agent = create_agent(
        config(workspace.path(), checkpoint_store, "resume-completion").initial_replay_batches(0),
    )
    .await
    .expect("resume completion");
    assert!(matches!(
        agent.next_event().await.expect("session event").msg,
        EventMsg::SessionConfigured(_)
    ));
    while !matches!(
        agent.next_event().await.expect("completion event").msg,
        EventMsg::TurnComplete(_)
    ) {}

    let saved = checkpoints
        .load("resume-completion")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    assert!(saved.active_execution.is_none());
    assert_eq!(saved.execution_stats.run_count, 1);
    assert_eq!(saved.execution_stats.aborted_run_count, 0);
}

#[tokio::test]
async fn restart_resumes_the_same_model_cursor() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let mut checkpoint = Checkpoint::empty("resume-model");
    checkpoint.model_route = Some("test".into());
    checkpoint.active_execution = Some(crate::backend::checkpoint::ActiveExecution {
        submission_id: "submission-1".into(),
        turn_id: "turn-1".into(),
        started_at_ms: 1,
        model_calls: 3,
        tool_calls: 0,
        failed_tool_calls: 0,
        usage: TokenUsage::default(),
        next_model_step: 3,
        stop_hook_active: false,
        phase: crate::backend::checkpoint::ExecutionPhase::Model,
    });
    checkpoints
        .save(&checkpoint, &[], None)
        .await
        .expect("save model boundary");
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([scripted_message("resumed")])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();

    let mut agent = create_agent(
        config_with_model(
            workspace.path(),
            checkpoint_store,
            "resume-model",
            "test",
            model,
        )
        .initial_replay_batches(0),
    )
    .await
    .expect("resume model boundary");
    assert!(matches!(
        agent.next_event().await.expect("session event").msg,
        EventMsg::SessionConfigured(_)
    ));
    let mut resumed_step = None;
    loop {
        match agent.next_event().await.expect("turn event").msg {
            EventMsg::ModelStepStarted(step) => resumed_step = Some(step.step_index),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    let saved = checkpoints
        .load("resume-model")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    assert_eq!(resumed_step, Some(3));
    assert!(saved.active_execution.is_none());
    assert_eq!(saved.execution_stats.aborted_run_count, 0);
}

#[tokio::test]
async fn restart_closes_an_active_model_step_with_the_recovery_checkpoint() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let mut checkpoint = Checkpoint::empty("recover-step");
    checkpoint.model_route = Some("test".into());
    checkpoint.active_execution = Some(crate::backend::checkpoint::ActiveExecution {
        submission_id: "submission-1".into(),
        turn_id: "turn-1".into(),
        started_at_ms: 10,
        model_calls: 1,
        tool_calls: 0,
        failed_tool_calls: 0,
        usage: TokenUsage::default(),
        next_model_step: 1,
        stop_hook_active: false,
        phase: crate::backend::checkpoint::ExecutionPhase::Model,
    });
    checkpoint.active_model_step = Some(crate::backend::checkpoint::ActiveModelStep {
        model_step_id: "step-1".into(),
        step_index: 0,
        started_at_ms: 20,
    });
    checkpoints
        .save(&checkpoint, &[], None)
        .await
        .expect("save active step");
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();

    let mut agent = create_agent(
        config(workspace.path(), checkpoint_store, "recover-step").initial_replay_batches(0),
    )
    .await
    .expect("recover agent");
    let configured = agent.next_event().await.expect("session event");
    let completed = agent.next_event().await.expect("step completion");
    let aborted = agent.next_event().await.expect("turn abort");
    let saved = checkpoints
        .load("recover-step")
        .await
        .expect("load checkpoint")
        .expect("recovered checkpoint");

    assert!(matches!(configured.msg, EventMsg::SessionConfigured(_)));
    assert!(matches!(
        completed.msg,
        EventMsg::ModelStepCompleted(event)
            if event.model_step_id == "step-1"
                && event.outcome == ModelStepOutcome::Interrupted
    ));
    assert!(matches!(
        aborted.msg,
        EventMsg::TurnAborted(event) if event.turn_id == "turn-1"
    ));
    assert!(saved.active_execution.is_none());
    assert!(saved.active_model_step.is_none());
}
