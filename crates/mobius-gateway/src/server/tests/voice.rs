use super::*;
use mobius::backend::checkpoint::EventPageRequest;

const OFFER: &str =
    "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\na=x-mobius-voice-offer-marker\r\n";

async fn expect_voice_failure(events: &mut GatewayEvents, request: &str, session: &str) -> String {
    loop {
        match next_gateway_message(events).await {
            ServerMessage::RealtimeVoiceFailed {
                request_id,
                session_id,
                message,
            } if request_id == request => {
                assert_eq!(session_id, session);
                return message;
            }
            ServerMessage::RealtimeVoiceStarted { request_id, .. } if request_id == request => {
                panic!("invalid or unsupported voice request started a call");
            }
            ServerMessage::Error {
                fatal: true,
                message,
                ..
            } => panic!("voice request closed the connection: {message}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn signaling_requires_the_selected_session_is_ephemeral_and_keeps_chat_usable() {
    let root = tempfile::tempdir().expect("temporary directory");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let listen = listener.local_addr().expect("listen address");
    let (store, config) = ConfigStore::initialize(root.path().join("state"), listen, None)
        .expect("gateway configuration");
    let checkpoints = SqliteCheckpoint::new(store.checkpoints_path()).expect("checkpoint store");
    // A nondefault endpoint without its own credential cannot use ambient API keys or voice.
    let composition = crate::wire::AgentComposition {
        provider: crate::wire::ProviderConfig {
            instance: "voice-boundary-test".into(),
            provider: "responses".into(),
            model: "local-test".into(),
            base_url: Some("http://127.0.0.1:1/v1".into()),
            endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
            reasoning_effort: None,
            web_search: mobius::backend::model::provider::HostedWebSearch::Off,
        },
        ..crate::wire::AgentComposition::default()
    };
    let config = config
        .registering_provider(
            composition.provider.clone(),
            "Voice boundary".into(),
            Default::default(),
            vec!["local-test".into()],
            Vec::new(),
        )
        .expect("register unavailable local provider");
    store.save(&config).expect("save configuration");
    let (_, grant) = AuthStore::initialize(store.auth_path()).expect("authentication");
    let server = GatewayServer::assemble(store, config, listener)
        .await
        .expect("gateway");
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    let endpoint = format!("tcp://{listen}")
        .parse::<Endpoint>()
        .expect("endpoint");
    let (connection, _) = GatewayClient::pair(
        &endpoint,
        grant.code,
        "voice boundary test",
        ClientKind::Ios,
    )
    .await
    .expect("paired client");
    let (sender, mut events) = connection.into_parts();
    wait_gateway_ready(&mut events).await;

    let request = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::StartRealtimeVoice {
            request_id: request.clone(),
            session_id: "unselected".into(),
            offer_sdp: OFFER.into(),
        })
        .await
        .expect("start without selection");
    assert!(
        expect_voice_failure(&mut events, &request, "unselected")
            .await
            .contains("chat")
    );

    let (other, _) =
        create_bot_chat_with_config(&sender, &mut events, &workspace, composition.clone()).await;
    let (selected, _) =
        create_bot_chat_with_config(&sender, &mut events, &workspace, composition).await;
    let before = checkpoints
        .event_page(
            &selected,
            EventPageRequest {
                before_sequence: None,
                limit: 128,
            },
        )
        .await
        .expect("initial journal");
    let request = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::StartRealtimeVoice {
            request_id: request.clone(),
            session_id: other.clone(),
            offer_sdp: OFFER.into(),
        })
        .await
        .expect("start on another session");
    assert!(
        expect_voice_failure(&mut events, &request, &other)
            .await
            .contains("open this chat")
    );

    for (request, offer) in [
        ("invalid-uuid".to_string(), OFFER.to_string()),
        (Uuid::new_v4().to_string(), String::new()),
        (Uuid::new_v4().to_string(), "x".repeat(64 * 1024 + 1)),
    ] {
        sender
            .send(ClientMessage::StartRealtimeVoice {
                request_id: request.clone(),
                session_id: selected.clone(),
                offer_sdp: offer,
            })
            .await
            .expect("invalid signaling");
        assert!(
            expect_voice_failure(&mut events, &request, &selected)
                .await
                .contains("invalid voice connection request")
        );
    }

    let request = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::StartRealtimeVoice {
            request_id: request.clone(),
            session_id: selected.clone(),
            offer_sdp: OFFER.into(),
        })
        .await
        .expect("unsupported provider");
    assert!(
        expect_voice_failure(&mut events, &request, &selected)
            .await
            .contains("does not support realtime voice")
    );

    for session_id in [&selected, &other] {
        sender
            .send(ClientMessage::EndRealtimeVoice {
                session_id: session_id.clone(),
                voice_id: Uuid::new_v4().to_string(),
            })
            .await
            .expect("foreign voice end");
    }
    sender
        .send(ClientMessage::GetSessionHistory {
            request_id: "voice-barrier".into(),
            session_id: selected.clone(),
            before_sequence: None,
        })
        .await
        .expect("history after signaling");
    loop {
        match next_gateway_message(&mut events).await {
            ServerMessage::SessionHistory { request_id, .. } if request_id == "voice-barrier" => {
                break;
            }
            ServerMessage::RealtimeVoiceEnded { .. } => {
                panic!("foreign voice identity ended a call")
            }
            ServerMessage::Error {
                fatal: true,
                message,
                ..
            } => panic!("signaling closed chat: {message}"),
            _ => {}
        }
    }
    let after = checkpoints
        .event_page(
            &selected,
            EventPageRequest {
                before_sequence: None,
                limit: 128,
            },
        )
        .await
        .expect("journal after signaling");
    assert_eq!(
        after, before,
        "voice signaling must not create durable agent events"
    );

    let submission_id = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::Submit {
            session_id: selected.clone(),
            submission: Submission {
                id: submission_id.clone(),
                op: user_message(
                    "Ordinary chat still works after rejected voice signaling.",
                    Vec::new(),
                ),
            },
        })
        .await
        .expect("ordinary chat message");
    let mut saw_message = false;
    loop {
        match next_gateway_message(&mut events).await {
            ServerMessage::AgentEvent {
                session_id, record, ..
            } if session_id == selected
                && record.event.submission_id.as_deref() == Some(&submission_id) =>
            {
                match record.event.msg {
                    EventMsg::Message(message) => {
                        assert_eq!(
                            message.text,
                            "Ordinary chat still works after rejected voice signaling."
                        );
                        saw_message = true;
                    }
                    EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_) => break,
                    _ => {}
                }
            }
            ServerMessage::Rejected {
                request_id,
                message,
                ..
            } if request_id == submission_id => panic!("ordinary message was rejected: {message}"),
            _ => {}
        }
    }
    assert!(
        saw_message,
        "ordinary user message reached the durable message lifecycle"
    );
    let journal = checkpoints
        .event_page(
            &selected,
            EventPageRequest {
                before_sequence: None,
                limit: 128,
            },
        )
        .await
        .expect("final journal");
    assert!(
        !serde_json::to_string(&journal)
            .unwrap()
            .contains("x-mobius-voice-offer-marker")
    );
    shutdown.send(()).expect("shutdown");
    serving.await.expect("gateway task").expect("gateway stop");
}
