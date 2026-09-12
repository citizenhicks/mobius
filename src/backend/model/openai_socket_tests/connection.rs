use super::super::*;
use super::support::model_request;
use tokio_tungstenite::connect_async;

#[tokio::test]
async fn idle_connection_pump_answers_ping() {
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
        socket
            .send(Message::Ping(vec![1, 2, 3].into()))
            .await
            .expect("ping");
        loop {
            match socket
                .next()
                .await
                .expect("pong frame")
                .expect("valid frame")
            {
                Message::Pong(payload) => break payload,
                Message::Ping(_) | Message::Text(_) | Message::Binary(_) | Message::Frame(_) => {}
                Message::Close(_) => panic!("connection closed before pong"),
            }
        }
    });
    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client connection");
    let connection = OpenAiWsConnection::new(socket);

    let pong = timeout(Duration::from_secs(1), server)
        .await
        .expect("idle pong timed out")
        .expect("WebSocket server");
    connection.close().await;

    assert_eq!(pong.as_ref(), [1, 2, 3]);
}

#[tokio::test]
async fn active_connection_pump_forwards_bursts_without_blocking_ping() {
    use futures_util::SinkExt as _;
    use futures_util::StreamExt as _;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("WebSocket listener");
    let address = listener.local_addr().expect("WebSocket address");
    let message_count = 96;
    let (pong_sender, pong_received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("WebSocket connection");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("WebSocket handshake");
        socket
            .next()
            .await
            .expect("response request")
            .expect("valid response request");
        for index in 0..message_count {
            socket
                .send(Message::text(index.to_string()))
                .await
                .expect("stream message");
        }
        socket
            .send(Message::Ping(vec![1, 2, 3].into()))
            .await
            .expect("active ping");
        loop {
            match socket
                .next()
                .await
                .expect("pong frame")
                .expect("valid pong frame")
            {
                Message::Pong(payload) => {
                    pong_sender.send(payload).expect("report pong");
                    break;
                }
                Message::Ping(_) | Message::Text(_) | Message::Binary(_) | Message::Frame(_) => {}
                Message::Close(_) => panic!("connection closed before pong"),
            }
        }
        let _ = socket.next().await;
    });
    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client connection");
    let mut connection = OpenAiWsConnection::new(socket);
    connection
        .start(Message::text("request"))
        .await
        .expect("start exchange");
    tokio::time::sleep(Duration::from_millis(25)).await;
    let pong = timeout(Duration::from_secs(1), pong_received)
        .await
        .expect("active pong timed out")
        .expect("pong sender");

    for index in 0..message_count {
        let event = timeout(Duration::from_secs(1), connection.messages.recv())
            .await
            .expect("stream message timed out")
            .expect("stream remained open");
        let SocketEvent::Message(Message::Text(text)) = event else {
            panic!("unexpected socket event");
        };
        assert_eq!(text.as_str(), index.to_string());
    }
    assert!(!connection.closed.load(Ordering::Acquire));
    assert_eq!(pong.as_ref(), [1, 2, 3]);

    connection.finish();
    connection.close().await;
    server.await.expect("WebSocket server");
}

#[tokio::test]
async fn idle_connection_remains_reusable() {
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
        for response in ["first response", "second response"] {
            socket
                .next()
                .await
                .expect("response request")
                .expect("valid response request");
            socket
                .send(Message::text(response))
                .await
                .expect("response message");
        }
    });
    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client connection");
    let mut connection = OpenAiWsConnection::new(socket);
    connection
        .start(Message::text("first request"))
        .await
        .expect("start first exchange");
    let first = timeout(Duration::from_secs(1), connection.messages.recv())
        .await
        .expect("first response timed out")
        .expect("first response event");
    assert!(matches!(first, SocketEvent::Message(Message::Text(_))));
    connection.finish();

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(connection.is_usable());
    connection
        .start(Message::text("second request"))
        .await
        .expect("start second exchange");
    let second = timeout(Duration::from_secs(1), connection.messages.recv())
        .await
        .expect("second response timed out")
        .expect("second response event");

    assert!(matches!(second, SocketEvent::Message(Message::Text(_))));
    connection.finish();
    connection.close().await;
    server.await.expect("WebSocket server");
}

#[tokio::test]
async fn connection_limit_closes_other_idle_connections() {
    use futures_util::SinkExt as _;
    use futures_util::StreamExt as _;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("WebSocket listener");
    let address = listener.local_addr().expect("WebSocket address");
    let server = tokio::spawn(async move {
        let (idle_stream, _) = listener.accept().await.expect("idle connection");
        let mut idle_socket = tokio_tungstenite::accept_async(idle_stream)
            .await
            .expect("idle WebSocket handshake");

        let (limited_stream, _) = listener.accept().await.expect("limited connection");
        let mut limited_socket = tokio_tungstenite::accept_async(limited_stream)
            .await
            .expect("limited WebSocket handshake");
        limited_socket
            .next()
            .await
            .expect("response request")
            .expect("valid response request");
        limited_socket
            .send(Message::text(
                serde_json::json!({
                    "type": "error",
                    "error": {
                        "code": "websocket_connection_limit_reached",
                        "message": "connection limit reached"
                    }
                })
                .to_string(),
            ))
            .await
            .expect("connection-limit event");

        match timeout(Duration::from_secs(1), idle_socket.next())
            .await
            .expect("idle connection close")
        {
            Some(Ok(Message::Close(_))) | None => {}
            Some(Ok(message)) => panic!("idle socket received {message:?}"),
            Some(Err(_)) => {}
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
    let (idle_socket, _) = connect_async(&socket_url)
        .await
        .expect("idle client connection");
    let idle_session = provider
        .session("idle-session")
        .await
        .expect("idle session");
    idle_session.lock().await.connection = Some(OpenAiWsConnection::new(idle_socket));
    drop(idle_session);
    let events: ModelEventSink = Arc::new(|_| Box::pin(async { Ok(()) }));

    let Error::Provider(error) = provider
        .send_response(model_request(), events)
        .await
        .expect_err("connection limit should interrupt the attempt")
    else {
        panic!("expected provider error");
    };
    server.await.expect("WebSocket server");

    assert!(error.is_stream_interrupted());
    let sessions = provider.sessions.lock().await;
    let idle = Arc::clone(sessions.get("idle-session").expect("idle session retained"));
    drop(sessions);
    assert!(idle.lock().await.connection.is_none());
}
