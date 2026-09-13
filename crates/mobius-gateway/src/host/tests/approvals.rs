use super::group_delivery::{pause_deliveries, seed_active};
use super::group_management::gateway_with_group;
use super::*;
use mobius::backend::checkpoint;
use mobius::backend::model::ToolCall;
use std::time::Duration;

struct DelayedApprovalStore {
    inner: Arc<dyn CheckpointStore>,
    request_entered: tokio::sync::Notify,
    release_request: tokio::sync::Notify,
    decision_entered: tokio::sync::Notify,
    release_decision: tokio::sync::Notify,
}

impl CheckpointStore for DelayedApprovalStore {
    fn load<'a>(
        &'a self,
        id: &'a str,
    ) -> mobius::BoxFuture<'a, mobius::Result<Option<Checkpoint>>> {
        self.inner.load(id)
    }

    fn delete_sessions<'a>(
        &'a self,
        ids: &'a [String],
    ) -> mobius::BoxFuture<'a, mobius::Result<bool>> {
        self.inner.delete_sessions(ids)
    }

    fn save<'a>(
        &'a self,
        checkpoint: &'a Checkpoint,
        transcript: &'a [serde_json::Value],
        execution: Option<&'a ExecutionRecord>,
    ) -> mobius::BoxFuture<'a, mobius::Result<()>> {
        self.inner.save(checkpoint, transcript, execution)
    }

    fn save_with_events<'a>(
        &'a self,
        checkpoint: Checkpoint,
        transcript: Vec<serde_json::Value>,
        execution: Option<ExecutionRecord>,
        events: Vec<checkpoint::TimestampedEvent>,
    ) -> mobius::BoxFuture<'a, mobius::Result<Vec<JournalEvent>>> {
        Box::pin(async move {
            if checkpoint
                .pending_approval
                .as_ref()
                .is_some_and(|pending| pending.decision_received)
            {
                self.decision_entered.notify_one();
                self.release_decision.notified().await;
            }
            self.inner
                .save_with_events(checkpoint, transcript, execution, events)
                .await
        })
    }

    fn append_event<'a>(
        &'a self,
        id: &'a str,
        at: i64,
        event: &'a Event,
    ) -> mobius::BoxFuture<'a, mobius::Result<JournalEvent>> {
        Box::pin(async move {
            if matches!(event.msg, EventMsg::ExecApprovalRequest(_)) {
                self.request_entered.notify_one();
                self.release_request.notified().await;
            }
            self.inner.append_event(id, at, event).await
        })
    }

    fn event_page<'a>(
        &'a self,
        id: &'a str,
        request: EventPageRequest,
    ) -> mobius::BoxFuture<'a, mobius::Result<checkpoint::EventPage>> {
        self.inner.event_page(id, request)
    }

    fn transcript_page<'a>(
        &'a self,
        id: &'a str,
        request: checkpoint::TranscriptPageRequest,
    ) -> mobius::BoxFuture<'a, mobius::Result<checkpoint::TranscriptPage>> {
        self.inner.transcript_page(id, request)
    }

    fn session_summary<'a>(
        &'a self,
        id: &'a str,
    ) -> mobius::BoxFuture<'a, mobius::Result<Option<checkpoint::SessionSummary>>> {
        self.inner.session_summary(id)
    }

    fn load_state<'a>(
        &'a self,
        scope: &'a str,
        key: &'a str,
    ) -> mobius::BoxFuture<'a, mobius::Result<Option<serde_json::Value>>> {
        self.inner.load_state(scope, key)
    }

    fn save_state<'a>(
        &'a self,
        scope: &'a str,
        key: &'a str,
        value: &'a serde_json::Value,
    ) -> mobius::BoxFuture<'a, mobius::Result<()>> {
        self.inner.save_state(scope, key, value)
    }
}

#[tokio::test]
async fn delayed_request_event_does_not_reopen_an_accepted_approval() {
    let (root, gateway, bot) = bots::gateway_with_bot().await;
    pause_deliveries(&gateway).await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let chat = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let execution = seed_active(&gateway, &chat, &bot, "waiting", true).await;
    let store = {
        let mut state = gateway.state.lock().await;
        let store = Arc::new(DelayedApprovalStore {
            inner: Arc::clone(&state.checkpoints),
            request_entered: Default::default(),
            release_request: Default::default(),
            decision_entered: Default::default(),
            release_decision: Default::default(),
        });
        state.checkpoints = store.clone();
        store
    };
    let actor = tokio::time::timeout(Duration::from_secs(5), chat.participant(bot.id))
        .await
        .unwrap()
        .unwrap();
    let mut events = actor.subscribe();
    tokio::time::timeout(Duration::from_secs(5), store.request_entered.notified())
        .await
        .unwrap();
    let review = |id: &str| Submission {
        id: id.into(),
        op: Op::ExecApproval {
            id: "waiting-approval".into(),
            decision: ReviewDecision::Abort,
        },
    };
    actor.submit(review("first-client")).await.unwrap();
    store.release_request.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerMessage::AgentEvent { record, .. } = events.recv().await.unwrap().message
                && matches!(record.event.msg, EventMsg::ExecApprovalRequest(_))
            {
                break;
            }
        }
        store.decision_entered.notified().await;
    })
    .await
    .expect("the duplicate event is observed before the accepted decision commits");
    assert!(
        !store
            .load(&execution)
            .await
            .unwrap()
            .unwrap()
            .pending_approval
            .unwrap()
            .decision_received
    );
    let second = actor.submit(review("second-client")).await;
    store.release_decision.notify_one();
    assert_eq!(second.unwrap_err().code, "stale_approval");
    chat.stop().await.unwrap();
    gateway.shutdown().await;
}

async fn restore_approvals(gateway: &GatewayHost) {
    let state = gateway.state.lock().await;
    catalog::restore_pending_approval_activities(
        &state.checkpoints,
        &state.chat_store,
        &state.bots,
        &state.activities,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn private_actor_accepts_only_one_concurrent_decision_for_its_current_approval() {
    let (root, gateway, bot) = bots::gateway_with_bot().await;
    pause_deliveries(&gateway).await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let chat = gateway.create_session(&workspace, &bot.id).await.unwrap();
    seed_active(&gateway, &chat, &bot, "waiting", true).await;
    let actor = chat.participant(bot.id).await.unwrap();
    let review = |id: &str, request_id: &str| Submission {
        id: id.into(),
        op: Op::ExecApproval {
            id: request_id.into(),
            decision: ReviewDecision::Abort,
        },
    };
    assert_eq!(
        actor
            .submit(review("wrong-token", "not-current"))
            .await
            .unwrap_err()
            .code,
        "stale_approval"
    );
    // Both clients have already resolved the opaque token to the same private actor.
    let (first, second) = tokio::join!(
        actor.submit(review("first-client", "waiting-approval")),
        actor.submit(review("second-client", "waiting-approval")),
    );
    let decisions = [first, second];
    assert_eq!(decisions.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        decisions
            .into_iter()
            .find_map(|result| result.err())
            .unwrap()
            .code,
        "stale_approval"
    );
    assert_eq!(
        actor
            .submit(review("later-client", "waiting-approval"))
            .await
            .unwrap_err()
            .code,
        "stale_approval"
    );
    chat.stop().await.unwrap();
    gateway.shutdown().await;
}

#[tokio::test]
async fn opaque_approval_targets_its_chat_member_and_rejects_a_later_review() {
    let (_root, gateway, _workspace, chat, bots) = gateway_with_group().await;
    pause_deliveries(&gateway).await;
    let first = seed_active(&gateway, &chat, &bots[0], "first", true).await;
    let second = seed_active(&gateway, &chat, &bots[1], "second", true).await;
    let (checkpoints, activities) = {
        let state = gateway.state.lock().await;
        let mut checkpoint = state.checkpoints.load(&first).await.unwrap().unwrap();
        let pending = checkpoint.pending_approval.as_mut().unwrap();
        pending.request_id = "opaque-review-token".into();
        pending.approval_call_ids = vec!["write-call".into()];
        pending.calls = vec![ToolCall {
            call_id: "write-call".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "notes.txt", "content": "draft"}),
        }];
        checkpoint.sequence += 1;
        state
            .checkpoints
            .save(&checkpoint, &[], None)
            .await
            .unwrap();
        catalog::restore_pending_approval_activities(
            &state.checkpoints,
            &state.chat_store,
            &state.bots,
            &state.activities,
        )
        .await
        .unwrap();
        (
            Arc::clone(&state.checkpoints),
            Arc::clone(&state.activities),
        )
    };
    let approvals = gateway.ready().await.unwrap().background_approvals;
    assert_eq!(approvals.len(), 2);
    let approval = approvals
        .iter()
        .find(|approval| approval.request.id == "opaque-review-token")
        .unwrap();
    assert_eq!(approval.chat_id.as_deref(), Some(chat.session_id()));
    assert_eq!(approval.bot_id, bots[0].id);
    assert_eq!(approval.request.turn_id, "first-turn");
    assert_eq!(approval.request.reason, "Approve work");
    assert_eq!(approval.request.calls[0].name, "write_file");
    assert_eq!(approval.request.calls[0].arguments["path"], "notes.txt");
    let public_payload = serde_json::to_string(&approvals).unwrap();
    assert!(!public_payload.contains(&first));
    assert!(!public_payload.contains(&second));
    assert_eq!(
        gateway
            .review_approval(&first, ReviewDecision::Abort)
            .await
            .unwrap_err()
            .code,
        "stale_approval"
    );

    let another_client = gateway.clone();
    gateway
        .review_approval("opaque-review-token", ReviewDecision::Abort)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let checkpoint = checkpoints.load(&first).await.unwrap().unwrap();
            if checkpoint.pending_approval.is_none() && checkpoint.active_execution.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the opaque request aborts only its addressed execution");
    assert_eq!(
        checkpoints
            .load(&second)
            .await
            .unwrap()
            .unwrap()
            .pending_approval
            .unwrap()
            .request_id,
        "second-approval"
    );
    assert!(checkpoints.load(chat.session_id()).await.unwrap().is_none());
    // The durable decision wins even while another client still has the old catalog entry.
    activities
        .lock()
        .await
        .approvals
        .insert(first, approval.clone());
    assert_eq!(
        another_client
            .review_approval("opaque-review-token", ReviewDecision::Approved)
            .await
            .unwrap_err()
            .code,
        "stale_approval"
    );
    chat.stop().await.unwrap();
    gateway.shutdown().await;
}

#[tokio::test]
async fn stopping_chat_clears_background_approval_and_invalidates_its_request() {
    let (root, gateway, bot) = bots::gateway_with_bot().await;
    pause_deliveries(&gateway).await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let chat = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let execution = seed_active(&gateway, &chat, &bot, "waiting", true).await;
    restore_approvals(&gateway).await;
    let ready = chat.snapshot(None).await.unwrap().ready;
    assert_eq!(ready.pending_approvals.len(), 1);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !gateway
                .ready()
                .await
                .unwrap()
                .background_approvals
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the pending request appears in the background catalog");
    chat.stop().await.unwrap();
    assert!(
        gateway
            .ready()
            .await
            .unwrap()
            .background_approvals
            .is_empty()
    );
    let checkpoint = gateway
        .state
        .lock()
        .await
        .checkpoints
        .load(&execution)
        .await
        .unwrap()
        .unwrap();
    assert!(checkpoint.pending_approval.is_none());
    assert!(checkpoint.active_execution.is_none());
    assert_eq!(
        gateway
            .review_approval("waiting-approval", ReviewDecision::Approved)
            .await
            .unwrap_err()
            .code,
        "stale_approval"
    );
    gateway.shutdown().await;
}
