use super::group_management::{gateway_with_group, message};
use super::*;
use crate::groups::participant_session_id;
use std::time::Duration;

#[tokio::test]
async fn stale_acknowledgement_does_not_clear_a_newer_delivery_attempt() {
    let root = tempfile::tempdir().expect("root");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    let mut attempts = HashMap::from([(
        "target-bot".into(),
        GroupDeliveryAttempt::Submitted("new-message".into()),
    )]);
    gateway
        .handle_group_delivery(
            GroupDelivery::Acknowledged {
                target_bot_id: "target-bot".into(),
                message_id: "old-message".into(),
            },
            &mut attempts,
        )
        .await;

    assert_eq!(
        attempts.get("target-bot"),
        Some(&GroupDeliveryAttempt::Submitted("new-message".into()))
    );
}

#[tokio::test]
async fn rejected_delivery_waits_for_capacity_or_an_explicit_retry() {
    let root = tempfile::tempdir().expect("root");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    let mut attempts = HashMap::from([(
        "target-bot".into(),
        GroupDeliveryAttempt::Submitted("message-1".into()),
    )]);
    gateway
        .handle_group_delivery(
            GroupDelivery::Rejected {
                target_bot_id: "target-bot".into(),
                message_id: "message-1".into(),
            },
            &mut attempts,
        )
        .await;
    assert_eq!(
        attempts.get("target-bot"),
        Some(&GroupDeliveryAttempt::Rejected("message-1".into()))
    );

    gateway
        .handle_group_delivery(
            GroupDelivery::Pending {
                target_bot_id: "target-bot".into(),
            },
            &mut attempts,
        )
        .await;
    assert!(matches!(
        attempts.get("target-bot"),
        Some(GroupDeliveryAttempt::Rejected(message_id)) if message_id == "message-1"
    ));

    gateway
        .handle_group_delivery(
            GroupDelivery::CapacityAvailable {
                target_bot_id: "target-bot".into(),
            },
            &mut attempts,
        )
        .await;
    assert!(!attempts.contains_key("target-bot"));
    attempts.insert(
        "target-bot".into(),
        GroupDeliveryAttempt::Rejected("message-1".into()),
    );
    attempts.insert(
        "running-bot".into(),
        GroupDeliveryAttempt::Submitted("message-2".into()),
    );
    gateway
        .handle_group_delivery(GroupDelivery::RetryPending, &mut attempts)
        .await;
    assert!(!attempts.contains_key("target-bot"));
    assert_eq!(
        attempts.get("running-bot"),
        Some(&GroupDeliveryAttempt::Submitted("message-2".into()))
    );
    gateway.shutdown().await;
}

#[tokio::test]
async fn gateway_busy_delivery_waits_for_mutation_completion_before_retrying() {
    let root = tempfile::tempdir().expect("root");
    let bots = Arc::new(BotStore::open(root.path()).expect("Bots"));
    let (group, mut deliveries) = GroupStore::new(root.path(), bots).unwrap();
    let group = Arc::new(group);
    let session_mutations = Arc::new(RwLock::new(()));
    let mutation = Arc::clone(&session_mutations).write_owned().await;
    let retry = notify_group_delivery_after_mutation(
        Arc::clone(&session_mutations),
        Arc::clone(&group),
        "target-bot".into(),
    );
    tokio::pin!(retry);
    tokio::select! {
        biased;
        () = &mut retry => panic!("delivery retried before the mutation completed"),
        () = std::future::ready(()) => {}
    }
    assert!(matches!(
        deliveries.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    drop(mutation);
    retry.await;

    assert!(
        matches!(deliveries.recv().await, Some(GroupDelivery::CapacityAvailable { target_bot_id }) if target_bot_id == "target-bot")
    );
}

#[tokio::test]
async fn mentions_use_private_sessions_in_the_addressed_chat_workspace() {
    let (root, gateway, workspace, group, bots) = gateway_with_group().await;
    let unrelated_workspace = root.path().join("unrelated");
    let second_workspace = root.path().join("second");
    std::fs::create_dir(&unrelated_workspace).unwrap();
    std::fs::create_dir(&second_workspace).unwrap();
    let unrelated = gateway
        .create_session(&unrelated_workspace, &bots[0].id)
        .await
        .unwrap();
    let overlapping = gateway
        .create_chat(
            &second_workspace,
            &bots.iter().map(|bot| bot.id.clone()).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let groups = Arc::clone(&gateway.state.lock().await.group);
    let session_id = participant_session_id(group.session_id(), &bots[0].id);
    let other_id = participant_session_id(overlapping.session_id(), &bots[0].id);
    for (chat, expected_workspace, private_id, post_id) in [
        (&group, &workspace, &session_id, "first"),
        (&overlapping, &second_workspace, &other_id, "second"),
    ] {
        chat.submit(message(
            post_id,
            format!("@{} review this project", bots[0].handle),
        ))
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let page = groups.event_page(chat.session_id(), EventPageRequest { before_sequence: None, limit: 100 }).await.unwrap();
                if page.events.iter().any(|event| matches!(&event.event.msg, EventMsg::Message(message) if matches!(&message.author, MessageAuthor::Peer { session_id, handle, .. } if session_id == private_id && handle == &bots[0].handle))) { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("Bot reply should be posted to the originating group");
        let checkpoint = checkpoints.load(private_id).await.unwrap().unwrap();
        assert_eq!(checkpoint.session_context.bot_id, bots[0].id);
        assert!(!checkpoint.catalog_visible);
        let state = gateway.state.lock().await;
        let spec = ChatSpec::from_metadata(
            &checkpoint.metadata,
            &state.bots,
            state.store.state_dir(),
            None,
        )
        .unwrap();
        assert_eq!(spec.workspace, expected_workspace.canonicalize().unwrap());
        assert!(spec.attached_folders.is_empty());
        drop(state);
        assert!(
            checkpoints
                .load(&participant_session_id(chat.session_id(), &bots[1].id))
                .await
                .unwrap()
                .is_none()
        );
    }
    let agent_sessions = gateway_session_summaries(&checkpoints).await.unwrap();
    assert_eq!(agent_sessions.len(), 3);
    assert!(
        agent_sessions
            .iter()
            .all(|session| session.session_context.bot_id == bots[0].id)
    );
    assert!(
        checkpoints
            .load(group.session_id())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        checkpoints
            .load(overlapping.session_id())
            .await
            .unwrap()
            .is_none()
    );
    assert_ne!(session_id, other_id);
    let original = checkpoints
        .event_page(
            unrelated.session_id(),
            EventPageRequest {
                before_sequence: None,
                limit: 100,
            },
        )
        .await
        .unwrap();
    assert!(
        !original
            .events
            .iter()
            .any(|event| matches!(event.event.msg, EventMsg::Message(_)))
    );
    assert_eq!(gateway.sessions().await.unwrap().len(), 3);
    gateway.shutdown().await;
}

#[tokio::test]
async fn shutdown_stops_group_delivery_before_later_notifications() {
    let (_root, gateway, _workspace, chat, bots) = gateway_with_group().await;
    let groups = Arc::clone(&gateway.state.lock().await.group);
    gateway.shutdown().await;
    let Op::Message { message } = message("late", format!("@{} after shutdown", bots[0].handle)).op
    else {
        unreachable!()
    };
    groups
        .post_user(chat.session_id(), "late".into(), message)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(gateway.state.lock().await.sessions.is_empty());
}

#[tokio::test]
async fn restart_reopens_pending_approval_without_resubmitting_the_group_message() {
    let (root, gateway, workspace, group, bots) = gateway_with_group().await;
    let chat_id = group.session_id().to_owned();
    gateway.shutdown().await;
    drop(group);
    drop(gateway);
    let (store, config) = ConfigStore::open(root.path().join("state")).unwrap();
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(store.checkpoints_path()).unwrap());
    let bot_store = Arc::new(BotStore::open(store.state_dir()).unwrap());
    let (groups, _) = GroupStore::new(store.state_dir(), bot_store).unwrap();
    let Op::Message { message: input } =
        message("replayed-message", format!("@{} resume", bots[0].handle)).op
    else {
        unreachable!()
    };
    groups
        .post_user(&chat_id, "replayed-message".into(), input.clone())
        .await
        .unwrap();
    let claim = groups
        .claim_next_delivery(&bots[0].id)
        .await
        .unwrap()
        .unwrap();
    let session_id = claim.session_id().to_owned();
    drop(claim);
    let approval_id = Uuid::new_v4().to_string();
    let mut checkpoint = Checkpoint::empty(&session_id);
    checkpoint.catalog_visible = false;
    let spec = ChatSpec::for_bot(&workspace, &bots[0], store.state_dir(), None).unwrap();
    checkpoint.metadata = spec.metadata().unwrap();
    checkpoint.session_context.bot_id = bots[0].id.clone();
    checkpoint.session_context.workspace_id = Some(spec.workspace_info().id);
    checkpoint.session_context.workspace_label =
        Some(spec.workspace_info().path.display().to_string());
    checkpoint.active_execution = Some(ActiveExecution {
        submission_id: "replayed-message".into(),
        turn_id: "replayed-turn".into(),
        started_at_ms: 1_000,
        model_calls: 1,
        tool_calls: 0,
        failed_tool_calls: 0,
        usage: Default::default(),
        next_model_step: 1,
        stop_hook_active: false,
        phase: mobius::backend::checkpoint::ExecutionPhase::Model,
    });
    checkpoint.pending_approval = Some(mobius::backend::checkpoint::PendingApproval {
        submission_id: "replayed-message".into(),
        turn_id: "replayed-turn".into(),
        request_id: approval_id.clone(),
        approval_call_ids: Vec::new(),
        authorized_call_ids: Vec::new(),
        calls: Vec::new(),
        reason: "Approve replayed work".into(),
        sandbox_mode: Default::default(),
        network_access: Default::default(),
        decision_received: false,
    });
    checkpoints.save(&checkpoint, &[], None).await.unwrap();
    checkpoints
        .append_event(
            &session_id,
            1,
            &Event {
                submission_id: Some("replayed-message".into()),
                msg: EventMsg::Message(mobius::protocol::MessageEvent {
                    author: input.author,
                    delivery: mobius::protocol::MessageDelivery::Turn,
                    text: input.text,
                    attachments: Vec::new(),
                    reply: None,
                    message_target: None,
                }),
            },
        )
        .await
        .unwrap();
    checkpoints
        .append_event(
            &session_id,
            2,
            &Event {
                submission_id: Some("replayed-message".into()),
                msg: EventMsg::ExecApprovalRequest(mobius::protocol::ExecApprovalRequestEvent {
                    id: approval_id,
                    turn_id: "replayed-turn".into(),
                    calls: Vec::new(),
                    reason: "Approve replayed work".into(),
                }),
            },
        )
        .await
        .unwrap();
    drop(groups);
    let credentials = Arc::new(CredentialStore::open(store.credentials_path()).unwrap());
    let bot_store = Arc::new(BotStore::open(store.state_dir()).unwrap());
    let gateway = GatewayHost::start(store, config, credentials, bot_store)
        .await
        .unwrap();
    let mut startup_events = gateway.subscribe();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            while let Ok(frame) = startup_events.try_recv() {
                if let ServerMessage::Error { code, message, .. } = frame.message {
                    panic!("startup delivery failed ({code}): {message}");
                }
            }
            if gateway
                .state
                .lock()
                .await
                .sessions
                .contains_key(&session_id)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("restart should reopen the pending participant");
    let reopened = gateway.open_session(&session_id).await.unwrap();
    assert!(!reopened.stop_if_idle().await, "approval is still pending");
    let public = gateway.open_session(&chat_id).await.unwrap();
    let replay = public.snapshot(None).await.unwrap();
    assert_eq!(replay.replay.iter().filter(|frame| matches!(&frame.message, ServerMessage::AgentEvent { record, .. } if matches!(record.event.msg, EventMsg::Message(_)))).count(), 1);
    let events = checkpoints
        .event_page(
            &session_id,
            EventPageRequest {
                before_sequence: None,
                limit: 100,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        events
            .events
            .iter()
            .filter(|record| matches!(record.event.msg, EventMsg::Message(_)))
            .count(),
        1
    );
    let groups = Arc::clone(&gateway.state.lock().await.group);
    assert_eq!(
        groups.pending_recipient_bot_ids().await.unwrap(),
        vec![bots[0].id.clone()]
    );
    assert!(
        groups
            .settle_delivery(
                "replayed-message",
                &session_id,
                &bots[0].id,
                crate::groups::GroupRunOutcome::Succeeded {
                    summary: "Completed after restart".into()
                }
            )
            .await
            .unwrap()
    );
    assert!(
        !groups
            .settle_delivery(
                "replayed-message",
                &session_id,
                &bots[0].id,
                crate::groups::GroupRunOutcome::Succeeded {
                    summary: "Duplicate".into()
                }
            )
            .await
            .unwrap()
    );
    gateway.shutdown().await;
}
