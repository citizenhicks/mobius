//! The agent's browser, watched over Chrome DevTools: the computer-control worker opens a
//! loopback DevTools port, so a möbius app on the gateway's machine can stream the agent's
//! page as a screencast and share it by sending input to the same page.

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use gpui::RenderImage;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper::{Request, header};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::numeric::ToFloat;

/// What the watched browser reports to the tab.
pub enum BrowserEvent {
    /// A decoded frame and the page's size in CSS pixels, for mapping input.
    Frame {
        image: Arc<RenderImage>,
        width: f32,
        height: f32,
    },
    Page {
        url: String,
        title: String,
    },
    /// The connection ended; the text says why.
    Closed(String),
}

/// Input for the watched page, in its CSS pixels.
#[derive(Clone, Debug, PartialEq)]
pub enum BrowserInput {
    Visible(bool),
    Press { x: f32, y: f32, clicks: u32 },
    Release { x: f32, y: f32, clicks: u32 },
    Move { x: f32, y: f32 },
    Wheel { x: f32, y: f32, dx: f32, dy: f32 },
    Text(String),
    Key(Key),
}

/// A non-text key as DevTools names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub key: &'static str,
    pub code: &'static str,
    pub virtual_code: u32,
}

/// The DevTools key for a GPUI key name, for keys that do not type text.
pub fn special_key(name: &str) -> Option<Key> {
    let (key, code, virtual_code) = match name {
        "enter" => ("Enter", "Enter", 13),
        "backspace" => ("Backspace", "Backspace", 8),
        "tab" => ("Tab", "Tab", 9),
        "escape" => ("Escape", "Escape", 27),
        "delete" => ("Delete", "Delete", 46),
        "left" => ("ArrowLeft", "ArrowLeft", 37),
        "up" => ("ArrowUp", "ArrowUp", 38),
        "right" => ("ArrowRight", "ArrowRight", 39),
        "down" => ("ArrowDown", "ArrowDown", 40),
        "home" => ("Home", "Home", 36),
        "end" => ("End", "End", 35),
        "pageup" => ("PageUp", "PageUp", 33),
        "pagedown" => ("PageDown", "PageDown", 34),
        _ => return None,
    };
    Some(Key {
        key,
        code,
        virtual_code,
    })
}

/// Starts watching the browser at `origin` (`http://127.0.0.1:<port>`). Dropping the
/// receiver cancels startup or streaming; dropping the sender ends an established stream.
pub fn watch(origin: String) -> (mpsc::Sender<BrowserInput>, mpsc::Receiver<BrowserEvent>) {
    let (input, inputs) = mpsc::channel(64);
    // Keep frame memory bounded when the UI stalls, without reordering page events.
    let (events, received) = mpsc::channel(2);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        let reason = match runtime {
            Ok(runtime) => runtime
                .block_on(async {
                    tokio::select! {
                        result = stream(&origin, inputs, &events) => result,
                        () = events.closed() => Ok(()),
                    }
                })
                .err()
                .map_or_else(
                    || "The agent's browser closed.".into(),
                    |error| error.to_string(),
                ),
            Err(error) => error.to_string(),
        };
        if let Err(error) = events.blocking_send(BrowserEvent::Closed(reason)) {
            tracing::debug!(%error, "The browser tab closed before hearing why its agent's browser ended");
        }
    });
    (input, received)
}

/// Only a loopback origin: the port is useful on this machine alone.
pub fn loopback_origin(origin: &str) -> bool {
    origin
        .strip_prefix("http://127.0.0.1:")
        .and_then(|port| port.parse::<u16>().ok())
        .is_some_and(|port| port != 0)
}

async fn browser_socket(origin: &str) -> anyhow::Result<String> {
    const MAX_RESPONSE_BYTES: usize = 64 * 1024;
    anyhow::ensure!(loopback_origin(origin), "not a loopback DevTools address");
    let address = origin.trim_start_matches("http://");
    // Connecting this socket ourselves bypasses proxies; the HTTP/1 client never redirects.
    let socket = tokio::net::TcpStream::connect(address).await?;
    let (mut sender, connection) = http1::Builder::new()
        .max_buf_size(MAX_RESPONSE_BYTES)
        .handshake(TokioIo::new(socket))
        .await?;
    let request = Request::get("/json/version")
        .header(header::HOST, address)
        .header(header::CONNECTION, "close")
        .body(Empty::<Bytes>::new())?;
    // Both futures belong to discovery: cancellation or either failure drops the socket.
    let ((), body) = tokio::try_join!(
        async { connection.await.map_err(anyhow::Error::from) },
        async move {
            let response = sender.send_request(request).await?;
            let length = response
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok()?.parse::<usize>().ok())
                .ok_or_else(|| anyhow::anyhow!("DevTools response has no length"))?;
            anyhow::ensure!(length <= MAX_RESPONSE_BYTES, "DevTools response too long");
            let body = Limited::new(response.into_body(), MAX_RESPONSE_BYTES)
                .collect()
                .await
                .map_err(anyhow::Error::from_boxed)?;
            Ok::<_, anyhow::Error>(body.to_bytes())
        }
    )?;
    let version: Value = serde_json::from_str(&String::from_utf8_lossy(&body))?;
    let socket = version
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("the agent's browser gave no DevTools socket"))?;
    let uri: hyper::Uri = socket.parse()?;
    anyhow::ensure!(
        uri.scheme_str() == Some("ws")
            && uri.host() == Some("127.0.0.1")
            && uri.port_u16().is_some_and(|port| port != 0)
            && uri
                .authority()
                .is_some_and(|authority| !authority.as_str().contains('@')),
        "not a loopback DevTools socket"
    );
    Ok(socket.to_owned())
}

/// The page the agent is on: its newest open page, since popups and new tabs open after
/// the one it started with.
#[derive(Default)]
struct Pages {
    order: Vec<String>,
    info: HashMap<String, (String, String)>,
}

impl Pages {
    fn update(&mut self, info: &Value) {
        if info["type"] != "page" {
            return;
        }
        let Some(id) = info["targetId"].as_str() else {
            return;
        };
        if !self.order.iter().any(|known| known == id) {
            self.order.push(id.to_owned());
        }
        let text = |key: &str| info[key].as_str().unwrap_or_default().to_owned();
        self.info
            .insert(id.to_owned(), (text("url"), text("title")));
    }

    fn remove(&mut self, id: &str) {
        self.order.retain(|known| known != id);
        self.info.remove(id);
    }

    fn current(&self) -> Option<&str> {
        self.order.last().map(String::as_str)
    }
}

struct Session {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next: u64,
}

impl Session {
    async fn input(
        &mut self,
        input: BrowserInput,
        watched: Option<&str>,
        visible: &mut bool,
    ) -> anyhow::Result<()> {
        if let BrowserInput::Visible(enabled) = input {
            *visible = enabled;
            if let Some(page) = watched {
                let (method, params) = if enabled {
                    (
                        "Page.startScreencast",
                        json!({"format": "jpeg", "quality": 70, "everyNthFrame": 1}),
                    )
                } else {
                    ("Page.stopScreencast", json!({}))
                };
                self.send(method, params, Some(page)).await?;
            }
        } else if let Some(page) = watched {
            for (method, params) in dispatch(&input) {
                self.send(method, params, Some(page)).await?;
            }
        }
        Ok(())
    }

    async fn send(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> anyhow::Result<u64> {
        self.next += 1;
        let mut message = json!({"id": self.next, "method": method, "params": params});
        if let (Some(session), Some(fields)) = (session, message.as_object_mut()) {
            fields.insert("sessionId".to_owned(), json!(session));
        }
        self.socket.send(Message::text(message.to_string())).await?;
        Ok(self.next)
    }

    async fn switch_page(
        &mut self,
        page: &str,
        previous: Option<&str>,
        visible: bool,
    ) -> anyhow::Result<()> {
        if let Some(previous) = previous {
            self.send("Page.stopScreencast", json!({}), Some(previous))
                .await?;
            self.send(
                "Target.detachFromTarget",
                json!({"sessionId": previous}),
                None,
            )
            .await?;
        }
        if visible {
            self.send(
                "Page.startScreencast",
                json!({"format": "jpeg", "quality": 70, "everyNthFrame": 1}),
                Some(page),
            )
            .await?;
        }
        Ok(())
    }

    async fn screencast_frame(
        &mut self,
        message: &Value,
        watched: Option<&str>,
        events: &mpsc::Sender<BrowserEvent>,
    ) -> anyhow::Result<()> {
        let Some(page) = message["sessionId"].as_str() else {
            return Ok(());
        };
        let params = &message["params"];
        // ACKs and decoded frames stop advancing when the tab's event queue fills.
        self.send(
            "Page.screencastFrameAck",
            json!({"sessionId": params["sessionId"]}),
            Some(page),
        )
        .await?;
        if watched == Some(page)
            && let Some(image) = params["data"].as_str().and_then(decode)
        {
            let metadata = &params["metadata"];
            let size = |key: &str| metadata[key].as_f64().unwrap_or_default().to_f32();
            events
                .send(BrowserEvent::Frame {
                    image,
                    width: size("deviceWidth"),
                    height: size("deviceHeight"),
                })
                .await?;
        }
        Ok(())
    }
}

async fn stream(
    origin: &str,
    mut inputs: mpsc::Receiver<BrowserInput>,
    events: &mpsc::Sender<BrowserEvent>,
) -> anyhow::Result<()> {
    let (socket, _) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio_tungstenite::connect_async(browser_socket(origin).await?)
            .await
            .map_err(anyhow::Error::from)
    })
    .await
    .map_err(|error| {
        anyhow::Error::new(error).context("the agent's browser connection timed out")
    })??;
    let mut session = Session { socket, next: 0 };
    let mut pages = Pages::default();
    // The attached page, its DevTools session, and the attach request in flight.
    let mut watched: Option<(String, String)> = None;
    let mut visible = true;
    let mut attaching: Option<(u64, String)> = None;
    session
        .send("Target.setDiscoverTargets", json!({"discover": true}), None)
        .await?;
    loop {
        // Follow the agent to its newest page.
        if let Some(target) = pages.current()
            && watched.as_ref().is_none_or(|(id, _)| id != target)
            && attaching.is_none()
        {
            let request = session
                .send(
                    "Target.attachToTarget",
                    json!({"targetId": target, "flatten": true}),
                    None,
                )
                .await?;
            attaching = Some((request, target.to_owned()));
        }
        tokio::select! {
            input = inputs.recv() => {
                let Some(input) = input else { return Ok(()) };
                session.input(input, watched.as_ref().map(|(_, page)| page.as_str()), &mut visible).await?;
            }
            message = session.socket.next() => {
                let Some(message) = message else { return Ok(()) };
                let Message::Text(text) = message? else { continue };
                let message: Value = serde_json::from_str(&text)?;
                if let Some((_, target)) = attaching.take_if(|(request, _)| message.get("id").and_then(Value::as_u64) == Some(*request))
                {
                    let Some(page) = message.pointer("/result/sessionId").and_then(Value::as_str) else { continue };
                    session.switch_page(page, watched.as_ref().map(|(_, page)| page.as_str()), visible).await?;
                    if let Some((url, title)) = pages.info.get(&target) {
                        events.send(BrowserEvent::Page { url: url.clone(), title: title.clone() }).await?; // rules-allow: the page's info stays for a later re-attach; the tab gets its own copy
                    }
                    watched = Some((target, page.to_owned()));
                    continue;
                }
                let params = message.get("params").unwrap_or(&Value::Null);
                match message.get("method").and_then(Value::as_str) {
                    Some("Target.targetCreated" | "Target.targetInfoChanged") => {
                        let info = &params["targetInfo"];
                        pages.update(info);
                        if watched.as_ref().is_some_and(|(id, _)| Some(id.as_str()) == info["targetId"].as_str()) {
                            let text = |key: &str| info[key].as_str().unwrap_or_default().to_owned();
                            events.send(BrowserEvent::Page { url: text("url"), title: text("title") }).await?;
                        }
                    }
                    Some("Target.targetDestroyed") => {
                        if let Some(id) = params["targetId"].as_str() {
                            pages.remove(id);
                            if watched.as_ref().is_some_and(|(watched, _)| watched == id) {
                                // Destroying a target also destroys its sessions and screencast.
                                watched = None;
                            }
                        }
                    }
                    Some("Page.screencastFrame") => {
                        session.screencast_frame(&message, watched.as_ref().filter(|_| visible).map(|(_, page)| page.as_str()), events).await?;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// A screencast JPEG as a GPUI image (BGRA frames).
fn decode(data: &str) -> Option<Arc<RenderImage>> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    let mut pixels = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
        .ok()?
        .into_rgba8();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])))
}

/// The DevTools commands for one input.
fn dispatch(input: &BrowserInput) -> Vec<(&'static str, Value)> {
    let mouse = |kind: &str, x: f32, y: f32, clicks: u32| json!({"type": kind, "x": x, "y": y, "button": "left", "clickCount": clicks});
    match input {
        BrowserInput::Visible(_) => Vec::new(),
        BrowserInput::Press { x, y, clicks } => {
            vec![(
                "Input.dispatchMouseEvent",
                mouse("mousePressed", *x, *y, *clicks),
            )]
        }
        BrowserInput::Release { x, y, clicks } => {
            vec![(
                "Input.dispatchMouseEvent",
                mouse("mouseReleased", *x, *y, *clicks),
            )]
        }
        BrowserInput::Move { x, y } => vec![(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseMoved", "x": x, "y": y}),
        )],
        BrowserInput::Wheel { x, y, dx, dy } => vec![(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy}),
        )],
        BrowserInput::Text(text) => vec![("Input.insertText", json!({"text": text}))],
        BrowserInput::Key(key) => ["rawKeyDown", "keyUp"]
            .into_iter()
            .map(|kind| {
                (
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": kind,
                        "key": key.key,
                        "code": key.code,
                        "windowsVirtualKeyCode": key.virtual_code,
                    }),
                )
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    type TestSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

    async fn send(socket: &mut TestSocket, message: Value) {
        socket
            .send(Message::text(message.to_string()))
            .await
            .unwrap();
    }

    async fn command(socket: &mut TestSocket) -> Value {
        let message = socket.next().await.unwrap().unwrap();
        serde_json::from_str(message.to_text().unwrap()).unwrap()
    }

    async fn announce(socket: &mut TestSocket, id: &str) {
        send(
            socket,
            json!({"method": "Target.targetCreated", "params": {"targetInfo": {
                "type": "page", "targetId": id, "url": id, "title": id
            }}}),
        )
        .await;
    }

    async fn discovery_request(listener: &tokio::net::TcpListener) -> tokio::net::TcpStream {
        let (mut http, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut bytes = [0; 1024];
            let count = http.read(&mut bytes).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&bytes[..count]);
        }
        assert!(request.starts_with(b"GET /json/version HTTP/1.1\r\n"));
        http
    }

    async fn accept_browser(listener: &tokio::net::TcpListener) -> TestSocket {
        let address = listener.local_addr().unwrap();
        let mut http = discovery_request(listener).await;
        let body = json!({"webSocketDebuggerUrl": format!("ws://{address}/browser")}).to_string();
        http.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        drop(http);
        let (tcp, _) = listener.accept().await.unwrap();
        tokio_tungstenite::accept_async(tcp).await.unwrap()
    }

    async fn discovery_reply(response: &[u8], close_after_reply: bool) -> anyhow::Result<String> {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let server = async {
                let mut http = discovery_request(&listener).await;
                // Rejection can close the client while an oversized response is still writing.
                if let Err(error) = http.write_all(response).await {
                    tracing::debug!(%error, "The client closed during the discovery reply");
                }
                if !close_after_reply {
                    assert_eq!(http.read(&mut [0; 1]).await.unwrap(), 0);
                }
            };
            let (result, ()) = tokio::join!(browser_socket(&origin), server);
            result
        })
        .await
        .expect("DevTools discovery did not finish")
    }

    #[tokio::test]
    async fn discovery_never_redirects_browser_input_to_a_remote_socket() {
        for socket in [
            "ws://example.com:9222/browser",
            "ws://user@127.0.0.1:9222/browser",
            "wss://127.0.0.1:9222/browser",
        ] {
            let body = json!({"webSocketDebuggerUrl": socket}).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            assert!(
                discovery_reply(response.as_bytes(), true).await.is_err(),
                "{socket}"
            );
        }
    }

    #[tokio::test]
    async fn discovery_rejects_non_loopback_origins_before_connecting() {
        for origin in [
            "http://192.0.2.1:9222",
            "http://localhost:9222",
            "https://127.0.0.1:9222",
            "http://127.0.0.1:9222/path",
        ] {
            assert_eq!(
                browser_socket(origin).await.unwrap_err().to_string(),
                "not a loopback DevTools address"
            );
        }
    }

    #[tokio::test]
    async fn browser_handoff_releases_sessions_and_backpressures_frames() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (input, mut events) = watch(format!("http://{address}"));
            let mut socket = accept_browser(&listener).await;
            assert_eq!(
                command(&mut socket).await["method"],
                "Target.setDiscoverTargets"
            );

            announce(&mut socket, "a").await;
            let first = command(&mut socket).await;
            assert_eq!(first["params"]["targetId"], "a");
            // A newer page must not overwrite the first attach still in flight.
            announce(&mut socket, "b").await;
            send(
                &mut socket,
                json!({"id": first["id"], "result": {"sessionId": "session-a"}}),
            )
            .await;
            assert_eq!(command(&mut socket).await["method"], "Page.startScreencast");
            let second = command(&mut socket).await;
            assert_eq!(second["params"]["targetId"], "b");
            send(
                &mut socket,
                json!({"id": second["id"], "result": {"sessionId": "session-b"}}),
            )
            .await;
            let stop = command(&mut socket).await;
            assert_eq!(stop["method"], "Page.stopScreencast");
            assert_eq!(stop["sessionId"], "session-a");
            let detach = command(&mut socket).await;
            assert_eq!(detach["method"], "Target.detachFromTarget");
            assert_eq!(detach["params"]["sessionId"], "session-a");
            let start = command(&mut socket).await;
            assert_eq!(start["method"], "Page.startScreencast");
            assert_eq!(start["sessionId"], "session-b");

            let mut jpeg = Vec::new();
            image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
                .encode(&[0, 0, 0], 1, 1, image::ExtendedColorType::Rgb8)
                .unwrap();
            let data = base64::engine::general_purpose::STANDARD.encode(jpeg);
            let frame = |id| {
                json!({"method": "Page.screencastFrame", "sessionId": "session-b", "params": {
                    "sessionId": id, "data": data, "metadata": {"deviceWidth": 1, "deviceHeight": 1}
                }})
            };
            // Page events fill the ordered queue. One pending decoded frame blocks further ACKs.
            send(&mut socket, frame(1)).await;
            assert_eq!(command(&mut socket).await["params"]["sessionId"], 1);
            send(&mut socket, frame(2)).await;
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), command(&mut socket))
                    .await
                    .is_err()
            );
            assert!(
                matches!(events.recv().await, Some(BrowserEvent::Page { url, .. }) if url == "a")
            );
            assert_eq!(command(&mut socket).await["params"]["sessionId"], 2);
            assert!(
                matches!(events.recv().await, Some(BrowserEvent::Page { url, .. }) if url == "b")
            );
            for _ in 0..2 {
                assert!(matches!(
                    events.recv().await,
                    Some(BrowserEvent::Frame {
                        width: 1.,
                        height: 1.,
                        ..
                    })
                ));
            }
            input.send(BrowserInput::Text("kept".into())).await.unwrap();
            assert_eq!(command(&mut socket).await["params"]["text"], "kept");

            // Closing the newest target returns to the older page with a fresh session.
            send(
                &mut socket,
                json!({"method": "Target.targetDestroyed", "params": {"targetId": "b"}}),
            )
            .await;
            let reopened = command(&mut socket).await;
            assert_eq!(reopened["params"]["targetId"], "a");
            send(
                &mut socket,
                json!({"id": reopened["id"], "result": {"sessionId": "session-a2"}}),
            )
            .await;
            assert_eq!(command(&mut socket).await["sessionId"], "session-a2");
            assert!(
                matches!(events.recv().await, Some(BrowserEvent::Page { url, .. }) if url == "a")
            );
            drop(input);
            assert!(matches!(events.recv().await, Some(BrowserEvent::Closed(_))));
        })
        .await
        .expect("fake DevTools exchange timed out");
    }
}
