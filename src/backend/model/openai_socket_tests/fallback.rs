use super::super::*;
use super::support::{
    completed_events, read_http_json, write_http_compaction_stream, write_http_stream,
};
use crate::backend::model::PromptCacheIdentity;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[tokio::test]
async fn upgrade_required_switches_only_that_session_to_sticky_http() {
    use futures_util::SinkExt as _;
    use futures_util::StreamExt as _;

    let websocket_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("WebSocket listener");
    let websocket_address = websocket_listener.local_addr().expect("WebSocket address");
    let websocket_server = tokio::spawn(async move {
        let (stream, _) = websocket_listener
            .accept()
            .await
            .expect("initial WebSocket connection");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("initial WebSocket handshake");
        let initial: Value = serde_json::from_slice(
            &socket
                .next()
                .await
                .expect("initial response request")
                .expect("valid initial response request")
                .into_data(),
        )
        .expect("initial response body");
        assert!(initial.get("previous_response_id").is_none());
        for event in completed_events("Warm response.", "response-warm") {
            socket
                .send(Message::text(event.to_string()))
                .await
                .expect("initial completed event");
        }
        let continued: Value = serde_json::from_slice(
            &socket
                .next()
                .await
                .expect("continued response request")
                .expect("valid continued response request")
                .into_data(),
        )
        .expect("continued response body");
        assert_eq!(continued["previous_response_id"], "response-warm");
        assert_eq!(
            continued["input"]
                .as_array()
                .expect("incremental input")
                .len(),
            1
        );
        drop(socket);

        let (mut stream, _) = websocket_listener
            .accept()
            .await
            .expect("fallback WebSocket connection");
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut chunk = [0; 1_024];
            let count = stream.read(&mut chunk).await.expect("handshake request");
            assert_ne!(count, 0, "request ended before its headers");
            request.extend_from_slice(&chunk[..count]);
        }
        stream
            .write_all(
                b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("fallback handshake response");
    });

    let http_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("HTTP listener");
    let http_address = http_listener.local_addr().expect("HTTP address");
    let (request_sender, mut requests) = mpsc::channel(4);
    let http_server = tokio::spawn(async move {
        for attempt in 0..4 {
            let (mut stream, _) = http_listener.accept().await.expect("HTTP connection");
            let request = read_http_json(&mut stream).await;
            request_sender
                .send(request)
                .await
                .expect("captured request");
            if attempt < 2 {
                write_http_stream(
                    &mut stream,
                    if attempt == 0 {
                        "HTTP fallback."
                    } else {
                        "Still HTTP."
                    },
                    &format!("response-http-{attempt}"),
                )
                .await;
            } else if attempt == 2 {
                write_http_compaction_stream(&mut stream).await;
            }
        }
    });

    let socket_url = format!("ws://{websocket_address}/responses");
    let provider = OpenAiSocket::with_authorization(
        Arc::new(ApiKeyAuthorization::new("test-key".into())),
        &format!("http://{http_address}"),
        &socket_url,
        "gpt-5.6-sol",
        reqwest::Client::new(),
    )
    .expect("provider")
    .with_reasoning_effort("medium")
    .expect("reasoning effort")
    .with_cached_web_search();
    let input = vec![serde_json::json!({
        "role": "user",
        "content": [{"type": "input_text", "text": "hello"}]
    })];
    let events: ModelEventSink = Arc::new(|_| Ok(()));

    let warm = provider
        .send_response(
            ModelRequest {
                session_id: "fallback-session",
                prompt_cache: Some(PromptCacheIdentity {
                    key: "hashed-fallback-session",
                    context_epoch: 1,
                }),
                instructions: "Test instructions",
                input: &input,
                catalog_revision: "catalog-1",
                tools: &[],
                deferred_tools: &[],
                allow_hosted_tools: true,
                allow_continuation: true,
            },
            Arc::clone(&events),
        )
        .await
        .expect("initial WebSocket response");
    let mut continued_input = input.clone();
    continued_input.extend(warm.output().iter().cloned());
    continued_input.push(serde_json::json!({
        "role": "user",
        "content": [{"type": "input_text", "text": "continue"}]
    }));

    let Error::Provider(error) = provider
        .send_response(
            ModelRequest {
                session_id: "fallback-session",
                prompt_cache: Some(PromptCacheIdentity {
                    key: "hashed-fallback-session",
                    context_epoch: 1,
                }),
                instructions: "Test instructions",
                input: &continued_input,
                catalog_revision: "catalog-1",
                tools: &[],
                deferred_tools: &[],
                allow_hosted_tools: true,
                allow_continuation: true,
            },
            Arc::clone(&events),
        )
        .await
        .expect_err("closed WebSocket should be retried before fallback")
    else {
        panic!("expected provider error");
    };
    assert!(error.is_stream_interrupted());

    let fallback = provider
        .send_response(
            ModelRequest {
                session_id: "fallback-session",
                prompt_cache: Some(PromptCacheIdentity {
                    key: "hashed-fallback-session",
                    context_epoch: 1,
                }),
                instructions: "Test instructions",
                input: &continued_input,
                catalog_revision: "catalog-1",
                tools: &[],
                deferred_tools: &[],
                allow_hosted_tools: true,
                allow_continuation: true,
            },
            Arc::clone(&events),
        )
        .await
        .expect("HTTP fallback");
    let sticky = provider
        .send_response(
            ModelRequest {
                session_id: "fallback-session",
                prompt_cache: Some(PromptCacheIdentity {
                    key: "hashed-fallback-session",
                    context_epoch: 1,
                }),
                instructions: "Test instructions",
                input: &continued_input,
                catalog_revision: "catalog-1",
                tools: &[],
                deferred_tools: &[],
                allow_hosted_tools: true,
                allow_continuation: true,
            },
            Arc::clone(&events),
        )
        .await
        .expect("sticky HTTP fallback");
    let compacted = provider
        .compact(CompactRequest {
            session_id: "fallback-session",
            prompt_cache: Some(PromptCacheIdentity {
                key: "hashed-fallback-session",
                context_epoch: 1,
            }),
            instructions: "Test instructions",
            input: &continued_input,
            catalog_revision: "catalog-1",
            tools: &[],
            deferred_tools: &[],
        })
        .await
        .expect("HTTP v2 compaction");
    let Error::Provider(http_error) = provider
        .send_response(
            ModelRequest {
                session_id: "fallback-session",
                prompt_cache: Some(PromptCacheIdentity {
                    key: "hashed-fallback-session",
                    context_epoch: 1,
                }),
                instructions: "Test instructions",
                input: &continued_input,
                catalog_revision: "catalog-1",
                tools: &[],
                deferred_tools: &[],
                allow_hosted_tools: true,
                allow_continuation: true,
            },
            Arc::clone(&events),
        )
        .await
        .expect_err("HTTPS failure should be terminal after fallback")
    else {
        panic!("expected provider error");
    };
    let first_http = requests.recv().await.expect("first HTTP request");
    let second_http = requests.recv().await.expect("second HTTP request");
    let compact_http = requests.recv().await.expect("compaction HTTP request");
    let failed_http = requests.recv().await.expect("failed HTTP request");
    http_server.await.expect("HTTP server");
    websocket_server.await.expect("WebSocket server");

    assert_eq!(fallback.text(), "HTTP fallback.");
    assert_eq!(sticky.text(), "Still HTTP.");
    assert_eq!(
        compacted.output(),
        &[serde_json::json!({
            "type": "compaction",
            "encrypted_content": "opaque"
        })]
    );
    assert_eq!(http_error.status(), None);
    assert!(!http_error.is_stream_interrupted());
    assert_eq!(http_error.to_string(), "HTTPS fallback transport failed");
    for request in [first_http, second_http, failed_http] {
        assert!(request.get("previous_response_id").is_none());
        assert_eq!(
            request["input"].as_array().expect("full HTTP input").len(),
            continued_input.len()
        );
        assert_eq!(
            request["reasoning"],
            serde_json::json!({"effort": "medium", "summary": "auto"})
        );
        assert_eq!(
            request["tools"],
            serde_json::json!([{"type": "web_search", "external_web_access": false}])
        );
    }
    assert!(compact_http.get("previous_response_id").is_none());
    let compact_input = compact_http["input"]
        .as_array()
        .expect("full HTTP compaction input");
    assert_eq!(compact_input.len(), continued_input.len() + 1);
    assert_eq!(
        compact_input.last(),
        Some(&serde_json::json!({"type": "compaction_trigger"}))
    );
}
