use super::super::*;
use super::support::{completed_events, model_request};
use crate::backend::model::{ImageInputLimits, MediaPreparation};
use crate::backend::session_files::{SessionFileStore, grant_context};
use crate::protocol::ImageDetail;
use futures_util::{SinkExt as _, StreamExt as _};

#[tokio::test]
async fn continuation_skips_old_pixels_and_preparation_failure_preserves_connection() {
    fn request(input: &[Value]) -> ModelRequest<'_> {
        ModelRequest {
            input: input.into(),
            allow_continuation: true,
            ..model_request()
        }
    }
    let session_id = model_request().session_id.expect("session");
    let backup_id = crate::SessionId::new("backup").expect("session");
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
                        .starts_with("data:image/jpeg;base64,")
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
        BaseUrl::new("http://127.0.0.1:1").unwrap(),
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
            session_id,
            "image.png".into(),
            bytes.into_inner(),
            ImageDetail::High,
        )
        .await
        .expect("image");
    let mut input =
        vec![serde_json::json!({"role":"user","content":[{"type":"input_image","image": image}]})];
    grant_context(Some(&files), session_id, &backup_id, &input)
        .await
        .expect("backup");
    let media = MediaPreparation {
        files: Some(&files),
        limits: ImageInputLimits::default(),
    };
    let events: ModelEventSink = Arc::new(|_| Box::pin(async { Ok(()) }));
    let output = provider
        .respond_prepared(request(&input), Arc::clone(&events), media)
        .await
        .expect("initial");
    input.extend(output.output);
    files
        .delete_session(session_id)
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
        drop(state);
    }
    input.pop();
    input.push(serde_json::json!({"role":"user","content":"continue"}));
    let output = provider
        .respond_prepared(request(&input), Arc::clone(&events), media)
        .await
        .expect("only suffix reads pixels");
    input.extend(output.output);
    grant_context(Some(&files), &backup_id, session_id, &input[..1])
        .await
        .expect("restore original files for replay");
    input.push(serde_json::json!({"role":"user","content":"again"}));
    provider
        .respond_prepared(request(&input), events, media)
        .await
        .expect("missing previous rebuilds logical history");
    server.await.expect("same socket server");
}
