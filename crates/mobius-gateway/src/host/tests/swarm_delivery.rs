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
        source_event_id: None,
        text: "Review this".into(),
        mentioned_recipient_bot_ids: vec!["target-bot".into()],
        pending_recipient_bot_ids: vec!["target-bot".into()],
        assigned_recipient_session_ids: BTreeMap::new(),
        in_reply_to_message_id: None,
        reply_depth: 0,
    };

    let submission = swarm_message_submission(entry);

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
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    let mut attempts = HashMap::from([(
        "target-bot".into(),
        SwarmDeliveryAttempt::Submitted("new-message".into()),
    )]);
    let mut attention_attempts = HashSet::new();

    gateway
        .handle_swarm_delivery(
            SwarmDelivery::Acknowledged {
                target_bot_id: "target-bot".into(),
                message_id: "old-message".into(),
            },
            &mut attempts,
            &mut attention_attempts,
        )
        .await;

    assert_eq!(
        attempts.get("target-bot"),
        Some(&SwarmDeliveryAttempt::Submitted("new-message".into()))
    );
}

#[tokio::test]
async fn duplicate_user_attention_waits_for_its_exact_acknowledgement() {
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
    let mut attempts = HashMap::new();
    let mut attention_attempts = HashSet::from(["attention-1".into()]);

    gateway
        .handle_swarm_delivery(
            SwarmDelivery::UserAttention {
                message_id: "attention-1".into(),
            },
            &mut attempts,
            &mut attention_attempts,
        )
        .await;
    assert!(attention_attempts.contains("attention-1"));

    gateway
        .handle_swarm_delivery(
            SwarmDelivery::UserAttentionAcknowledged {
                message_id: "attention-1".into(),
            },
            &mut attempts,
            &mut attention_attempts,
        )
        .await;
    assert!(attention_attempts.is_empty());
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
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    let mut attempts = HashMap::from([(
        "target-bot".into(),
        SwarmDeliveryAttempt::Submitted("message-1".into()),
    )]);
    let mut attention_attempts = HashSet::new();

    gateway
        .handle_swarm_delivery(
            SwarmDelivery::Rejected {
                target_bot_id: "target-bot".into(),
                message_id: "message-1".into(),
            },
            &mut attempts,
            &mut attention_attempts,
        )
        .await;
    assert_eq!(
        attempts.get("target-bot"),
        Some(&SwarmDeliveryAttempt::Rejected("message-1".into()))
    );

    gateway
        .handle_swarm_delivery(
            SwarmDelivery::RetryPending,
            &mut attempts,
            &mut attention_attempts,
        )
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
            &mut attention_attempts,
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
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
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
async fn mention_delivery_uses_the_bots_private_swarm_conversation() {
    let root = tempfile::tempdir().expect("root");
    let (gateway, _, source, source_bot, target, target_bot, swarm_id) =
        gateway_with_swarm(&root).await;
    let mut gateway_events = gateway.subscribe();
    let source_session_id = source.session_id().to_owned();
    let original_target_session_id = target.session_id().to_owned();
    let swarm = Arc::clone(&gateway.state.lock().await.swarm);
    let text = format!("@{} please review the parser", target_bot.handle);
    let post = swarm
        .post(&source_bot.id, &source_session_id, text.clone(), None)
        .await
        .expect("post mention");

    let assigned_session_id = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(frame) = gateway_events.try_recv()
                && let ServerMessage::Error { code, message, .. } = frame.message
            {
                panic!("swarm delivery failed ({code}): {message}");
            }
            let entry = swarm
                .board_page(&swarm_id, None, 32)
                .await
                .expect("board")
                .entries
                .into_iter()
                .find(|entry| entry.id == post.entry.id)
                .expect("posted entry");
            if let Some(session_id) = entry.assigned_recipient_session_ids.get(&target_bot.id) {
                let page = gateway
                    .state
                    .lock()
                    .await
                    .checkpoints
                    .event_page(
                        session_id,
                        EventPageRequest {
                            before_sequence: None,
                            limit: 128,
                        },
                    )
                    .await
                    .expect("target journal");
                if page.events.iter().any(|record| {
                    record.event.submission_id.as_deref() == Some(post.entry.id.as_str())
                        && matches!(record.event.msg, EventMsg::Message(_))
                }) {
                    break session_id.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("peer delivery input");

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
    assert!(!checkpoint.catalog_visible);
    assert_eq!(
        checkpoint.session_context.origin_label.as_deref(),
        Some("Swarm Chat · Review team")
    );
    let background_workspace = gateway.state.lock().await.background_workspace.clone();
    assert_eq!(
        checkpoint.session_context.workspace_label.as_deref(),
        Some(background_workspace.to_string_lossy().as_ref())
    );
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

    let second = swarm
        .post(
            &source_bot.id,
            &source_session_id,
            format!("@{} review the follow-up", target_bot.handle),
            None,
        )
        .await
        .expect("post follow-up mention");
    let reused_session_id = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let entry = swarm
                .board_page(&swarm_id, None, 32)
                .await
                .expect("board")
                .entries
                .into_iter()
                .find(|entry| entry.id == second.entry.id)
                .expect("follow-up entry");
            if let Some(session_id) = entry.assigned_recipient_session_ids.get(&target_bot.id) {
                break session_id.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("follow-up delivery");
    assert_eq!(reused_session_id, assigned_session_id);
}

#[tokio::test]
async fn startup_ack_reuses_the_reserved_conversation_without_resubmitting() {
    let root = tempfile::tempdir().expect("root");
    let state_dir = root.path().join("state");
    let (gateway, _workspace, source, source_bot, target, target_bot, _) =
        gateway_with_swarm(&root).await;
    let source_session_id = source.session_id().to_owned();
    let visible_tool_count = target
        .snapshot(None)
        .await
        .expect("visible target snapshot")
        .ready
        .tool_count;
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
    checkpoint.catalog_visible = false;
    let background_workspace =
        prepare_background_workspace(store.state_dir(), None).expect("background workspace");
    let target_spec =
        ChatSpec::for_bot(&background_workspace, &target_bot, store.state_dir(), None)
            .expect("target spec");
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
    let approval_id = Uuid::new_v4().to_string();
    checkpoints
        .append_event(
            &assigned_session_id,
            2,
            &Event {
                submission_id: Some(post.entry.id.clone()),
                msg: EventMsg::ExecApprovalRequest(mobius::protocol::ExecApprovalRequestEvent {
                    id: approval_id.clone(),
                    turn_id: "replayed-turn".into(),
                    calls: Vec::new(),
                    reason: "Approve replayed work".into(),
                }),
            },
        )
        .await
        .expect("persist approval request");
    drop(swarm);
    drop(checkpoints);

    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("restart gateway");
    let mut events = gateway.subscribe();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(frame) = events.try_recv()
                && let ServerMessage::Error { code, message, .. } = frame.message
            {
                panic!("startup delivery failed ({code}): {message}");
            }
            if gateway
                .state
                .lock()
                .await
                .sessions
                .contains_key(&assigned_session_id)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("startup reopen");

    let reopened = gateway
        .state
        .lock()
        .await
        .sessions
        .get(&assigned_session_id)
        .cloned()
        .expect("reopened hidden conversation");
    let hidden = reopened.snapshot(None).await.expect("hidden snapshot");
    assert_eq!(hidden.ready.tool_count + 1, visible_tool_count);
    assert!(
        gateway
            .sessions()
            .await
            .expect("visible sessions")
            .iter()
            .all(|session| session.session_id != assigned_session_id)
    );
    assert!(
        gateway
            .hidden_bot_sessions(&target_bot.id)
            .await
            .expect("hidden Bot sessions")
            .iter()
            .any(|session| session.session_id == assigned_session_id)
    );

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
                }) if message_id.as_str() == post.entry.id.as_str()
            ))
            .count(),
        1,
        "startup acknowledgement must not resubmit the persisted peer message"
    );

    let swarm = Arc::clone(&gateway.state.lock().await.swarm);
    let board = swarm
        .board_page(
            &swarm
                .snapshot_for_bot(&target_bot.id)
                .await
                .expect("swarm snapshot")
                .expect("target membership")
                .swarm
                .id,
            None,
            128,
        )
        .await
        .expect("board");
    assert_eq!(
        board
            .entries
            .iter()
            .filter(|entry| entry.source_event_id.as_deref() == Some(approval_id.as_str()))
            .count(),
        1,
        "replay projects each approval request once"
    );
    assert_eq!(
        swarm
            .pending_deliveries(&target_bot.id)
            .await
            .expect("in-flight delivery")
            .len(),
        1,
        "approval remains nonterminal"
    );
    assert!(
        swarm
            .settle_delivery(
                &post.entry.id,
                &assigned_session_id,
                &target_bot.id,
                SwarmRunOutcome::Succeeded {
                    summary: "Replayed work completed".into(),
                },
            )
            .await
            .expect("settle replayed work")
    );
    assert!(
        !swarm
            .settle_delivery(
                &post.entry.id,
                &assigned_session_id,
                &target_bot.id,
                SwarmRunOutcome::Failed {
                    message: "duplicate terminal".into(),
                },
            )
            .await
            .expect("dedupe terminal")
    );
}

#[tokio::test]
async fn startup_attention_ack_does_not_resubmit_a_persisted_visible_message() {
    let root = tempfile::tempdir().expect("root");
    let state_dir = root.path().join("state");
    let (gateway, _, source, source_bot, target, _, _) = gateway_with_swarm(&root).await;
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
    let post = swarm
        .post(
            &source_bot.id,
            &source_session_id,
            "Choose the release scope @user".into(),
            None,
        )
        .await
        .expect("attention post");
    swarm
        .assign_user_attention(&post.entry.id, &source_session_id)
        .await
        .expect("assign exact causal chat");
    checkpoints
        .append_event(
            &source_session_id,
            1,
            &Event {
                submission_id: Some(post.entry.id.clone()),
                msg: EventMsg::Message(MessageEvent {
                    author: MessageAuthor::Peer {
                        message_id: post.entry.id.clone(),
                        session_id: source_session_id.clone(),
                        handle: source_bot.handle,
                    },
                    delivery: MessageDelivery::Queue,
                    text: "Choose the release scope".into(),
                    attachments: Vec::new(),
                    message_target: None,
                }),
            },
        )
        .await
        .expect("persist attention delivery");
    drop(swarm);
    drop(checkpoints);

    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("restart gateway");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if gateway
                .state
                .lock()
                .await
                .swarm
                .pending_user_attention()
                .await
                .expect("pending attention")
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("startup attention reconciliation");

    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let page = checkpoints
        .event_page(
            &source_session_id,
            EventPageRequest {
                before_sequence: None,
                limit: 128,
            },
        )
        .await
        .expect("visible journal");
    assert_eq!(
        page.events
            .iter()
            .filter(|record| {
                record.event.submission_id.as_deref() == Some(post.entry.id.as_str())
                    && matches!(record.event.msg, EventMsg::Message(_))
            })
            .count(),
        1,
        "startup reconciliation must not duplicate the persisted attention message"
    );
}
