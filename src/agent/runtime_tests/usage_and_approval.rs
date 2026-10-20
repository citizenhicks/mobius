//! Usage And Approval agent runtime tests.

use super::*;

#[tokio::test]
async fn rejected_prompt_aborts_without_persisting_or_wedging_the_next_turn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(HandoffModel::default());
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("main", Arc::<HandoffModel>::clone(&model))),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        Arc::<SqliteCheckpoint>::clone(&checkpoints),
        test_middleware(vec![Arc::new(RejectFirstPrompt(AtomicBool::new(false)))]),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id("rejected-prompt".parse().expect("session ID"));
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
        .load(&"rejected-prompt".parse().expect("session ID"))
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
                let _interrupt = (request.session_id.map(crate::SessionId::as_str)
                    == Some("interrupt"))
                .then(|| {
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
            let mut checkpoint = Checkpoint::empty(outcome.parse().expect("session ID"));
            checkpoint.model_route = Some("main".into());
            checkpoint.session_context = test_session_context().into();
            checkpoint.context = Arc::new(vec![Arc::new(crate::backend::model::user_message(
                &"prior history ".repeat(1_000),
            ))]);
            checkpoints
                .save(checkpoint.clone(), checkpoint.context.to_vec(), None)
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
                .session_id(outcome.parse().expect("session ID")),
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
                let before = checkpoints
                    .load(&outcome.parse().expect("session ID"))
                    .await
                    .unwrap()
                    .unwrap();
                let admission = agent
                    .sender()
                    .send_with_admission(crate::protocol::Submission {
                        id: "steer-during-writer".into(),
                        op: active_user_op(
                            "keep this correction after compaction",
                            turn_id.as_deref().unwrap(),
                            ActiveMessageDelivery::Steer,
                        ),
                    })
                    .unwrap();
                assert_eq!(
                    admission.wait().await.unwrap(),
                    super::super::MessageAcceptance::Accepted
                );
                let queued = checkpoints
                    .load(&outcome.parse().expect("session ID"))
                    .await
                    .unwrap()
                    .unwrap();
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
            let saved = checkpoints
                .load(&outcome.parse().expect("session ID"))
                .await
                .unwrap()
                .unwrap();
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
async fn compaction_marker_survives_transcript_replay() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::<SqliteCheckpoint>::clone(&checkpoints);
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
    .session_id("durable-compaction".parse().expect("session ID"));
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
        .load(&"durable-compaction".parse().expect("session ID"))
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    let transcript = checkpoints
        .transcript_page(
            &"durable-compaction".parse().expect("session ID"),
            TranscriptPageRequest {
                before_sequence: None,
                max_batches: 100,
            },
        )
        .await
        .expect("load transcript")
        .into_positioned_items_chronological();
    let replayed = crate::protocol::replay_events(
        &transcript,
        &"durable-compaction".parse().expect("session ID"),
    );

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
            .find(|contribution| &*contribution.capability == "compaction")
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
async fn failing_usage_observer_aborts_before_checkpoint_usage_is_committed() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::<SqliteCheckpoint>::clone(&checkpoints);
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
    .session_id("usage-observer-failure".parse().expect("session ID"))
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
        .load(&"usage-observer-failure".parse().expect("session ID"))
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    let execution = checkpoints
        .execution_page(
            &"usage-observer-failure".parse().expect("session ID"),
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
