//! A desktop pane's private localhost noVNC server and authenticated gateway connection.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::{SinkExt, StreamExt};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::HeaderValue;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use mobius::SessionId;
use mobius_gateway::client::{Endpoint, GatewayEvents, GatewaySender};
use mobius_gateway::wire::{ClientMessage, ComputerView, ServerMessage};
use rust_embed::RustEmbed;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

const CHUNK: usize = mobius_gateway::wire::MAX_DESKTOP_CHUNK_BYTES;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(RustEmbed)]
#[folder = "../../assets/desktop-viewer/"]
struct ViewerAssets;

#[derive(Debug)]
pub enum DesktopEvent {
    Control {
        enabled: bool,
        is_owner: bool,
        session_id: Option<SessionId>,
    },
    Error(Arc<str>),
    Closed(String),
}

/// Dropping the pane closes its listener, stream and user-control lease.
#[derive(Debug)]
pub struct DesktopViewer {
    pub url: String,
    commands: mpsc::Sender<Control>,
    events: Option<mpsc::Receiver<DesktopEvent>>,
    stop: watch::Sender<bool>,
}

struct Control {
    enabled: bool,
    session_id: Option<SessionId>,
}

impl DesktopViewer {
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<DesktopEvent>> {
        self.events.take()
    }

    /// Takes or hands back control of the computer.
    ///
    /// # Errors
    ///
    /// When the computer connection's queue is full or it has closed.
    pub fn set_control(&self, enabled: bool, session_id: Option<SessionId>) -> Result<()> {
        self.commands
            .try_send(Control {
                enabled,
                session_id,
            })
            .context("The computer connection is busy")
    }
}

impl Drop for DesktopViewer {
    fn drop(&mut self) {
        if let Err(error) = self.stop.send(true) {
            tracing::debug!(%error, "the computer connection had already closed");
        }
    }
}

pub(super) async fn open(
    endpoint: Endpoint,
    token: String,
    stop: watch::Sender<bool>,
    mut stopped: watch::Receiver<bool>,
) -> Result<DesktopViewer> {
    ensure!(
        !endpoint.is_plaintext(),
        "The computer viewer requires a secure gateway"
    );
    let mut options = super::catalog_free();
    let viewer = mobius_gateway::wire::ClientCapabilities {
        remote_desktop_viewer: true,
        ..Default::default()
    };
    let connect = super::connection::connect_as(&endpoint, &token, viewer, &mut options);
    let client = tokio::select! {
        _ = stopped.changed() => anyhow::bail!("The gateway changed"),
        result = tokio::time::timeout(Duration::from_secs(30), connect) => result.context("The computer connection timed out")??,
    };
    let (sender, mut gateway) = client.into_parts();
    let ready = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(frame) = gateway.next().await? {
            if let ServerMessage::Ready { payload } = frame.message {
                return Ok::<_, mobius_gateway::Error>(
                    payload.computer_view == ComputerView::RemoteDesktop,
                );
            }
        }
        Ok(false)
    })
    .await
    .context("The computer did not become ready")??;
    ensure!(ready, "This gateway has no computer desktop");
    ensure!(!*stopped.borrow(), "The gateway changed");
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let host = listener.local_addr()?.to_string();
    let prefix = format!("/{}/", uuid::Uuid::new_v4().simple());
    let url = format!("http://{host}{prefix}index.html");
    let (local, incoming) = mpsc::channel(2);
    let context = Arc::new(HttpContext {
        origin: format!("http://{host}"),
        host,
        prefix,
        connected: AtomicBool::new(false),
        local,
    });
    let serving = tokio::spawn(serve(listener, context, stopped.clone())); // rules-allow: a watch receiver is a channel handle and the server and the relay each need one
    let (commands, control) = mpsc::channel::<Control>(8);
    let (events, receiver) = mpsc::channel(8);
    tokio::spawn(run(
        (sender, gateway),
        serving,
        stopped,
        control,
        incoming,
        events,
    ));
    Ok(DesktopViewer {
        url,
        commands,
        events: Some(receiver),
        stop,
    })
}

async fn run(
    (sender, mut gateway): (GatewaySender, GatewayEvents),
    serving: tokio::task::JoinHandle<()>,
    mut stopped: watch::Receiver<bool>,
    mut control: mpsc::Receiver<Control>,
    mut incoming: mpsc::Receiver<LocalEvent>,
    events: mpsc::Sender<DesktopEvent>,
) {
    let mut output: Option<mpsc::Sender<Vec<u8>>> = None;
    let mut stream_request = None;
    let mut streaming = false;
    let result: Result<()> = async {
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    command = control.recv() => match command {
                        Some(command) => send(&sender, ClientMessage::SetDesktopControl {
                            request_id: super::request_id(), enabled: command.enabled, session_id: command.session_id,
                        }).await?,
                        None => break,
                    },
                    event = incoming.recv() => match event {
                        Some(LocalEvent::Opened(channel)) => {
                            output = Some(channel);
                            streaming = false;
                            let request_id = super::request_id();
                            send(&sender, ClientMessage::SetDesktopStream { request_id: Arc::clone(&request_id), enabled: true }).await?;
                            stream_request = Some(request_id);
                        },
                        Some(LocalEvent::Data(data)) => {
                            for chunk in data.chunks(CHUNK) {
                                send(&sender, ClientMessage::DesktopData { data: chunk.to_vec() }).await?;
                            }
                        },
                        Some(LocalEvent::Closed) => {
                            output = None;
                            streaming = false;
                            let request_id = super::request_id();
                            send(&sender, ClientMessage::SetDesktopStream { request_id: Arc::clone(&request_id), enabled: false }).await?;
                            stream_request = Some(request_id);
                        },
                        None => break,
                    },
                    frame = gateway.next() => {
                        let Some(frame) = frame? else { anyhow::bail!("The computer connection closed"); };
                        match frame.message {
                            ServerMessage::DesktopData { data } => {
                                if streaming && let Some(channel) = &output {
                                    // The local socket can close before its Closed event reaches this loop.
                                    if let Err(error) = tokio::time::timeout(IO_TIMEOUT, channel.send(data)).await.context("The computer viewer stopped receiving")? {
                                        tracing::debug!(%error, "the computer viewer closed before its data arrived");
                                    }
                                }
                            },
                            ServerMessage::DesktopControlState { enabled, is_owner, session_id, .. } => {
                                emit(&events, DesktopEvent::Control { enabled, is_owner, session_id }).await?;
                            },
                            ServerMessage::DesktopStreamState { request_id, enabled } if stream_request.as_ref() == Some(&request_id) => {
                                streaming = enabled;
                                if !enabled { output = None; }
                            },
                            ServerMessage::Rejected { request_id, message, .. } => {
                                if stream_request.as_ref() == Some(&request_id) { anyhow::bail!("{message}"); }
                                emit(&events, DesktopEvent::Error(message)).await?;
                            },
                            ServerMessage::Error { message, fatal, .. } => {
                                if fatal { anyhow::bail!("{message}"); }
                                emit(&events, DesktopEvent::Error(message)).await?;
                            },
                            _ => {},
                        }
                    },
                }
            }
            Ok(())
        }.await;
    // The connection is the lease's owner. These requests speed up cleanup; dropping
    // the authenticated connection still releases it when a send cannot complete.
    if let Err(error) = send(
        &sender,
        ClientMessage::SetDesktopControl {
            request_id: super::request_id(),
            enabled: false,
            session_id: None,
        },
    )
    .await
    {
        tracing::debug!(%error, "releasing desktop control failed; closing the connection releases it");
    }
    serving.abort();
    if let Err(error) = emit(
        &events,
        DesktopEvent::Closed(
            result
                .err()
                .map_or_else(|| "Computer disconnected".into(), |error| error.to_string()),
        ),
    )
    .await
    {
        tracing::debug!(%error, "the computer pane was gone when its viewer closed");
    }
}

async fn emit(events: &mpsc::Sender<DesktopEvent>, event: DesktopEvent) -> Result<()> {
    tokio::time::timeout(IO_TIMEOUT, events.send(event))
        .await
        .context("The computer pane stopped receiving")??;
    Ok(())
}

async fn send(sender: &GatewaySender, message: ClientMessage) -> Result<()> {
    tokio::time::timeout(IO_TIMEOUT, sender.send(message))
        .await
        .context("The computer gateway stopped receiving")??;
    Ok(())
}

enum LocalEvent {
    Opened(mpsc::Sender<Vec<u8>>),
    Data(Vec<u8>),
    Closed,
}

struct HttpContext {
    host: String,
    origin: String,
    prefix: String,
    connected: AtomicBool,
    local: mpsc::Sender<LocalEvent>,
}

async fn serve(
    listener: TcpListener,
    context: Arc<HttpContext>,
    mut stopped: watch::Receiver<bool>,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = stopped.changed() => break,
            accepted = listener.accept(), if connections.len() < 32 => {
                let Ok((stream, _)) = accepted else { break; };
                let context = Arc::clone(&context);
                let stopped = stopped.clone(); // rules-allow: a watch receiver is a channel handle and each connection needs one
                connections.spawn(async move {
                    let service = service_fn(move |request| std::future::ready(Ok::<_, Infallible>(handle(request, Arc::clone(&context), stopped.clone())))); // rules-allow: a watch receiver is a channel handle and each request needs one
                    match tokio::time::timeout(Duration::from_secs(30),
                        http1::Builder::new().serve_connection(TokioIo::new(stream), service).with_upgrades()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::debug!(%error, "a computer viewer connection failed"),
                        Err(elapsed) => tracing::debug!(%elapsed, "a computer viewer connection timed out"),
                    }
                });
            },
            Some(_) = connections.join_next() => {},
        }
    }
    connections.abort_all();
}

fn reply(status: StatusCode, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(body.into()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

fn asset(path: &str, context: &HttpContext) -> Response<Full<Bytes>> {
    let Some(asset) = ViewerAssets::get(path) else {
        return reply(StatusCode::NOT_FOUND, "");
    };
    let mut response = reply(StatusCode::OK, asset.data.into_owned());
    if let Ok(mime) = mime_guess::from_path(path)
        .first_or_octet_stream()
        .as_ref()
        .parse()
    {
        response.headers_mut().insert("content-type", mime);
    }
    response.headers_mut().insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    if let Ok(csp) = format!("default-src 'none'; base-uri 'none'; frame-ancestors 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src ws://{}", context.host).parse() {
            response.headers_mut().insert("content-security-policy", csp);
        }
    response
}

fn handle(
    mut request: Request<Incoming>,
    context: Arc<HttpContext>,
    stopped: watch::Receiver<bool>,
) -> Response<Full<Bytes>> {
    if *stopped.borrow()
        || request.method() != hyper::Method::GET
        || request
            .headers()
            .get("host")
            .and_then(|value| value.to_str().ok())
            != Some(context.host.as_str())
    {
        return reply(StatusCode::FORBIDDEN, "");
    }
    let Some(path) = request.uri().path().strip_prefix(&context.prefix) else {
        return reply(StatusCode::NOT_FOUND, "");
    };
    if path != "rfb" {
        return asset(path, &context);
    }

    if request
        .headers()
        .get("origin")
        .and_then(|value| value.to_str().ok())
        != Some(context.origin.as_str())
        || request
            .headers()
            .get("upgrade")
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| !value.eq_ignore_ascii_case("websocket"))
        || request
            .headers()
            .get("connection")
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| {
                !value
                    .split(',')
                    .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
            })
        || request
            .headers()
            .get("sec-websocket-version")
            .and_then(|value| value.to_str().ok())
            != Some("13")
    {
        return reply(StatusCode::FORBIDDEN, "");
    }
    let Some(key) = request
        .headers()
        .get("sec-websocket-key")
        .filter(|key| {
            STANDARD
                .decode(key.as_bytes())
                .is_ok_and(|decoded| decoded.len() == 16)
        })
        .map(|key| derive_accept_key(key.as_bytes()))
    else {
        return reply(StatusCode::BAD_REQUEST, "");
    };
    if context.connected.swap(true, Ordering::AcqRel) {
        return reply(StatusCode::CONFLICT, "");
    }
    let upgrade = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        if let Ok(upgraded) = upgrade.await {
            let config = WebSocketConfig::default()
                .max_message_size(Some(1024 * 1024))
                .max_frame_size(Some(1024 * 1024));
            let socket = WebSocketStream::from_raw_socket(
                TokioIo::new(upgraded),
                Role::Server,
                Some(config),
            )
            .await;
            if let Err(error) = relay(socket, &context.local, stopped).await {
                tracing::debug!(%error, "the computer viewer's socket ended");
            }
        }
        if let Err(error) = context.local.send(LocalEvent::Closed).await {
            tracing::debug!(%error, "the computer connection was gone when its viewer closed");
        }
        context.connected.store(false, Ordering::Release);
    });
    let mut response = reply(StatusCode::SWITCHING_PROTOCOLS, "");
    let headers = response.headers_mut();
    headers.insert("upgrade", HeaderValue::from_static("websocket"));
    headers.insert("connection", HeaderValue::from_static("Upgrade"));
    if let Ok(key) = key.parse() {
        headers.insert("sec-websocket-accept", key);
    }
    response
}

async fn relay<S>(
    socket: WebSocketStream<S>,
    local: &mpsc::Sender<LocalEvent>,
    mut stopped: watch::Receiver<bool>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (outgoing, mut data) = mpsc::channel(2);
    local.send(LocalEvent::Opened(outgoing)).await?;
    let (mut writer, mut reader) = socket.split();
    let result = {
        let read = async {
            while let Some(message) = reader.next().await {
                match message? {
                    Message::Binary(bytes) => {
                        local.send(LocalEvent::Data(bytes.to_vec())).await?;
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {}
                    _ => anyhow::bail!("Desktop WebSocket requires binary messages"),
                }
            }
            Ok(())
        };
        let write = async {
            while let Some(bytes) = data.recv().await {
                tokio::time::timeout(IO_TIMEOUT, writer.send(Message::Binary(bytes.into())))
                    .await
                    .context("Desktop WebSocket send timed out")??;
            }
            Ok(())
        };
        tokio::select! {
            _ = stopped.changed() => Ok(()),
            result = read => result,
            result = write => result,
        }
    };
    if let Err(error) = writer.close().await {
        tracing::debug!(%error, "the computer viewer's socket did not close cleanly");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    struct LocalServer {
        host: String,
        local: mpsc::Receiver<LocalEvent>,
        stop: watch::Sender<bool>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for LocalServer {
        fn drop(&mut self) {
            if let Err(error) = self.stop.send(true) {
                tracing::debug!(%error, "the test server had already stopped");
            }
            self.task.abort();
        }
    }

    async fn server() -> LocalServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let (sender, local) = mpsc::channel(2);
        let (stop, stopped) = watch::channel(false);
        let context = Arc::new(HttpContext {
            host: host.clone(),
            origin: format!("http://{host}"),
            prefix: "/capability/".into(),
            connected: AtomicBool::new(false),
            local: sender,
        });
        let task = tokio::spawn(serve(listener, context, stopped));
        LocalServer {
            host,
            local,
            stop,
            task,
        }
    }

    async fn get(server: &LocalServer, host: &str, path: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(&server.host).await.unwrap();
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        String::from_utf8(bytes).unwrap()
    }

    fn websocket(
        server: &LocalServer,
        origin: &str,
    ) -> tokio_tungstenite::tungstenite::http::Request<()> {
        let mut request = format!("ws://{}/capability/rfb", server.host)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
        request
    }

    #[tokio::test]
    async fn assets_require_the_capability_and_exact_loopback_host() {
        let server = server().await;
        let valid = get(&server, &server.host, "/capability/index.html").await;
        assert!(valid.starts_with("HTTP/1.1 200"));
        assert!(valid.contains("cache-control: no-store"));
        assert!(valid.contains("referrer-policy: no-referrer"));
        assert!(valid.contains("style-src 'self'"));
        assert!(!valid.contains("unsafe-inline"));
        assert!(!valid.contains("<style>"));
        assert!(valid.contains("<link rel=\"stylesheet\" href=\"viewer.css\">"));
        let stylesheet = get(&server, &server.host, "/capability/viewer.css").await;
        assert!(stylesheet.starts_with("HTTP/1.1 200"));
        assert!(stylesheet.contains("content-type: text/css"));
        assert!(
            get(&server, &server.host, "/index.html")
                .await
                .starts_with("HTTP/1.1 404")
        );
        assert!(
            get(&server, "attacker.example", "/capability/index.html")
                .await
                .starts_with("HTTP/1.1 403")
        );
        assert!(
            get(&server, &server.host, "/capability/../Cargo.toml")
                .await
                .starts_with("HTTP/1.1 404")
        );
    }

    #[tokio::test]
    async fn relay_drains_output_while_its_input_queue_is_full() {
        use std::future::Future;
        use std::task::Context;

        let (browser, server) = tokio::io::duplex(8 * CHUNK);
        let mut browser = WebSocketStream::from_raw_socket(browser, Role::Client, None).await;
        let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let (sender, mut local) = mpsc::channel(2);
        let (stop, stopped) = watch::channel(false);
        let mut relay = Box::pin(relay(server, &sender, stopped));
        let mut context = Context::from_waker(futures_util::task::noop_waker_ref());
        assert!(relay.as_mut().poll(&mut context).is_pending());
        let Some(LocalEvent::Opened(output)) = local.recv().await else {
            panic!("expected stream start");
        };

        let input: Vec<Vec<u8>> = (1..=3).map(|byte| vec![byte; CHUNK]).collect();
        for frame in &input {
            browser
                .send(Message::Binary(frame.clone().into()))
                .await
                .unwrap();
        }
        // All three frames are already readable. With no output or stop event, this
        // poll can only stop at the third send to the bounded, undrained input queue.
        assert!(relay.as_mut().poll(&mut context).is_pending());
        assert_eq!(local.len(), 2);

        let frames: Vec<Vec<u8>> = (4..=6).map(|byte| vec![byte; CHUNK]).collect();
        for frame in &frames[..2] {
            output.send(frame.clone()).await.unwrap();
        }
        assert!(relay.as_mut().poll(&mut context).is_pending());
        output
            .try_send(frames[2].clone())
            .expect("a full input queue must not block the output pump");
        assert!(relay.as_mut().poll(&mut context).is_pending());
        for frame in &frames {
            assert_eq!(local.len(), 2, "input must stay full while output drains");
            assert_eq!(
                tokio::time::timeout(IO_TIMEOUT, browser.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
                Message::Binary(frame.clone().into())
            );
        }

        for expected in &input[..2] {
            let Some(LocalEvent::Data(actual)) = local.recv().await else {
                panic!("expected input frame");
            };
            assert_eq!(&actual, expected);
        }
        assert!(relay.as_mut().poll(&mut context).is_pending());
        let Some(LocalEvent::Data(actual)) = local.recv().await else {
            panic!("expected final input frame");
        };
        assert_eq!(actual, input[2]);

        // Cancellation must also work while the reader is blocked on a full queue.
        for frame in &input {
            browser
                .send(Message::Binary(frame.clone().into()))
                .await
                .unwrap();
        }
        assert!(relay.as_mut().poll(&mut context).is_pending());
        assert_eq!(local.len(), 2);
        stop.send(true).unwrap();
        tokio::time::timeout(IO_TIMEOUT, relay.as_mut())
            .await
            .expect("stopping must cancel the blocked reader")
            .unwrap();
        assert!(output.is_closed());
    }

    #[tokio::test]
    async fn websocket_requires_the_viewer_origin_and_preserves_binary_bytes() {
        let mut server = server().await;
        let rejected =
            tokio_tungstenite::connect_async(websocket(&server, "https://attacker.example"))
                .await
                .unwrap_err();
        let tokio_tungstenite::tungstenite::Error::Http(response) = rejected else {
            panic!("expected HTTP refusal");
        };
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let origin = format!("http://{}", server.host);
        let (mut socket, _) = tokio_tungstenite::connect_async(websocket(&server, &origin))
            .await
            .unwrap();
        let Some(LocalEvent::Opened(output)) = server.local.recv().await else {
            panic!("expected stream start");
        };
        let input: Vec<u8> = (0..=255).collect();
        socket
            .send(Message::Binary(input.clone().into()))
            .await
            .unwrap();
        let Some(LocalEvent::Data(data)) = server.local.recv().await else {
            panic!("expected binary data");
        };
        assert_eq!(data, input);
        output.send(vec![0, 128, 255]).await.unwrap();
        assert_eq!(
            socket.next().await.unwrap().unwrap(),
            Message::Binary(vec![0, 128, 255].into())
        );
        let rejected = tokio_tungstenite::connect_async(websocket(&server, &origin))
            .await
            .unwrap_err();
        let tokio_tungstenite::tungstenite::Error::Http(response) = rejected else {
            panic!("expected duplicate refusal");
        };
        assert_eq!(response.status(), StatusCode::CONFLICT);
        socket.close(None).await.unwrap();
        assert!(matches!(
            server.local.recv().await,
            Some(LocalEvent::Closed)
        ));
        let (mut resumed, _) = tokio_tungstenite::connect_async(websocket(&server, &origin))
            .await
            .unwrap();
        assert!(matches!(
            server.local.recv().await,
            Some(LocalEvent::Opened(_))
        ));
        resumed.close(None).await.unwrap();
    }
}
