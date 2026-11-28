use super::*;
use crate::wire::{ProviderForm, ProviderModelKind};
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt as _;

async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
    let mut reader = tokio::io::BufReader::new(socket);
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        if line == "\r\n" {
            break;
        }
        headers.push_str(&line);
    }
    let headers = headers.to_lowercase();
    let length = headers
        .lines()
        .find_map(|l| l.strip_prefix("content-length: "))
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.unwrap();
    (headers, body)
}

async fn reply(socket: &mut tokio::net::TcpStream, status: &str, body: &str) {
    socket.write_all(format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
}

#[tokio::test]
async fn provider_group_uses_each_unsaved_model_and_key_without_registering_it() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}/v1", upstream.local_addr().unwrap());
    let (received, first_request) = tokio::sync::oneshot::channel();
    let (release, proceed) = tokio::sync::oneshot::channel();
    let provider = tokio::spawn(async move {
        // Both models must reach the provider before either response is released.
        let mut models = std::collections::BTreeSet::new();
        let mut sockets = Vec::new();
        for _ in 0..2 {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let (headers, body) = read_request(&mut socket).await;
            assert!(headers.starts_with("post /v1/draft-transcribe "));
            assert!(headers.contains("authorization: bearer unsaved-only"));
            let body = String::from_utf8_lossy(&body);
            let model = ["one", "two"]
                .into_iter()
                .find(|id| body.contains(&format!("\r\n\r\n{id}\r\n")))
                .unwrap();
            assert!(models.insert(model));
            assert!(body.contains("RIFF") && body.contains("WAVE"));
            sockets.push(socket);
        }
        received.send(()).unwrap();
        proceed.await.unwrap();
        for mut socket in sockets {
            reply(
                &mut socket,
                "200 OK",
                r#"{"text":"The meeting starts at nine."}"#,
            )
            .await;
        }
        let (mut socket, _) = upstream.accept().await.unwrap();
        let (_, body) = read_request(&mut socket).await;
        assert!(String::from_utf8_lossy(&body).contains("\r\n\r\nbad-model\r\n"));
        reply(
            &mut socket,
            "400 Bad Request",
            r#"{"error":{"message":"bad model; authorization: Bearer unsaved-only"}}"#,
        )
        .await;
    });
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let (server, grant) = configured_test_server(state.clone()).await;
    let (tx, mut rx, gateway) = paired_gateway(server, grant, PRESENT_CLIENT).await;
    let config_path = state.join("gateway.toml");
    let credentials_path = state.join("credentials.json");
    let saved_credentials = fs::read(&credentials_path).ok();
    let saved = fs::read(&config_path).unwrap();
    // A paired client must use the advertised form; even well-shaped raw operator
    // metadata cannot enter the catalog through the provisioning command.
    tx.send(serde_json::from_value(json!({
        "type": "register_provider", "request_id": "raw-voice",
        "config": {"instance": "raw", "provider": "openai_socket", "endpoint_auth":"provider_default","web_search":"off"},
        "voice_models": [{"id":"replacement","label":"Operator label","description":"","variants":[]}],
    })).unwrap()).await.unwrap();
    loop {
        if let ServerMessage::Rejected {
            request_id, code, ..
        } = next_gateway_message(&mut rx).await
        {
            assert_eq!(
                (&*request_id, code.as_str()),
                ("raw-voice", "operator_required")
            );
            break;
        }
    }
    assert_eq!(fs::read(&config_path).unwrap(), saved);
    let mut first = Some((first_request, release));
    for (id, models) in [
        ("valid", vec!["one", "two"]),
        ("invalid", vec!["bad-model"]),
    ] {
        let form: ProviderForm = serde_json::from_value(json!({
            "config":{"instance":"unsaved","provider":"responses","base_url":origin,"model":"chat-test","endpoint_auth":"provider_default","web_search":"off"},
            "groups":{
                "dictation":{"values":{"api":"transcriptions","endpoint":"draft-transcribe"},"models":models.iter().map(|m|json!({"id":m})).collect::<Vec<_>>()},
                "image":{"values":{"unknown_field":"not valid"},"models":[]}
            }
        })).unwrap();
        tx.send(ClientMessage::TestProviderGroup {
            request_id: id.into(),
            group: ProviderModelKind::Dictation,
            form,
            api_key: Some("unsaved-only".into()),
        })
        .await
        .unwrap();
        if let Some((first_request, release)) = first.take() {
            tokio::time::timeout(Duration::from_secs(5), first_request)
                .await
                .unwrap()
                .unwrap();
            tx.send(ClientMessage::ListBots {
                request_id: "still-responsive".into(),
            })
            .await
            .unwrap();
            while !matches!(next_gateway_message(&mut rx).await, ServerMessage::Bots { request_id: Some(id), .. } if &*id == "still-responsive")
            {
            }
            release.send(()).unwrap();
        }
        loop {
            match next_gateway_message(&mut rx).await {
                ServerMessage::ProviderGroupTested { request_id } => {
                    assert_eq!(request_id.as_ref(), "valid");
                    assert_eq!(id, "valid");
                    break;
                }
                ServerMessage::Rejected {
                    request_id,
                    message,
                    code,
                    fatal,
                } => {
                    assert_eq!(id, "invalid", "{message}");
                    assert_eq!(request_id.as_ref(), id);
                    assert_eq!(code, "invalid_config");
                    assert!(!fatal);
                    assert!(message.contains("bad-model") && message.contains("400"));
                    assert!(!message.contains("unsaved-only") && !message.contains("Bearer"));
                    break;
                }
                _ => {}
            }
        }
    }
    provider.await.unwrap();
    assert_eq!(saved, fs::read(config_path).unwrap());
    assert_eq!(fs::read(credentials_path).ok(), saved_credentials);
    gateway.shutdown().await;
}

#[tokio::test]
async fn live_group_test_requires_text_before_commit_and_final_text_after_it() {
    use futures_util::SinkExt as _;

    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}/v1", upstream.local_addr().unwrap());
    let provider = tokio::spawn(async move {
        for streams in [true, false] {
            let (socket, _) = upstream.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let event = ws.next().await.unwrap().unwrap();
            let session: Value = serde_json::from_str(event.to_text().unwrap()).unwrap();
            assert_eq!(
                session["session"]["audio"]["input"]["transcription"]["model"],
                "custom-streaming-asr"
            );
            let event = ws.next().await.unwrap().unwrap();
            let audio: Value = serde_json::from_str(event.to_text().unwrap()).unwrap();
            assert_eq!(audio["type"], "input_audio_buffer.append");
            if streams {
                ws.send(Message::text(json!({"type":"conversation.item.input_audio_transcription.delta","delta":"The meeting"}).to_string())).await.unwrap();
                loop {
                    let event = ws.next().await.unwrap().unwrap();
                    let frame: Value = serde_json::from_str(event.to_text().unwrap()).unwrap();
                    if frame["type"] == "input_audio_buffer.commit" {
                        break;
                    }
                    assert_eq!(frame["type"], "input_audio_buffer.append");
                }
            }
            ws.send(Message::text(json!({"type":"conversation.item.input_audio_transcription.completed","transcript":"The meeting starts at nine."}).to_string())).await.unwrap();
            ws.close(None).await.unwrap();
        }
    });
    let state = tempfile::tempdir().unwrap();
    let (server, grant) = configured_test_server(state.path().join("state")).await;
    let (tx, mut rx, gateway) = paired_gateway(server, grant, PRESENT_CLIENT).await;
    let form: ProviderForm = serde_json::from_value(json!({
        "config":{"instance":"unsaved-live","provider":"responses","base_url":origin,"model":"chat","endpoint_auth":"provider_default","web_search":"off"},
        "groups":{"dictation":{"values":{"api":"realtime_transcription","endpoint":"custom/live"},"models":[{"id":"custom-streaming-asr"}]}}
    })).unwrap();
    for request in ["streams", "premature-final"] {
        tx.send(ClientMessage::TestProviderGroup {
            request_id: request.into(),
            group: ProviderModelKind::Dictation,
            form: form.clone(),
            api_key: Some("temporary-live-key".into()),
        })
        .await
        .unwrap();
        loop {
            match next_gateway_message(&mut rx).await {
                ServerMessage::ProviderGroupTested { request_id } => {
                    assert_eq!((&*request_id, request), ("streams", "streams"));
                    break;
                }
                ServerMessage::Rejected {
                    request_id,
                    message,
                    ..
                } => {
                    assert_eq!(
                        (&*request_id, request),
                        ("premature-final", "premature-final"),
                        "{message}"
                    );
                    assert!(message.contains("custom-streaming-asr"));
                    break;
                }
                _ => {}
            }
        }
    }
    provider.await.unwrap();
    tx.send(ClientMessage::SetProviderCredential {
        request_id: "store".into(),
        instance: form.config.instance.clone(),
        provider: form.config.provider.clone(),
        base_url: form.config.base_url.clone(),
        expires_at: None,
        api_key: "temporary-live-key".into(),
    })
    .await
    .unwrap();
    loop {
        match next_gateway_message(&mut rx).await {
            ServerMessage::ProviderCredentialSaved { .. } => break,
            ServerMessage::Rejected { message, .. } => {
                panic!("could not store fixture credential: {message}")
            }
            _ => {}
        }
    }
    tx.send(ClientMessage::SaveProviderForm {
        request_id: "save".into(),
        form,
    })
    .await
    .unwrap();
    loop {
        match next_gateway_message(&mut rx).await {
            ServerMessage::GatewayConfigured { payload, .. } => {
                let setup = payload
                    .provider_instances
                    .iter()
                    .find(|p| p.selection.instance == "unsaved-live")
                    .unwrap();
                let group = setup
                    .model_groups
                    .iter()
                    .find(|group| group.kind == ProviderModelKind::Dictation)
                    .unwrap();
                let mode = group.models[0]
                    .fields
                    .iter()
                    .find(|f| f.setting.id == "dictation_mode")
                    .unwrap();
                assert!(!mode.editable);
                assert_eq!(
                    mode.value,
                    mobius::protocol::FrontendSettingValue::String("Live".into())
                );
                break;
            }
            ServerMessage::Rejected { message, .. } => {
                panic!("could not save tested setup: {message}")
            }
            _ => {}
        }
    }
    gateway.shutdown().await;
}

#[tokio::test]
async fn language_test_sends_configured_hosted_search_in_compatible_apis() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}/v1", upstream.local_addr().unwrap());
    let provider = tokio::spawn(async move {
        for api in ["responses", "chat_completions"] {
            for search in ["live", "off"] {
                let (mut socket, _) = upstream.accept().await.unwrap();
                let (headers, body) = read_request(&mut socket).await;
                assert!(headers.starts_with(&format!("post /v1/probe/{api} ")));
                let data: Value = serde_json::from_slice(&body).unwrap();
                let offers_search = data["tools"].as_array().is_some_and(|tools| {
                    tools.iter().any(|t| t["type"] == "openrouter:web_search")
                });
                assert_eq!(offers_search, search == "live");
                let text = if api == "responses" {
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"test\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ready\"}]}]}}\n\n"
                } else {
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ready\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                };
                socket.write_all(format!("HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{text}",text.len()).as_bytes()).await.unwrap();
            }
        }
    });
    let state = tempfile::tempdir().unwrap();
    let (server, grant) = configured_test_server(state.path().join("state")).await;
    let (tx, mut rx, gateway) = paired_gateway(server, grant, PRESENT_CLIENT).await;
    for api in ["responses", "chat_completions"] {
        for search in ["live", "off"] {
            let id = format!("{api}-{search}");
            let form: ProviderForm=serde_json::from_value(json!({
                "config":{"instance":"search","provider":"openrouter","base_url":origin,"model":"search-model","endpoint_auth":"provider_default","web_search":"off"},
                "groups":{"language":{"values":{"api":api,"endpoint":format!("probe/{api}"),"web_search":search},"models":[{"id":"search-model"}]}}
            })).unwrap();
            tx.send(ClientMessage::TestProviderGroup {
                request_id: id.as_str().into(),
                group: ProviderModelKind::Language,
                form,
                api_key: Some("temporary-search-key".into()),
            })
            .await
            .unwrap();
            loop {
                match next_gateway_message(&mut rx).await {
                    ServerMessage::ProviderGroupTested { request_id } => {
                        assert_eq!(request_id.as_ref(), id);
                        break;
                    }
                    ServerMessage::Rejected { message, .. } => {
                        panic!("language probe failed: {message}")
                    }
                    _ => {}
                }
            }
        }
    }
    provider.await.unwrap();
    gateway.shutdown().await;
}
