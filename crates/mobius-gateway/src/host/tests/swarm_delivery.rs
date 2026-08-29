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
        author: crate::swarm::SwarmMember {
            session_id: "source".into(),
            handle: "agent_source".into(),
            joined_at_ms: 1,
        },
        text: "Review this".into(),
        mentioned_recipient_session_ids: vec!["target".into()],
        pending_recipient_session_ids: vec!["target".into()],
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
            && session_id == "source"
            && handle == "agent_source"
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
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let mut attempts = HashMap::from([(
        "target".into(),
        SwarmDeliveryAttempt::Submitted("new-message".into()),
    )]);

    gateway
        .handle_swarm_delivery(
            SwarmDelivery::Acknowledged {
                target_session_id: "target".into(),
                message_id: "old-message".into(),
            },
            &mut attempts,
        )
        .await;

    assert_eq!(
        attempts.get("target"),
        Some(&SwarmDeliveryAttempt::Submitted("new-message".into()))
    );
}

#[tokio::test]
async fn rejected_delivery_waits_for_message_capacity_before_retrying() {
    let root = tempfile::tempdir().expect("root");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let mut attempts = HashMap::from([(
        "target".into(),
        SwarmDeliveryAttempt::Submitted("message-1".into()),
    )]);

    gateway
        .handle_swarm_delivery(
            SwarmDelivery::Rejected {
                target_session_id: "target".into(),
                message_id: "message-1".into(),
            },
            &mut attempts,
        )
        .await;
    assert_eq!(
        attempts.get("target"),
        Some(&SwarmDeliveryAttempt::Rejected("message-1".into()))
    );

    gateway
        .handle_swarm_delivery(SwarmDelivery::RetryPending, &mut attempts)
        .await;
    assert!(matches!(
        attempts.get("target"),
        Some(SwarmDeliveryAttempt::Rejected(message_id)) if message_id == "message-1"
    ));

    gateway
        .handle_swarm_delivery(
            SwarmDelivery::CapacityAvailable {
                target_session_id: "target".into(),
            },
            &mut attempts,
        )
        .await;
    assert!(!attempts.contains_key("target"));
}

#[tokio::test]
async fn mentioned_stopped_chat_reopens_records_peer_message_and_acknowledges_delivery() {
    let root = tempfile::tempdir().expect("root");
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
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let source = gateway
        .create_session(&workspace)
        .await
        .expect("source chat");
    let target = gateway
        .create_session(&workspace)
        .await
        .expect("target chat");
    let source_session_id = source.session_id().to_owned();
    let target_session_id = target.session_id().to_owned();
    let swarm = gateway
        .create_swarm(
            source_session_id.clone(),
            vec![source_session_id.clone(), target_session_id.clone()],
        )
        .await
        .expect("create swarm");
    let target_handle = swarm[0]
        .members
        .iter()
        .find(|member| member.session_id == target_session_id)
        .expect("target member")
        .handle
        .clone();

    assert!(target.stop_if_idle().await);
    while target.is_alive() {
        tokio::task::yield_now().await;
    }
    gateway
        .state
        .lock()
        .await
        .sessions
        .remove(&target_session_id);
    drop(target);

    let swarm_store = Arc::clone(&gateway.state.lock().await.swarm);
    let text = format!("@{target_handle} please review the parser");
    let post = swarm_store
        .post(&source_session_id, text.clone())
        .await
        .expect("post mention");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if swarm_store
                .pending_deliveries(&target_session_id)
                .await
                .expect("pending deliveries")
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("peer delivery acknowledgement");

    assert!(
        gateway
            .state
            .lock()
            .await
            .sessions
            .get(&target_session_id)
            .is_some_and(HostHandle::is_alive),
        "a stopped mentioned chat must be reopened"
    );
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let page = checkpoints
        .event_page(
            &target_session_id,
            EventPageRequest {
                before_sequence: None,
                limit: 128,
            },
        )
        .await
        .expect("target event journal");
    assert!(page.events.iter().any(|record| {
        matches!(
            &record.event.msg,
            EventMsg::Message(MessageEvent {
                author: MessageAuthor::Peer {
                    message_id,
                    session_id,
                    handle,
                },
                delivery,
                text: message_text,
                ..
            }) if record.event.submission_id.as_deref() == Some(post.entry.id.as_str())
                && message_id == &post.entry.id
                && session_id == &source_session_id
                && handle == &post.entry.author.handle
                && delivery == &MessageDelivery::Turn
                && message_text == &text
        )
    }));
}

#[tokio::test]
async fn startup_acknowledges_a_peer_event_persisted_before_the_previous_gateway_stopped() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let state_dir = root.path().join("state");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state_dir.clone(), listen, None).expect("config");
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
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let source = gateway
        .create_session(&workspace)
        .await
        .expect("source chat");
    let target = gateway
        .create_session(&workspace)
        .await
        .expect("target chat");
    let source_session_id = source.session_id().to_owned();
    let target_session_id = target.session_id().to_owned();
    let swarm = gateway
        .create_swarm(
            source_session_id.clone(),
            vec![source_session_id.clone(), target_session_id.clone()],
        )
        .await
        .expect("create swarm");
    let target_handle = swarm[0]
        .members
        .iter()
        .find(|member| member.session_id == target_session_id)
        .expect("target member")
        .handle
        .clone();
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
        Arc::new(SqliteCheckpoint::new(store.checkpoints_path()).expect("reopen checkpoints"));
    let (swarm_store, _deliveries) = SwarmStore::new(Arc::clone(&checkpoints));
    let text = format!("@{target_handle} verify restart delivery");
    let post = swarm_store
        .post(&source_session_id, text.clone())
        .await
        .expect("persist pending mention");
    checkpoints
        .append_event(
            &target_session_id,
            1,
            &Event {
                submission_id: Some(post.entry.id.clone()),
                msg: EventMsg::Message(MessageEvent {
                    author: MessageAuthor::Peer {
                        message_id: post.entry.id.clone(),
                        session_id: source_session_id.clone(),
                        handle: post.entry.author.handle.clone(),
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
    drop(swarm_store);
    drop(checkpoints);

    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("restart gateway");
    let swarm_store = Arc::clone(&gateway.state.lock().await.swarm);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if swarm_store
                .pending_deliveries(&target_session_id)
                .await
                .expect("pending deliveries")
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
            &target_session_id,
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
        "a durable peer event must be acknowledged without duplicate resubmission"
    );
}

#[tokio::test]
async fn pending_delivery_retries_when_a_chat_releases_gateway_capacity() {
    let root = tempfile::tempdir().expect("root");
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
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let source = gateway
        .create_session(&workspace)
        .await
        .expect("source chat");
    let target = gateway
        .create_session(&workspace)
        .await
        .expect("target chat");
    let source_session_id = source.session_id().to_owned();
    let target_session_id = target.session_id().to_owned();
    let swarm = gateway
        .create_swarm(
            source_session_id.clone(),
            vec![source_session_id.clone(), target_session_id.clone()],
        )
        .await
        .expect("create swarm");
    let target_handle = swarm[0]
        .members
        .iter()
        .find(|member| member.session_id == target_session_id)
        .expect("target member")
        .handle
        .clone();
    let mut blockers = Vec::new();
    for _ in 0..MAX_ACTIVE_SESSIONS - 2 {
        blockers.push(
            gateway
                .create_session(&workspace)
                .await
                .expect("capacity blocker"),
        );
    }
    assert!(target.stop_if_idle().await);
    while target.is_alive() {
        tokio::task::yield_now().await;
    }
    gateway
        .state
        .lock()
        .await
        .sessions
        .remove(&target_session_id);
    drop(target);
    blockers.push(
        gateway
            .create_session(&workspace)
            .await
            .expect("replacement blocker"),
    );

    let mut events = gateway.subscribe();
    let swarm_store = Arc::clone(&gateway.state.lock().await.swarm);
    swarm_store
        .post(
            &source_session_id,
            format!("@{target_handle} retry after capacity changes"),
        )
        .await
        .expect("post mention");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                events.recv().await.expect("gateway event").message,
                ServerMessage::Error { ref code, .. } if code == "swarm_delivery"
            ) {
                break;
            }
        }
    })
    .await
    .expect("delivery must first exhaust gateway capacity");

    drop(blockers.pop());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if swarm_store
                .pending_deliveries(&target_session_id)
                .await
                .expect("pending deliveries")
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("capacity release must retry durable delivery");

    assert!(
        gateway
            .state
            .lock()
            .await
            .sessions
            .get(&target_session_id)
            .is_some_and(HostHandle::is_alive)
    );
}
