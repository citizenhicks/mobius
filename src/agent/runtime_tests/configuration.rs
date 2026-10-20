//! Configuration agent runtime tests.

use super::*;

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
