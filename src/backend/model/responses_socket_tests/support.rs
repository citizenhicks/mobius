use std::fmt::Write as _;
use std::net::SocketAddr;

pub(super) use super::super::connection::{SocketEvent, read_exchange_with_timeout};
use super::super::*;
pub(super) use crate::backend::model::ModelTransportSettings;
use crate::backend::model::PromptCacheIdentity;
pub(super) use crate::backend::model::test_support::read_http_json;
pub(super) use std::sync::atomic::Ordering;
pub(super) use std::time::Duration;
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
pub(super) use tokio::sync::mpsc;
pub(super) use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
pub(super) use tokio_tungstenite::tungstenite::Message;

pub(super) fn model_request() -> ModelRequest<'static> {
    static SESSION: std::sync::LazyLock<crate::SessionId> =
        std::sync::LazyLock::new(|| crate::SessionId::new("test-session").expect("session"));
    ModelRequest {
        cancellation: None,
        session_id: Some(&SESSION),
        prompt_cache: Some(PromptCacheIdentity {
            key: "hashed-cache-key",
            context_epoch: 1,
        }),
        instructions: "Test instructions",
        input: (&[]).into(),
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

pub(super) use crate::backend::model::test_support::loopback_listener;

/// Accepts the next connection and completes its WebSocket handshake.
pub(super) async fn accept_websocket(listener: &TcpListener) -> WebSocketStream<TcpStream> {
    let (stream, _) = listener.accept().await.expect("WebSocket connection");
    tokio_tungstenite::accept_async(stream)
        .await
        .expect("WebSocket handshake")
}

pub(super) async fn read_request(socket: &mut WebSocketStream<TcpStream>) -> Value {
    use futures_util::StreamExt as _;
    let frame = socket
        .next()
        .await
        .expect("request frame")
        .expect("valid WebSocket frame");
    serde_json::from_slice(&frame.into_data()).expect("JSON request")
}

/// Builds an API-key provider whose HTTP and WebSocket endpoints are `address`.
pub(super) fn socket_provider(address: SocketAddr) -> OpenAiSocket {
    OpenAiSocket::from_authorization(
        Arc::new(ApiKeyAuthorization::new("test-key".into())),
        BaseUrl::new(format!("http://{address}")).expect("base URL"),
        format!("ws://{address}/responses"),
        "test-model".into(),
        reqwest::Client::new(),
        ModelTransportSettings::default(),
    )
    .expect("provider")
}

pub(super) async fn write_http_stream(stream: &mut TcpStream, text: &str, response_id: &str) {
    let body =
        completed_events(text, response_id)
            .into_iter()
            .fold(String::new(), |mut body, event| {
                write!(body, "data: {event}\n\n").expect("write event");
                body
            });
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("HTTP stream response");
}

pub(super) async fn read_exchange(
    messages: &mut mpsc::UnboundedReceiver<SocketEvent>,
    events: &ModelEventSink,
) -> Result<connection::Exchange> {
    let idle = ModelTransportSettings::default().socket_idle_timeout_ms;
    read_exchange_with_timeout(messages, events, Duration::from_millis(idle)).await
}
