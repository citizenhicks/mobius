use super::*;

fn cron_schedule(expression: &str) -> crate::wire::CronSchedule {
    crate::wire::CronSchedule {
        kind: crate::wire::CronScheduleKind::Cron,
        at: None,
        every_seconds: None,
        expression: Some(expression.into()),
        time_zone: Some("UTC".into()),
    }
}

#[tokio::test]
async fn ready_exposes_the_global_scratchpad_without_a_session() {
    let root = tempfile::tempdir().expect("root");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
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
        .submit_global_scratchpad(Op::CapabilityCommand {
            capability: "scratchpad".into(),
            command: "scratchpad".into(),
            arguments: "refresh".into(),
            input: None,
            target: None,
        })
        .await
        .expect("refresh global scratchpad");
    assert_eq!(refreshed.capability, "scratchpad");

    let rejection = gateway
        .submit_global_scratchpad(Op::CapabilityCommand {
            capability: "scratchpad".into(),
            command: "scratchpad".into(),
            arguments: "forget session note-1".into(),
            input: None,
            target: None,
        })
        .await
        .expect_err("session operations need a selected chat");
    assert_eq!(rejection.code, "invalid_global_scratchpad");
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
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway =
        GatewayHost::start(store, config, credentials, Arc::clone(&cron)).expect("gateway");
    let host = gateway
        .create_session(&workspace)
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
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let host = gateway
        .create_session(&workspace)
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
async fn replacement_ready_precedes_every_reconciled_startup_event() {
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
    let host = gateway
        .create_session(&workspace)
        .await
        .expect("create session");
    let before = host.snapshot(None).await.expect("initial snapshot").ready;
    let mut composition = before.config.config.clone();
    composition.system_prompt.push_str("\nupdated");
    let mut updates = host.subscribe();

    host.configure(before.config.revision, composition)
        .await
        .expect("replace agent");

    let changed = updates.try_recv().expect("session changed");
    let ServerMessage::SessionChanged { payload } = changed.message else {
        panic!("replacement must publish ready before startup events");
    };
    let startup = std::iter::from_fn(|| updates.try_recv().ok()).collect::<Vec<_>>();
    assert!(!payload.widgets.is_empty());
    assert!(startup.iter().any(|frame| {
        matches!(
            &frame.message,
            ServerMessage::AgentEvent {
                record: RecordedEvent {
                    event: Event {
                        msg: EventMsg::Frontend(FrontendEvent::Widget { .. }),
                        ..
                    },
                    ..
                },
                ..
            }
        )
    }));
    assert!(startup.iter().all(|frame| {
        event_sequence(frame).is_none_or(|sequence| {
            sequence > before.latest_sequence && sequence <= payload.latest_sequence
        })
    }));

    host.submit(Submission {
        id: "post-replacement".into(),
        op: Op::Interrupt {
            turn_id: "not-active".into(),
        },
    })
    .await
    .expect("submit after replacement");
    let sequences = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        let mut sequences = Vec::new();
        loop {
            let frame = updates.recv().await.expect("post-replacement event");
            let ServerMessage::AgentEvent { record, .. } = frame.message else {
                continue;
            };
            sequences.push(record.sequence);
            if record.event.submission_id.as_deref() == Some("post-replacement") {
                return sequences;
            }
        }
    })
    .await
    .expect("post-replacement delivery");
    assert_eq!(
        sequences.first().copied(),
        payload.latest_sequence.checked_add(1)
    );
    assert!(
        sequences
            .windows(2)
            .all(|pair| pair[1] == pair[0].saturating_add(1))
    );
}

#[tokio::test]
async fn provider_replacement_cuts_over_defaults_and_every_chat_before_ack() {
    let root = tempfile::tempdir().expect("root");
    let first_workspace = root.path().join("first");
    let second_workspace = root.path().join("second");
    std::fs::create_dir(&first_workspace).expect("first workspace");
    std::fs::create_dir(&second_workspace).expect("second workspace");
    let state_dir = root.path().join("state");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state_dir.clone(), listen, None).expect("config");
    let old = ProviderConfig {
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openai/gpt-5".into(),
        base_url: Some("https://old.example/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let config = config
        .registering_provider(
            old.clone(),
            "Test".into(),
            Default::default(),
            vec![old.model.clone()],
            Vec::new(),
        )
        .expect("register old provider route");
    store.save(&config).expect("save old provider route");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let (stale_commands, stale_receiver) = mpsc::channel(1);
    drop(stale_receiver);
    let (stale_events, _) = broadcast::channel(1);
    gateway.state.lock().await.sessions.insert(
        "stale".into(),
        HostHandle {
            inner: Arc::new(HostInner {
                session_id: "stale".into(),
                commands: stale_commands,
                events: stale_events,
                accepts_file_attachments: Arc::new(AtomicBool::new(false)),
                alive: Arc::new(AtomicBool::new(false)),
            }),
        },
    );
    let first = gateway
        .create_session(&first_workspace)
        .await
        .expect("resident chat");
    let second = gateway
        .create_session(&second_workspace)
        .await
        .expect("persisted chat");
    let first_id = first.session_id().to_owned();
    let second_id = second.session_id().to_owned();
    assert!(second.stop_if_idle().await);
    while second.is_alive() {
        tokio::task::yield_now().await;
    }
    gateway.state.lock().await.sessions.remove(&second_id);
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let before = checkpoints
        .load(&first_id)
        .await
        .expect("load resident chat")
        .expect("resident checkpoint");
    let replacement = ProviderConfig {
        base_url: Some("https://new.example/v1".into()),
        ..old
    };

    let ready = gateway
        .register_provider(
            replacement.clone(),
            "Test".into(),
            Default::default(),
            vec![replacement.model.clone()],
            Vec::new(),
            true,
        )
        .await
        .expect("replace provider route");

    let default = ready.default_config.as_ref().expect("gateway default");
    assert_eq!(default.revision, 2);
    assert_eq!(default.config.provider, replacement);
    assert!(first.is_alive(), "the resident chat remains available");
    assert!(!gateway.state.lock().await.sessions.contains_key("stale"));
    assert_eq!(
        gateway
            .state
            .lock()
            .await
            .sessions
            .get(&first_id)
            .expect("resident chat")
            .inner
            .session_id,
        first.inner.session_id
    );
    for id in [&first_id, &second_id] {
        let checkpoint = checkpoints
            .load(id)
            .await
            .expect("load replaced chat")
            .expect("replaced checkpoint");
        let spec = ChatSpec::from_metadata(&checkpoint.metadata, &state_dir, None)
            .expect("replaced chat spec");
        assert_eq!(spec.agent.revision, 2);
        assert_eq!(spec.agent.config.provider, replacement);
    }
    let (_, persisted) = ConfigStore::open(state_dir).expect("persisted gateway config");
    assert_eq!(
        persisted
            .default_agent
            .expect("persisted default")
            .config
            .provider,
        replacement
    );

    let reopened = gateway
        .open_session(&first_id)
        .await
        .expect("reopen replaced chat");
    assert!(Arc::ptr_eq(&reopened.inner, &first.inner));
    let repaired = checkpoints
        .load(&first_id)
        .await
        .expect("load repaired chat")
        .expect("repaired checkpoint");
    gateway
        .register_provider(
            replacement.clone(),
            "Test".into(),
            Default::default(),
            vec![replacement.model.clone()],
            Vec::new(),
            true,
        )
        .await
        .expect("idempotent provider replacement");
    assert!(
        reopened.is_alive(),
        "an exact rerun must not evict the chat"
    );
    assert_eq!(
        checkpoints
            .load(&first_id)
            .await
            .expect("load idempotent chat")
            .expect("idempotent checkpoint")
            .sequence,
        repaired.sequence
    );
    assert!(repaired.sequence > before.sequence);
}

#[tokio::test]
async fn provider_replacement_rejects_before_mutation_when_a_chat_is_busy() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_dir = root.path().join("state");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state_dir.clone(), listen, None).expect("config");
    let old = ProviderConfig {
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openai/gpt-5".into(),
        base_url: Some("https://old.example/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let config = config
        .registering_provider(
            old.clone(),
            "Test".into(),
            Default::default(),
            vec![old.model.clone()],
            Vec::new(),
        )
        .expect("register old provider route");
    store.save(&config).expect("save old provider route");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let idle = gateway
        .create_session(&workspace)
        .await
        .expect("idle resident chat");
    let idle_id = idle.session_id().to_owned();
    let before = gateway
        .state
        .lock()
        .await
        .checkpoints
        .load(&idle_id)
        .await
        .expect("load idle chat")
        .expect("idle checkpoint");
    let (commands, mut receiver) = mpsc::channel(1);
    let busy_selection = old.clone();
    tokio::spawn(async move {
        if let Some(HostCommand::ProviderCutoverStatus { reply }) = receiver.recv().await {
            let _ = reply.send(ProviderCutoverStatus {
                selection: busy_selection,
                provider_epoch: 0,
                idle: false,
            });
        }
    });
    let (events, _) = broadcast::channel(1);
    gateway.state.lock().await.sessions.insert(
        "busy".into(),
        HostHandle {
            inner: Arc::new(HostInner {
                session_id: "busy".into(),
                commands,
                events,
                accepts_file_attachments: Arc::new(AtomicBool::new(false)),
                alive: Arc::new(AtomicBool::new(true)),
            }),
        },
    );
    let replacement = ProviderConfig {
        base_url: Some("https://new.example/v1".into()),
        ..old.clone()
    };

    let error = gateway
        .register_provider(
            replacement,
            "Test".into(),
            Default::default(),
            vec![old.model.clone()],
            Vec::new(),
            true,
        )
        .await
        .expect_err("busy chat must block provider replacement");

    assert_eq!(error.code, "agent_busy");
    assert!(
        idle.is_alive(),
        "preflight must not stop an earlier idle chat"
    );
    assert!(Arc::ptr_eq(
        &gateway.state.lock().await.sessions[&idle_id].inner,
        &idle.inner
    ));
    assert_eq!(
        gateway
            .state
            .lock()
            .await
            .checkpoints
            .load(&idle_id)
            .await
            .expect("reload idle chat")
            .expect("idle checkpoint after rejection")
            .sequence,
        before.sequence
    );
    assert_eq!(
        gateway
            .state
            .lock()
            .await
            .config
            .lock()
            .expect("gateway config")
            .configured_providers["openrouter"]
            .selection,
        old
    );
    assert_eq!(
        ConfigStore::open(state_dir)
            .expect("persisted gateway config")
            .1
            .default_agent
            .expect("persisted default")
            .config
            .provider,
        old
    );
}

#[tokio::test]
async fn provider_cutover_gate_rejects_a_concurrent_route_change() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_dir = root.path().join("state");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state_dir, listen, None).expect("config");
    let current = ProviderConfig {
        instance: "kimi".into(),
        provider: "kimi".into(),
        model: "kimi-k3".into(),
        base_url: Some("https://api.moonshot.ai/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: Some("max".into()),
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let retiring = ProviderConfig {
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openai/gpt-5".into(),
        base_url: Some("https://old.example/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let config = config
        .registering_provider(
            current.clone(),
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("register current provider")
        .registering_provider(
            retiring.clone(),
            "Test".into(),
            Default::default(),
            vec![retiring.model.clone()],
            Vec::new(),
        )
        .expect("register retiring route");
    store.save(&config).expect("save providers");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    credentials
        .set(
            "kimi",
            "kimi",
            "test-secret",
            Some("https://api.moonshot.ai/v1"),
        )
        .expect("Kimi credential");
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let host = gateway.create_session(&workspace).await.expect("chat");
    let before = host.snapshot(None).await.expect("chat snapshot").ready;
    let gate = Arc::clone(&gateway.state.lock().await.session_mutations);
    let _cutover = gate.write().await;
    let mut composition = before.config.config.clone();
    composition.provider = retiring;

    let error = host
        .configure(before.config.revision, composition)
        .await
        .expect_err("route change must not cross the provider cutover gate");

    assert_eq!(error.code, "gateway_busy");
    assert_eq!(
        host.snapshot(None)
            .await
            .expect("unchanged chat")
            .ready
            .config,
        before.config
    );
    assert_eq!(
        host.snapshot(None)
            .await
            .expect("unchanged provider")
            .ready
            .config
            .config
            .provider,
        current
    );
}

#[tokio::test]
async fn provider_replacement_save_failure_keeps_resident_chats_available() {
    let root = tempfile::tempdir().expect("root");
    let first_workspace = root.path().join("first");
    let second_workspace = root.path().join("second");
    std::fs::create_dir(&first_workspace).expect("first workspace");
    std::fs::create_dir(&second_workspace).expect("second workspace");
    let state_dir = root.path().join("state");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state_dir.clone(), listen, None).expect("config");
    let old = ProviderConfig {
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openai/gpt-5".into(),
        base_url: Some("https://old.example/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let config = config
        .registering_provider(
            old.clone(),
            "Test".into(),
            Default::default(),
            vec![old.model.clone()],
            Vec::new(),
        )
        .expect("register old provider route");
    store.save(&config).expect("save old provider route");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let resident = gateway
        .create_session(&first_workspace)
        .await
        .expect("resident chat");
    let persisted = gateway
        .create_session(&second_workspace)
        .await
        .expect("persisted chat");
    let resident_id = resident.session_id().to_owned();
    let persisted_id = persisted.session_id().to_owned();
    assert!(persisted.stop_if_idle().await);
    while persisted.is_alive() {
        tokio::task::yield_now().await;
    }
    gateway.state.lock().await.sessions.remove(&persisted_id);
    let config_path = state_dir.join("gateway.toml");
    std::fs::remove_file(&config_path).expect("remove gateway config");
    std::fs::create_dir(&config_path).expect("block gateway config save");
    let replacement = ProviderConfig {
        base_url: Some("https://new.example/v1".into()),
        ..old.clone()
    };

    gateway
        .register_provider(
            replacement,
            "Test".into(),
            Default::default(),
            vec![old.model.clone()],
            Vec::new(),
            true,
        )
        .await
        .expect_err("gateway config save must fail");

    assert!(resident.is_alive());
    assert!(Arc::ptr_eq(
        &gateway.state.lock().await.sessions[&resident_id].inner,
        &resident.inner
    ));
    assert_eq!(
        gateway
            .state
            .lock()
            .await
            .config
            .lock()
            .expect("gateway config")
            .configured_providers["openrouter"]
            .selection,
        old
    );
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    for id in [resident_id, persisted_id] {
        let checkpoint = checkpoints
            .load(&id)
            .await
            .expect("load chat")
            .expect("chat checkpoint");
        assert_eq!(
            ChatSpec::from_metadata(&checkpoint.metadata, &state_dir, None)
                .expect("chat spec")
                .agent
                .config
                .provider,
            old
        );
    }
}

#[tokio::test]
async fn provider_replacement_retry_migrates_a_stale_resident_router() {
    let root = tempfile::tempdir().expect("root");
    let state_dir = root.path().join("state");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state_dir, listen, None).expect("config");
    let old = ProviderConfig {
        instance: "openrouter".into(),
        provider: "openrouter".into(),
        model: "openai/gpt-5".into(),
        base_url: Some("https://old.example/v1".into()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::Credentialless,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let config = config
        .registering_provider(
            old.clone(),
            "Test".into(),
            Default::default(),
            vec![old.model.clone()],
            Vec::new(),
        )
        .expect("register old provider route");
    store.save(&config).expect("save old provider route");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let (commands, mut receiver) = mpsc::channel(4);
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let task_attempts = Arc::clone(&attempts);
    let stale_selection = old.clone();
    tokio::spawn(async move {
        while let Some(command) = receiver.recv().await {
            match command {
                HostCommand::ProviderCutoverStatus { reply } => {
                    let _ = reply.send(ProviderCutoverStatus {
                        selection: stale_selection.clone(),
                        provider_epoch: 0,
                        idle: true,
                    });
                }
                HostCommand::CutOverProvider { reply, .. } => {
                    let attempt = task_attempts.fetch_add(1, Ordering::Relaxed);
                    let result = if attempt == 0 {
                        Err(Rejection {
                            code: "host_error",
                            message: "injected migration failure".into(),
                            fatal: false,
                        })
                    } else {
                        Ok(())
                    };
                    let _ = reply.send(result);
                }
                _ => panic!("unexpected command during provider cutover"),
            }
        }
    });
    let (events, _) = broadcast::channel(1);
    gateway.state.lock().await.sessions.insert(
        "resident".into(),
        HostHandle {
            inner: Arc::new(HostInner {
                session_id: "resident".into(),
                commands,
                events,
                accepts_file_attachments: Arc::new(AtomicBool::new(false)),
                alive: Arc::new(AtomicBool::new(true)),
            }),
        },
    );
    let replacement = ProviderConfig {
        base_url: Some("https://new.example/v1".into()),
        ..old.clone()
    };

    gateway
        .register_provider(
            replacement.clone(),
            "Test".into(),
            Default::default(),
            vec![old.model.clone()],
            Vec::new(),
            true,
        )
        .await
        .expect_err("first actor migration must fail");
    gateway
        .register_provider(
            replacement,
            "Test".into(),
            Default::default(),
            vec![old.model],
            Vec::new(),
            true,
        )
        .await
        .expect("retry must revisit the stale resident router");

    assert_eq!(attempts.load(Ordering::Relaxed), 2);
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
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway =
        GatewayHost::start(store, config, credentials, Arc::clone(&cron)).expect("gateway");
    let deleted = gateway
        .create_session(&workspace)
        .await
        .expect("create deleted session");
    let retained = gateway
        .create_session(&workspace)
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
    checkpoints
        .fork(
            &deleted_id,
            parent.sequence,
            &Checkpoint::empty("deleted-child"),
        )
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
    let task = cron
        .add_for_test(
            &deleted_id,
            "scheduled task",
            cron_schedule("0 9 * * *"),
            None,
        )
        .expect("schedule task");
    let run = match cron.begin_run(&task.id).expect("begin run") {
        BeginRun::Started(run) => run,
        BeginRun::Skipped => panic!("new run must start"),
    };
    cron.finish_run(run, CronRunStatus::Succeeded, None)
        .expect("finish run");

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
    assert!(
        cron.list()
            .expect("remaining schedules")
            .iter()
            .all(|task| task.session_id != deleted_id)
    );
    assert!(
        cron.history(None)
            .expect("remaining schedule history")
            .iter()
            .all(|run| run.source_session_id != deleted_id)
    );
    assert!(!task.task.exists());
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
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let root_host = gateway
        .create_session(&workspace)
        .await
        .expect("create root session");
    let root_id = root_host.session_id().to_owned();
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let root_checkpoint = checkpoints
        .load(&root_id)
        .await
        .expect("load root")
        .expect("root checkpoint");
    checkpoints
        .fork(
            &root_id,
            root_checkpoint.sequence,
            &Checkpoint::empty("child"),
        )
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
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");

    for session_id in [" ".to_owned(), "x".repeat(4097)] {
        let error = match gateway.open_session(&session_id).await {
            Ok(_) => panic!("invalid session ID must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.code, "invalid_session_id");
        assert_eq!(error.message, "session ID must be 1–4096 bytes");
    }
}

#[test]
fn scheduled_execution_spec_frames_execution() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&state).expect("state");
    let config = AgentComposition {
        system_prompt: "base instructions".into(),
        ..AgentComposition::default()
    };
    let spec = ChatSpec::new(
        &workspace,
        VersionedAgentConfig {
            revision: 1,
            config,
        },
        &state,
        None,
    )
    .expect("chat spec");

    let execution = scheduled_execution_spec(spec);
    let restored = ChatSpec::from_metadata(
        &execution.metadata().expect("execution metadata"),
        &state,
        None,
    )
    .expect("restore execution spec");

    assert_eq!(
        restored.agent.config.system_prompt,
        "base instructions\n\nExecute the scheduled task in the next user message. Do not create or modify schedules."
    );
}

#[test]
fn cron_execution_inherits_the_chat_recipe_without_transcript_state() {
    let mut source = Checkpoint::empty("source");
    source.context.push(serde_json::json!({"role": "user"}));
    source.first_user_message = Some("source message".into());
    source.model_route = Some("kimi::kimi-k2.5::high".into());
    source.metadata.insert(
        "mobius_gateway.chat".into(),
        serde_json::json!({"version": 1}),
    );
    source.session_context.workspace_id = Some("workspace".into());

    let execution = cron_execution_checkpoint(&source, "execution", "cron · task");

    assert_eq!(execution.model_route, source.model_route);
    assert_eq!(
        execution.metadata["mobius_gateway.chat"],
        source.metadata["mobius_gateway.chat"]
    );
    assert_eq!(
        execution.metadata["mobius_gateway.cron_execution"],
        serde_json::json!("execution")
    );
    assert!(is_cron_execution_checkpoint(&execution));
    let mut inherited = execution.clone();
    inherited.session_id = "child".into();
    assert!(!is_cron_execution_checkpoint(&inherited));
    assert_eq!(
        execution.session_context,
        mobius::protocol::SessionContext {
            workspace_id: Some("workspace".into()),
            origin_label: Some("cron · task".into()),
            ..mobius::protocol::SessionContext::default()
        }
    );
    assert!(execution.context.is_empty());
    assert!(execution.first_user_message.is_none());
    assert_eq!(execution.sequence, 0);
}

#[test]
fn stopped_agent_finishes_its_active_cron_run() {
    let state = tempfile::tempdir().expect("state");
    let cron = CronStore::open(state.path()).expect("cron");
    let task = cron
        .add_for_test("source", "do work", cron_schedule("17 3 * * *"), None)
        .expect("task");
    let run = match cron.begin_run(&task.id).expect("begin run") {
        BeginRun::Started(run) => run,
        BeginRun::Skipped => panic!("new task must start"),
    };
    let mut active = Some(ActiveCron {
        run,
        submission_id: "submission".into(),
        turn_id: None,
        failure: None,
    });

    fail_active_cron(&cron, &mut active, "agent stopped").expect("finish run");
    let history = cron.history(Some(&task.id)).expect("history");

    assert!(active.is_none());
    assert_eq!(history[0].status, CronRunStatus::Failed);
    assert_eq!(history[0].message.as_deref(), Some("agent stopped"));
}

#[tokio::test]
async fn scheduled_task_creation_requires_a_visible_source_chat() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway =
        GatewayHost::start(store, config, credentials, Arc::clone(&cron)).expect("gateway");

    let missing = gateway
        .create_cron("missing", "do work", cron_schedule("0 9 * * *"), None)
        .await
        .expect_err("missing source chat must be rejected");
    assert_eq!(missing.code, "unknown_session");

    let source = gateway
        .create_session(&workspace)
        .await
        .expect("source chat");
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let source_checkpoint = checkpoints
        .load(source.session_id())
        .await
        .expect("load source")
        .expect("source checkpoint");
    checkpoints
        .fork(
            source.session_id(),
            source_checkpoint.sequence,
            &cron_execution_checkpoint(&source_checkpoint, "hidden", "scheduled"),
        )
        .await
        .expect("fork hidden session");

    let hidden = gateway
        .create_cron("hidden", "do work", cron_schedule("0 9 * * *"), None)
        .await
        .expect_err("hidden source chat must be rejected");
    assert_eq!(hidden.code, "unknown_session");

    gateway
        .create_cron(
            source.session_id(),
            "do work",
            cron_schedule("0 9 * * *"),
            None,
        )
        .await
        .expect("visible source chat");
    assert_eq!(cron.list().expect("tasks").len(), 1);
}

#[tokio::test]
async fn due_preflight_failure_finishes_the_reserved_run() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway =
        GatewayHost::start(store, config, credentials, Arc::clone(&cron)).expect("gateway");
    let source = gateway
        .create_session(&workspace)
        .await
        .expect("source chat");
    let now = Utc::now().timestamp();
    let task = cron
        .add_for_test(
            source.session_id(),
            "do work",
            crate::wire::CronSchedule {
                kind: crate::wire::CronScheduleKind::Once,
                at: Some(now - 1),
                every_seconds: None,
                expression: None,
                time_zone: None,
            },
            None,
        )
        .expect("task");
    std::fs::remove_file(&task.task).expect("remove task input");
    let mut due = cron.take_due(now).expect("reserve due run");
    assert_eq!(due.len(), 1);
    let (task_id, run) = due.pop().expect("due run");

    gateway
        .run_due_cron(task_id, run)
        .await
        .expect_err("missing task input must fail preflight");

    let history = cron.history(Some(&task.id)).expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, CronRunStatus::Failed);
    assert!(history[0].finished_at.is_some());
    assert_eq!(cron.task(&task.id).expect("task").next_run_at, None);
}

#[tokio::test]
async fn overlapping_cron_does_not_create_a_visible_execution_chat() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let state_dir = root.path().join("state");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state_dir, listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway =
        GatewayHost::start(store, config, credentials, Arc::clone(&cron)).expect("gateway");
    let source = gateway
        .create_session(&workspace)
        .await
        .expect("source chat");
    let task = cron
        .add_for_test(
            source.session_id(),
            "do work",
            cron_schedule("* * * * *"),
            None,
        )
        .expect("task");
    let held = match cron.begin_run(&task.id).expect("claim run") {
        BeginRun::Started(run) => run,
        BeginRun::Skipped => panic!("first run must start"),
    };
    let before = gateway.sessions().await.expect("sessions before");

    let error = gateway
        .run_cron(task.id)
        .await
        .expect_err("overlap must fail");
    let after = gateway.sessions().await.expect("sessions after");
    cron.finish_run(held, CronRunStatus::Succeeded, None)
        .expect("finish held run");

    assert_eq!(error.code, "cron_overlap");
    assert_eq!(after, before);
}

#[tokio::test]
async fn repeated_cron_run_previews_do_not_consume_resident_capacity() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let state_dir = root.path().join("state");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state_dir, listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway =
        GatewayHost::start(store, config, credentials, Arc::clone(&cron)).expect("gateway");
    let source = gateway
        .create_session(&workspace)
        .await
        .expect("source chat");
    let source_id = source.session_id().to_owned();
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let source_checkpoint = checkpoints
        .load(&source_id)
        .await
        .expect("load source")
        .expect("source checkpoint");
    let task = cron
        .add_for_test(&source_id, "do work", cron_schedule("* * * * *"), None)
        .expect("task");
    assert!(source.stop_if_idle().await);
    while source.is_alive() {
        tokio::task::yield_now().await;
    }
    gateway.state.lock().await.sessions.remove(&source_id);
    drop(source);

    for index in 0..MAX_ACTIVE_SESSIONS {
        let execution_id = format!("execution-{index}");
        let execution = cron_execution_checkpoint(&source_checkpoint, &execution_id, "preview");
        checkpoints
            .fork(&source_id, source_checkpoint.sequence, &execution)
            .await
            .expect("fork execution checkpoint");
        let run = match cron.begin_run(&task.id).expect("begin run") {
            BeginRun::Started(run) => run,
            BeginRun::Skipped => panic!("new run must start"),
        };
        cron.attach_execution_session(&run, &execution_id)
            .expect("attach execution session");
        let run = cron
            .finish_run(run, CronRunStatus::Succeeded, None)
            .expect("finish run");

        gateway
            .cron_run_preview(&run.id, None)
            .await
            .expect("preview run");
        assert!(gateway.state.lock().await.sessions.is_empty());
    }
}

#[tokio::test]
async fn chats_keep_independent_workspace_and_agent_configuration() {
    let root = tempfile::tempdir().expect("root");
    let first = root.path().join("first");
    let second = root.path().join("second");
    let state = root.path().join("state");
    std::fs::create_dir(&first).expect("first workspace");
    std::fs::create_dir(&second).expect("second workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state, listen, None).expect("config");
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
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credential store"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway =
        GatewayHost::start(store, config, credentials, Arc::clone(&cron)).expect("gateway");
    let first_host = gateway.create_session(&first).await.expect("first chat");
    let second_host = gateway.create_session(&second).await.expect("second chat");
    let scheduled = cron
        .add_for_test(
            first_host.session_id(),
            "keep scheduled work",
            cron_schedule("0 9 * * *"),
            None,
        )
        .expect("scheduled task");
    let first_before = first_host
        .snapshot(None)
        .await
        .expect("first snapshot")
        .ready;
    let second_before = second_host
        .snapshot(None)
        .await
        .expect("second snapshot")
        .ready;
    let mut composition = first_before.config.config.clone();
    composition.system_prompt.push_str("\nupdated");

    first_host
        .configure(first_before.config.revision, composition)
        .await
        .expect("configure first chat");
    let first_after = first_host
        .snapshot(None)
        .await
        .expect("first updated")
        .ready;
    let second_after = second_host
        .snapshot(None)
        .await
        .expect("second unchanged")
        .ready;

    assert_ne!(first_after.workspace, second_after.workspace);
    assert_eq!(first_after.tool_count, first_before.tool_count);
    assert_eq!(
        cron.list()
            .expect("existing schedules")
            .into_iter()
            .find(|task| task.id == scheduled.id)
            .map(|task| task.id),
        Some(scheduled.id)
    );
    assert!(
        first_after
            .contributions
            .iter()
            .any(|contribution| contribution.capability == "sessions"),
        "the /resume picker is gateway-standard, not an optional agent feature"
    );
    assert_eq!(second_after.config, second_before.config);

    let first_id = first_host.session_id().to_owned();
    let second_id = second_host.session_id().to_owned();
    let (first_renamed, second_renamed) = tokio::join!(
        first_host.rename_session(first_id.clone(), "first".into()),
        second_host.rename_session(second_id.clone(), "second".into())
    );
    first_renamed.expect("rename first chat");
    second_renamed.expect("rename second chat");
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let metadata = load_session_metadata(&checkpoints)
        .await
        .expect("catalog metadata");
    assert_eq!(metadata[&first_id].title.as_deref(), Some("first"));
    assert_eq!(metadata[&second_id].title.as_deref(), Some("second"));
}

#[tokio::test]
async fn model_selection_updates_only_the_chat_and_new_chats_keep_the_gateway_default() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    std::fs::create_dir(&workspace).expect("workspace");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state, listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credential store"));
    credentials
        .set("openai_socket", "openai_socket", "test-secret", None)
        .expect("OpenAI credential");
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let mut gateway_updates = gateway.subscribe();
    let ready = gateway
        .register_provider(
            ProviderConfig {
                instance: "openai_socket".into(),
                provider: "openai_socket".into(),
                model: "gpt-5.6-sol".into(),
                base_url: None,
                endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: Some("medium".into()),
                web_search: mobius::backend::model::provider::HostedWebSearch::Off,
            },
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
            false,
        )
        .await
        .expect("register OpenAI");
    let broadcast = gateway_updates
        .try_recv()
        .expect("gateway-wide catalog update");
    assert!(matches!(
        broadcast.message,
        ServerMessage::Ready { payload } if payload.models == ready.models
    ));
    let alternate = ready
        .models
        .iter()
        .find(|choice| {
            choice.model == "gpt-5.6-terra" && choice.reasoning_effort.as_deref() == Some("high")
        })
        .expect("alternate OpenAI model")
        .route
        .clone();
    let selected = gateway
        .create_session(&workspace)
        .await
        .expect("selected chat");
    let mut selected_config = selected
        .snapshot(None)
        .await
        .expect("selected snapshot")
        .ready
        .config
        .config;
    selected_config.provider.web_search = mobius::backend::model::provider::HostedWebSearch::Live;
    selected
        .configure(1, selected_config)
        .await
        .expect("configure selected chat search");

    selected
        .submit(Submission {
            id: "set-model".into(),
            op: Op::SetModel {
                route: alternate.clone(),
            },
        })
        .await
        .expect("select alternate model");
    let selected_ready = selected
        .snapshot(None)
        .await
        .expect("selected snapshot")
        .ready;
    let fresh = gateway
        .create_session(&workspace)
        .await
        .expect("fresh chat");
    let fresh_ready = fresh.snapshot(None).await.expect("fresh snapshot").ready;

    assert_eq!(selected_ready.session.model.route, alternate);
    assert_eq!(selected_ready.config.config.provider.model, "gpt-5.6-terra");
    assert_eq!(
        selected_ready.config.config.provider.web_search,
        mobius::backend::model::provider::HostedWebSearch::Live
    );
    assert_eq!(fresh_ready.config.config.provider.model, "gpt-5.6-sol");
    assert_eq!(
        fresh_ready.config.config.provider.web_search,
        mobius::backend::model::provider::HostedWebSearch::Off
    );
    assert_ne!(selected.session_id(), fresh.session_id());
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
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
    let original = gateway
        .create_session(&workspace)
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
async fn capacity_reclaims_an_unreferenced_idle_chat() {
    let root = tempfile::tempdir().expect("root");
    let state_dir = root.path().join("state");
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) = ConfigStore::initialize(state_dir, listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    let gateway = GatewayHost::start(store, config, credentials, cron).expect("gateway");
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
