use super::*;
use mobius::backend::checkpoint::{CheckpointStore, sqlite::SqliteCheckpoint};
use mobius::backend::model::{ModelRouter, openai::OpenAi};
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
async fn voice_delegation_runs_one_bot_model_call_and_keeps_its_transcript() {
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
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(
                request["input"]
                    .to_string()
                    .contains("Use blue; preserve toolbar.")
            );
            assert!(request["input"].to_string().contains("Yes, do it."));
            let output = serde_json::json!({"id":"message-1","type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]});
            let body = [
                serde_json::json!({"type":"response.output_item.done","output_index":0,"item":output}),
                serde_json::json!({"type":"response.completed","response":{"id":"response-1","output":[]}}),
            ].into_iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let (store, config) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().unwrap(),
        None,
    )
    .unwrap();
    let provider = crate::wire::ProviderConfig {
        instance: "voice-test".into(),
        provider: "responses".into(),
        model: "local-test".into(),
        base_url: Some(base.clone()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let config = config
        .registering_provider(
            provider,
            "Voice test".into(),
            Default::default(),
            vec!["local-test".into()],
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
    let mut transcript = VoiceTranscript::open(
        Arc::clone(&checkpoints),
        host.session_id(),
        Arc::clone(&frontend),
    )
    .await
    .unwrap();
    let voice_id = transcript.session_id().to_owned();
    let model = crate::host::RealtimeModel {
        bot_name: "Builder".into(),
        bot_instructions: "You are Builder.".into(),
        router: Arc::new(ModelRouter::new(
            &route,
            Arc::new(OpenAi::new("test-key", &base, "local-test").unwrap()),
        )),
        voice: None,
        route,
        provider_instance: "voice-test".into(),
        active_turn_id: None,
        checkpoints: Arc::clone(&checkpoints),
        frontend,
    };
    let (commands, mut received) = mpsc::channel(32);
    let (send, voice_events) = mpsc::channel(8);
    let (cancel, _cancelled) = oneshot::channel();
    let mut call = RealtimeVoiceCall::new(
        "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n".into(),
        commands,
        voice_events,
        cancel,
    )
    .unwrap();
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
    let (stop, stopped) = oneshot::channel();
    let check_reply = async move {
        loop {
            if let Some(RealtimeVoiceCommand::Reply { handoff_id, text }) = received.recv().await {
                assert_eq!(handoff_id, "h1");
                assert!(text.contains("Done."), "{text}");
                stop.send(()).unwrap();
                break;
            }
        }
        assert!(matches!(
            received.recv().await,
            Some(RealtimeVoiceCommand::Close)
        ));
        drop(send);
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        let (result, ()) = tokio::join!(
            drive(
                &host,
                &model,
                &mut call,
                &mut transcript,
                &mut events,
                stopped
            ),
            check_reply
        );
        result.unwrap();
    })
    .await
    .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
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
