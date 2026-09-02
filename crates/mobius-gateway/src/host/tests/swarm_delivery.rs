use std::collections::BTreeMap;
use std::time::Duration;

use mobius::protocol::MessageDelivery;

use super::*;

#[test]
fn peer_submission_uses_the_board_id_and_defers_delivery_policy() {
    let message_id = Uuid::new_v4().to_string();
    let entry = BoardEntry {
        id: message_id.clone(),
        sequence: 1,
        created_at_ms: 1,
        author: crate::bots::swarm::SwarmMember {
            bot_id: "source-bot".into(),
            handle: "source".into(),
            joined_at_ms: 1,
        },
        source_session_id: "source-session".into(),
        text: "Review this".into(),
        mentioned_recipient_bot_ids: vec!["target-bot".into()],
        pending_recipient_bot_ids: vec!["target-bot".into()],
        assigned_recipient_session_ids: BTreeMap::new(),
        in_reply_to_message_id: None,
        reply_depth: 0,
    };

    let submission = peer_message_submission(entry);

    assert_eq!(submission.id, message_id);
    assert!(matches!(
        submission.op,
        Op::Message {
            message: MessageSubmission {
                author: MessageAuthor::Peer {
                    message_id: peer_message_id,
                    session_id,
                    handle,
                },
                text,
                requested_delivery: None,
                ..
            },
        } if peer_message_id == message_id
            && session_id == "source-session"
            && handle == "source"
            && text == "Review this"
    ));
}

#[tokio::test]
async fn stale_acknowledgement_does_not_clear_a_newer_delivery_attempt() {
    let root = tempfile::tempdir().expect("root");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots).expect("gateway");
    let mut attempts = HashMap::from([(
        "target-bot".into(),
        SwarmDeliveryAttempt::Submitted("new-message".into()),
    )]);

    gateway
        .handle_swarm_delivery(
            SwarmDelivery::Acknowledged {
                target_bot_id: "target-bot".into(),
                message_id: "old-message".into(),
            },
            &mut attempts,
        )
        .await;

    assert_eq!(
        attempts.get("target-bot"),
        Some(&SwarmDeliveryAttempt::Submitted("new-message".into()))
    );
}

#[tokio::test]
async fn rejected_delivery_waits_for_bot_capacity_before_retrying() {
    let root = tempfile::tempdir().expect("root");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots).expect("gateway");
    let mut attempts = HashMap::from([(
        "target-bot".into(),
        SwarmDeliveryAttempt::Submitted("message-1".into()),
    )]);

    gateway
        .handle_swarm_delivery(
            SwarmDelivery::Rejected {
                target_bot_id: "target-bot".into(),
                message_id: "message-1".into(),
            },
            &mut attempts,
        )
        .await;
    assert_eq!(
        attempts.get("target-bot"),
        Some(&SwarmDeliveryAttempt::Rejected("message-1".into()))
    );

    gateway
        .handle_swarm_delivery(SwarmDelivery::RetryPending, &mut attempts)
        .await;
    assert!(matches!(
        attempts.get("target-bot"),
        Some(SwarmDeliveryAttempt::Rejected(message_id)) if message_id == "message-1"
    ));

    gateway
        .handle_swarm_delivery(
            SwarmDelivery::CapacityAvailable {
                target_bot_id: "target-bot".into(),
            },
            &mut attempts,
        )
        .await;
    assert!(!attempts.contains_key("target-bot"));
}

#[tokio::test]
async fn gateway_busy_delivery_waits_for_mutation_completion_before_retrying() {
    let root = tempfile::tempdir().expect("root");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(root.path().join("checkpoints.sqlite3")).expect("checkpoints"),
    );
    let bots = Arc::new(BotStore::open(root.path()).expect("Bots"));
    let gateway = Arc::new(StdMutex::new(
        GatewayConfig::new(crate::config::DEFAULT_LISTEN, None).expect("gateway config"),
    ));
    let (swarm, mut deliveries) = SwarmStore::new(checkpoints, bots, gateway);
    let swarm = Arc::new(swarm);
    let session_mutations = Arc::new(RwLock::new(()));
    let mutation = Arc::clone(&session_mutations).write_owned().await;
    let retry = notify_swarm_delivery_after_mutation(
        Arc::clone(&session_mutations),
        Arc::clone(&swarm),
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

    assert_eq!(
        deliveries.recv().await,
        Some(SwarmDelivery::Pending {
            target_bot_id: "target-bot".into(),
        })
    );
}

async fn gateway_with_swarm(
    root: &tempfile::TempDir,
) -> (
    GatewayHost,
    PathBuf,
    HostHandle,
    crate::wire::BotRecord,
    HostHandle,
    crate::wire::BotRecord,
    String,
) {
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let config = config
        .registering_provider(
            AgentComposition::default().provider,
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("register provider");
    store.save(&config).expect("save config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots).expect("gateway");
    let source_bot = ensure_test_bot(&gateway).await.expect("source Bot");
    let source = gateway
        .create_session(&workspace, &source_bot.id)
        .await
        .expect("source chat");
    let (target, target_bot) = create_distinct_test_session(&gateway, &workspace, "target_bot")
        .await
        .expect("target chat");
    let swarm = gateway
        .create_swarm(
            "Review team".into(),
            source_bot.id.clone(),
            vec![target_bot.id.clone()],
        )
        .await
        .expect("create swarm");
    (
        gateway,
        workspace,
        source,
        source_bot,
        target,
        target_bot,
        swarm[0].id.clone(),
    )
}

#[tokio::test]
async fn mention_delivery_uses_one_fresh_reserved_bot_conversation() {
    let root = tempfile::tempdir().expect("root");
    let (gateway, _, source, source_bot, target, target_bot, swarm_id) =
        gateway_with_swarm(&root).await;
    let source_session_id = source.session_id().to_owned();
    let original_target_session_id = target.session_id().to_owned();
    let swarm = Arc::clone(&gateway.state.lock().await.swarm);
    let text = format!("@{} please review the parser", target_bot.handle);
    let post = swarm
        .post(&source_bot.id, &source_session_id, text.clone(), None)
        .await
        .expect("post mention");

    let assigned_session_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let entry = swarm
                .board_page(&swarm_id, None, 1)
                .await
                .expect("board")
                .entries
                .into_iter()
                .next()
                .expect("entry");
            if entry.pending_recipient_bot_ids.is_empty()
                && let Some(session_id) = entry.assigned_recipient_session_ids.get(&target_bot.id)
            {
                break session_id.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("peer delivery acknowledgement");

    assert_ne!(assigned_session_id, original_target_session_id);
    let checkpoint = gateway
        .state
        .lock()
        .await
        .checkpoints
        .load(&assigned_session_id)
        .await
        .expect("load target")
        .expect("fresh target checkpoint");
    assert_eq!(checkpoint.session_context.bot_id, target_bot.id);
    let page = gateway
        .state
        .lock()
        .await
        .checkpoints
        .event_page(
            &assigned_session_id,
            EventPageRequest {
                before_sequence: None,
                limit: 128,
            },
        )
        .await
        .expect("target journal");
    assert!(page.events.iter().any(|record| {
        matches!(
            &record.event.msg,
            EventMsg::Message(MessageEvent {
                author: MessageAuthor::Peer { message_id, session_id, handle },
                delivery,
                text: message_text,
                ..
            }) if record.event.submission_id.as_deref() == Some(post.entry.id.as_str())
                && message_id == &post.entry.id
                && session_id == &source_session_id
                && handle == &source_bot.handle
                && delivery == &MessageDelivery::Turn
                && message_text == &text
        )
    }));
}

#[tokio::test]
async fn startup_ack_reuses_the_reserved_conversation_without_resubmitting() {
    let root = tempfile::tempdir().expect("root");
    let state_dir = root.path().join("state");
    let (gateway, workspace, source, source_bot, target, target_bot, _) =
        gateway_with_swarm(&root).await;
    let source_session_id = source.session_id().to_owned();
    for host in [&source, &target] {
        assert!(host.stop_if_idle().await);
        while host.is_alive() {
            tokio::task::yield_now().await;
        }
    }
    drop(source);
    drop(target);
    drop(gateway);

    let (store, config) = ConfigStore::open(state_dir).expect("reopen config");
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(store.checkpoints_path()).expect("checkpoints"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway_config = Arc::new(StdMutex::new(config.clone()));
    let (swarm, _deliveries) = SwarmStore::new(Arc::clone(&checkpoints), bots, gateway_config);
    let text = format!("@{} verify restart delivery", target_bot.handle);
    let post = swarm
        .post(&source_bot.id, &source_session_id, text.clone(), None)
        .await
        .expect("persist mention");
    let claim = swarm
        .claim_next_delivery(&target_bot.id)
        .await
        .expect("claim target conversation")
        .expect("pending target delivery");
    let assigned_session_id = claim.session_id().to_owned();
    drop(claim);
    let mut checkpoint = Checkpoint::empty(&assigned_session_id);
    let target_spec =
        ChatSpec::for_bot(&workspace, &target_bot, store.state_dir(), None).expect("target spec");
    let target_workspace = target_spec.workspace_info();
    checkpoint.metadata = target_spec.metadata().expect("target metadata");
    checkpoint.session_context.bot_id = target_bot.id.clone();
    checkpoint.session_context.workspace_id = Some(target_workspace.id);
    checkpoint.session_context.workspace_label = Some(target_workspace.path.display().to_string());
    checkpoints
        .save(&checkpoint, &[], None)
        .await
        .expect("save reserved checkpoint");
    checkpoints
        .append_event(
            &assigned_session_id,
            1,
            &Event {
                submission_id: Some(post.entry.id.clone()),
                msg: EventMsg::Message(MessageEvent {
                    author: MessageAuthor::Peer {
                        message_id: post.entry.id.clone(),
                        session_id: source_session_id,
                        handle: source_bot.handle,
                    },
                    delivery: MessageDelivery::Turn,
                    text,
                    attachments: Vec::new(),
                    message_target: None,
                }),
            },
        )
        .await
        .expect("persist peer event");
    drop(swarm);
    drop(checkpoints);

    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots).expect("restart gateway");
    let mut events = gateway.subscribe();
    let swarm = Arc::clone(&gateway.state.lock().await.swarm);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(frame) = events.try_recv()
                && let ServerMessage::Error { code, message, .. } = frame.message
            {
                panic!("startup delivery failed ({code}): {message}");
            }
            if swarm
                .pending_deliveries(&target_bot.id)
                .await
                .expect("pending")
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("startup acknowledgement");

    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let page = checkpoints
        .event_page(
            &assigned_session_id,
            EventPageRequest {
                before_sequence: None,
                limit: 128,
            },
        )
        .await
        .expect("target events");
    assert_eq!(
        page.events
            .iter()
            .filter(|record| matches!(
                &record.event.msg,
                EventMsg::Message(MessageEvent {
                    author: MessageAuthor::Peer { message_id, .. },
                    ..
                }) if message_id == &post.entry.id
            ))
            .count(),
        1,
        "startup acknowledgement must not resubmit the persisted peer message"
    );
}
