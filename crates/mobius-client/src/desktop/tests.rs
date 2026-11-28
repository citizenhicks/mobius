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
            format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
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
    let rejected = tokio_tungstenite::connect_async(websocket(&server, "https://attacker.example"))
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
