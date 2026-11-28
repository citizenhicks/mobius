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
        assert!(matches!(events.recv().await, Some(BrowserEvent::Page { url, .. }) if url == "a"));
        assert_eq!(command(&mut socket).await["params"]["sessionId"], 2);
        assert!(matches!(events.recv().await, Some(BrowserEvent::Page { url, .. }) if url == "b"));
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
        assert!(matches!(events.recv().await, Some(BrowserEvent::Page { url, .. }) if url == "a"));
        drop(input);
        assert!(matches!(events.recv().await, Some(BrowserEvent::Closed(_))));
    })
    .await
    .expect("fake DevTools exchange timed out");
}
