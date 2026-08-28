use std::time::Duration;

use super::*;

#[tokio::test]
async fn stale_acknowledgement_does_not_clear_a_newer_in_flight_message() {
    let root = tempfile::tempdir().expect("root");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let mut in_flight = HashMap::from([("target".into(), "new-message".into())]);

    gateway
        .handle_swarm_delivery(
            SwarmDelivery::Acknowledged {
                target_session_id: "target".into(),
                message_id: "old-message".into(),
            },
            &mut in_flight,
        )
        .await;

    assert_eq!(
        in_flight.get("target").map(String::as_str),
        Some("new-message")
    );
}

#[tokio::test]
async fn mentioned_stopped_chat_reopens_records_peer_input_and_acknowledges_delivery() {
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
            EventMsg::PeerMessage(message)
                if message.message_id == post.entry.id
                    && message.source_session_id == source_session_id
                    && message.source_handle == post.entry.author.handle
                    && message.message == text
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
                submission_id: Some(format!("swarm-{}", post.entry.id)),
                msg: EventMsg::PeerMessage(mobius::protocol::PeerMessageEvent {
                    message_id: post.entry.id.clone(),
                    source_session_id: source_session_id.clone(),
                    source_handle: post.entry.author.handle.clone(),
                    message: text,
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
                EventMsg::PeerMessage(message) if message.message_id == post.entry.id
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
