use super::*;
use mobius::backend::checkpoint::{CheckpointStore, sqlite::SqliteCheckpoint};
use mobius::backend::model::{
    ModelRouter, RealtimeVoiceCall, RealtimeVoiceCommand, RealtimeVoiceEvent, openai::OpenAi,
};
use mobius::middleware::voice::VoiceCallContext;
use mobius::middleware::voice::transcript::VoiceTranscript;
use mobius::protocol::ConversationRole;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn startup_cancellation_wins_before_voice_result_is_reported() {
    let (stop, mut stopped) = oneshot::channel();
    let task =
        tokio::spawn(
            async move { startup(&mut stopped, std::future::pending::<Result<()>>()).await },
        );
    stop.send(()).expect("stop startup");
    assert!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("startup cancellation")
            .expect("startup task")
            .is_none()
    );

    let (stop, mut stopped) = oneshot::channel();
    stop.send(()).expect("stop ready startup");
    assert!(
        startup(&mut stopped, async { Ok::<_, Error>(()) })
            .await
            .is_none()
    );
}

#[tokio::test]
async fn voice_delegation_preserves_committed_speech_and_delayed_final_transcripts() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = Arc::clone(&calls);
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            while bytes.len() < header_end + length {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            let request: serde_json::Value = serde_json::from_slice(&bytes[header_end..]).unwrap();
            let index = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let input = request["input"].to_string();
            assert_eq!(input.matches("Use blue; preserve toolbar.").count(), 1);
            assert_eq!(input.matches("Yes, do it.").count(), 1);
            if index == 1 {
                assert_eq!(input.matches("Also add keyboard shortcuts.").count(), 1);
                let latest = request["input"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()
                    .to_string();
                assert!(latest.contains("Also add keyboard shortcuts."), "{latest}");
                assert!(!latest.contains("Use blue; preserve toolbar."), "{latest}");
                assert!(!latest.contains("Yes, do it."), "{latest}");
            }
            let output = serde_json::json!({"id":"message-1","type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]});
            let body = [
                serde_json::json!({"type":"response.output_item.done","output_index":0,"item":output}),
                serde_json::json!({"type":"response.completed","response":{"id":"response-1","output":[]}}),
            ].into_iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let (store, mut config) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().unwrap(),
        None,
    )
    .unwrap();
    config.model_transport.voice_io_timeout_ms = 15_000;
    let transport = config.model_transport;
    let provider = crate::wire::ProviderConfig {
        tool_discovery: None,
        instance: "voice-test".into(),
        provider: "responses".into(),
        model: "local-test".into(),
        base_url: Some(base.clone()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: None,
        service_tier: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let config = config
        .registering_provider(
            provider,
            "Voice test".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: "local-test".into(),
                reasoning_efforts: Some(Vec::new()),
                default_reasoning: None,
                ..Default::default()
            }],
            Vec::new(),
        )
        .unwrap();
    store.save(&config).unwrap();
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(store.checkpoints_path()).unwrap());
    let credentials = Arc::new(CredentialStore::open(store.credentials_path()).unwrap());
    credentials
        .set("voice-test", "responses", "test-key", Some(&base), None)
        .unwrap();
    let bots = Arc::new(BotStore::open(store.state_dir()).unwrap());
    let bot = bots
        .create_bot(
            "Builder",
            "Build things.",
            config.bot_defaults.as_ref().unwrap().config.clone(),
        )
        .unwrap();
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .unwrap();
    let host = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let mut events = host.subscribe();
    let parent = checkpoints.load(host.session_id()).await.unwrap().unwrap();
    let route = parent.model_route.clone().unwrap();
    let frontend: mobius::middleware::FrontendEventSink = Arc::new(|_| Ok(()));
    let voice_id = VoiceTranscript::open(
        Arc::clone(&checkpoints),
        host.session_id(),
        Arc::clone(&frontend),
    )
    .await
    .unwrap()
    .session_id()
    .to_owned();
    let voice = "voice-test::gpt-live-1::sol";
    let mut router = ModelRouter::new(
        &route,
        Arc::new(OpenAi::new("test-key", &base, "local-test").unwrap()),
    );
    router
        .register_voice(
            Arc::new(
                OpenAi::new_with_transport("test-key", &base, "gpt-live-1", transport).unwrap(),
            ),
            mobius::protocol::ModelChoice {
                route: voice.into(),
                group: "Voice".into(),
                model: "gpt-live-1".into(),
                reasoning_effort: Some("sol".into()),
                variant_label: None,
                context_window: None,
                supports_image_input: false,
                supports_image_generation: false,
                supports_realtime_voice: true,
                tool_discovery: mobius::protocol::ToolDiscoveryMode::Rebuild,
            },
            Default::default(),
        )
        .unwrap();
    let context = VoiceCallContext {
        session_id: host.session_id().into(),
        router: Arc::new(router),
        voice: voice.into(),
        bot_name: "Builder".into(),
        bot_handle: "builder".into(),
        bot_instructions: "You are Builder.".into(),
        active_turn_id: None,
        checkpoints: Arc::clone(&checkpoints),
        frontend: Arc::clone(&frontend),
    };
    let shutdown_timeout = context.shutdown_timeout().unwrap();
    let (commands, mut received) = mpsc::channel(32);
    let (send, voice_events) = mpsc::channel(8);
    let (cancel, _cancelled) = oneshot::channel();
    let call = RealtimeVoiceCall::new(
        "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n".into(),
        "sol".into(),
        commands,
        voice_events,
        cancel,
    );
    for (id, role, text) in [
        (
            "decision",
            ConversationRole::Assistant,
            "Use blue; preserve toolbar.",
        ),
        ("request", ConversationRole::User, "Yes, do it."),
    ] {
        send.send(Ok(RealtimeVoiceEvent::Transcript {
            id: id.into(),
            role,
            text: text.into(),
            complete: true,
        }))
        .await
        .unwrap();
    }
    for _ in 0..2 {
        send.send(Ok(RealtimeVoiceEvent::Handoff {
            id: "h1".into(),
            text: None,
        }))
        .await
        .unwrap();
    }
    let call = VoiceCall::with_call(context, call).await.unwrap();
    let (stop, stopped) = oneshot::channel();
    let check_reply = async {
        let mut replies = 0;
        loop {
            match received.recv().await.expect("voice command") {
                RealtimeVoiceCommand::Context { text } => {
                    assert!(!text.contains("Use blue; preserve toolbar."), "{text}");
                    assert!(!text.contains("Yes, do it."), "{text}");
                    assert!(!text.contains("Also add keyboard shortcuts."), "{text}");
                    assert!(!text.contains("Recent voice discussion"), "{text}");
                }
                RealtimeVoiceCommand::Reply { handoff_id, text } => {
                    replies += 1;
                    assert_eq!(handoff_id, format!("h{replies}"));
                    assert!(text.contains("Done."), "{text}");
                    let persisted = VoiceTranscript::open(
                        Arc::clone(&checkpoints),
                        host.session_id(),
                        Arc::clone(&frontend),
                    )
                    .await
                    .unwrap();
                    assert!(persisted.handoff_context().await.unwrap().text.is_empty());
                    if replies == 2 {
                        stop.send(()).unwrap();
                        break;
                    }
                    send.send(Ok(RealtimeVoiceEvent::Handoff {
                        id: "h1".into(),
                        text: None,
                    }))
                    .await
                    .unwrap();
                    send.send(Ok(RealtimeVoiceEvent::Transcript {
                        id: "follow-up".into(),
                        role: ConversationRole::User,
                        text: "Also add keyboard shortcuts.".into(),
                        complete: true,
                    }))
                    .await
                    .unwrap();
                    send.send(Ok(RealtimeVoiceEvent::Handoff {
                        id: "h2".into(),
                        text: None,
                    }))
                    .await
                    .unwrap();
                }
                RealtimeVoiceCommand::Close => panic!("closed before both handoffs completed"),
            }
        }
        assert!(matches!(
            received.recv().await,
            Some(RealtimeVoiceCommand::Close)
        ));
        tokio::time::pause();
        tokio::time::sleep(Duration::from_secs(14)).await;
        tokio::time::resume();
        send.send(Ok(RealtimeVoiceEvent::Transcript {
            id: "final-response".into(),
            role: ConversationRole::Assistant,
            text: "The final spoken response.".into(),
            complete: true,
        }))
        .await
        .expect("gateway still accepts the delayed final transcript");
        drop(send);
    };
    tokio::time::timeout(shutdown_timeout + Duration::from_secs(10), async {
        let (result, ()) = tokio::join!(
            drive(&host, &route, call, &mut events, stopped),
            check_reply
        );
        result.unwrap();
    })
    .await
    .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let context = VoiceTranscript::open(Arc::clone(&checkpoints), host.session_id(), frontend)
        .await
        .unwrap()
        .handoff_context()
        .await
        .unwrap()
        .text;
    assert!(context.contains("The final spoken response."));
    for consumed in [
        "Use blue; preserve toolbar.",
        "Yes, do it.",
        "Also add keyboard shortcuts.",
    ] {
        assert!(!context.contains(consumed), "{context}");
    }
    let history = checkpoints
        .event_page(
            &voice_id,
            mobius::backend::checkpoint::EventPageRequest {
                before_sequence: None,
                limit: 128,
            },
        )
        .await
        .unwrap();
    assert!(history.events.iter().any(|record| matches!(&record.event.msg, EventMsg::Message(message) if message.text == "Yes, do it.")));
    gateway.shutdown().await;
    server.abort();
}

#[tokio::test(start_paused = true)]
async fn voice_stop_preserves_the_calls_nondefault_finalization_window() {
    let (stop, stopped) = oneshot::channel();
    let (shutdown, shutdown_timeout) = oneshot::channel();
    shutdown.send(Duration::from_secs(30)).unwrap();
    let (_updates, updates) = mpsc::channel(2);
    let (finished, completion) = oneshot::channel();
    let task = tokio::spawn(async move {
        stopped.await.unwrap();
        tokio::time::sleep(Duration::from_secs(14)).await;
        finished.send(()).unwrap();
    });
    let mut voice = ConnectionVoice {
        session_id: "session".into(),
        voice_id: "voice".into(),
        updates,
        task,
        stop: Some(stop),
        shutdown_timeout,
    };
    voice.stop().await;
    completion
        .await
        .expect("finalization was not aborted at the default deadline");
}
