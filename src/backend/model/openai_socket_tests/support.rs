use super::super::*;
use crate::backend::model::PromptCacheIdentity;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

pub(super) fn model_request() -> ModelRequest<'static> {
    ModelRequest {
        session_id: "test-session",
        prompt_cache: Some(PromptCacheIdentity {
            key: "hashed-cache-key",
            context_epoch: 1,
        }),
        instructions: "Test instructions",
        input: &[],
        catalog_revision: "catalog-1",
        tools: &[],
        deferred_tools: &[],
        allow_hosted_tools: false,
        allow_continuation: false,
    }
}

pub(super) fn completed_events(text: &str, response_id: &str) -> [Value; 2] {
    [
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": format!("message-{response_id}"),
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}]
            }
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {"id": response_id, "output": []}
        }),
    ]
}

pub(super) async fn read_http_json(stream: &mut tokio::net::TcpStream) -> Value {
    let mut request = Vec::new();
    let header_end = loop {
        if let Some(index) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break index + 4;
        }
        let mut chunk = [0; 4_096];
        let count = stream.read(&mut chunk).await.expect("HTTP request");
        assert_ne!(count, 0, "HTTP request ended before its headers");
        request.extend_from_slice(&chunk[..count]);
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
        })
        .expect("content-length header");
    while request.len() < header_end + content_length {
        let mut chunk = [0; 4_096];
        let count = stream.read(&mut chunk).await.expect("HTTP request body");
        assert_ne!(count, 0, "HTTP request body ended early");
        request.extend_from_slice(&chunk[..count]);
    }
    serde_json::from_slice(&request[header_end..header_end + content_length])
        .expect("HTTP JSON body")
}

pub(super) async fn write_http_stream(
    stream: &mut tokio::net::TcpStream,
    text: &str,
    response_id: &str,
) {
    let body = completed_events(text, response_id)
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("HTTP stream response");
}

pub(super) async fn write_http_compaction_stream(stream: &mut tokio::net::TcpStream) {
    let events = [
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "compaction",
                "encrypted_content": "opaque"
            }
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {"id": "response-compact", "output": []}
        }),
    ];
    let body = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("HTTP compaction response");
}
