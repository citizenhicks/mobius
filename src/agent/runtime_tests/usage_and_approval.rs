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

#[tokio::test]
async fn idle_compaction_runs_once_without_a_model_reply() {
    for (min_tokens, compacts) in [(1_000, true), (10_000_000, false)] {
        let workspace = tempfile::tempdir().expect("workspace");
        let checkpoints = Arc::new(
            SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
                .expect("checkpoint store"),
        );
        let mut checkpoint = Checkpoint::empty("idle".parse().expect("session ID"));
        checkpoint.model_route = Some("main".into());
        checkpoint.session_context = test_session_context().into();
        checkpoint.context = Arc::new(vec![Arc::new(crate::backend::model::user_message(
            &"prior history ".repeat(1_000),
        ))]);
        checkpoints
            .save(checkpoint.clone(), checkpoint.context.to_vec(), None)
            .await
            .expect("seed checkpoint");
        let model = Arc::new(HandoffModel::default());
        let compaction = Compaction::new(10_000_000)
            .expect("compaction middleware")
            .idle_compaction(Some(std::time::Duration::from_millis(50)), min_tokens)
            .expect("idle compaction");
        let mut agent = create_agent(
            AgentConfig::new(
                Arc::new(ModelRouter::new("main", Arc::<HandoffModel>::clone(&model))),
                Arc::new(Sandbox::new(
                    Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
                    ApprovalPolicy::Ask,
                )),
                Arc::<SqliteCheckpoint>::clone(&checkpoints),
                test_middleware(vec![Arc::new(compaction)]),
                "test prompt",
            )
            .session_context(test_session_context())
            .session_id("idle".parse().expect("session ID")),
        )
        .await
        .expect("create agent");
        agent.sender().submit(user_op("hello")).expect("submit");
        while !matches!(
            agent.next_event().await.expect("event").msg,
            EventMsg::TurnComplete(_)
        ) {}
        let quiet = std::time::Duration::from_millis(500);
        if compacts {
            let mut compacted = false;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    match agent.next_event().await.expect("event").msg {
                        EventMsg::Frontend(FrontendEvent::Render { capability, block })
                            if capability == "compaction"
                                && block.state == crate::protocol::FrontendBlockState::Complete =>
                        {
                            assert_eq!(block.title, "Context compacted");
                            compacted = true;
                        }
                        EventMsg::TurnComplete(_) => break,
                        EventMsg::TurnAborted(turn) => panic!("idle turn aborted: {}", turn.reason),
                        _ => {}
                    }
                }
            })
            .await
            .expect("idle turn completed");
            assert!(compacted);
            assert_eq!(compaction_count(checkpoints.as_ref(), "idle").await, 1);
        }
        assert!(
            tokio::time::timeout(quiet, agent.next_event())
                .await
                .is_err(),
            "the idle timer fires at most once per idle period"
        );
        assert_eq!(model.responses.load(Ordering::SeqCst), 1);
        assert_eq!(
            model.compactions.load(Ordering::SeqCst),
            usize::from(compacts)
        );
        let saved = checkpoints
            .load(&"idle".parse().expect("session ID"))
            .await
            .expect("load")
            .expect("checkpoint");
        assert!(saved.active_execution.is_none());
    }
}

#[tokio::test]
async fn a_message_steered_into_the_idle_turn_is_answered_after_compaction() {
    use tokio::sync::Notify;

    struct RejectSteer(bool);

    impl Middleware for RejectSteer {
        fn name(&self) -> &'static str {
            "reject_idle_steer"
        }

        fn message_submit<'a>(
            &'a self,
            context: &'a mut MessageSubmitContext<'_>,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                if self.0 && context.message == "answer this after idle compaction" {
                    context.reject("steer rejected by policy")?;
                }
                Ok(())
            })
        }
    }

    #[derive(Default)]
    struct PausedIdleCompaction {
        entered: Notify,
        release: Notify,
        responses: AtomicUsize,
    }

    impl Model for PausedIdleCompaction {
        fn respond<'a>(
            &'a self,
            request: ModelRequest<'a>,
            _: ModelEventSink,
        ) -> BoxFuture<'a, Result<ModelOutput>> {
            Box::pin(async move {
                if request.tools.len() != 1 || request.tools[0].name != "write_handoff" {
                    self.responses.fetch_add(1, Ordering::SeqCst);
                    return Ok(scripted_message("done"));
                }
                self.entered.notify_one();
                self.release.notified().await;
                Ok(scripted_handoff("idle-checkpoint"))
            })
        }
    }

    for reject in [false, true] {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let workspace = tempfile::tempdir().expect("workspace");
            let checkpoints = Arc::new(
                SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
                    .expect("checkpoint store"),
            );
            let mut checkpoint = Checkpoint::empty("idle-steer".parse().expect("session ID"));
            checkpoint.model_route = Some("main".into());
            checkpoint.session_context = test_session_context().into();
            checkpoint.context = Arc::new(vec![Arc::new(crate::backend::model::user_message(
                &"prior history ".repeat(1_000),
            ))]);
            checkpoints
                .save(checkpoint.clone(), checkpoint.context.to_vec(), None)
                .await
                .expect("seed checkpoint");
            let model = Arc::new(PausedIdleCompaction::default());
            let compaction = Compaction::new(10_000_000)
                .expect("compaction middleware")
                .idle_compaction(Some(std::time::Duration::from_millis(50)), 1)
                .expect("idle compaction");
            let mut agent = create_agent(
                AgentConfig::new(
                    Arc::new(ModelRouter::new(
                        "main",
                        Arc::<PausedIdleCompaction>::clone(&model),
                    )),
                    Arc::new(Sandbox::new(
                        Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
                        ApprovalPolicy::Ask,
                    )),
                    Arc::<SqliteCheckpoint>::clone(&checkpoints),
                    test_middleware(vec![Arc::new(compaction), Arc::new(RejectSteer(reject))]),
                    "test prompt",
                )
                .session_context(test_session_context())
                .session_id("idle-steer".parse().expect("session ID")),
            )
            .await
            .expect("create agent");
            agent.sender().submit(user_op("hello")).expect("submit");
            while !matches!(
                agent.next_event().await.expect("event").msg,
                EventMsg::TurnComplete(_)
            ) {}
            let idle_turn = loop {
                if let EventMsg::TurnStarted(turn) = agent.next_event().await.expect("event").msg {
                    break turn.turn_id;
                }
            };
            model.entered.notified().await;
            let admission = agent
                .sender()
                .send_with_admission(crate::protocol::Submission {
                    id: "steer-into-idle".into(),
                    op: active_user_op(
                        "answer this after idle compaction",
                        idle_turn.as_ref(),
                        ActiveMessageDelivery::Steer,
                    ),
                })
                .expect("steer");
            assert_eq!(
                admission.wait().await.expect("admission"),
                super::super::MessageAcceptance::Accepted
            );
            model.release.notify_one();
            let mut rejected = false;
            let mut promoted_starts = 0;
            let mut accepted_message = false;
            loop {
                let event = agent.next_event().await.expect("event");
                match event.msg {
                    EventMsg::TurnStarted(turn) if !turn.idle => {
                        assert_eq!(turn.turn_id, idle_turn);
                        assert_eq!(event.submission_id.as_deref(), Some("steer-into-idle"));
                        assert!(!accepted_message, "promotion precedes the accepted message");
                        promoted_starts += 1;
                    }
                    EventMsg::Message(message)
                        if message.text == "answer this after idle compaction" =>
                    {
                        assert_eq!(message.delivery, MessageDelivery::Steer);
                        assert_eq!(
                            promoted_starts, 1,
                            "the accepted steer promotes its idle turn once"
                        );
                        accepted_message = true;
                    }
                    EventMsg::TurnComplete(turn) => {
                        assert_eq!(turn.turn_id, idle_turn, "the idle turn answers the steer");
                        assert_eq!(turn.idle, reject);
                        break;
                    }
                    EventMsg::TurnAborted(turn) => panic!("idle turn aborted: {}", turn.reason),
                    EventMsg::SubmissionRejected(_) => rejected = true,
                    _ => {}
                }
            }
            assert_eq!(rejected, reject);
            assert_eq!(promoted_starts, usize::from(!reject));
            assert_eq!(accepted_message, !reject);
            assert_eq!(
                model.responses.load(Ordering::SeqCst),
                if reject { 1 } else { 2 }
            );
            assert_eq!(
                compaction_count(checkpoints.as_ref(), "idle-steer").await,
                1
            );
            let saved = checkpoints
                .load(&"idle-steer".parse().expect("session ID"))
                .await
                .expect("load")
                .expect("checkpoint");
            assert!(saved.active_execution.is_none());
            assert!(saved.pending_messages.is_empty());
            assert_eq!(
                saved.context.iter().any(|item| {
                    item.to_string()
                        .contains("answer this after idle compaction")
                }),
                !reject
            );
            if !reject {
                model.entered.notified().await;
                model.release.notify_one();
                loop {
                    match agent.next_event().await.expect("event").msg {
                        EventMsg::TurnComplete(_) => break,
                        EventMsg::TurnAborted(turn) => panic!("idle turn aborted: {}", turn.reason),
                        _ => {}
                    }
                }
                assert_eq!(
                    compaction_count(checkpoints.as_ref(), "idle-steer").await,
                    2
                );
                assert_eq!(model.responses.load(Ordering::SeqCst), 2);
            }
            assert!(!agent.frontend().has_background_work().await.expect("idle"));
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(250), agent.next_event())
                    .await
                    .is_err(),
                "the new idle period also fires once"
            );
        })
        .await
        .expect("steered idle turn completed");
    }
}

#[tokio::test]
async fn failed_idle_compaction_disarms_without_losing_the_chat() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let workspace = tempfile::tempdir().expect("workspace");
        let checkpoints = Arc::new(
            SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
                .expect("checkpoint store"),
        );
        let model = Arc::new(ScriptedModel {
            outputs: Mutex::new(VecDeque::from([scripted_message("done")])),
            tool_counts: Mutex::default(),
            inputs: Mutex::default(),
        });
        let compaction = Compaction::new(10_000_000)
            .expect("compaction middleware")
            .idle_compaction(Some(std::time::Duration::from_millis(50)), 1)
            .expect("idle compaction");
        let mut agent = create_agent(
            AgentConfig::new(
                Arc::new(ModelRouter::new(
                    "main",
                    Arc::<ScriptedModel>::clone(&model),
                )),
                Arc::new(Sandbox::new(
                    Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
                    ApprovalPolicy::Ask,
                )),
                Arc::<SqliteCheckpoint>::clone(&checkpoints),
                test_middleware(vec![Arc::new(compaction)]),
                "test prompt",
            )
            .session_context(test_session_context())
            .session_id("idle-failure".parse().expect("session ID")),
        )
        .await
        .expect("create agent");
        agent
            .sender()
            .submit(user_op("keep this request"))
            .expect("submit");
        while !matches!(
            agent.next_event().await.expect("event").msg,
            EventMsg::TurnComplete(_)
        ) {}
        let before = checkpoints
            .load(&"idle-failure".parse().expect("session ID"))
            .await
            .expect("load")
            .expect("checkpoint");
        while !matches!(
            agent.next_event().await.expect("event").msg,
            EventMsg::TurnAborted(_)
        ) {}
        let saved = checkpoints
            .load(&"idle-failure".parse().expect("session ID"))
            .await
            .expect("load")
            .expect("checkpoint");
        assert_eq!(saved.context, before.context);
        assert_eq!(saved.context_epoch, before.context_epoch);
        assert!(saved.active_execution.is_none());
        assert!(!agent.frontend().has_background_work().await.expect("idle"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), agent.next_event())
                .await
                .is_err(),
            "a failed idle compaction must not retry forever"
        );
        assert_eq!(model.inputs.lock().expect("inputs").len(), 2);
    })
    .await
    .expect("failed idle turn completed");
}
