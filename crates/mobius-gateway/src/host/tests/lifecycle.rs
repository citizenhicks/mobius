use super::*;

#[tokio::test]
async fn full_session_queue_rejects_external_work_but_preserves_shutdown() {
    let (commands, mut pending) = mpsc::channel(1);
    let (events, _) = broadcast::channel(1);
    let handle = HostHandle {
        inner: Arc::new(HostInner {
            session_id: Arc::from("saturated"),
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
            .find(|session| session.session_id == session_id)
            .expect("broadcast session");
        assert_eq!(session.title.as_deref(), Some("Updated title"));
        assert!(session.pinned);
        let ready = gateway.ready().await.expect("ready catalog");
        assert!(ready.sessions.iter().any(|session| {
            session.session_id == session_id
                && session.title.as_deref() == Some("Updated title")
                && session.pinned
        }));
    }
}

#[tokio::test]
async fn capacity_reclaims_real_idle_agents() {
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
    for index in 0..=MAX_ACTIVE_SESSIONS {
        let host = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            create_test_session(&gateway, &workspace),
        )
        .await
        .unwrap_or_else(|_| panic!("chat {index} timed out"))
        .expect("chat");
        drop(host);
    }
    gateway.shutdown().await;
}

struct BlockingStateStore {
    inner: Arc<dyn CheckpointStore>,
    block_next: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    block_next_load: AtomicBool,
    load_entered: tokio::sync::Notify,
    load_release: tokio::sync::Notify,
}

impl BlockingStateStore {
    fn new(inner: Arc<dyn CheckpointStore>) -> Self {
        Self {
            inner,
            block_next: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            block_next_load: AtomicBool::new(false),
            load_entered: tokio::sync::Notify::new(),
            load_release: tokio::sync::Notify::new(),
        }
    }
}

impl CheckpointStore for BlockingStateStore {
    fn load<'a>(
        &'a self,
        session_id: &'a str,
    ) -> mobius::BoxFuture<'a, mobius::Result<Option<mobius::backend::checkpoint::Checkpoint>>>
    {
        self.inner.load(session_id)
    }

    fn delete_sessions<'a>(
        &'a self,
        session_ids: &'a [String],
    ) -> mobius::BoxFuture<'a, mobius::Result<bool>> {
        self.inner.delete_sessions(session_ids)
    }

    fn save<'a>(
        &'a self,
        checkpoint: &'a mobius::backend::checkpoint::Checkpoint,
        transcript_delta: &'a [serde_json::Value],
        execution: Option<&'a ExecutionRecord>,
    ) -> mobius::BoxFuture<'a, mobius::Result<()>> {
        self.inner.save(checkpoint, transcript_delta, execution)
    }

    fn save_with_events<'a>(
        &'a self,
        checkpoint: mobius::backend::checkpoint::Checkpoint,
        transcript_delta: Vec<serde_json::Value>,
        execution: Option<ExecutionRecord>,
        events: Vec<mobius::backend::checkpoint::TimestampedEvent>,
    ) -> mobius::BoxFuture<'a, mobius::Result<Vec<JournalEvent>>> {
        self.inner
            .save_with_events(checkpoint, transcript_delta, execution, events)
    }

    fn append_event<'a>(
        &'a self,
        session_id: &'a str,
        recorded_at_ms: i64,
        event: &'a Event,
    ) -> mobius::BoxFuture<'a, mobius::Result<JournalEvent>> {
        self.inner.append_event(session_id, recorded_at_ms, event)
    }

    fn event_page<'a>(
        &'a self,
        session_id: &'a str,
        request: EventPageRequest,
    ) -> mobius::BoxFuture<'a, mobius::Result<mobius::backend::checkpoint::EventPage>> {
        self.inner.event_page(session_id, request)
    }

    fn load_state<'a>(
        &'a self,
        scope: &'a str,
        key: &'a str,
    ) -> mobius::BoxFuture<'a, mobius::Result<Option<serde_json::Value>>> {
        Box::pin(async move {
            if self.block_next_load.swap(false, Ordering::SeqCst) {
                self.load_entered.notify_one();
                self.load_release.notified().await;
            }
            self.inner.load_state(scope, key).await
        })
    }

    fn save_state<'a>(
        &'a self,
        scope: &'a str,
        key: &'a str,
        value: &'a serde_json::Value,
    ) -> mobius::BoxFuture<'a, mobius::Result<()>> {
        Box::pin(async move {
            if self.block_next.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.inner.save_state(scope, key, value).await
        })
    }
}

#[tokio::test]
async fn ready_holds_one_gateway_generation_while_loading_catalogs() {
    let root = tempfile::tempdir().expect("root");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let provider = ProviderConfig {
        instance: "kimi-ready".into(),
        provider: "kimi".into(),
        model: "kimi-k3".into(),
        base_url: Some("https://api.moonshot.ai/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let config = config
        .registering_provider(
            provider.clone(),
            "Kimi".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("provider catalog");
    store.save(&config).expect("save provider catalog");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    credentials
        .set(
            &provider.instance,
            &provider.provider,
            "ready-secret",
            provider.base_url.as_deref(),
            None,
        )
        .expect("provider credential");
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    let blocking = {
        let mut state = gateway.state.lock().await;
        let blocking = Arc::new(BlockingStateStore::new(Arc::clone(&state.checkpoints)));
        state.scratchpad = ScratchpadStore::new(blocking.clone());
        blocking
    };
    blocking.block_next_load.store(true, Ordering::SeqCst);
    let ready = tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.ready().await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        blocking.load_entered.notified(),
    )
    .await
    .expect("Ready reached catalog loading");

    gateway
        .clear_credential(provider.instance.clone())
        .await
        .expect("clear credential during Ready");

    let mut mutation = tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.begin_exclusive_mutation().await }
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut mutation)
            .await
            .is_err()
    );

    blocking.load_release.notify_one();
    let ready = ready.await.expect("Ready task").expect("Ready payload");
    assert!(!ready.models.is_empty());
    assert!(
        ready
            .provider_instances
            .iter()
            .any(|instance| instance.selection.instance == provider.instance && instance.configured)
    );
    mutation
        .await
        .expect("mutation task")
        .expect("exclusive mutation");
    let refreshed = gateway.ready().await.expect("refreshed Ready payload");
    assert!(refreshed.models.is_empty());
    assert!(refreshed.provider_instances.iter().any(|instance| {
        instance.selection.instance == provider.instance && !instance.configured
    }));
}

#[tokio::test]
async fn idle_stop_waits_for_an_accepted_capability_command_to_finish() {
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
    let blocking = {
        let mut state = gateway.state.lock().await;
        let blocking = Arc::new(BlockingStateStore::new(Arc::clone(&state.checkpoints)));
        state.scratchpad = ScratchpadStore::new(blocking.clone());
        blocking
    };
    let host = create_test_session(&gateway, &workspace)
        .await
        .expect("create session");
    let note_id = Uuid::new_v4().to_string();
    blocking
        .inner
        .save_state(
            "scratchpad.global",
            "entries.v1",
            &serde_json::json!([{
                "id": note_id,
                "note": "before",
                "basis": { "type": "user_confirmed" },
                "created_at": "1"
            }]),
        )
        .await
        .expect("seed note");
    blocking.block_next.store(true, Ordering::SeqCst);
    host.submit(Submission {
        id: Uuid::new_v4().to_string(),
        op: Op::CapabilityCommand {
            capability: "scratchpad".into(),
            command: "scratchpad".into(),
            arguments: format!("edit {note_id}"),
            input: Some("after".into()),
            target: None,
        },
    })
    .await
    .expect("accept scratchpad edit");
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        blocking.entered.notified(),
    )
    .await
    .expect("scratchpad edit reached durable save");

    let mut first_stop = tokio::spawn({
        let host = host.clone();
        async move { host.stop_if_idle().await }
    });
    let mut second_stop = tokio::spawn({
        let host = host.clone();
        async move { host.stop_if_idle().await }
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut first_stop)
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut second_stop)
            .await
            .is_err()
    );
    blocking.release.notify_one();
    for stopping in [first_stop, second_stop] {
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), stopping)
                .await
                .expect("stop completed")
                .expect("stop task")
        );
    }
    let saved = blocking
        .inner
        .load_state("scratchpad.global", "entries.v1")
        .await
        .expect("load scratchpad")
        .expect("saved scratchpad");
    assert_eq!(saved[0]["note"], "after");
}

#[tokio::test]
async fn global_contribution_reads_and_updates_preserve_extension_references_without_a_session() {
    let root = tempfile::tempdir().expect("root");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    let gateway = GatewayHost::start(store, config, credentials, Arc::clone(&bots))
        .await
        .expect("gateway");
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
    assert_eq!(
        gateway.contributions().await.expect("global contributions"),
        contributions
    );

    let refreshed = gateway
        .submit_contribution(Op::CapabilityCommand {
            capability: "scratchpad".into(),
            command: "scratchpad".into(),
            arguments: "refresh".into(),
            input: None,
            target: None,
        })
        .await
        .expect("refresh global scratchpad");
    assert_eq!(refreshed, contributions);

    let updated = gateway
        .submit_contribution(Op::CapabilityCommand {
            capability: "scratchpad".into(),
            command: "scratchpad".into(),
            arguments: "add".into(),
            input: Some("Shared context".into()),
            target: None,
        })
        .await
        .expect("add global note");
    assert_eq!(&updated[..expected.len()], expected);
    assert!(matches!(
        &updated.last().expect("scratchpad").widgets[0].content,
        Some(mobius::protocol::FrontendWidgetContent::ActionList { items, .. })
            if items[0].text == "Shared context"
    ));
    assert_eq!(
        gateway
            .contributions()
            .await
            .expect("updated global contributions"),
        updated
    );

    let rejection = gateway
        .submit_contribution(Op::CapabilityCommand {
            capability: "scratchpad".into(),
            command: "scratchpad".into(),
            arguments: "forget session note-1".into(),
            input: None,
            target: None,
        })
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
    let gateway = GatewayHost::start(store, config, credentials, Arc::clone(&bots))
        .await
        .expect("gateway");
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
                    symbol: None,
                    duration_ms: None,
                    started_at_ms: None,
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
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
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
async fn delete_sessions_remove_distinct_roots_and_collapse_duplicate_descendants() {
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
    let deleted = create_test_session(&gateway, &workspace)
        .await
        .expect("create deleted session");
    let also_deleted = create_test_session(&gateway, &workspace)
        .await
        .expect("create second deleted session");
    let retained = create_test_session(&gateway, &workspace)
        .await
        .expect("create retained session");
    let deleted_id = deleted.session_id().to_owned();
    let also_deleted_id = also_deleted.session_id().to_owned();
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
    gateway
        .rename_session(&deleted_id, "Deleted")
        .await
        .expect("title deleted session");
    gateway
        .rename_session(&retained_id, "Retained")
        .await
        .expect("title retained session");
    gateway
        .delete_sessions(&[
            deleted_id.clone(),
            "deleted-child".into(),
            also_deleted_id.clone(),
            deleted_id.clone(),
        ])
        .await
        .expect("delete sessions");

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
        checkpoints
            .load(&also_deleted_id)
            .await
            .expect("load second deleted")
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
    let root_id = root_host.session_id().to_owned();
    let other_id = other_host.session_id().to_owned();
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
        if let Some(HostCommand::ProviderCutoverStatus { reply }) = receiver.recv().await {
            let _ = reply.send(ProviderCutoverStatus { idle: false });
        }
    });
    let (events, _) = broadcast::channel(1);
    gateway.state.lock().await.sessions.insert(
        "child".into(),
        HostHandle {
            inner: Arc::new(HostInner {
                session_id: "child".into(),
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

    let error = gateway
        .delete_sessions(&[other_id.clone(), root_id.clone()])
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
    assert!(
        checkpoints
            .load(&other_id)
            .await
            .expect("load other root")
            .is_some()
    );
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
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");

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
    let gateway = GatewayHost::start(store, config, credentials, Arc::clone(&bots))
        .await
        .expect("gateway");
    let original = create_test_session(&gateway, &workspace)
        .await
        .expect("create chat");
    let session_id = original.session_id().to_string();

    assert!(original.stop_if_idle().await);
    assert!(!original.is_alive());
    let rejection = original
        .begin_session_file_mutation(&bots)
        .expect_err("stopped chat must reject a stale upload");
    assert_eq!(rejection.code, "gateway_stopped");
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
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
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
    checkpoint.session_context.owner_id = Uuid::new_v4().to_string();
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
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    let stop_entered = Arc::new(tokio::sync::Notify::new());
    let stop_release = Arc::new(tokio::sync::Notify::new());
    let mut state = gateway.state.lock().await;
    for index in 0..MAX_ACTIVE_SESSIONS {
        let (commands, mut receiver) = mpsc::channel(1);
        let entered = Arc::clone(&stop_entered);
        let release = Arc::clone(&stop_release);
        tokio::spawn(async move {
            if let Some(HostCommand::StopIfIdle { reply }) = receiver.recv().await {
                entered.notify_one();
                release.notified().await;
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
    }

    drop(state);
    let reclaim = {
        let gateway = gateway.clone();
        tokio::spawn(async move { gateway.ensure_capacity().await })
    };
    stop_entered.notified().await;
    let guard = tokio::time::timeout(std::time::Duration::from_secs(1), gateway.state.lock())
        .await
        .expect("capacity reclaim must not hold global state while awaiting a chat");
    assert_eq!(guard.sessions.len(), MAX_ACTIVE_SESSIONS - 1);
    drop(guard);
    stop_release.notify_one();
    reclaim
        .await
        .expect("reclaim task")
        .expect("reclaim capacity");

    let state = gateway.state.lock().await;
    assert_eq!(state.sessions.len(), MAX_ACTIVE_SESSIONS - 1);
}
