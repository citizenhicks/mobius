use super::*;

#[tokio::test]
async fn ready_exposes_the_global_scratchpad_without_a_session() {
    let root = tempfile::tempdir().expect("root");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots).expect("gateway");
    let expected = vec![FrontendContribution {
        capability: "extensions".into(),
        references: vec![mobius::protocol::FrontendReference {
            trigger: '$',
            value: "cached-skill".into(),
            description: "Cached once".into(),
        }],
        ..FrontendContribution::default()
    }];
    gateway.state.lock().await.contributions = expected.clone();

    let contributions = gateway.ready().await.expect("ready").contributions;
    assert_eq!(&contributions[..expected.len()], expected);
    assert_eq!(
        contributions.last().expect("scratchpad").capability,
        "scratchpad"
    );

    let refreshed = gateway
        .submit_scratchpad(
            &crate::wire::ScratchpadScope::Global,
            Op::CapabilityCommand {
                capability: "scratchpad".into(),
                command: "scratchpad".into(),
                arguments: "refresh".into(),
                input: None,
                target: None,
            },
        )
        .await
        .expect("refresh global scratchpad");
    assert_eq!(refreshed.capability, "scratchpad");

    let rejection = gateway
        .submit_scratchpad(
            &crate::wire::ScratchpadScope::Global,
            Op::CapabilityCommand {
                capability: "scratchpad".into(),
                command: "scratchpad".into(),
                arguments: "forget session note-1".into(),
                input: None,
                target: None,
            },
        )
        .await
        .expect_err("session operations need a selected chat");
    assert_eq!(rejection.code, "invalid_scratchpad");
}

#[tokio::test]
async fn durable_event_journal_restores_complete_turn_pages() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway =
        GatewayHost::start(store, config, credentials, Arc::clone(&bots)).expect("gateway");
    let host = create_test_session(&gateway, &workspace)
        .await
        .expect("create session");
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let session_id = host.session_id().to_owned();
    assert!(host.stop_if_idle().await);
    gateway.state.lock().await.sessions.remove(&session_id);
    drop(host);
    let mut latest_start = 0;
    for (index, event) in [
        EventMsg::TurnStarted(mobius::protocol::TurnStartedEvent {
            turn_id: "older".into(),
            model_context_window: None,
        }),
        EventMsg::Warning(mobius::protocol::WarningEvent {
            message: "older work".into(),
        }),
        EventMsg::TurnComplete(mobius::protocol::TurnCompleteEvent {
            turn_id: "older".into(),
        }),
        EventMsg::TurnStarted(mobius::protocol::TurnStartedEvent {
            turn_id: "latest".into(),
            model_context_window: None,
        }),
        EventMsg::Warning(mobius::protocol::WarningEvent {
            message: "latest work".into(),
        }),
        EventMsg::TurnComplete(mobius::protocol::TurnCompleteEvent {
            turn_id: "latest".into(),
        }),
    ]
    .into_iter()
    .enumerate()
    {
        let record = checkpoints
            .append_event(
                &session_id,
                i64::try_from(index).expect("timestamp"),
                &Event {
                    submission_id: None,
                    msg: event,
                },
            )
            .await
            .expect("append journal event");
        if index == 3 {
            latest_start = record.sequence;
        }
    }
    let durable_highwater = checkpoints
        .append_event(
            &session_id,
            6,
            &Event {
                submission_id: None,
                msg: EventMsg::Frontend(FrontendEvent::Preview {
                    id: "transient".into(),
                    title: "Transient".into(),
                    subtitle: String::new(),
                    page_id: "transient:latest".into(),
                    update: mobius::protocol::FrontendPreviewUpdate::Replace,
                    events: Vec::new(),
                    next: None,
                }),
            },
        )
        .await
        .expect("advance journal high-water")
        .sequence;

    let reopened = gateway
        .open_session(&session_id)
        .await
        .expect("reopen session");
    let snapshot = reopened.snapshot(None).await.expect("session snapshot");
    let older = reopened
        .history_page(snapshot.ready.next_before_sequence)
        .await
        .expect("older turn");

    assert!(snapshot.ready.latest_sequence >= durable_highwater);
    assert_eq!(snapshot.replay.len(), 3);
    assert_eq!(snapshot.ready.next_before_sequence, Some(latest_start));
    assert!(matches!(
        &older.records[..],
        [
            RecordedEvent {
                event: Event { msg: EventMsg::TurnStarted(started), .. },
                ..
            },
            RecordedEvent {
                event: Event { msg: EventMsg::Warning(warning), .. },
                ..
            },
            RecordedEvent {
                event: Event { msg: EventMsg::TurnComplete(completed), .. },
                ..
            }
        ] if started.turn_id == "older"
            && warning.message == "older work"
            && completed.turn_id == "older"
    ));
}

#[tokio::test]
async fn initial_snapshot_restores_transient_widgets_without_replaying_them() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots).expect("gateway");
    let host = create_test_session(&gateway, &workspace)
        .await
        .expect("create session");

    let snapshot = host.snapshot(None).await.expect("session snapshot");

    let context_window = snapshot
        .ready
        .session
        .model
        .model_context_window
        .expect("model context window");
    assert_eq!(
        snapshot.ready.context_limit_tokens,
        Some(mobius::middleware::compaction::Compaction::default().trigger_tokens(context_window))
    );
    assert!(!snapshot.ready.widgets.is_empty());
    assert!(snapshot.replay.iter().all(|frame| {
        !matches!(
            &frame.message,
            ServerMessage::AgentEvent {
                record: RecordedEvent {
                    event: Event {
                        msg: EventMsg::Frontend(
                            FrontendEvent::Widget { .. } | FrontendEvent::RemoveWidget { .. }
                        ),
                        ..
                    },
                    ..
                },
                ..
            }
        )
    }));
}

#[tokio::test]
async fn delete_session_stops_the_host_and_removes_its_durable_tree() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway =
        GatewayHost::start(store, config, credentials, Arc::clone(&bots)).expect("gateway");
    let deleted = create_test_session(&gateway, &workspace)
        .await
        .expect("create deleted session");
    let retained = create_test_session(&gateway, &workspace)
        .await
        .expect("create retained session");
    let deleted_id = deleted.session_id().to_owned();
    let retained_id = retained.session_id().to_owned();
    let (checkpoints, session_files) = {
        let state = gateway.state.lock().await;
        (Arc::clone(&state.checkpoints), state.session_files.clone())
    };
    let parent = checkpoints
        .load(&deleted_id)
        .await
        .expect("load parent")
        .expect("parent checkpoint");
    let mut deleted_child = Checkpoint::empty("deleted-child");
    deleted_child.session_context = parent.session_context.clone();
    checkpoints
        .fork(&deleted_id, parent.sequence, &deleted_child)
        .await
        .expect("fork child");
    for session_id in [&deleted_id, "deleted-child"] {
        session_files
            .publish_artifact(
                session_id,
                "result.txt".into(),
                "text/plain".into(),
                b"result",
            )
            .await
            .expect("publish artifact");
    }
    deleted
        .rename_session(deleted_id.clone(), "Deleted".into())
        .await
        .expect("title deleted session");
    retained
        .rename_session(retained_id.clone(), "Retained".into())
        .await
        .expect("title retained session");
    gateway
        .delete_session(&deleted_id)
        .await
        .expect("delete session");

    assert!(
        checkpoints
            .load(&deleted_id)
            .await
            .expect("load deleted")
            .is_none()
    );
    assert!(
        checkpoints
            .load("deleted-child")
            .await
            .expect("load deleted child")
            .is_none()
    );
    assert!(
        session_files
            .list_artifacts(&deleted_id)
            .await
            .expect("deleted artifacts")
            .is_empty()
    );
    let metadata = load_session_metadata(&checkpoints)
        .await
        .expect("catalog metadata");
    assert!(!metadata.contains_key(&deleted_id));
    assert_eq!(metadata[&retained_id].title.as_deref(), Some("Retained"));
    assert!(deleted.snapshot(None).await.is_err());
    assert_eq!(
        gateway
            .sessions()
            .await
            .expect("remaining sessions")
            .into_iter()
            .map(|session| session.session_id)
            .collect::<Vec<_>>(),
        [retained_id]
    );
}

#[tokio::test]
async fn delete_session_keeps_all_resident_hosts_when_a_descendant_is_busy() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots).expect("gateway");
    let root_host = create_test_session(&gateway, &workspace)
        .await
        .expect("create root session");
    let root_id = root_host.session_id().to_owned();
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let root_checkpoint = checkpoints
        .load(&root_id)
        .await
        .expect("load root")
        .expect("root checkpoint");
    let mut child = Checkpoint::empty("child");
    child.session_context = root_checkpoint.session_context.clone();
    checkpoints
        .fork(&root_id, root_checkpoint.sequence, &child)
        .await
        .expect("fork child");

    let (commands, mut receiver) = mpsc::channel(1);
    tokio::spawn(async move {
        if let Some(HostCommand::StopIfIdle { reply }) = receiver.recv().await {
            let _ = reply.send(false);
        }
    });
    let (events, _) = broadcast::channel(1);
    gateway.state.lock().await.sessions.insert(
        "child".into(),
        HostHandle {
            inner: Arc::new(HostInner {
                session_id: "child".into(),
                bot_id: "test-bot".into(),
                commands,
                events,
                accepts_file_attachments: Arc::new(AtomicBool::new(false)),
                alive: Arc::new(AtomicBool::new(true)),
            }),
        },
    );

    let error = gateway
        .delete_session(&root_id)
        .await
        .expect_err("busy descendant must reject deletion");

    assert_eq!(error.code, "agent_busy");
    let state = gateway.state.lock().await;
    assert!(state.sessions.contains_key(&root_id));
    assert!(state.sessions.contains_key("child"));
}

#[tokio::test]
async fn open_session_rejects_invalid_ids_before_checkpoint_lookup() {
    let root = tempfile::tempdir().expect("root");
    let (store, config) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().expect("listen address"),
        None,
    )
    .expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots).expect("gateway");

    for session_id in [" ".to_owned(), "x".repeat(4097)] {
        let error = match gateway.open_session(&session_id).await {
            Ok(_) => panic!("invalid session ID must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.code, "invalid_session_id");
        assert_eq!(error.message, "session ID must be 1–4096 bytes");
    }
}

#[tokio::test]
async fn opening_a_stopped_cached_chat_creates_a_fresh_actor() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_dir = root.path().join("state");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state_dir, listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots).expect("gateway");
    let original = create_test_session(&gateway, &workspace)
        .await
        .expect("create chat");
    let session_id = original.session_id().to_string();

    assert!(original.stop_if_idle().await);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while original.is_alive() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actor stopped");
    let reopened = gateway
        .open_session(&session_id)
        .await
        .expect("reopen chat");

    assert!(reopened.is_alive());
    assert!(!Arc::ptr_eq(&original.inner, &reopened.inner));
}

#[tokio::test]
async fn opening_a_chat_rejects_tampered_bot_identity() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots).expect("gateway");
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);

    let host = create_test_session(&gateway, &workspace)
        .await
        .expect("create session");
    let session_id = host.session_id().to_owned();
    assert!(host.stop_if_idle().await);
    while host.is_alive() {
        tokio::task::yield_now().await;
    }
    let mut checkpoint = checkpoints
        .load(&session_id)
        .await
        .expect("load checkpoint")
        .expect("checkpoint");
    checkpoint.session_context.bot_id = Uuid::new_v4().to_string();
    checkpoint.sequence += 1;
    checkpoints
        .save(&checkpoint, &[], None)
        .await
        .expect("save tampered checkpoint");

    let rejection = match gateway.open_session(&session_id).await {
        Ok(_) => panic!("tampered Bot identity must be rejected"),
        Err(rejection) => rejection,
    };
    assert_eq!(rejection.code, "invalid_session_bot");
}

#[tokio::test]
async fn capacity_reclaims_an_unreferenced_idle_chat() {
    let root = tempfile::tempdir().expect("root");
    let state_dir = root.path().join("state");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state_dir, listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots).expect("gateway");
    let mut state = gateway.state.lock().await;
    for index in 0..MAX_ACTIVE_SESSIONS {
        let (commands, mut receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            if let Some(HostCommand::StopIfIdle { reply }) = receiver.recv().await {
                let _ = reply.send(true);
            }
        });
        let (events, _) = broadcast::channel(1);
        let id = format!("chat-{index}");
        state.sessions.insert(
            id.clone(),
            HostHandle {
                inner: Arc::new(HostInner {
                    session_id: id.into(),
                    bot_id: "test-bot".into(),
                    commands,
                    events,
                    accepts_file_attachments: Arc::new(AtomicBool::new(false)),
                    alive: Arc::new(AtomicBool::new(true)),
                }),
            },
        );
    }

    state.ensure_capacity().await.expect("reclaim capacity");

    assert_eq!(state.sessions.len(), MAX_ACTIVE_SESSIONS - 1);
}
