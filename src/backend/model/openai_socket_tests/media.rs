use super::super::*;
use super::support::{completed_events, model_request};
use crate::backend::model::{ImageInputLimits, MediaPreparation};
use crate::backend::session_files::{SessionFileStore, grant_context};
use crate::protocol::ImageDetail;
use futures_util::{SinkExt as _, StreamExt as _};

#[tokio::test]
async fn continuation_skips_old_pixels_and_preparation_failure_preserves_connection() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("connection");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake");
        for step in 0..4 {
            let body: Value = serde_json::from_slice(
                &socket
                    .next()
                    .await
                    .expect("request")
                    .expect("frame")
                    .into_data(),
            )
            .expect("JSON");
            match step {
                0 => assert!(
                    body["input"][0]["content"][0]["image_url"]
                        .as_str()
                        .expect("image")
                        .starts_with("data:image/png;base64,")
                ),
                1 | 2 => {
                    assert!(body.get("previous_response_id").is_some());
                    assert_eq!(body["input"].as_array().expect("suffix").len(), 1);
                }
                3 => {
                    assert!(body.get("previous_response_id").is_none());
                    assert!(body["input"][0]["content"][0]["image_url"].is_string());
                }
                _ => unreachable!(),
            }
            if step == 2 {
                socket.send(Message::text(serde_json::json!({"type":"error","error":{"code":"previous_response_not_found","message":"missing previous response"}}).to_string())).await.expect("missing");
            } else {
                for event in completed_events("observed", &format!("response-{step}")) {
                    socket
                        .send(Message::text(event.to_string()))
                        .await
                        .expect("completed");
                }
            }
        }
    });
    let provider = OpenAiSocket::with_authorization(
        Arc::new(ApiKeyAuthorization::new("test".into())),
        "http://127.0.0.1:1",
        format!("ws://{address}/responses"),
        "test-model",
        reqwest::Client::new(),
        crate::backend::model::ModelTransportSettings::default(),
    )
    .expect("provider");
    let directory = tempfile::tempdir().expect("state");
    let files = SessionFileStore::new(directory.path(), None);
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(16, 16)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("PNG");
    let image = files
        .ingest_image(
            "test-session",
            "image.png".into(),
            bytes.into_inner(),
            ImageDetail::High,
        )
        .await
        .expect("image");
    let mut input =
        vec![serde_json::json!({"role":"user","content":[{"type":"input_image","image": image}]})];
    grant_context(Some(&files), "test-session", "backup", &input)
        .await
        .expect("backup");
    let media = MediaPreparation {
        files: Some(&files),
        limits: ImageInputLimits::default(),
    };
    fn request(input: &[Value]) -> ModelRequest<'_> {
        ModelRequest {
            input,
            allow_continuation: true,
            ..model_request()
        }
    }
    let events: ModelEventSink = Arc::new(|_| Box::pin(async { Ok(()) }));
    let output = provider
        .respond_prepared(request(&input), Arc::clone(&events), media)
        .await
        .expect("initial");
    input.extend(output.output);
    files
        .delete_session("test-session")
        .await
        .expect("old pixels unavailable");
    input
        .push(serde_json::json!({"role":"user","content":[{"type":"input_image","image": image}]}));
    assert!(
        provider
            .respond_prepared(request(&input), Arc::clone(&events), media)
            .await
            .is_err()
    );
    {
        let session = provider.session("test-session").await.expect("session");
        let state = session.lock().await;
        assert!(
            state
                .connection
                .as_ref()
                .is_some_and(OpenAiWsConnection::is_usable)
        );
        assert!(state.continuation.is_some());
    }
    input.pop();
    input.push(serde_json::json!({"role":"user","content":"continue"}));
    let output = provider
        .respond_prepared(request(&input), Arc::clone(&events), media)
        .await
        .expect("only suffix reads pixels");
    input.extend(output.output);
    grant_context(Some(&files), "backup", "test-session", &input[..1])
        .await
        .expect("restore original files for replay");
    input.push(serde_json::json!({"role":"user","content":"again"}));
    provider
        .respond_prepared(request(&input), events, media)
        .await
        .expect("missing previous rebuilds logical history");
    server.await.expect("same socket server");
}

#[tokio::test]
async fn projected_replay_keeps_logical_prefix_for_continuation_and_rebuild() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("connection");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("handshake");
        for step in 0..4 {
            let body: Value = serde_json::from_slice(
                &socket
                    .next()
                    .await
                    .expect("request")
                    .expect("frame")
                    .into_data(),
            )
            .expect("JSON");
            if matches!(step, 0 | 3) {
                assert!(body.get("previous_response_id").is_none());
                assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
                assert!(
                    body["input"][0]["content"][0]["text"]
                        .as_str()
                        .expect("historical placeholder")
                        .contains("Reopen the original session file")
                );
                assert!(body["input"][2]["content"][0]["image_url"].is_string());
            } else {
                assert!(body.get("previous_response_id").is_some());
                assert_eq!(body["input"].as_array().expect("suffix").len(), 1);
            }
            if step == 2 {
                socket
                    .send(Message::text(
                        serde_json::json!({"type":"error","error":{"code":"previous_response_not_found","message":"missing previous response"}}).to_string(),
                    ))
                    .await
                    .expect("missing previous response");
            } else {
                for event in completed_events("observed", &format!("response-{step}")) {
                    socket
                        .send(Message::text(event.to_string()))
                        .await
                        .expect("completed");
                }
            }
        }
    });
    let provider = OpenAiSocket::with_authorization(
        Arc::new(ApiKeyAuthorization::new("test".into())),
        "http://127.0.0.1:1",
        format!("ws://{address}/responses"),
        "test-model",
        reqwest::Client::new(),
        crate::backend::model::ModelTransportSettings::default(),
    )
    .expect("provider");
    let directory = tempfile::tempdir().expect("state");
    let files = SessionFileStore::new(directory.path(), None);
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(16, 16)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("PNG");
    let image = files
        .ingest_image(
            "test-session",
            "image.png".into(),
            bytes.into_inner(),
            ImageDetail::High,
        )
        .await
        .expect("image");
    let mut input = vec![
        serde_json::json!({"role":"user","content":[{"type":"input_image","image":image}]}),
        serde_json::json!({"role":"assistant","content":"first observation"}),
        serde_json::json!({"role":"user","content":[{"type":"input_image","image":image}]}),
    ];
    let media = MediaPreparation {
        files: Some(&files),
        limits: ImageInputLimits {
            max_images: 1,
            ..ImageInputLimits::default()
        },
    };
    let events: ModelEventSink = Arc::new(|_| Box::pin(async { Ok(()) }));
    for step in 0..3 {
        let output = provider
            .respond_prepared(
                ModelRequest {
                    input: &input,
                    allow_continuation: true,
                    ..model_request()
                },
                Arc::clone(&events),
                media,
            )
            .await
            .expect("projected replay or continuation");
        assert_eq!(input[0]["content"][0]["type"], "input_image");
        input.extend(output.output);
        if step < 2 {
            input.push(serde_json::json!({"role":"user","content":"continue"}));
        }
    }
    server.await.expect("same socket server");
}
