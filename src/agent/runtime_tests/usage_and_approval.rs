//! Usage And Approval agent runtime tests.

use super::*;

#[tokio::test]
async fn idle_session_start_stop_does_not_consume_the_next_prompt() {
    let workspace = tempfile::tempdir().expect("workspace");
    let model = Arc::new(NativeCompactionModel::default());
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
        MiddlewareStack::new(vec![Arc::new(StoppingSessionStart)]).expect("middleware"),
        "test prompt",
    )
    .session_id("startup-session-stop");
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(Op::UserInput {
            text: "first".into(),
            attachments: Vec::new(),
        })
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
    let model = Arc::new(NativeCompactionModel::default());
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("main", model.clone())),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoints.clone(),
        MiddlewareStack::new(vec![Arc::new(RejectFirstPrompt(AtomicBool::new(false)))])
            .expect("middleware"),
        "test prompt",
    )
    .session_id("rejected-prompt");
    let mut agent = create_agent(config).await.expect("create agent");
    let rejected_submission = agent
        .sender()
        .submit(Op::UserInput {
            text: "do not persist this secret".into(),
            attachments: vec![SessionFileReference {
                id: "da913625-36d8-4624-815f-5523eb93b95f".into(),
                name: "secret.txt".into(),
                size: 6,
                media_type: "text/plain".into(),
            }],
        })
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

    assert!(matches!(events.first(), Some(EventMsg::TurnStarted(_))));
    assert!(matches!(events.last(), Some(EventMsg::TurnAborted(_))));
    assert!(!events.iter().any(|event| matches!(
        event,
        EventMsg::UserMessage(_) | EventMsg::ModelStepStarted(_) | EventMsg::TurnComplete(_)
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
        .submit(Op::UserInput {
            text: "continue".into(),
            attachments: Vec::new(),
        })
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
        MiddlewareStack::new(vec![
            Arc::new(Tools::new(vec![Arc::new(ApprovalRequiredTestTool)])),
            Arc::new(ToolHookContext),
        ])
        .expect("middleware"),
        "test prompt",
    )
    .session_id("pre-tool-hook-approval");
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(Op::UserInput {
            text: "run it".into(),
            attachments: Vec::new(),
        })
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
        MiddlewareStack::new(vec![
            Arc::new(Tools::new(vec![Arc::new(ApprovalRequiredTestTool)])),
            Arc::new(ToolHookContext),
        ])
        .expect("middleware"),
        "test prompt",
    )
    .session_id("tool-hook-context");
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(Op::UserInput {
            text: "run it".into(),
            attachments: Vec::new(),
        })
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
    let model = Arc::new(NativeCompactionModel::default());
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("main", model.clone())),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoints.clone(),
        MiddlewareStack::new(vec![
            Arc::new(Compaction::new(1).expect("compaction middleware")),
            Arc::new(StoppingCompaction(boundary)),
        ])
        .expect("middleware"),
        "test prompt",
    )
    .session_id(session_id);
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(Op::UserInput {
            text: "hello".into(),
            attachments: Vec::new(),
        })
        .expect("submit input");
    let mut reason = None;
    loop {
        match agent.next_event().await.expect("agent event").msg {
            EventMsg::Warning(warning) if warning.message == expected_reason => {
                reason = Some(warning.message)
            }
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
            checkpoint.compaction_count,
            reason.as_deref(),
        ),
        (
            0,
            expected_compactions,
            expected_compactions as u64,
            Some(expected_reason)
        )
    );
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
async fn compaction_marker_survives_transcript_replay() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new(
            "main",
            Arc::new(NativeCompactionModel::default()),
        )),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoint_store,
        MiddlewareStack::new(vec![Arc::new(
            Compaction::new(1).expect("compaction middleware"),
        )])
        .expect("middleware"),
        "test prompt",
    )
    .session_id("durable-compaction");
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(Op::UserInput {
            text: "hello".into(),
            attachments: Vec::new(),
        })
        .expect("submit input");

    let mut live_markers = 0;
    let mut completed = None;
    loop {
        match agent.next_event().await.expect("agent event").msg {
            EventMsg::ContextCompacted => live_markers += 1,
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
    assert_eq!(checkpoint.compaction_count, 1);
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
        .submit(Op::UserInput {
            text: "fail".into(),
            attachments: Vec::new(),
        })
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
async fn automatic_approval_counts_isolated_review_usage_without_replacing_primary_usage() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let mut review_output =
        scripted_message("{\"decision\":\"approve\",\"call_ids\":[\"reviewed-call\"]}");
    review_output.usage = TokenUsage {
        input_tokens: 7,
        total_tokens: 7,
        ..TokenUsage::default()
    };
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([
            scripted_tool_call(),
            review_output,
            scripted_message("done"),
        ])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let observed_usage = Arc::new(Mutex::new(Vec::new()));
    let usage_observer = Arc::clone(&observed_usage);
    let config = auto_review_config(
        workspace.path(),
        checkpoint_store,
        model.clone(),
        "auto-review",
    )
    .usage_observer(move |route, usage| {
        usage_observer
            .lock()
            .expect("usage observer lock")
            .push((route.to_owned(), usage.total_tokens));
        Ok(())
    });
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(Op::UserInput {
            text: "do it".into(),
            attachments: Vec::new(),
        })
        .expect("submit input");
    let mut usage_events = Vec::new();
    let mut review_events = Vec::new();
    loop {
        match agent.next_event().await.expect("agent event").msg {
            EventMsg::TokenCount(count) => {
                let usage = count.info.expect("usage info");
                usage_events.push((
                    usage.total_token_usage.total_tokens,
                    usage.last_token_usage.total_tokens,
                ));
            }
            EventMsg::ExecApprovalReview(review) => {
                review_events.push((review.status, review.reason));
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    let saved = checkpoints
        .load("auto-review")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    let execution = checkpoints
        .execution_page(
            "auto-review",
            ExecutionPageRequest {
                before_sequence: None,
                limit: 1,
            },
        )
        .await
        .expect("execution page")
        .executions
        .pop()
        .expect("completed execution");

    assert_eq!(
        model
            .tool_counts
            .lock()
            .expect("tool count lock")
            .as_slice(),
        [1, 0, 1]
    );
    assert_eq!(saved.total_usage.total_tokens, 9);
    assert_eq!(saved.last_usage, Some(scripted_usage()));
    assert_eq!(usage_events, [(1, 1), (8, 1), (9, 1)]);
    assert_eq!(
        review_events,
        [
            (ApprovalReviewStatus::Reviewing, None),
            (ApprovalReviewStatus::Approved, None),
        ]
    );
    assert_eq!(
        observed_usage
            .lock()
            .expect("observed usage lock")
            .as_slice(),
        [
            ("main".into(), 1),
            ("reviewer".into(), 7),
            ("main".into(), 1)
        ]
    );
    assert_eq!(
        (
            execution.outcome,
            execution.model_calls,
            execution.tool_calls,
            execution.failed_tool_calls,
            execution.usage.total_tokens,
            saved.execution_stats.run_count,
        ),
        (ExecutionOutcome::Completed, 3, 1, 0, 9, 1)
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
        MiddlewareStack::new(Vec::new()).expect("middleware"),
        "test prompt",
    )
    .usage_observer(move |route, usage| {
        usage_observer
            .lock()
            .expect("usage observer lock")
            .push((route.to_owned(), usage.total_tokens));
        Ok(())
    });
    let config = template
        .clone()
        .session_id("child")
        .model_route("alternate", None)
        .expect("child route");
    let mut agent = create_agent(config).await.expect("create child agent");
    agent
        .sender()
        .submit(Op::UserInput {
            text: "hello".into(),
            attachments: Vec::new(),
        })
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
        MiddlewareStack::new(Vec::new()).expect("middleware"),
        "test prompt",
    )
    .session_id("usage-observer-failure")
    .usage_observer(|_, _| Err(Error::Checkpoint("usage sink failed".into())));
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(Op::UserInput {
            text: "hello".into(),
            attachments: Vec::new(),
        })
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

#[tokio::test]
async fn malformed_automatic_review_durably_asks_without_dropping_network_access() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([
            scripted_tool_call(),
            scripted_message("not a review decision"),
        ])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let mut agent = create_agent(auto_review_config(
        workspace.path(),
        checkpoint_store,
        model.clone(),
        "review-escalation",
    ))
    .await
    .expect("create agent");
    agent
        .sender()
        .submit(Op::UserInput {
            text: "do it".into(),
            attachments: Vec::new(),
        })
        .expect("submit input");
    let review_events = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut review_events = Vec::new();
        loop {
            match agent.next_event().await.expect("agent event").msg {
                EventMsg::ExecApprovalReview(review) => {
                    review_events.push((review.status, review.reason));
                }
                EventMsg::ExecApprovalRequest(_) => break review_events,
                _ => {}
            }
        }
    })
    .await
    .expect("approval escalation");
    let saved = checkpoints
        .load("review-escalation")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    let pending = saved.pending_approval.expect("pending approval");
    let recorded = checkpoints
        .event_page(
            "review-escalation",
            EventPageRequest {
                before_sequence: None,
                limit: 1,
            },
        )
        .await
        .expect("event journal")
        .events
        .pop()
        .expect("recorded approval request");

    assert_eq!(
        pending.sandbox_mode,
        crate::backend::sandbox::SandboxMode::WorkspaceWrite
    );
    assert_eq!(
        pending.network_access,
        crate::backend::sandbox::NetworkAccess::Allowed
    );
    assert_eq!(saved.total_usage.total_tokens, 2);
    assert_eq!(
        review_events,
        [
            (ApprovalReviewStatus::Reviewing, None),
            (
                ApprovalReviewStatus::Escalated,
                Some(ApprovalReviewEscalation::InvalidResponse),
            ),
        ]
    );
    assert!(matches!(
        recorded.event.msg,
        EventMsg::ExecApprovalRequest(request) if request.id == pending.request_id
    ));
    assert_eq!(
        model
            .tool_counts
            .lock()
            .expect("tool count lock")
            .as_slice(),
        [1, 0]
    );
}

#[tokio::test]
async fn escalated_review_and_deferred_approval_are_saved_atomically() {
    let workspace = tempfile::tempdir().expect("workspace");
    let database = workspace.path().join("checkpoints.sqlite3");
    let checkpoints = Arc::new(SqliteCheckpoint::new(&database).expect("checkpoint store"));
    rusqlite::Connection::open(&database)
        .expect("open checkpoint database")
        .execute_batch(
            "CREATE TRIGGER reject_approval_request
             BEFORE INSERT ON event_journal
             WHEN NEW.event_kind = 'exec_approval_request'
             BEGIN
                 SELECT RAISE(ABORT, 'forced approval request failure');
             END;",
        )
        .expect("install approval request failure");
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([
            scripted_tool_call(),
            scripted_message("not a review decision"),
        ])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let checkpoint_store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let mut agent = create_agent(auto_review_config(
        workspace.path(),
        checkpoint_store,
        model,
        "review-atomicity",
    ))
    .await
    .expect("create agent");
    agent
        .sender()
        .submit(Op::UserInput {
            text: "do it".into(),
            attachments: Vec::new(),
        })
        .expect("submit input");
    while agent.next_event().await.is_some() {}

    let events = checkpoints
        .event_page(
            "review-atomicity",
            EventPageRequest {
                before_sequence: None,
                limit: 100,
            },
        )
        .await
        .expect("event journal")
        .events;

    assert!(!events.iter().any(|event| matches!(
        &event.event.msg,
        EventMsg::ExecApprovalReview(review)
            if review.status == ApprovalReviewStatus::Escalated
    )));
}
