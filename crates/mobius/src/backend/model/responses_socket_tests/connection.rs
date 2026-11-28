use super::super::*;
use super::support::model_request;
use super::support::*;
use tokio_tungstenite::connect_async;

#[tokio::test]
async fn cancelled_request_flushes_reason_and_reconnects() {
    use crate::backend::model::{ModelCancellation, ModelCancellationReason};
    use futures_util::StreamExt as _;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    for reason in [
        ModelCancellationReason::Interrupted,
        ModelCancellationReason::FrontendDisconnected,
        ModelCancellationReason::RequestDropped,
    ] {
        let (listener, address) = loopback_listener().await;
        let (started, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut socket = accept_websocket(&listener).await;
            let request = read_request(&mut socket).await;
            assert_eq!(request["type"], "response.create");
            started.send(()).unwrap();
            let frame = socket.next().await.unwrap().unwrap();
            let Message::Close(Some(frame)) = frame else {
                panic!("expected clean close, received {frame:?}");
            };
            assert_eq!(frame.code, CloseCode::Normal);
            assert_eq!(frame.reason.as_str(), reason.as_str());

            let mut socket = accept_websocket(&listener).await;
            let request = read_request(&mut socket).await;
            assert!(request.get("previous_response_id").is_none());
            write_completed(&mut socket, "recovered", "response-2").await;
        });
        let provider = socket_provider(address);
        let session_id = (reason != ModelCancellationReason::RequestDropped)
            .then_some(model_request().session_id)
            .flatten();
        let cancellation = ModelCancellation::default();
        let events: ModelEventSink = Arc::new(|_| Box::pin(async { Ok(()) }));
        let mut request = Box::pin(provider.respond(
            ModelRequest {
                cancellation: Some(&cancellation),
                session_id,
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
            provider.respond(
                ModelRequest {
                    session_id,
                    ..model_request()
                },
                events,
            ),
        )
        .await
        .expect("fresh response timeout")
        .expect("fresh response");
        assert_eq!(output.text(), "recovered");
        assert_eq!(
            provider.sessions.lock().await.len(),
            usize::from(session_id.is_some())
        );
        timeout(Duration::from_secs(2), server)
            .await
            .expect("close flush timeout")
            .expect("server task");
    }
}

#[tokio::test]
async fn active_connection_pump_forwards_bursts_without_blocking_ping() {
    use futures_util::SinkExt as _;
    use futures_util::StreamExt as _;

    let (listener, address) = loopback_listener().await;
    let message_count = 2048;
    let (pong_sender, pong_received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = accept_websocket(&listener).await;
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
        drop(socket.next().await);
    });
    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client connection");
    let mut connection =
        OpenAiWsConnection::with_settings(socket, &ModelTransportSettings::default());
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
async fn connection_limit_closes_other_idle_connections() {
    use futures_util::SinkExt as _;
    use futures_util::StreamExt as _;

    let (listener, address) = loopback_listener().await;
    let server = tokio::spawn(async move {
        let mut idle_socket = accept_websocket(&listener).await;
        let mut limited_socket = accept_websocket(&listener).await;
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
            Some(Ok(Message::Close(_)) | Err(_)) | None => {}
            Some(Ok(message)) => panic!("idle socket received {message:?}"),
        }
    });
    let provider = socket_provider(address);
    let (idle_socket, _) = connect_async(format!("ws://{address}/responses"))
        .await
        .expect("idle client connection");
    let idle_session = provider
        .session("idle-session")
        .await
        .expect("idle session");
    idle_session.lock().await.connection = Some(OpenAiWsConnection::with_settings(
        idle_socket,
        &ModelTransportSettings::default(),
    ));
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
