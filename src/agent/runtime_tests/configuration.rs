//! Configuration agent runtime tests.

use super::*;
use crate::backend::checkpoint::ContextRewriteReason;

#[tokio::test]
async fn auxiliary_usage_preserves_the_last_foreground_context() {
    struct AuxiliaryUsage(AtomicUsize);
    impl Middleware for AuxiliaryUsage {
        fn name(&self) -> &'static str {
            "auxiliary_usage"
        }

        fn pre_model<'a>(&'a self, context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                if self.0.fetch_add(1, Ordering::SeqCst) == 2 {
                    context.rewrite_input(
                        ContextRewriteReason::Compaction,
                        [crate::backend::model::internal_user_message(
                            "compaction",
                            "fresh context",
                        )],
                    )?;
                }
                context.record_usage(
                    context.provider,
                    TokenUsage {
                        input_tokens: 10_000,
                        total_tokens: 10_000,
                        ..TokenUsage::default()
                    },
                );
                Ok(())
            })
        }
    }

    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3")).expect("checkpoints"),
    );
    let mut output = scripted_message("done");
    let usage = TokenUsage {
        input_tokens: 1_000,
        total_tokens: 1_000,
        ..TokenUsage::default()
    };
    output.usage = usage;
    let mut config = config_with_model(
        workspace.path(),
        checkpoints,
        "auxiliary-context",
        "test",
        Arc::new(ScriptedModel {
            outputs: Mutex::new(VecDeque::from([output.clone(), output.clone(), output])),
            tool_counts: Mutex::new(Vec::new()),
            inputs: Mutex::new(Vec::new()),
        }),
    );
    config.middleware = Arc::new(test_middleware(vec![Arc::new(AuxiliaryUsage(
        AtomicUsize::new(0),
    ))]));
    let mut agent = create_agent(config).await.expect("agent");
    let mut foreground_context = None;
    for completed_turns in 0..3_i64 {
        if completed_turns == 2 {
            foreground_context = None;
        }
        agent.sender().submit(user_op("next")).expect("submit");
        let mut counts = Vec::new();
        loop {
            match agent.next_event().await.expect("event").msg {
                EventMsg::TokenCount(event) => counts.push(event.info.expect("usage")),
                EventMsg::TurnComplete(_) => break,
                EventMsg::TurnAborted(event) => panic!("unexpected abort: {event:?}"),
                _ => {}
            }
        }
        assert_eq!(counts.len(), 2);
        assert_eq!(
            counts[0].last_token_usage.input_tokens,
            i64::from(completed_turns == 1) * 1_000
        );
        assert_eq!(counts[0].context, foreground_context);
        assert_eq!(
            counts[0].total_token_usage.total_tokens,
            completed_turns * 11_000 + 10_000
        );
        assert_eq!(counts[1].last_token_usage, usage);
        assert!(counts[1].context.is_some(), "foreground request breakdown");
        assert_eq!(
            counts[1].total_token_usage.total_tokens,
            (completed_turns + 1) * 11_000
        );
        foreground_context = counts[1].context.clone();
    }

    for text in ["failed context ".repeat(1_000), "next failure".into()] {
        agent.sender().submit(user_op(text)).expect("submit");
        let mut counts = Vec::new();
        loop {
            match agent.next_event().await.expect("event").msg {
                EventMsg::TokenCount(event) => counts.push(event.info.expect("usage")),
                EventMsg::TurnAborted(_) => break,
                EventMsg::TurnComplete(_) => panic!("exhausted model should fail"),
                _ => {}
            }
        }
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0].last_token_usage.input_tokens, 1_000);
        assert_eq!(counts[0].context, foreground_context);
    }
}

#[tokio::test]
async fn completed_pre_model_usage_is_settled_when_a_later_hook_rejects_preparation() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::<SqliteCheckpoint>::clone(&checkpoints);
    let observed_usage = Arc::new(Mutex::new(Vec::new()));
    let usage_observer = Arc::clone(&observed_usage);
    let config = AgentConfig::new(
        Arc::new(ModelRouter::new("main", Arc::new(TestModel))),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoint_store,
        test_middleware(vec![
            Arc::new(DurableBeforeModel),
            Arc::new(FailingBeforeModel),
        ]),
        "test prompt",
    )
    .session_context(test_session_context())
    .session_id("settled-hooks".parse().expect("session ID"))
    .usage_observer(move |route, usage| {
        usage_observer
            .lock()
            .expect("usage observer lock")
            .push((route.to_owned(), usage.total_tokens));
        Box::pin(async { Ok(()) })
    });
    let mut agent = create_agent(config).await.expect("create agent");
    agent.next_event().await.expect("configured event");
    agent.next_event().await.expect("sandbox widget");
    agent
        .sender()
        .submit(user_op("hello"))
        .expect("submit input");
    let mut saw_effect = false;
    loop {
        match agent.next_event().await.expect("agent event").msg {
            EventMsg::ContextCompacted => saw_effect = true,
            EventMsg::TurnAborted(_) => break,
            _ => {}
        }
    }
    let saved = checkpoints
        .load(&"settled-hooks".parse().expect("session ID"))
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    let execution = checkpoints
        .execution_page(
            &"settled-hooks".parse().expect("session ID"),
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

    assert!(!saw_effect);
    assert_eq!(
        observed_usage
            .lock()
            .expect("observed usage lock")
            .as_slice(),
        [("main".into(), 1)]
    );
    assert_eq!(saved.total_usage.total_tokens, 1);
    assert_eq!(
        (
            execution.outcome,
            execution.model_calls,
            execution.usage.total_tokens,
            saved.execution_stats.failed_run_count,
        ),
        (ExecutionOutcome::Failed, 1, 1, 1)
    );
    assert!(
        !saved
            .context
            .iter()
            .any(|item| internal_message_kind(item) == Some("settled"))
    );
    assert_eq!(saved.context_epoch, 0);
    assert!(saved.context.iter().any(|item| {
        crate::protocol::message_metadata(item).is_some_and(|message| message.text == "hello")
    }));
}

#[tokio::test]
async fn retained_agent_closes_its_event_stream_after_a_fatal_routing_error() {
    struct FailingMessages;
    impl Middleware for FailingMessages {
        fn name(&self) -> &'static str {
            "failing_messages"
        }
        fn handles_messages(&self) -> bool {
            true
        }
        fn route_message<'a>(
            &'a self,
            _context: &'a mut crate::middleware::MessageRouteContext<'_>,
        ) -> crate::BoxFuture<'a, Result<crate::middleware::SubmissionResult>> {
            Box::pin(async { Err(Error::Stopped("message routing failed".into())) })
        }
    }
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3")).expect("checkpoints"),
    );
    let mut config = config(workspace.path(), checkpoints, "retained-agent");
    config.middleware =
        Arc::new(MiddlewareStack::new(vec![Arc::new(FailingMessages)]).expect("middleware"));
    let mut agent = create_agent(config).await.expect("agent");
    let frontend = agent.frontend_sink();
    agent
        .sender()
        .submit(user_op("trigger error"))
        .expect("submit");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(event) = agent.next_event().await {
            if matches!(event.msg, EventMsg::Error(_)) {
                break;
            }
        }
    })
    .await
    .expect("runner reports failure");
    assert!(agent.sender().submit(user_op("runner is gone")).is_err());
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), agent.next_event())
            .await
            .expect("retaining Agent must not retain the recorder")
            .is_none()
    );
    assert!(
        frontend(FrontendEvent::RemoveWidget {
            capability: "test".into(),
            id: "test".into()
        })
        .is_err()
    );
}
