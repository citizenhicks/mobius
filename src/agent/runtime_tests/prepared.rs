use super::*;
use crate::agent::prepare_agent;
use crate::backend::checkpoint::{
    ActiveExecution, ActiveModelStep, ExecutionPhase, PendingApproval,
};

#[derive(Default)]
struct LifecycleProbe {
    starts: AtomicUsize,
    ends: AtomicUsize,
    turns: AtomicUsize,
    tools: AtomicUsize,
}

impl Middleware for LifecycleProbe {
    fn name(&self) -> &'static str {
        "lifecycle_probe"
    }

    fn session_start<'a>(
        &'a self,
        _context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn session_end<'a>(&'a self, _runtime: &'a RuntimeContext) -> BoxFuture<'a, Result<()>> {
        self.ends.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn turn_end<'a>(
        &'a self,
        _context: &'a mut crate::middleware::TurnEndContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        self.turns.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn pre_tool_use<'a>(
        &'a self,
        _context: &'a mut PreToolUseContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        self.tools.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn prepared_cancellation_drains_restored_work_without_starting_execution() {
    for kind in ["queue", "approval", "model"] {
        let workspace = tempfile::tempdir().expect("workspace");
        let checkpoints = Arc::new(
            SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
                .expect("checkpoint store"),
        );
        let mut checkpoint = Checkpoint::empty(kind);
        checkpoint.model_route = Some("test".into());
        if kind != "queue" {
            checkpoint.active_execution = Some(ActiveExecution {
                submission_id: "original-submission".into(),
                turn_id: "original-turn".into(),
                started_at_ms: 1,
                model_calls: 1,
                tool_calls: 0,
                failed_tool_calls: 0,
                usage: TokenUsage::default(),
                next_model_step: 1,
                stop_hook_active: false,
                phase: ExecutionPhase::Model,
            });
        }
        if kind == "approval" {
            let call = ToolCall {
                call_id: "saved-call".into(),
                name: "approval_test".into(),
                arguments: serde_json::json!({}),
            };
            checkpoint.context.push(serde_json::json!({
                "type": "function_call", "call_id": call.call_id,
                "name": call.name, "arguments": "{}"
            }));
            checkpoint.pending_tools.push(call.clone());
            checkpoint.pending_approval = Some(PendingApproval {
                submission_id: "original-submission".into(),
                turn_id: "original-turn".into(),
                request_id: "saved-approval".into(),
                approval_call_ids: vec![call.call_id.clone()],
                authorized_call_ids: Vec::new(),
                calls: vec![call],
                reason: "requires approval".into(),
                sandbox_mode: Default::default(),
                network_access: Default::default(),
                decision_received: false,
            });
        } else if kind == "model" {
            checkpoint.active_model_step = Some(ActiveModelStep {
                model_step_id: "saved-step".into(),
                step_index: 0,
                started_at_ms: 1,
            });
        }
        // More terminal events than the bounded event channel can hold.
        for index in 0..300 {
            checkpoint.pending_messages.push(queued_user_message(
                &format!("queued-{index}"),
                "must not run",
                match index % 3 {
                    0 if kind != "queue" => QueuedMessageBoundary::Steer {
                        turn_id: "original-turn".into(),
                    },
                    1 => QueuedMessageBoundary::Queue,
                    _ => QueuedMessageBoundary::Turn,
                },
            ));
        }
        checkpoints
            .save(&checkpoint, &checkpoint.context, None)
            .await
            .expect("save");
        let model = Arc::new(NativeCompactionModel::default());
        let probe = Arc::new(LifecycleProbe::default());
        let make_config = || {
            let mut config = config_with_model(
                workspace.path(),
                checkpoints.clone(),
                kind,
                "test",
                model.clone(),
            );
            config.middleware = test_middleware(vec![probe.clone()]);
            config
        };
        let prepared = prepare_agent(make_config()).await.expect("prepare");
        assert_eq!(probe.starts.load(Ordering::SeqCst), 0);
        assert_eq!(probe.turns.load(Ordering::SeqCst), 0);
        assert_eq!(
            checkpoints.load(kind).await.expect("load").expect("saved"),
            checkpoint
        );
        let mut cancelled = prepared.cancel();
        assert!(matches!(
            cancelled.sender().submit(user_op("too late")),
            Err(Error::Stopped(_))
        ));
        let events = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut events = Vec::new();
            while let Some(event) = cancelled.next_event().await {
                assert!(!matches!(event.msg, EventMsg::Error(_)), "{event:?}");
                events.push(event);
            }
            events
        })
        .await
        .expect("cancellation completes while drained");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.msg, EventMsg::SubmissionRejected(_)))
                .map(|event| event
                    .submission_id
                    .clone()
                    .expect("queued rejection identity"))
                .collect::<std::collections::BTreeSet<_>>(),
            (0..300).map(|index| format!("queued-{index}")).collect()
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.msg, EventMsg::TurnStarted(_)))
        );
        let aborted: Vec<_> = events
            .iter()
            .filter(|event| matches!(event.msg, EventMsg::TurnAborted(_)))
            .collect();
        assert_eq!(aborted.len(), usize::from(kind != "queue"));
        if let Some(event) = aborted.first() {
            assert_eq!(event.submission_id.as_deref(), Some("original-submission"));
            assert!(
                matches!(&event.msg, EventMsg::TurnAborted(aborted) if aborted.turn_id == "original-turn")
            );
        }
        if kind == "approval" {
            assert!(
                events
                    .iter()
                    .any(|event| matches!(&event.msg, EventMsg::ToolCallEnd(call)
                if call.call_id == "saved-call" && call.is_error)
                        && event.submission_id.as_deref() == Some("original-submission"))
            );
        }
        let saved = checkpoints.load(kind).await.expect("load").expect("saved");
        assert!(saved.active_execution.is_none());
        assert!(saved.active_model_step.is_none());
        assert!(saved.pending_approval.is_none());
        assert!(saved.pending_tools.is_empty());
        assert!(saved.pending_messages.is_empty());
        assert_eq!(
            probe.turns.load(Ordering::SeqCst),
            usize::from(kind != "queue")
        );
        assert_eq!(probe.starts.load(Ordering::SeqCst), 0);
        assert_eq!(probe.ends.load(Ordering::SeqCst), 0);
        assert_eq!(probe.tools.load(Ordering::SeqCst), 0);
        assert_eq!(model.responses.load(Ordering::SeqCst), 0);
        let restarted = prepare_agent(make_config())
            .await
            .expect("prepare again")
            .start()
            .await
            .expect("start idle checkpoint");
        assert_eq!(probe.starts.load(Ordering::SeqCst), 1);
        let (sender, mut events) = restarted.into_parts();
        drop(sender);
        while events.recv().await.is_some() {}
        assert_eq!(model.responses.load(Ordering::SeqCst), 0);
        assert_eq!(probe.ends.load(Ordering::SeqCst), 1);
    }
}
