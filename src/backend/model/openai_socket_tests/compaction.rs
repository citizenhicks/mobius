use super::super::*;
use super::support::completed_events;
use crate::backend::model::ModelTransportSettings;
use crate::backend::model::PromptCacheIdentity;

#[test]
fn compaction_retry_is_bounded_and_preserves_valid_hints() {
    let transport = ModelTransportSettings::default();
    assert_eq!(
        compaction_retry_delay(
            &ProviderError::stream_interrupted(Some(u64::MAX.to_string())),
            1,
            &transport,
        ),
        Duration::from_millis(400),
    );
    assert_eq!(
        compaction_retry_delay(
            &ProviderError::stream_interrupted(Some("3".into())),
            1,
            &transport,
        ),
        Duration::from_secs(3),
    );
    assert_eq!(
        compaction_retry_delay(
            &ProviderError::stream_interrupted(Some("3600".into())),
            1,
            &transport,
        ),
        Duration::from_millis(transport.stream_retry_max_backoff_ms)
    );
}

#[tokio::test]
async fn native_compaction_reuses_the_websocket_with_a_v2_trigger() {
    use futures_util::SinkExt as _;
    use futures_util::StreamExt as _;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("WebSocket listener");
    let address = listener.local_addr().expect("WebSocket address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("WebSocket connection");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("WebSocket handshake");
        let initial: Value = serde_json::from_slice(
            &socket
                .next()
                .await
                .expect("initial request")
                .expect("valid initial request")
                .into_data(),
        )
        .expect("initial request body");
        assert!(initial.get("previous_response_id").is_none());
        for event in completed_events("Warm response.", "response-warm") {
            socket
                .send(Message::text(event.to_string()))
                .await
                .expect("initial completed event");
        }

        let compact: Value = serde_json::from_slice(
            &socket
                .next()
                .await
                .expect("compaction request")
                .expect("valid compaction request")
                .into_data(),
        )
        .expect("compaction request body");
        assert_eq!(compact["previous_response_id"], "response-warm");
        assert_eq!(
            compact["input"],
            serde_json::json!([{"type": "compaction_trigger"}])
        );
        socket
            .send(Message::text(
                serde_json::json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "opaque"
                    }
                })
                .to_string(),
            ))
            .await
            .expect("compaction output");
        socket
            .send(Message::text(
                serde_json::json!({
                    "type": "response.completed",
                    "response": {"id": "response-compact", "output": []}
                })
                .to_string(),
            ))
            .await
            .expect("completed compaction response");
    });
    let provider = OpenAiSocket::with_authorization(
        Arc::new(ApiKeyAuthorization::new("test-key".into())),
        "http://127.0.0.1:1",
        format!("ws://{address}/responses"),
        "test-model",
        reqwest::Client::new(),
        crate::backend::model::ModelTransportSettings::default(),
    )
    .expect("provider");
    let directory = tempfile::tempdir().expect("state");
    let files = crate::backend::session_files::SessionFileStore::new(directory.path(), None);
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(8, 8)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("PNG");
    let image = files
        .ingest_image(
            "test-session",
            "image.png".into(),
            bytes.into_inner(),
            crate::protocol::ImageDetail::High,
        )
        .await
        .expect("image");
    let media = crate::backend::model::MediaPreparation {
        files: Some(&files),
        limits: crate::backend::model::ImageInputLimits::default(),
    };
    let initial_input = vec![
        serde_json::json!({"role": "user", "content": [{"type":"input_image", "image":image}]}),
    ];
    let initial_output = provider
        .respond_prepared(
            ModelRequest {
                session_id: "test-session",
                prompt_cache: Some(PromptCacheIdentity {
                    key: "hashed-cache-key",
                    context_epoch: 1,
                }),
                instructions: "Test instructions",
                input: &initial_input,
                catalog_revision: "catalog-1",
                tools: &[],
                deferred_tools: &[],
                allow_hosted_tools: false,
                allow_continuation: true,
            },
            Arc::new(|_| Box::pin(async { Ok(()) })),
            media,
        )
        .await
        .expect("initial response");
    let mut input = initial_input;
    input.extend_from_slice(initial_output.output());
    files
        .delete_session("test-session")
        .await
        .expect("compaction needs only the suffix");

    let compacted = provider
        .compact_prepared(
            CompactRequest {
                session_id: "test-session",
                prompt_cache: Some(PromptCacheIdentity {
                    key: "hashed-cache-key",
                    context_epoch: 1,
                }),
                instructions: "Test instructions",
                input: &input,
                catalog_revision: "catalog-1",
                tools: &[],
                deferred_tools: &[],
            },
            media,
        )
        .await
        .expect("native compaction");
    server.await.expect("WebSocket server");

    assert_eq!(
        compacted.output(),
        &[serde_json::json!({
            "type": "compaction",
            "encrypted_content": "opaque"
        })]
    );
}

#[tokio::test]
async fn native_compaction_retries_an_interrupted_websocket() {
    use futures_util::SinkExt as _;
    use futures_util::StreamExt as _;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("WebSocket listener");
    let address = listener.local_addr().expect("WebSocket address");
    let server = tokio::spawn(async move {
        for attempt in 0..2 {
            let (stream, _) = listener.accept().await.expect("WebSocket connection");
            let mut socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("WebSocket handshake");
            let compact: Value = serde_json::from_slice(
                &socket
                    .next()
                    .await
                    .expect("compaction request")
                    .expect("valid compaction request")
                    .into_data(),
            )
            .expect("compaction request body");
            assert!(compact.get("previous_response_id").is_none());
            assert_eq!(
                compact["input"].as_array().and_then(|input| input.last()),
                Some(&serde_json::json!({"type": "compaction_trigger"}))
            );
            if attempt == 0 {
                drop(socket);
                continue;
            }
            socket
                .send(Message::text(
                    serde_json::json!({
                        "type": "response.output_item.done",
                        "output_index": 0,
                        "item": {
                            "type": "compaction",
                            "encrypted_content": "opaque"
                        }
                    })
                    .to_string(),
                ))
                .await
                .expect("compaction output");
            socket
                .send(Message::text(
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {"id": "response-compact", "output": []}
                    })
                    .to_string(),
                ))
                .await
                .expect("completed compaction response");
        }
    });
    let provider = OpenAiSocket::with_authorization(
        Arc::new(ApiKeyAuthorization::new("test-key".into())),
        "http://127.0.0.1:1",
        format!("ws://{address}/responses"),
        "test-model",
        reqwest::Client::new(),
        crate::backend::model::ModelTransportSettings::default(),
    )
    .expect("provider");
    let input = vec![serde_json::json!({"role": "user", "content": "one"})];

    let compacted = provider
        .compact(CompactRequest {
            session_id: "test-session",
            prompt_cache: Some(PromptCacheIdentity {
                key: "hashed-cache-key",
                context_epoch: 1,
            }),
            instructions: "Test instructions",
            input: &input,
            catalog_revision: "catalog-1",
            tools: &[],
            deferred_tools: &[],
        })
        .await
        .expect("retried native compaction");
    server.await.expect("WebSocket server");

    assert_eq!(compacted.output()[0]["type"], "compaction");
}
