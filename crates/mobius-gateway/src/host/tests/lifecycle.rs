use super::*;
use mobius::SessionId;

#[tokio::test]
async fn full_session_queue_rejects_external_work_but_preserves_shutdown() {
    let (commands, mut pending) = mpsc::channel(1);
    let (events, _) = broadcast::channel(1);
    let handle = HostHandle {
        inner: Arc::new(HostInner {
            session_id: SessionId::new("saturated").unwrap(),
            commands,
            events,
            alive: Arc::new(AtomicBool::new(true)),
            terminated: Arc::new(AtomicBool::new(true)),
            termination: Arc::new(tokio::sync::Notify::new()),
            session_mutations: Arc::new(RwLock::new(())),
            realtime_voice: Arc::new(Mutex::new(())),
        }),
    };
    let (reply, _) = tokio::sync::oneshot::channel();
    handle
        .inner
        .commands
        .try_send(HostCommand::BotId { reply })
        .unwrap();
    let rejected = tokio::time::timeout(std::time::Duration::from_secs(1), handle.snapshot(None))
        .await
        .expect("reject without waiting")
        .err()
        .expect("full queue");
    assert_eq!(rejected.code, "server_busy");
    assert!(!rejected.fatal);
    let shutdown = handle.shutdown();
    tokio::pin!(shutdown);
    tokio::select! {
        biased;
        () = &mut shutdown => panic!("shutdown must wait for capacity"),
        () = std::future::ready(()) => {}
    }
    assert!(matches!(
        pending.recv().await,
        Some(HostCommand::BotId { .. })
    ));
    shutdown.await;
    assert!(matches!(pending.recv().await, Some(HostCommand::Shutdown)));
    assert!(
        pending.try_recv().is_err(),
        "rejected snapshot was never queued"
    );
    drop(pending);
    assert_eq!(
        handle.snapshot(None).await.err().unwrap().code,
        stopped().code
    );
}

#[tokio::test]
async fn concurrent_catalog_updates_preserve_both_fields_for_resident_and_stopped_chats() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let (store, config) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().expect("listen"),
        None,
    )
    .expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    for stopped in [false, true] {
        let host = create_test_session(&gateway, &workspace)
            .await
            .expect("chat");
        let session_id = host.session_id();
        if stopped {
            assert!(host.stop_if_idle().await);
        }
        let mut events = gateway.subscribe();
        let (rename, pin) = tokio::join!(
            gateway.rename_session(session_id, "  Updated title  "),
            gateway.set_session_pinned(session_id, true),
        );
        rename.expect("rename");
        pin.expect("pin");
        let mut catalog_updates = 0;
        let mut approval_updates = 0;
        let mut latest_sessions = None;
        while let Ok(frame) = events.try_recv() {
            match frame.message {
                ServerMessage::Sessions { sessions, .. } => {
                    catalog_updates += 1;
                    latest_sessions = Some(sessions);
                }
                ServerMessage::BackgroundApprovals { .. } => approval_updates += 1,
                _ => {}
            }
        }
        assert!(catalog_updates >= 2);
        assert!(approval_updates >= 2);
        let sessions = latest_sessions.expect("broadcast catalog");
        let session = sessions
            .iter()
            .find(|session| session.session_id == *session_id)
            .expect("broadcast session");
        assert_eq!(session.title.as_deref(), Some("Updated title"));
        assert!(session.pinned);
        let ready = gateway.ready().await.expect("ready catalog");
        assert!(ready.sessions.iter().any(|session| {
            session.session_id == *session_id
                && session.title.as_deref() == Some("Updated title")
                && session.pinned
        }));
    }
    drop(gateway);
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
    let gateway = GatewayHost::start(store, config, credentials, Arc::clone(&bots))
        .await
        .expect("gateway");
    let host = create_test_session(&gateway, &workspace)
        .await
        .expect("create session");
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let session_id = host.session_id().share();
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
                Event {
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
            Event {
                submission_id: None,
                msg: EventMsg::Frontend(FrontendEvent::Preview {
                    kind: mobius::protocol::FrontendPreviewKind::Agent,
                    state: None,
                    symbol: None,
                    duration_ms: None,
                    started_at_ms: None,
                    id: "transient".into(),
                    title: "Transient".into(),
                    subtitle: String::new().into(),
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
    drop(gateway);
    let snapshot = reopened.snapshot(None).await.expect("session snapshot");
    let older = reopened
        .history_page(Some(crate::wire::HistoryCursor {
            session_id: session_id.share(),
            before_sequence: latest_start,
        }))
        .await
        .expect("older turn");

    assert!(snapshot.ready.latest_sequence >= durable_highwater);
    let crate::host::OpenReplay::Transcript {
        through_sequence,
        page,
        next,
    } = snapshot.replay
    else {
        panic!("a fresh open receives the reduced transcript");
    };
    let rows = |page: &crate::wire::TranscriptPage| {
        page.rows
            .iter()
            .map(|row| row.id.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(through_sequence, snapshot.ready.latest_sequence);
    assert_eq!(next, None);
    assert_eq!(
        rows(&page),
        [
            "worked:older",
            "activity:block:6:3",
            "worked:latest",
            "activity:block:9:3"
        ]
    );
    assert_eq!(rows(&older.page), ["worked:older", "activity:block:6:3"]);
    assert_eq!(older.next, None);
}

#[tokio::test]
async fn delete_sessions_preflight_all_roots_before_durable_removal() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    let root_host = create_test_session(&gateway, &workspace)
        .await
        .expect("create root session");
    let other_host = create_test_session(&gateway, &workspace)
        .await
        .expect("create other root session");
    let root_id = root_host.session_id().share();
    let other_id = other_host.session_id().share();
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let root_checkpoint = checkpoints
        .load(&root_id)
        .await
        .expect("load root")
        .expect("root checkpoint");
    let mut child = Checkpoint::empty(SessionId::new("child").unwrap());
    child.session_context = Arc::clone(&root_checkpoint.session_context);
    checkpoints
        .fork(&root_id, root_checkpoint.sequence, child.clone())
        .await
        .expect("fork child");

    gateway
        .delete_sessions(
            std::slice::from_ref(&other_id),
            mobius::backend::session_files::SessionFileSelection::Origins(vec![
                mobius::backend::session_files::SessionFileOrigin::Artifact,
            ]),
        )
        .await
        .expect("idle purge");
    assert!(other_host.is_alive());
    assert!(checkpoints.load(&other_id).await.unwrap().is_some());

    let (commands, mut receiver) = mpsc::channel(1);
    tokio::spawn(async move {
        for _ in 0..2 {
            if let Some(HostCommand::ProviderCutoverStatus { reply }) = receiver.recv().await
                && reply.send(ProviderCutoverStatus { idle: false }).is_err()
            {
                tracing::debug!("cutover status requester stopped waiting");
            }
        }
    });
    let (events, _) = broadcast::channel(1);
    gateway.state.lock().await.sessions.insert(
        SessionId::new("child").unwrap(),
        HostHandle {
            inner: Arc::new(HostInner {
                session_id: SessionId::new("child").unwrap(),
                commands,
                events,
                alive: Arc::new(AtomicBool::new(true)),
                terminated: Arc::new(AtomicBool::new(true)),
                termination: Arc::new(tokio::sync::Notify::new()),
                session_mutations: Arc::new(tokio::sync::RwLock::new(())),
                realtime_voice: Arc::new(tokio::sync::Mutex::new(())),
            }),
        },
    );

    let purge_error = super::super::deletion::prepare_session_tree_deletion(
        &*gateway.state.lock().await,
        &[SessionId::new("child").unwrap()],
        mobius::backend::session_files::SessionFileSelection::Origins(vec![
            mobius::backend::session_files::SessionFileOrigin::Artifact,
        ]),
    )
    .await
    .expect_err("busy purge rejected");
    assert_eq!(purge_error.code, "agent_busy");

    let error = gateway
        .delete_sessions(
            &[other_id.clone(), root_id.clone()],
            mobius::backend::session_files::SessionFileSelection::All,
        )
        .await
        .expect_err("busy descendant must reject deletion");

    assert_eq!(error.code, "agent_busy");
    assert!(other_host.is_alive());
    assert!(root_host.is_alive());
    let state = gateway.state.lock().await;
    assert!(state.sessions.contains_key(&root_id));
    assert!(state.sessions.contains_key(&other_id));
    assert!(state.sessions.contains_key("child"));
    drop(state);
    drop(gateway);
    assert!(
        checkpoints
            .load(&other_id)
            .await
            .expect("load other root")
            .is_some()
    );
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
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);

    let host = create_test_session(&gateway, &workspace)
        .await
        .expect("create session");
    let session_id = host.session_id().share();
    assert!(host.stop_if_idle().await);
    while host.is_alive() {
        tokio::task::yield_now().await;
    }
    let mut checkpoint = checkpoints
        .load(&session_id)
        .await
        .expect("load checkpoint")
        .expect("checkpoint");
    Arc::make_mut(&mut checkpoint.session_context).owner_id = Uuid::new_v4().to_string().into();
    checkpoint.sequence += 1;
    checkpoints
        .save(checkpoint.clone(), Vec::new(), None)
        .await
        .expect("save tampered checkpoint");

    let Err(rejection) = gateway.open_session(&session_id).await else {
        panic!("tampered Bot identity must be rejected");
    };
    drop(gateway);
    assert_eq!(rejection.code, "invalid_session_bot");
}

#[tokio::test]
async fn message_pins_accept_only_assistant_messages_and_survive_reopen() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, Arc::clone(&bots))
        .await
        .expect("gateway");
    let host = create_test_session(&gateway, &workspace)
        .await
        .expect("create session");
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let session_id = host.session_id().share();
    assert!(host.stop_if_idle().await);
    gateway.state.lock().await.sessions.remove(&session_id);
    drop(host);
    let mut checkpoint = checkpoints
        .load(&session_id)
        .await
        .expect("load")
        .expect("checkpoint");
    checkpoint.sequence += 1;
    checkpoints
        .save(
            checkpoint.clone(),
            vec![
                Arc::new(serde_json::json!({"type": "message", "role": "user", "content": []})),
                Arc::new(
                    serde_json::json!({"type": "message", "role": "assistant", "content": []}),
                ),
            ],
            None,
        )
        .await
        .expect("save transcript");
    let user = mobius::protocol::MessageTarget {
        checkpoint_sequence: checkpoint.sequence,
        batch_item_count: std::num::NonZeroUsize::MIN,
    };
    let assistant = mobius::protocol::MessageTarget {
        batch_item_count: std::num::NonZeroUsize::new(2).unwrap(),
        ..user
    };

    let opened = gateway.open_session(&session_id).await.expect("open");
    let mut events = opened.subscribe();
    assert_eq!(
        opened
            .set_message_pinned(user, true)
            .await
            .expect_err("user message")
            .code,
        "invalid_message_target"
    );
    opened
        .set_message_pinned(assistant, true)
        .await
        .expect("pin");
    assert!(matches!(
        &events.try_recv().expect("pin update").message,
        ServerMessage::MessagePinsChanged { session_id: changed, pinned_messages }
            if *changed == session_id && pinned_messages.iter().eq([&assistant])
    ));
    assert!(opened.stop_if_idle().await);
    gateway.state.lock().await.sessions.remove(&session_id);
    drop(opened);

    let reopened = gateway.open_session(&session_id).await.expect("reopen");
    drop(gateway);
    let ready = reopened.ready().await.expect("ready");
    assert_eq!(
        ready.pinned_messages.into_iter().collect::<Vec<_>>(),
        [assistant]
    );
    reopened
        .set_message_pinned(assistant, false)
        .await
        .expect("unpin");
    assert!(
        reopened
            .ready()
            .await
            .expect("ready")
            .pinned_messages
            .is_empty()
    );
}

#[tokio::test]
async fn fork_history_continues_into_the_parent_above_the_fork_point() {
    use crate::wire::{HistoryCursor, TranscriptItemContent};
    use mobius::protocol::{AssistantMessageEvent, MessageTarget, ModelStepContent};

    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    let parent = create_test_session(&gateway, &workspace)
        .await
        .expect("create parent");
    let parent_id = parent.session_id().share();
    assert!(parent.stop_if_idle().await);
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let parent_checkpoint = checkpoints
        .load(&parent_id)
        .await
        .expect("load parent")
        .expect("parent checkpoint");
    let fork_at = parent_checkpoint.sequence;
    let target = |checkpoint_sequence| {
        Some(MessageTarget {
            checkpoint_sequence,
            batch_item_count: std::num::NonZeroUsize::MIN,
        })
    };
    let turn = |turn: &str, submission: &str, checkpoint| {
        [
            (
                None,
                EventMsg::TurnStarted(mobius::protocol::TurnStartedEvent {
                    turn_id: turn.into(),
                    model_context_window: None,
                }),
            ),
            (
                Some(submission.to_owned()),
                EventMsg::Message(mobius::protocol::MessageEvent {
                    author: MessageAuthor::User,
                    delivery: mobius::protocol::MessageDelivery::Turn,
                    text: format!("ask {turn}"),
                    attachments: Vec::new(),
                    reply: None,
                    message_target: target(checkpoint),
                }),
            ),
            (
                None,
                EventMsg::AssistantMessage(AssistantMessageEvent {
                    session_id: parent_id.share(),
                    turn_id: turn.into(),
                    model_step_id: format!("step-{turn}").into(),
                    content: vec![ModelStepContent {
                        output_index: 0,
                        part_index: 0,
                        phase: mobius::protocol::ModelStepContentPhase::FinalAnswer,
                        text: format!("answer {turn}"),
                        annotations: Vec::new(),
                    }],
                    message_target: target(checkpoint),
                }),
            ),
            (
                None,
                EventMsg::TurnComplete(mobius::protocol::TurnCompleteEvent {
                    turn_id: turn.into(),
                }),
            ),
        ]
    };
    let mut child = Checkpoint::empty(SessionId::new("child").unwrap());
    child.session_context = Arc::clone(&parent_checkpoint.session_context);
    let child_id = checkpoints
        .fork(&parent_id, fork_at, child)
        .await
        .expect("fork child")
        .session_id;
    for (chat, events) in [
        (&parent_id, turn("kept", "kept", fork_at)),
        (&parent_id, turn("later", "later", fork_at + 1)),
        (&child_id, turn("own", "own", 1)),
    ] {
        for (index, (submission, msg)) in events.into_iter().enumerate() {
            checkpoints
                .append_event(
                    chat,
                    i64::try_from(index).expect("timestamp"),
                    Event {
                        submission_id: submission.map(Into::into),
                        msg,
                    },
                )
                .await
                .expect("append journal event");
        }
    }

    let rows = |page: &crate::wire::TranscriptPage| {
        page.rows
            .iter()
            .map(|row| row.id.to_string())
            .collect::<Vec<_>>()
    };
    let own = durable_history_page(
        checkpoints.as_ref(),
        &child_id,
        &FrameworkPresentation,
        None,
    )
    .await
    .expect("own page");
    assert_eq!(
        rows(&own.page),
        [
            "fork-point",
            "message:own",
            "assistant:step-own:final_answer"
        ]
    );
    let next = own.next.expect("history continues into the parent");
    assert_eq!(next.session_id, parent_id);
    let inherited = durable_history_page(
        checkpoints.as_ref(),
        &child_id,
        &FrameworkPresentation,
        Some(next),
    )
    .await
    .expect("inherited page");
    assert_eq!(
        rows(&inherited.page),
        [
            "fork:1:message:kept",
            "fork:1:assistant:step-kept:final_answer"
        ]
    );
    assert!(inherited.page.items.iter().all(|item| {
        item.inherited
            && item.target.is_none()
            && !matches!(&item.content, TranscriptItemContent::Message { message }
                if message.message_target.is_some())
    }));
    assert_eq!(inherited.next, None);
    let foreign = durable_history_page(
        checkpoints.as_ref(),
        &parent_id,
        &FrameworkPresentation,
        Some(HistoryCursor {
            session_id: child_id.share(),
            before_sequence: 1,
        }),
    )
    .await;
    assert_eq!(
        foreign.err().map(|rejection| rejection.code),
        Some("invalid_history_cursor")
    );
    drop(gateway);
}

#[tokio::test]
async fn history_and_a_cut_replay_mark_only_the_turn_after_an_agent_restart_resumed() {
    use crate::wire::TranscriptItemContent;

    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    let chat = create_test_session(&gateway, &workspace)
        .await
        .expect("create chat");
    let chat_id = chat.session_id().share();
    assert!(chat.stop_if_idle().await);
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let configured = || {
        (
            None,
            EventMsg::SessionConfigured(Arc::new(mobius::protocol::SessionConfiguredEvent {
                session_id: chat_id.share(),
                context: Arc::default(),
                model: mobius::protocol::ModelChangedEvent {
                    route: "route".into(),
                    model: "model".into(),
                    reasoning_effort: None,
                    model_context_window: None,
                },
            })),
        )
    };
    let turn = |turn: &str| {
        [
            (
                None,
                EventMsg::TurnStarted(mobius::protocol::TurnStartedEvent {
                    turn_id: turn.into(),
                    model_context_window: None,
                }),
            ),
            (
                Some(turn.to_owned()),
                EventMsg::Message(mobius::protocol::MessageEvent {
                    author: MessageAuthor::User,
                    delivery: mobius::protocol::MessageDelivery::Turn,
                    text: format!("ask {turn}"),
                    attachments: Vec::new(),
                    reply: None,
                    message_target: None,
                }),
            ),
            (
                None,
                EventMsg::TurnComplete(mobius::protocol::TurnCompleteEvent {
                    turn_id: turn.into(),
                }),
            ),
        ]
    };
    // Enough work in the first turn that the replay window starts after it.
    let filler = (0..REPLAY_CAPACITY).map(|_| (None, EventMsg::ContextCompacted));
    let [first_started, first_message, first_complete] = turn("first");
    let events = [first_started, first_message]
        .into_iter()
        .chain(filler)
        .chain([first_complete, configured()])
        .chain(turn("resumed"))
        .chain(turn("same-run"));
    for (index, (submission, msg)) in events.enumerate() {
        checkpoints
            .append_event(
                &chat_id,
                i64::try_from(index).expect("timestamp"),
                Event {
                    submission_id: submission.map(Into::into),
                    msg,
                },
            )
            .await
            .expect("append journal event");
    }

    let mut cursor = None;
    let mut opened = Vec::new();
    loop {
        let page = durable_history_page(
            checkpoints.as_ref(),
            &chat_id,
            &FrameworkPresentation,
            cursor,
        )
        .await
        .expect("history page");
        opened.extend(
            page.page
                .items
                .iter()
                .filter(|item| matches!(item.content, TranscriptItemContent::Message { .. }))
                .map(|item| (item.id.to_string(), item.resumed)),
        );
        let Some(next) = page.next else { break };
        cursor = Some(next);
    }
    assert_eq!(
        opened,
        [
            ("message:same-run".to_owned(), false),
            ("message:resumed".to_owned(), true),
            ("message:first".to_owned(), false),
        ]
    );
    let replay = load_replay(checkpoints.as_ref(), &chat_id, &FrameworkPresentation)
        .await
        .expect("replay");
    let snapshot = replay.transcript.snapshot();
    let resumed = |id: &str| {
        snapshot
            .items
            .iter()
            .find(|item| &*item.id == id)
            .map(|item| item.resumed)
    };
    assert_eq!(resumed("message:first"), None, "the window starts after it");
    assert_eq!(resumed("message:resumed"), Some(true));
    assert_eq!(resumed("message:same-run"), Some(false));
    drop(gateway);
}
