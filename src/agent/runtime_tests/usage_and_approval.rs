//! Usage And Approval agent runtime tests.

use super::*;

#[tokio::test]
async fn idle_session_start_stop_does_not_consume_the_next_prompt() {
    let workspace = tempfile::tempdir().expect("workspace");
    let model = Arc::new(HandoffModel::default());
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("main", model.clone())),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        Arc::new(
            SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
                .expect("checkpoint store"),
        ),
        test_middleware(vec![Arc::new(StoppingSessionStart)]),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id("startup-session-stop");
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(user_op("first"))
        .expect("submit first input");
    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::TurnComplete(_)
    ) {}

    assert_eq!(model.responses.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rejected_prompt_aborts_without_persisting_or_wedging_the_next_turn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(HandoffModel::default());
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("main", model.clone())),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoints.clone(),
        test_middleware(vec![Arc::new(RejectFirstPrompt(AtomicBool::new(false)))]),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id("rejected-prompt");
    let mut agent = create_agent(config).await.expect("create agent");
    let rejected_submission = agent
        .sender()
        .submit(user_op_with_attachments(
            "do not persist this secret",
            vec![SessionFileReference {
                id: "da913625-36d8-4624-815f-5523eb93b95f".into(),
                name: "secret.txt".into(),
                size: 6,
                media_type: "text/plain".into(),
            }],
        ))
        .expect("submit rejected input");
    let mut events = Vec::new();
    loop {
        let event = agent.next_event().await.expect("agent event");
        if event.submission_id.as_deref() != Some(&rejected_submission) {
            continue;
        }
        let terminal = matches!(event.msg, EventMsg::TurnAborted(_));
        events.push(event.msg);
        if terminal {
            break;
        }
    }

    assert!(
        events
            .iter()
            .any(|event| matches!(event, EventMsg::TurnStarted(_)))
    );
    assert!(matches!(events.last(), Some(EventMsg::TurnAborted(_))));
    assert!(!events.iter().any(|event| matches!(
        event,
        EventMsg::Message(_) | EventMsg::ModelStepStarted(_) | EventMsg::TurnComplete(_)
    )));
    let checkpoint = checkpoints
        .load("rejected-prompt")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    let checkpoint = serde_json::to_string(&checkpoint).expect("serialize checkpoint");
    assert!(!checkpoint.contains("do not persist this secret"));
    assert!(!checkpoint.contains("da913625-36d8-4624-815f-5523eb93b95f"));

    agent
        .sender()
        .submit(user_op("continue"))
        .expect("submit accepted input");
    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::TurnComplete(_)
    ) {}
    assert_eq!(model.responses.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pre_tool_hook_context_is_durable_before_open_call_at_approval() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([scripted_tool_call()])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("main", model)),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoint_store,
        test_middleware(vec![
            Arc::new(Tools::new(vec![Arc::new(ApprovalRequiredTestTool)])),
            Arc::new(ToolHookContext),
        ]),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id("pre-tool-hook-approval");
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(user_op("run it"))
        .expect("submit input");
    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::ExecApprovalRequest(_)
    ) {}

    let saved = checkpoints
        .load("pre-tool-hook-approval")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    let call = saved
        .context
        .iter()
        .position(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        .expect("tool call");
    let pre = saved
        .context
        .iter()
        .position(|item| internal_message_kind(item) == Some("pre_tool_hook"))
        .expect("pre-tool context");

    assert_eq!((pre + 1, saved.pending_approval.is_some()), (call, true));
}

#[tokio::test]
async fn post_tool_hook_context_follows_only_executed_tool_outputs() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([
            ModelOutput::from_output(
                vec![
                    serde_json::json!({
                        "type": "function_call",
                        "call_id": "call-1",
                        "name": "approval_required",
                        "arguments": "{}"
                    }),
                    serde_json::json!({
                        "type": "function_call",
                        "call_id": "call-2",
                        "name": "missing",
                        "arguments": "{}"
                    }),
                    serde_json::json!({
                        "type": "function_call",
                        "call_id": "call-3",
                        "name": "approval_required",
                        "arguments": "{}"
                    }),
                ],
                false,
                scripted_usage(),
            )
            .expect("tool output"),
            scripted_message("done"),
        ])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("main", model.clone())),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Allow,
        )),
        checkpoints,
        test_middleware(vec![
            Arc::new(Tools::new(vec![Arc::new(ApprovalRequiredTestTool)])),
            Arc::new(ToolHookContext),
        ]),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id("tool-hook-context");
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(user_op("run it"))
        .expect("submit input");
    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::TurnComplete(_)
    ) {}

    let inputs = model.inputs.lock().expect("model inputs");
    let second = &inputs[1];
    let sequence = second
        .iter()
        .filter_map(|item| {
            if let Some(kind) = internal_message_kind(item)
                && matches!(kind, "pre_tool_hook" | "post_tool_hook")
            {
                return Some((kind, None));
            }
            match item.get("type").and_then(Value::as_str) {
                Some(kind @ ("function_call" | "function_call_output")) => {
                    Some((kind, item.get("call_id").and_then(Value::as_str)))
                }
                _ => None,
            }
        })
        .collect::<Vec<_>>();

    assert_eq!(
        sequence,
        [
            ("pre_tool_hook", None),
            ("pre_tool_hook", None),
            ("function_call", Some("call-1")),
            ("function_call", Some("call-2")),
            ("function_call", Some("call-3")),
            ("function_call_output", Some("call-2")),
            ("function_call_output", Some("call-1")),
            ("post_tool_hook", None),
            ("function_call_output", Some("call-3")),
            ("post_tool_hook", None),
        ]
    );
}

async fn assert_compaction_stop(
    boundary: CompactStop,
    session_id: &str,
    expected_compactions: usize,
    expected_reason: &str,
) {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(HandoffModel::default());
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("main", model.clone())),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoints.clone(),
        test_middleware(vec![
            Arc::new(Compaction::new(1).expect("compaction middleware")),
            Arc::new(StoppingCompaction(boundary)),
        ]),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id(session_id);
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(user_op("hello"))
        .expect("submit input");
    let mut reason = None;
    let mut accounting = None;
    loop {
        match agent.next_event().await.expect("agent event").msg {
            EventMsg::Warning(warning) if warning.message == expected_reason => {
                reason = Some(warning.message)
            }
            EventMsg::TokenCount(event) => accounting = event.info,
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    let checkpoint = checkpoints
        .load(session_id)
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");

    assert_eq!(
        (
            model.responses.load(Ordering::SeqCst),
            model.compactions.load(Ordering::SeqCst),
            compaction_count(checkpoints.as_ref(), session_id).await,
            reason.as_deref(),
        ),
        (
            0,
            expected_compactions,
            expected_compactions as u64,
            Some(expected_reason)
        )
    );
    assert!(checkpoint.last_usage.is_none());
    if expected_compactions > 0 {
        let accounting = accounting.expect("compaction usage remains visible when the turn stops");
        assert_eq!(accounting.last_token_usage, scripted_usage());
        assert_eq!(accounting.total_token_usage, checkpoint.total_usage);
    } else {
        assert!(accounting.is_none());
    }
}

#[tokio::test]
async fn pre_compact_stop_completes_before_compaction() {
    assert_compaction_stop(
        CompactStop::Before,
        "pre-compact-stop",
        0,
        "pre-compact hook stopped the turn",
    )
    .await;
}

#[tokio::test]
async fn post_compact_stop_completes_after_compaction() {
    assert_compaction_stop(
        CompactStop::After,
        "post-compact-stop",
        1,
        "post-compact hook stopped the turn",
    )
    .await;
}

#[tokio::test]
async fn compact_session_start_stop_completes_after_compaction() {
    assert_compaction_stop(
        CompactStop::SessionStart,
        "compact-session-start-stop",
        1,
        "session-start hook stopped the turn",
    )
    .await;
}

#[tokio::test]
async fn compaction_notice_is_live_and_closes_on_success_failure_and_interrupt() {
    use crate::backend::model::{ModelCancellation, ModelCancellationReason};
    use crate::protocol::FrontendBlockState;
    use tokio::sync::Notify;

    struct InterruptedRequest<'a>(&'a ModelCancellation);
    impl Drop for InterruptedRequest<'_> {
        fn drop(&mut self) {
            assert_eq!(self.0.reason(), ModelCancellationReason::Interrupted);
        }
    }

    struct PausedCompaction {
        entered: Notify,
        release: Notify,
        fail: bool,
    }

    impl Model for PausedCompaction {
        fn respond<'a>(
            &'a self,
            request: ModelRequest<'a>,
            _: ModelEventSink,
        ) -> BoxFuture<'a, Result<ModelOutput>> {
            Box::pin(async move {
                if request.tools.len() != 1 || request.tools[0].name != "write_handoff" {
                    return Ok(scripted_message("done"));
                }
                let _interrupt = (request.session_id == "interrupt").then(|| {
                    InterruptedRequest(request.cancellation.expect("compaction cancellation"))
                });
                self.entered.notify_one();
                self.release.notified().await;
                if self.fail {
                    return Err(Error::Provider("compaction failed".into()));
                }
                Ok(scripted_handoff("paused-checkpoint"))
            })
        }
    }

    for outcome in ["success", "failure", "interrupt"] {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let workspace = tempfile::tempdir().expect("workspace");
            let model = Arc::new(PausedCompaction {
                entered: Notify::new(),
                release: Notify::new(),
                fail: outcome == "failure",
            });
            let checkpoints = Arc::new(
                SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3")).unwrap(),
            );
            let mut checkpoint = Checkpoint::empty(outcome);
            checkpoint.model_route = Some("main".into());
            checkpoint.session_context = test_session_context();
            checkpoint.context = Arc::new(vec![Arc::new(crate::backend::model::user_message(
                &"prior history ".repeat(1_000),
            ))]);
            checkpoints
                .save(&checkpoint, &checkpoint.context, None)
                .await
                .unwrap();
            let router_model = Arc::clone(&model) as Arc<dyn Model>;
            let store = Arc::clone(&checkpoints) as Arc<dyn CheckpointStore>;
            let mut agent = create_agent(
                AgentConfig::new(
                    Arc::new(ModelRouter::new("main", router_model)),
                    Arc::new(Sandbox::new(
                        Arc::new(LocalSandbox::new(workspace.path()).unwrap()),
                        ApprovalPolicy::Ask,
                    )),
                    store,
                    test_middleware(vec![Arc::new(Compaction::new(1_000).unwrap())]),
                    "test prompt",
                )
                .session_context(test_session_context())
                .session_id(outcome),
            )
            .await
            .unwrap();
            agent.sender().submit(user_op("compact this")).unwrap();
            let mut turn_id = None;
            let pending = loop {
                match agent.next_event().await.unwrap().msg {
                    EventMsg::TurnStarted(turn) => turn_id = Some(turn.turn_id),
                    EventMsg::Frontend(FrontendEvent::Render { capability, block })
                        if capability == "compaction" =>
                    {
                        break block;
                    }
                    _ => {}
                }
            };
            assert_eq!(pending.title, "Context compacting");
            assert_eq!(pending.state, FrontendBlockState::Pending);
            assert!(pending.id.is_some());
            // The pending row must arrive while the provider is still blocked.
            model.entered.notified().await;
            if outcome == "success" {
                let before = checkpoints.load(outcome).await.unwrap().unwrap();
                let admission = agent
                    .sender()
                    .send_with_admission(crate::protocol::Submission {
                        id: "steer-during-writer".into(),
                        op: active_user_op(
                            "keep this correction after compaction",
                            turn_id.as_ref().unwrap(),
                            ActiveMessageDelivery::Steer,
                        ),
                    })
                    .unwrap();
                assert_eq!(
                    admission.wait().await.unwrap(),
                    super::super::MessageAcceptance::Accepted
                );
                let queued = checkpoints.load(outcome).await.unwrap().unwrap();
                assert_eq!(
                    queued.context, before.context,
                    "the source writer cannot persist provisional history while accepting a steer"
                );
                assert_eq!(
                    (
                        queued.context_epoch,
                        compaction_count(checkpoints.as_ref(), outcome).await
                    ),
                    (0, 0)
                );
                assert!(
                    queued
                        .pending_messages
                        .iter()
                        .any(|message| message.id() == "steer-during-writer")
                );
            }
            if outcome == "interrupt" {
                agent
                    .sender()
                    .submit(Op::Interrupt {
                        turn_id: turn_id.unwrap(),
                    })
                    .unwrap();
            } else {
                model.release.notify_one();
            }
            let mut completions = 0;
            loop {
                match agent.next_event().await.unwrap().msg {
                    EventMsg::Frontend(FrontendEvent::Render { capability, block })
                        if capability == "compaction" =>
                    {
                        assert_eq!(block.id, pending.id);
                        assert_eq!(block.state, FrontendBlockState::Complete);
                        assert_eq!(
                            block.title,
                            match outcome {
                                "success" => "Context compacted",
                                "failure" => "Context compaction failed",
                                _ => "Context compaction cancelled",
                            }
                        );
                        completions += 1;
                    }
                    EventMsg::ContextCompacted => panic!("duplicate live compaction row"),
                    EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) => break,
                    _ => {}
                }
            }
            assert_eq!(completions, 1);
            let saved = checkpoints.load(outcome).await.unwrap().unwrap();
            assert_eq!(
                compaction_count(checkpoints.as_ref(), outcome).await,
                u64::from(outcome == "success")
            );
            if outcome == "success" {
                assert!(saved.pending_messages.is_empty());
                assert!(saved.context.iter().any(|item| {
                    item.to_string()
                        .contains("keep this correction after compaction")
                }));
            }
        })
        .await
        .expect("compaction notice lifecycle completed");
    }
}

#[tokio::test]
async fn compaction_notice_waits_for_later_preparation_hooks_to_settle() {
    use crate::protocol::FrontendBlockState;
    use tokio::sync::Notify;

    struct LaterHook {
        request: bool,
        fail: bool,
        entered: Notify,
        release: Notify,
    }

    impl LaterHook {
        async fn run(&self) -> Result<()> {
            self.entered.notify_one();
            if self.fail {
                return Err(Error::Provider("later preparation hook failed".into()));
            }
            self.release.notified().await;
            Ok(())
        }
    }

    impl Middleware for LaterHook {
        fn name(&self) -> &'static str {
            "later_hook"
        }

        fn pre_model<'a>(&'a self, _: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                if !self.request {
                    self.run().await?;
                }
                Ok(())
            })
        }

        fn model_request<'a>(
            &'a self,
            _: &'a mut ModelRequestContext<'_>,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                if self.request {
                    self.run().await?;
                }
                Ok(())
            })
        }
    }

    for request in [false, true] {
        for outcome in ["failure", "interrupt", "success"] {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let workspace = tempfile::tempdir().unwrap();
                let checkpoints = Arc::new(
                    SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3")).unwrap(),
                );
                let model = Arc::new(HandoffModel::default());
                let later = Arc::new(LaterHook {
                    request,
                    fail: outcome == "failure",
                    entered: Notify::new(),
                    release: Notify::new(),
                });
                let router_model = Arc::clone(&model) as Arc<dyn Model>;
                let store = Arc::clone(&checkpoints) as Arc<dyn CheckpointStore>;
                let later_hook = Arc::clone(&later) as Arc<dyn Middleware>;
                let mut agent = create_agent(
                    AgentConfig::new(
                        Arc::new(ModelRouter::new("main", router_model)),
                        Arc::new(Sandbox::new(
                            Arc::new(LocalSandbox::new(workspace.path()).unwrap()),
                            ApprovalPolicy::Ask,
                        )),
                        store,
                        test_middleware(vec![Arc::new(Compaction::new(1).unwrap()), later_hook]),
                        "test prompt",
                    )
                    .session_context(test_session_context())
                    .session_id(outcome),
                )
                .await
                .unwrap();
                agent.sender().submit(user_op("compact this")).unwrap();
                let mut turn_id = None;
                let pending = loop {
                    match agent.next_event().await.unwrap().msg {
                        EventMsg::TurnStarted(turn) => turn_id = Some(turn.turn_id),
                        EventMsg::Frontend(FrontendEvent::Render { capability, block })
                            if capability == "compaction" =>
                        {
                            break block;
                        }
                        _ => {}
                    }
                };
                assert_eq!(pending.state, FrontendBlockState::Pending);
                later.entered.notified().await;
                assert_eq!(model.compactions.load(Ordering::SeqCst), 1);
                if outcome != "failure" {
                    assert_eq!(
                        compaction_count(checkpoints.as_ref(), outcome).await,
                        0,
                        "compaction remains provisional while a later hook is blocked"
                    );
                    assert!(
                        tokio::time::timeout(
                            std::time::Duration::from_millis(20),
                            agent.next_event()
                        )
                        .await
                        .is_err(),
                        "the pending notice must remain open until preparation settles"
                    );
                    if outcome == "interrupt" {
                        agent
                            .sender()
                            .submit(Op::Interrupt {
                                turn_id: turn_id.unwrap(),
                            })
                            .unwrap();
                    } else {
                        later.release.notify_one();
                    }
                }
                let mut completions = 0;
                loop {
                    match agent.next_event().await.unwrap().msg {
                        EventMsg::Frontend(FrontendEvent::Render { capability, block })
                            if capability == "compaction" =>
                        {
                            assert_eq!(block.id, pending.id);
                            assert_eq!(block.state, FrontendBlockState::Complete);
                            assert_eq!(
                                block.title,
                                match outcome {
                                    "success" => "Context compacted",
                                    "failure" => "Context compaction failed",
                                    _ => "Context compaction cancelled",
                                }
                            );
                            let saved = checkpoints.load(outcome).await.unwrap().unwrap();
                            assert_eq!(saved.context_epoch, u64::from(outcome == "success"));
                            completions += 1;
                        }
                        EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) => break,
                        _ => {}
                    }
                }
                assert_eq!(
                    completions, 1,
                    "the row closes before the terminal turn event"
                );
                let saved = checkpoints.load(outcome).await.unwrap().unwrap();
                assert_eq!(
                    compaction_count(checkpoints.as_ref(), outcome).await,
                    u64::from(outcome == "success")
                );
                assert_eq!(saved.context_epoch, u64::from(outcome == "success"));
                assert_eq!(
                    saved.total_usage.total_tokens,
                    if outcome == "success" { 2 } else { 1 },
                    "completed checkpoint preparation is charged even when later hooks reject it"
                );
                assert_eq!(
                    saved.execution_stats.model_calls,
                    if outcome == "success" { 2 } else { 1 },
                    "completed preparation calls count alongside ordinary model calls"
                );
                if outcome != "success" {
                    assert!(
                        checkpoints
                            .load_state(outcome, "compaction.handoff")
                            .await
                            .unwrap()
                            .is_none(),
                        "rejected preparation must not restore provisional handoff notes"
                    );
                }
                let transcript = checkpoints
                    .transcript_page(
                        outcome,
                        TranscriptPageRequest {
                            before_sequence: None,
                            max_batches: 100,
                        },
                    )
                    .await
                    .unwrap()
                    .into_positioned_items_chronological();
                assert_eq!(
                    crate::protocol::replay_events(&transcript, outcome)
                        .iter()
                        .filter(|event| matches!(event, EventMsg::ContextCompacted))
                        .count(),
                    usize::from(outcome == "success")
                );
            })
            .await
            .expect("later preparation hook settled");
        }
    }
}

#[tokio::test]
async fn rejected_preparation_discards_appended_history_but_keeps_completed_usage() {
    struct RejectRequest;

    impl Middleware for RejectRequest {
        fn name(&self) -> &'static str {
            "reject_request"
        }

        fn model_request<'a>(
            &'a self,
            _: &'a mut ModelRequestContext<'_>,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Err(Error::Provider("request preparation rejected".into())) })
        }
    }

    let workspace = tempfile::tempdir().unwrap();
    let checkpoints =
        Arc::new(SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3")).unwrap());
    let model = Arc::new(HandoffModel::default());
    let mut agent = create_agent(
        AgentConfig::new(
            Arc::new(ModelRouter::new(
                "main",
                Arc::clone(&model) as Arc<dyn Model>,
            )),
            Arc::new(Sandbox::new(
                Arc::new(LocalSandbox::new(workspace.path()).unwrap()),
                ApprovalPolicy::Ask,
            )),
            Arc::clone(&checkpoints) as Arc<dyn CheckpointStore>,
            test_middleware(vec![Arc::new(DurableBeforeModel), Arc::new(RejectRequest)]),
            "test prompt",
        )
        .session_context(test_session_context())
        .session_id("rejected-preparation"),
    )
    .await
    .unwrap();
    agent
        .sender()
        .submit(user_op("preserve this user input"))
        .unwrap();
    loop {
        let event = agent.next_event().await.unwrap();
        assert!(!matches!(event.msg, EventMsg::ContextCompacted));
        if matches!(event.msg, EventMsg::TurnAborted(_)) {
            break;
        }
    }

    let checkpoint = checkpoints
        .load("rejected-preparation")
        .await
        .unwrap()
        .unwrap();
    assert!(
        !checkpoint
            .context
            .iter()
            .any(|item| internal_message_kind(item) == Some("settled"))
    );
    assert_eq!(checkpoint.total_usage.total_tokens, 1);
    assert_eq!(checkpoint.execution_stats.model_calls, 1);
    assert_eq!(
        (
            checkpoint.context_epoch,
            compaction_count(checkpoints.as_ref(), "rejected-preparation").await
        ),
        (0, 0)
    );
    assert_eq!(model.responses.load(Ordering::SeqCst), 0);
    let transcript = checkpoints
        .transcript_page(
            "rejected-preparation",
            TranscriptPageRequest {
                before_sequence: None,
                max_batches: 100,
            },
        )
        .await
        .unwrap()
        .into_positioned_items_chronological();
    assert!(
        !transcript
            .iter()
            .any(|(_, item)| internal_message_kind(item) == Some("settled"))
    );
    assert!(
        transcript
            .iter()
            .any(|(_, item)| item.to_string().contains("preserve this user input"))
    );
}

#[tokio::test]
async fn compaction_marker_survives_transcript_replay() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("main", Arc::new(HandoffModel::default()))),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoint_store,
        test_middleware(vec![Arc::new(
            Compaction::new(1).expect("compaction middleware"),
        )]),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id("durable-compaction");
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(user_op("hello"))
        .expect("submit input");

    let mut live_markers = 0;
    let mut completed = None;
    loop {
        match agent.next_event().await.expect("agent event").msg {
            EventMsg::Frontend(FrontendEvent::Render { capability, block })
                if capability == "compaction"
                    && block.state == crate::protocol::FrontendBlockState::Complete =>
            {
                live_markers += 1;
            }
            EventMsg::ModelStepCompleted(event) => completed = Some(event),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    let checkpoint = checkpoints
        .load("durable-compaction")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    let transcript = checkpoints
        .transcript_page(
            "durable-compaction",
            TranscriptPageRequest {
                before_sequence: None,
                max_batches: 100,
            },
        )
        .await
        .expect("load transcript")
        .into_positioned_items_chronological();
    let replayed = crate::protocol::replay_events(&transcript, "durable-compaction");

    assert_eq!(live_markers, 1);
    assert_eq!(checkpoint.context_epoch, 1);
    assert_eq!(
        compaction_count(checkpoints.as_ref(), "durable-compaction").await,
        1
    );
    assert_eq!(
        agent
            .frontend()
            .contributions()
            .expect("contributions")
            .iter()
            .find(|contribution| contribution.capability == "compaction")
            .and_then(|contribution| contribution.count),
        Some(1)
    );
    assert_eq!(
        checkpoint
            .last_context_rewrite
            .expect("context rewrite")
            .reasons,
        [crate::backend::checkpoint::ContextRewriteReason::Compaction]
    );
    assert_eq!(
        checkpoint
            .context
            .iter()
            .flat_map(|item| {
                item.get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter(|part| {
                part.get(crate::backend::model::PROMPT_CACHE_BREAKPOINT_FIELD)
                    .and_then(Value::as_bool)
                    == Some(true)
            })
            .count(),
        1
    );
    let diagnostics = completed
        .expect("completed model step")
        .diagnostics
        .expect("step diagnostics");
    assert_eq!(diagnostics.prompt_cache.context_epoch, 1);
    assert_eq!(
        diagnostics.prompt_cache.outcome,
        crate::protocol::PromptCacheOutcome::ContextRewrite
    );
    assert_eq!(diagnostics.prompt_cache.rewrite_reasons, ["compaction"]);
    assert_eq!(
        replayed
            .iter()
            .filter(|event| matches!(event, EventMsg::ContextCompacted))
            .count(),
        1
    );
}

#[tokio::test]
async fn provider_failure_records_one_failed_execution() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let mut agent = create_agent(config(
        workspace.path(),
        checkpoint_store,
        "provider-failure",
    ))
    .await
    .expect("create agent");
    agent
        .sender()
        .submit(user_op("fail"))
        .expect("submit input");
    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::TurnAborted(_)
    ) {}

    let execution = checkpoints
        .execution_page(
            "provider-failure",
            ExecutionPageRequest {
                before_sequence: None,
                limit: 1,
            },
        )
        .await
        .expect("execution page")
        .executions
        .pop()
        .expect("failed execution");

    assert_eq!(
        (
            execution.outcome,
            execution.model_calls,
            execution.tool_calls
        ),
        (ExecutionOutcome::Failed, 1, 0)
    );
}

#[tokio::test]
async fn cloned_agent_config_inherits_route_aware_usage_observer() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([scripted_message("done")])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let mut models = ModelRouter::new("main", model.clone());
    models
        .register("alternate", model)
        .expect("alternate route");
    let observed_usage = Arc::new(Mutex::new(Vec::new()));
    let usage_observer = Arc::clone(&observed_usage);
    let template = AgentConfig::new(
        Arc::new(models),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoints,
        test_middleware(Vec::new()),
        "test prompt",
    )
    .session_context(test_session_context())
    .usage_observer(move |route, usage| {
        usage_observer
            .lock()
            .expect("usage observer lock")
            .push((route.to_owned(), usage.total_tokens));
        Box::pin(async { Ok(()) })
    });
    let config = template
        .clone()
        .session_id("child")
        .model_route("alternate", None)
        .expect("child route");
    let mut agent = create_agent(config).await.expect("create child agent");
    agent
        .sender()
        .submit(user_op("hello"))
        .expect("submit input");
    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::TurnComplete(_)
    ) {}

    assert_eq!(
        observed_usage
            .lock()
            .expect("observed usage lock")
            .as_slice(),
        [("alternate".into(), 1)]
    );
}

#[tokio::test]
async fn failing_usage_observer_aborts_before_checkpoint_usage_is_committed() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([scripted_message("done")])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("main", model)),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoint_store,
        test_middleware(Vec::new()),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id("usage-observer-failure")
    .usage_observer(|_, _| {
        Box::pin(async {
            tokio::task::yield_now().await;
            Err(Error::Checkpoint("usage sink failed".into()))
        })
    });
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(user_op("hello"))
        .expect("submit input");
    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::TurnAborted(_)
    ) {}
    let saved = checkpoints
        .load("usage-observer-failure")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    let execution = checkpoints
        .execution_page(
            "usage-observer-failure",
            ExecutionPageRequest {
                before_sequence: None,
                limit: 1,
            },
        )
        .await
        .expect("execution page")
        .executions
        .pop()
        .expect("failed execution");

    assert_eq!(saved.total_usage, TokenUsage::default());
    assert_eq!(saved.last_usage, None);
    assert_eq!(execution.outcome, ExecutionOutcome::Failed);
    assert_eq!(execution.model_calls, 1);
    assert_eq!(execution.usage, TokenUsage::default());
}
