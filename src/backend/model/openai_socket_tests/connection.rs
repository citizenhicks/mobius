use super::super::*;
use super::support::model_request;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Error as WebSocketError;

#[test]
fn websocket_error_cause_keeps_only_the_io_category() {
    let error = WebSocketError::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset));

    assert_eq!(websocket_error_cause(&error), "I/O:ConnectionReset");
}

#[tokio::test]
async fn cancelled_request_flushes_reason_and_reconnects() {
    use crate::backend::model::{ModelCancellation, ModelCancellationReason};
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    for reason in [
        ModelCancellationReason::Interrupted,
        ModelCancellationReason::FrontendDisconnected,
        ModelCancellationReason::RequestDropped,
    ] {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("WebSocket listener");
        let address = listener.local_addr().expect("WebSocket address");
        let (started, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("first connection");
            let mut socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("first handshake");
            let request = socket.next().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
            assert_eq!(request["type"], "response.create");
            started.send(()).unwrap();
            let frame = socket.next().await.unwrap().unwrap();
            let Message::Close(Some(frame)) = frame else {
                panic!("expected clean close, received {frame:?}");
            };
            assert_eq!(frame.code, CloseCode::Normal);
            assert_eq!(frame.reason.as_str(), reason.as_str());

            let (stream, _) = listener.accept().await.expect("fresh connection");
            let mut socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("fresh handshake");
            let request = socket.next().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
            assert!(request.get("previous_response_id").is_none());
            for event in super::support::completed_events("recovered", "response-2") {
                socket.send(Message::text(event.to_string())).await.unwrap();
            }
        });
        let provider = OpenAiSocket::with_authorization(
            Arc::new(ApiKeyAuthorization::new("test-key".into())),
            &format!("http://{address}"),
            format!("ws://{address}/responses"),
            "test-model",
            reqwest::Client::new(),
            crate::backend::model::ModelTransportSettings::default(),
        )
        .unwrap();
        let cancellation = ModelCancellation::default();
        let events: ModelEventSink = Arc::new(|_| Box::pin(async { Ok(()) }));
        let mut request = Box::pin(provider.respond(
            ModelRequest {
                cancellation: Some(&cancellation),
                ..model_request()
            },
            Arc::clone(&events),
        ));
        tokio::select! {
            result = &mut request => panic!("request ended before cancellation: {result:?}"),
            result = received => result.unwrap(),
        }
        if reason != ModelCancellationReason::RequestDropped {
            cancellation.record(reason);
        }
        drop(request);
        let output = timeout(
            Duration::from_secs(2),
            provider.respond(model_request(), events),
        )
        .await
        .expect("fresh response timeout")
        .expect("fresh response");
        assert_eq!(output.text(), "recovered");
        timeout(Duration::from_secs(2), server)
            .await
            .expect("close flush timeout")
            .expect("server task");
    }
}

#[tokio::test]
async fn failed_exchange_flushes_request_failed_reason() {
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket.next().await.unwrap().unwrap();
        socket.send(Message::text("invalid JSON")).await.unwrap();
        let frame = socket.next().await.unwrap().unwrap();
        let Message::Close(Some(frame)) = frame else {
            panic!("expected clean close, received {frame:?}");
        };
        assert_eq!(frame.code, CloseCode::Normal);
        assert_eq!(frame.reason.as_str(), "request_failed");
    });
    let (socket, _) = connect_async(format!("ws://{address}")).await.unwrap();
    let mut connection = OpenAiWsConnection::new(socket);
    let events: ModelEventSink = Arc::new(|_| Box::pin(async { Ok(()) }));
    let result = timeout(
        Duration::from_secs(2),
        exchange(&mut connection, &serde_json::json!({}), &events, None),
    )
    .await
    .expect("exchange timeout");
    assert!(matches!(result, Err(crate::Error::Json(_))));
    drop(connection);
    timeout(Duration::from_secs(2), server)
        .await
        .expect("close flush timeout")
        .expect("server task");
}

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
    let message_count = 2048;
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
        crate::backend::model::ModelTransportSettings::default(),
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
        .respond(model_request(), events)
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

#[tokio::test]
async fn pump_still_limits_total_buffered_events() {
    use super::super::connection::MAX_STREAM_EVENTS;
    use futures_util::{SinkExt as _, StreamExt as _};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket.next().await.unwrap().unwrap();
        for _ in 0..=MAX_STREAM_EVENTS {
            socket.send(Message::text("x")).await.unwrap();
        }
        let Message::Close(Some(frame)) = socket.next().await.unwrap().unwrap() else {
            panic!("expected a close after the response cap");
        };
        assert_eq!(frame.reason.as_str(), "request_failed");
    });
    let (socket, _) = connect_async(format!("ws://{address}")).await.unwrap();
    let mut connection = OpenAiWsConnection::new(socket);
    connection.start(Message::text("request")).await.unwrap();
    timeout(Duration::from_secs(10), async {
        // Leave the consumer paused until the pump has enforced its total response cap.
        while !connection.closed.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        for _ in 0..MAX_STREAM_EVENTS {
            assert!(matches!(
                connection.messages.recv().await,
                Some(SocketEvent::Message(_))
            ));
        }
        assert!(matches!(
            connection.messages.recv().await,
            Some(SocketEvent::ProtocolError(
                "WebSocket response exceeded size limit"
            ))
        ));
    })
    .await
    .expect("response cap enforced");
    server.await.unwrap();
}
