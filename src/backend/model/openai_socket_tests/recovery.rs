use super::super::*;
use super::support::{completed_events, model_request};
use crate::backend::model::PromptCacheIdentity;

#[tokio::test]
async fn transient_response_failure_leaves_retry_to_a_fresh_model_attempt() {
    use futures_util::SinkExt as _;
    use futures_util::StreamExt as _;
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("WebSocket listener");
    let address = listener.local_addr().expect("WebSocket address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("WebSocket connection");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("WebSocket handshake");

        for attempt in 0..2 {
            let message = socket
                .next()
                .await
                .expect("response request")
                .expect("valid response request");
            let body: Value =
                serde_json::from_slice(&message.into_data()).expect("response request body");
            assert_eq!(body["type"], "response.create");

            if attempt == 0 {
                socket
                    .send(Message::text(
                        serde_json::json!({
                            "type": "response.failed",
                            "response": {
                                "error": {
                                    "type": "server_error",
                                    "message": "An error occurred while processing your request. You can retry your request, or contact support. Please include the request ID request-123 in your message."
                                }
                            }
                        })
                        .to_string(),
                    ))
                    .await
                    .expect("transient failure");
                continue;
            }

            socket
                .send(Message::text(
                    serde_json::json!({
                        "type": "response.output_item.done",
                        "output_index": 0,
                        "item": {
                            "id": "message-1",
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": "Recovered."}]
                        }
                    })
                    .to_string(),
                ))
                .await
                .expect("completed output item");
            socket
                .send(Message::text(
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {"id": "response-1", "output": []}
                    })
                    .to_string(),
                ))
                .await
                .expect("completed response");
        }
    });

    let socket_url = format!("ws://{address}/responses");
    let http_url = format!("http://{address}");
    let provider = OpenAiSocket::with_authorization(
        Arc::new(ApiKeyAuthorization::new("test-key".into())),
        &http_url,
        &socket_url,
        "test-model",
        reqwest::Client::new(),
    )
    .expect("provider");
    let events: ModelEventSink = Arc::new(|_| Ok(()));

    let Error::Provider(error) = provider
        .send_response(model_request(), Arc::clone(&events))
        .await
        .expect_err("first model attempt should be interrupted")
    else {
        panic!("expected provider error");
    };
    assert!(error.is_stream_interrupted());

    let output = provider
        .send_response(model_request(), events)
        .await
        .expect("fresh model attempt should recover");
    server.await.expect("WebSocket server");

    assert_eq!(output.text(), "Recovered.");
}

#[tokio::test]
async fn previous_response_not_found_does_not_repeat_full_context() {
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
        let request: Value = serde_json::from_slice(
            &socket
                .next()
                .await
                .expect("response request")
                .expect("valid response request")
                .into_data(),
        )
        .expect("response body");
        assert!(request.get("previous_response_id").is_none());
        socket
            .send(Message::text(
                serde_json::json!({
                    "type": "error",
                    "error": {
                        "code": "previous_response_not_found",
                        "message": "Previous response was not found"
                    }
                })
                .to_string(),
            ))
            .await
            .expect("missing previous response");
        assert!(
            timeout(Duration::from_millis(250), socket.next())
                .await
                .is_err(),
            "full-context request was repeated"
        );
    });
    let socket_url = format!("ws://{address}/responses");
    let provider = OpenAiSocket::with_authorization(
        Arc::new(ApiKeyAuthorization::new("test-key".into())),
        &format!("http://{address}"),
        &socket_url,
        "test-model",
        reqwest::Client::new(),
    )
    .expect("provider");
    let events: ModelEventSink = Arc::new(|_| Ok(()));

    let Error::Provider(error) = provider
        .send_response(model_request(), events)
        .await
        .expect_err("missing previous ID should interrupt the model attempt")
    else {
        panic!("expected provider error");
    };
    server.await.expect("WebSocket server");

    assert!(error.is_stream_interrupted());
}

#[tokio::test]
async fn previous_response_not_found_rebuilds_full_context_on_the_same_connection() {
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
        for event in completed_events("Old response.", "response-old") {
            socket
                .send(Message::text(event.to_string()))
                .await
                .expect("initial completed event");
        }

        let first: Value = serde_json::from_slice(
            &socket
                .next()
                .await
                .expect("continued request")
                .expect("valid continued request")
                .into_data(),
        )
        .expect("continued request body");
        assert_eq!(first["previous_response_id"], "response-old");
        assert_eq!(
            first["input"].as_array().expect("incremental input").len(),
            1
        );
        socket
            .send(Message::text(
                serde_json::json!({
                    "type": "error",
                    "error": {
                        "code": "previous_response_not_found",
                        "message": "Previous response was not found"
                    }
                })
                .to_string(),
            ))
            .await
            .expect("missing previous response");

        let rebuilt: Value = serde_json::from_slice(
            &socket
                .next()
                .await
                .expect("rebuilt request")
                .expect("valid rebuilt request")
                .into_data(),
        )
        .expect("rebuilt request body");
        assert!(rebuilt.get("previous_response_id").is_none());
        assert_eq!(rebuilt["input"].as_array().expect("full input").len(), 3);
        for event in completed_events("Recovered.", "response-new") {
            socket
                .send(Message::text(event.to_string()))
                .await
                .expect("completed event");
        }
    });
    let socket_url = format!("ws://{address}/responses");
    let provider = OpenAiSocket::with_authorization(
        Arc::new(ApiKeyAuthorization::new("test-key".into())),
        &format!("http://{address}"),
        &socket_url,
        "test-model",
        reqwest::Client::new(),
    )
    .expect("provider");
    let initial_input = vec![serde_json::json!({"role": "user", "content": "one"})];
    let events: ModelEventSink = Arc::new(|_| Ok(()));
    let initial_output = provider
        .send_response(
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
            Arc::clone(&events),
        )
        .await
        .expect("initial response");
    let mut input = initial_input;
    input.extend_from_slice(initial_output.output());
    input.push(serde_json::json!({"role": "user", "content": "two"}));

    let output = provider
        .send_response(
            ModelRequest {
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
                allow_hosted_tools: false,
                allow_continuation: true,
            },
            events,
        )
        .await
        .expect("context rebuild should recover");
    server.await.expect("WebSocket server");

    assert_eq!(output.text(), "Recovered.");
}
