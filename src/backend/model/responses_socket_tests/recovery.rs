use super::super::*;
use super::support::{completed_events, model_request};
use crate::backend::model::PromptCacheIdentity;

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
        BaseUrl::new(format!("http://{address}")).unwrap(),
        &socket_url,
        "test-model",
        reqwest::Client::new(),
        crate::backend::model::ModelTransportSettings::default(),
    )
    .expect("provider");
    let request = ModelRequest {
        prompt_cache: Some(PromptCacheIdentity {
            key: "hashed-cache-key",
            context_epoch: 1,
        }),
        allow_continuation: true,
        ..model_request()
    };
    let initial_input = vec![serde_json::json!({"role": "user", "content": "one"})];
    let events: ModelEventSink = Arc::new(|_| Box::pin(async { Ok(()) }));
    let initial_output = provider
        .respond(
            ModelRequest {
                input: (&initial_input).into(),
                ..request
            },
            Arc::clone(&events),
        )
        .await
        .expect("initial response");
    let mut input = initial_input;
    input.extend_from_slice(initial_output.output());
    input.push(serde_json::json!({"role": "user", "content": "two"}));

    let output = provider
        .respond(
            ModelRequest {
                input: (&input).into(),
                ..request
            },
            events,
        )
        .await
        .expect("context rebuild should recover");
    server.await.expect("WebSocket server");

    assert_eq!(output.text(), "Recovered.");
}
