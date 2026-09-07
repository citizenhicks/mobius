use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures_util::SinkExt;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::super::ModelEventSink;
use super::super::StreamingToolCalls;
use super::super::openai::attach_stream_output;
use super::super::openai::collect_stream_output;
use super::super::openai::emit_ready_tool_calls;
use super::super::openai::emit_reasoning_event;
use super::super::openai::emit_text_event;
use super::super::openai::emit_web_event;
use super::super::openai::response_error;
use super::super::openai_auth::OpenAiAuthorization;
use super::super::openai_auth::ResolvedAuthorization;
use super::super::transport::account_stream_bytes;
use crate::Error;
use crate::ProviderError;
use crate::Result;

const MAX_SOCKET_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_STREAM_EVENTS: usize = 65_536;
const SOCKET_COMMAND_CAPACITY: usize = 8;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const SOCKET_IO_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

type RawSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(super) enum Exchange {
    Completed(Value),
    PreviousMissing { output_delivered: bool },
    Retry { retry_after: Option<String> },
    ConnectionLimit { retry_after: Option<String> },
    Reconnect,
}

pub(super) struct OpenAiWsConnection {
    commands: mpsc::Sender<SocketCommand>,
    pub(super) messages: mpsc::UnboundedReceiver<SocketEvent>,
    pub(super) closed: Arc<AtomicBool>,
    pump: tokio::task::AbortHandle,
}

enum SocketCommand {
    Send {
        message: Message,
        result: oneshot::Sender<std::result::Result<(), ()>>,
    },
    Finish,
    Close {
        result: oneshot::Sender<()>,
    },
}

pub(super) enum SocketEvent {
    Message(Message),
    ProtocolError(&'static str),
    Closed,
}

impl OpenAiWsConnection {
    pub(super) fn new(socket: RawSocket) -> Self {
        let (commands, command_receiver) = mpsc::channel(SOCKET_COMMAND_CAPACITY);
        let (message_sender, messages) = mpsc::unbounded_channel();
        let closed = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(socket_pump(
            socket,
            command_receiver,
            message_sender,
            Arc::clone(&closed),
        ));
        Self {
            commands,
            messages,
            closed,
            pump: task.abort_handle(),
        }
    }

    pub(super) fn is_usable(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    pub(super) async fn start(&mut self, message: Message) -> std::result::Result<(), ()> {
        while self.messages.try_recv().is_ok() {}
        if !self.is_usable() {
            return Err(());
        }
        let (result, response) = oneshot::channel();
        if self
            .commands
            .try_send(SocketCommand::Send { message, result })
            .is_err()
        {
            self.retire();
            return Err(());
        }
        match timeout(SOCKET_IO_TIMEOUT, response).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) | Err(_) => {
                self.retire();
                Err(())
            }
        }
    }

    pub(super) fn finish(&self) {
        if self.commands.try_send(SocketCommand::Finish).is_err() {
            self.retire();
        }
    }

    pub(super) async fn close(self) {
        let (result, closed) = oneshot::channel();
        if self
            .commands
            .try_send(SocketCommand::Close { result })
            .is_ok()
        {
            let _ = timeout(SOCKET_IO_TIMEOUT, closed).await;
        }
        self.retire();
    }

    fn retire(&self) {
        self.closed.store(true, Ordering::Release);
        self.pump.abort();
    }
}

impl Drop for OpenAiWsConnection {
    fn drop(&mut self) {
        self.retire();
    }
}

async fn socket_pump(
    mut socket: RawSocket,
    mut commands: mpsc::Receiver<SocketCommand>,
    messages: mpsc::UnboundedSender<SocketEvent>,
    closed: Arc<AtomicBool>,
) {
    let mut active = false;
    let mut stream_bytes = 0;
    let mut stream_events = 0;
    let mut close_result = None;
    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(SocketCommand::Send { message, result }) if !active => {
                        let sent = matches!(
                            timeout(SOCKET_IO_TIMEOUT, socket.send(message)).await,
                            Ok(Ok(()))
                        );
                        active = sent;
                        stream_bytes = 0;
                        stream_events = 0;
                        let _ = result.send(if sent { Ok(()) } else { Err(()) });
                        if !sent {
                            break;
                        }
                    }
                    Some(SocketCommand::Send { result, .. }) => {
                        let _ = result.send(Err(()));
                    }
                    Some(SocketCommand::Finish) => {
                        active = false;
                    }
                    Some(SocketCommand::Close { result }) => {
                        close_result = Some(result);
                        break;
                    }
                    None => break,
                }
            }
            message = socket.next() => {
                match message {
                    Some(Ok(Message::Ping(payload))) => {
                        if !matches!(
                            timeout(SOCKET_IO_TIMEOUT, socket.send(Message::Pong(payload))).await,
                            Ok(Ok(()))
                        ) {
                            if active {
                                let _ = messages.send(SocketEvent::Closed);
                            }
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Ok(message @ (Message::Text(_) | Message::Binary(_)))) if active => {
                        if message.len() > MAX_SOCKET_MESSAGE_BYTES {
                            let _ = messages.send(SocketEvent::ProtocolError(
                                "WebSocket message exceeded size limit",
                            ));
                            break;
                        }
                        stream_events += 1;
                        if stream_events > MAX_STREAM_EVENTS
                            || account_stream_bytes(
                                &mut stream_bytes,
                                message.len(),
                                "WebSocket",
                            )
                            .is_err()
                        {
                            let _ = messages.send(SocketEvent::ProtocolError(
                                "WebSocket response exceeded size limit",
                            ));
                            break;
                        }
                        if messages.send(SocketEvent::Message(message)).is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(_) | Message::Binary(_))) => {}
                    Some(Err(WebSocketError::Capacity(_))) => {
                        if active {
                            let _ = messages.send(SocketEvent::ProtocolError(
                                "WebSocket message exceeded size limit",
                            ));
                        }
                        break;
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                        if active {
                            let _ = messages.send(SocketEvent::Closed);
                        }
                        break;
                    }
                }
            }
        }
    }
    closed.store(true, Ordering::Release);
    drop(messages);
    let _ = timeout(SOCKET_IO_TIMEOUT, socket.close(None)).await;
    if let Some(result) = close_result {
        let _ = result.send(());
    }
}

pub(super) async fn connect(
    auth: &dyn OpenAiAuthorization,
    socket_url: &str,
    session_id: &str,
) -> Result<OpenAiWsConnection> {
    for attempt in 0..2 {
        let authorization = auth.authorize_websocket(session_id).await?;
        let rejected_token = authorization.token.clone();
        let request = connection_request(socket_url, authorization)?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_SOCKET_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_SOCKET_MESSAGE_BYTES));
        let result = timeout(
            CONNECT_TIMEOUT,
            connect_async_with_config(request, Some(config), false),
        )
        .await
        .map_err(|_| Error::Provider(ProviderError::stream_interrupted(None)))?;
        match result {
            Ok((socket, _)) => return Ok(OpenAiWsConnection::new(socket)),
            Err(error) if attempt == 0 && unauthorized(&error) => {
                if auth.recover_unauthorized(&rejected_token).await? {
                    continue;
                }
                return Err(Error::Auth("WebSocket authorization was rejected".into()));
            }
            Err(error) if unauthorized(&error) => {
                return Err(Error::Auth("WebSocket authorization was rejected".into()));
            }
            Err(error) => return Err(websocket_connect_error(error)),
        }
    }
    unreachable!("WebSocket authorization retry is bounded")
}

fn websocket_connect_error(error: WebSocketError) -> Error {
    if let WebSocketError::Http(response) = error {
        let status = response.status();
        let retry_after = response
            .headers()
            .get(tokio_tungstenite::tungstenite::http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        return Error::Provider(ProviderError::http(
            format!("WebSocket HTTP {status}"),
            status.as_u16(),
            retry_after,
        ));
    }
    Error::Provider(ProviderError::stream_interrupted(None))
}

fn connection_request(
    socket_url: &str,
    authorization: ResolvedAuthorization,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>> {
    let mut request = socket_url.into_client_request().map_err(socket_error)?;
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", authorization.token))
            .map_err(|_| Error::Auth("access token is not a valid header value".into()))?,
    );
    for (name, value) in authorization.headers {
        let name = tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| Error::Auth(format!("{name} is not a valid header name")))?;
        request.headers_mut().insert(
            name,
            HeaderValue::from_str(&value)
                .map_err(|_| Error::Auth("authorization header value is invalid".into()))?,
        );
    }
    Ok(request)
}

fn unauthorized(error: &tokio_tungstenite::tungstenite::Error) -> bool {
    matches!(
        error,
        tokio_tungstenite::tungstenite::Error::Http(response)
            if response.status()
                == tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED
    )
}

pub(super) async fn exchange(
    connection: &mut OpenAiWsConnection,
    body: &Value,
    events: &ModelEventSink,
) -> Result<Exchange> {
    let message = Message::text(serde_json::to_string(body)?);
    if connection.start(message).await.is_err() {
        return Ok(Exchange::Reconnect);
    }
    let result = read_exchange(&mut connection.messages, events).await;
    connection.finish();
    result
}

pub(super) async fn read_exchange(
    messages: &mut mpsc::UnboundedReceiver<SocketEvent>,
    events: &ModelEventSink,
) -> Result<Exchange> {
    let mut web_searches = BTreeSet::new();
    let mut commentary = BTreeSet::new();
    let mut reasoning_part = None;
    let mut output = BTreeMap::new();
    let mut next_output_index = 0;
    let mut streamed_tool_calls = StreamingToolCalls::default();
    let mut stream_bytes = 0;
    let output_delivered = Arc::new(AtomicBool::new(false));
    let tracked_delivery = Arc::clone(&output_delivered);
    let downstream = Arc::clone(events);
    let tracked_events: ModelEventSink = Arc::new(move |event| {
        downstream(event)?;
        tracked_delivery.store(true, Ordering::Release);
        Ok(())
    });
    loop {
        let message = match timeout(STREAM_IDLE_TIMEOUT, messages.recv()).await {
            Ok(Some(SocketEvent::Message(message))) => message,
            Ok(Some(SocketEvent::ProtocolError(message))) => {
                return Err(Error::Provider(message.into()));
            }
            Ok(Some(SocketEvent::Closed) | None) | Err(_) => return Ok(Exchange::Reconnect),
        };
        if message.len() > MAX_SOCKET_MESSAGE_BYTES {
            return Err(Error::Provider(
                "WebSocket message exceeded size limit".into(),
            ));
        }
        account_stream_bytes(&mut stream_bytes, message.len(), "WebSocket")?;
        let event = match message {
            Message::Text(text) => serde_json::from_str(text.as_ref())?,
            Message::Binary(bytes) => serde_json::from_slice(&bytes)?,
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            Message::Close(_) => return Ok(Exchange::Reconnect),
        };
        collect_stream_output(&event, &mut output)?;
        emit_ready_tool_calls(
            &output,
            &mut next_output_index,
            &mut streamed_tool_calls,
            &tracked_events,
        )?;
        if emit_web_event(&event, &mut web_searches, &tracked_events)? {
            continue;
        }
        if emit_reasoning_event(&event, &mut reasoning_part, &tracked_events)? {
            continue;
        }
        if emit_text_event(&event, &mut commentary, &tracked_events)? {
            continue;
        }
        match event.get("type").and_then(Value::as_str) {
            Some("response.completed") => {
                let response = event
                    .get("response")
                    .cloned()
                    .ok_or_else(|| Error::Provider("completion omitted response".into()))?;
                return Ok(Exchange::Completed(attach_stream_output(response, &output)));
            }
            Some("error" | "response.failed" | "response.incomplete") => {
                return failed_exchange(&event, output_delivered.load(Ordering::Acquire));
            }
            _ => {}
        }
    }
}

pub(super) fn failed_exchange(event: &Value, output_delivered: bool) -> Result<Exchange> {
    let code = response_error_code(event);
    let message = response_error(event);
    let retry_after = response_retry_after(event);
    match code {
        Some("previous_response_not_found") => Ok(Exchange::PreviousMissing { output_delivered }),
        Some("websocket_connection_limit_reached") => Ok(Exchange::ConnectionLimit { retry_after }),
        _ if retryable_response_error(code, &message) => Ok(Exchange::Retry { retry_after }),
        _ => Err(Error::Provider(message.into())),
    }
}

fn response_retry_after(event: &Value) -> Option<String> {
    [
        "/headers/retry-after",
        "/headers/retry_after",
        "/error/retry_after",
        "/response/error/retry_after",
    ]
    .into_iter()
    .find_map(|pointer| event.pointer(pointer))
    .and_then(|value| match value {
        Value::String(value) if !value.trim().is_empty() => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn response_error_code(event: &Value) -> Option<&str> {
    event
        .pointer("/error/code")
        .or_else(|| event.pointer("/response/error/code"))
        .or_else(|| event.pointer("/error/type"))
        .or_else(|| event.pointer("/response/error/type"))
        .and_then(Value::as_str)
        .filter(|code| !code.is_empty())
}

fn retryable_response_error(code: Option<&str>, message: &str) -> bool {
    matches!(
        code,
        Some(
            "server_error"
                | "internal_server_error"
                | "rate_limit_exceeded"
                | "service_unavailable"
                | "temporarily_unavailable"
        )
    ) || (message.starts_with("An error occurred while processing your request.")
        && message.contains("retry your request"))
}

fn socket_error(error: WebSocketError) -> Error {
    let kind = match error {
        WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed => "closed",
        WebSocketError::Io(_) => "I/O",
        WebSocketError::Tls(_) => "TLS",
        WebSocketError::Capacity(_) => "capacity",
        WebSocketError::Protocol(_) => "protocol",
        WebSocketError::WriteBufferFull(_) => "write buffer",
        WebSocketError::Utf8(_) => "UTF-8",
        WebSocketError::AttackAttempt => "attack rejected",
        WebSocketError::Url(_) => "URL",
        WebSocketError::Http(_) => "HTTP handshake",
        WebSocketError::HttpFormat(_) => "HTTP format",
    };
    Error::Provider(format!("WebSocket {kind} failure").into())
}
