use super::*;

async fn next_websocket_frame(websocket: &mut WebSocketStream<TcpStream>) -> ServerFrame {
    let Message::Binary(payload) = websocket
        .next()
        .await
        .expect("gateway response")
        .expect("read gateway response")
    else {
        panic!("gateway response must be binary");
    };
    serde_json::from_slice(&payload).expect("decode gateway frame")
}

fn append_masked_binary_frame(output: &mut Vec<u8>, payload: &[u8]) {
    output.push(0x82);
    if payload.len() <= 125 {
        output.push(0x80 | u8::try_from(payload.len()).expect("small WebSocket payload"));
    } else {
        output.push(0x80 | 126);
        output.extend_from_slice(
            &u16::try_from(payload.len())
                .expect("WebSocket test payload fits u16")
                .to_be_bytes(),
        );
    }
    let mask = [0x12, 0x34, 0x56, 0x78];
    output.extend_from_slice(&mask);
    output.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % mask.len()]),
    );
}

#[tokio::test]
async fn connection_admission_wakes_waiters_and_bounds_authenticated_clients() {
    let admission = ConnectionAdmission::new(1, 1);
    let first = admission.admit().await;
    let waiting = admission.admit();
    tokio::pin!(waiting);
    tokio::select! {
        biased;
        _ = &mut waiting => panic!("second pre-auth connection bypassed the bound"),
        () = std::future::ready(()) => {}
    }
    let authenticated = first.promote().expect("promote first connection");
    let second = waiting.await;
    assert!(second.promote().is_none());
    drop(authenticated);
    assert!(admission.admit().await.promote().is_some());
}

#[tokio::test]
async fn pre_auth_reader_rejects_an_oversized_frame_from_its_prefix() {
    let (mut writer, reader) = tokio::io::duplex(4);
    let mut reader = FrameReader::new(reader);
    let oversized = u32::try_from(MAX_PRE_AUTH_FRAME_BYTES + 1).expect("frame limit fits u32");
    writer
        .write_all(&oversized.to_be_bytes())
        .await
        .expect("write prefix");

    let error = read_frame_with_limit::<PreAuthClientFrame>(&mut reader, MAX_PRE_AUTH_FRAME_BYTES)
        .await
        .expect_err("oversized pre-auth frame must fail");

    assert!(matches!(error, Error::Protocol(_)), "{error}");
}

#[test]
fn client_inventory_aggregates_connections_and_keeps_inactive_devices() {
    let clients = Arc::new(ClientConnections::default());
    let identity = ClientIdentity {
        id: "client-a".into(),
        label: "Mac".into(),
    };
    let paired = [identity.clone()];
    let first = clients
        .register(identity.id.clone(), ClientKind::Macos)
        .expect("first connection");
    let _dashboard = clients
        .register(identity.id.clone(), ClientKind::GatewayDashboard)
        .expect("dashboard connection");
    let second = clients
        .register(identity.id, ClientKind::Macos)
        .expect("second connection");

    let two = clients.snapshot(&paired).expect("two connections")[0].connections;
    drop(first);
    let one = clients.snapshot(&paired).expect("one connection")[0].connections;
    drop(second);
    let inactive = clients.snapshot(&paired).expect("inactive client")[0].clone();

    assert_eq!(
        (two, one, inactive.connections, inactive.kinds),
        (2, 1, 0, Vec::new())
    );
}

#[test]
fn websocket_upgrade_rejects_non_root_targets() {
    let request = Request::builder().uri("/other").body(()).expect("request");

    let rejection = WebSocketUpgradePolicy {
        expected_host: None,
    }
    .on_request(&request, Response::new(()))
    .expect_err("non-root path must fail");

    assert_eq!(rejection.status(), StatusCode::NOT_FOUND);
}

#[test]
fn websocket_upgrade_rejects_browser_origins() {
    let request = Request::builder()
        .uri("/")
        .header(ORIGIN, "https://attacker.example")
        .body(())
        .expect("request");

    let rejection = WebSocketUpgradePolicy {
        expected_host: None,
    }
    .on_request(&request, Response::new(()))
    .expect_err("Origin header must fail");

    assert_eq!(rejection.status(), StatusCode::FORBIDDEN);
}

#[test]
fn websocket_upgrade_rejects_the_wrong_cloudflare_host() {
    let request = Request::builder()
        .uri("/")
        .header(HOST, "other.example")
        .body(())
        .expect("request");

    let rejection = WebSocketUpgradePolicy {
        expected_host: Some("gateway.example".into()),
    }
    .on_request(&request, Response::new(()))
    .expect_err("wrong Host must fail");

    assert_eq!(rejection.status(), StatusCode::FORBIDDEN);
}

#[test]
fn websocket_upgrade_accepts_the_cloudflare_host_with_standard_port() {
    let request = Request::builder()
        .uri("/")
        .header(HOST, "gateway.example:443")
        .body(())
        .expect("request");

    let accepted = WebSocketUpgradePolicy {
        expected_host: Some("gateway.example".into()),
    }
    .on_request(&request, Response::new(()));

    assert!(accepted.is_ok());
}

#[tokio::test]
async fn websocket_preserves_a_pipelined_bulk_frame_across_authentication() {
    let root = tempfile::tempdir().expect("temporary directory");
    let (server, grant) = GatewayServer::bootstrap(
        root.path().join("state"),
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .await
    .expect("bootstrap gateway");
    let listen = server.listen_addr();
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    let mut stream = TcpStream::connect(listen).await.expect("connect gateway");
    let mut pipelined = format!(
        "GET / HTTP/1.1\r\nHost: {listen}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    )
    .into_bytes();
    let pairing = serde_json::to_vec(&ClientFrame::new(ClientMessage::Pair {
        code: grant.code,
        client_label: "WebSocket test".into(),
        client_kind: ClientKind::Ios,
    }))
    .expect("encode pair frame");
    append_masked_binary_frame(&mut pipelined, &pairing);
    let request_id = "x".repeat(MAX_PRE_AUTH_FRAME_BYTES + 1);
    let post_auth = serde_json::to_vec(&ClientFrame::new(ClientMessage::ListSessions {
        request_id: request_id.clone(),
    }))
    .expect("encode post-auth frame");
    assert!(post_auth.len() > MAX_PRE_AUTH_FRAME_BYTES);
    append_masked_binary_frame(&mut pipelined, &post_auth);
    stream
        .write_all(&pipelined)
        .await
        .expect("pipeline upgrade, pairing, and post-auth frames");
    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\n") {
        let mut byte = [0_u8; 1];
        let read = stream.read(&mut byte).await.expect("read upgrade response");
        assert_eq!(read, 1, "upgrade response ended early");
        response.push(byte[0]);
    }
    assert!(response.starts_with(b"HTTP/1.1 101"));
    let mut websocket = WebSocketStream::from_raw_socket(stream, Role::Client, None).await;
    let paired = next_websocket_frame(&mut websocket).await;
    let authenticated = next_websocket_frame(&mut websocket).await;
    let ready = next_websocket_frame(&mut websocket).await;
    let sessions = next_websocket_frame(&mut websocket).await;

    assert!(matches!(
        (paired.message, authenticated.message, ready.message, sessions.message),
        (
            ServerMessage::Paired { .. },
            ServerMessage::Authenticated,
            ServerMessage::Ready { .. },
            ServerMessage::Sessions {
                request_id: Some(actual),
                ..
            }
        ) if actual == request_id
    ));
    drop(websocket);
    shutdown.send(()).expect("stop gateway");
    serving.await.expect("gateway task").expect("gateway stop");
}

#[tokio::test(start_paused = true)]
async fn websocket_upgrade_and_authentication_share_one_deadline() {
    let root = tempfile::tempdir().expect("temporary directory");
    let (server, _) = GatewayServer::bootstrap(
        root.path().join("state"),
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .await
    .expect("bootstrap gateway");
    let listen = server.listen_addr();
    let GatewayServer {
        listener,
        auth,
        host,
        bots,
        ..
    } = server;
    let client_connections = Arc::new(ClientConnections::default());
    let (client_revocations, _) = broadcast::channel(MAX_CONNECTIONS);
    let admission = ConnectionAdmission::new(1, 1).admit().await;
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept connection");
        let auth_deadline = Instant::now() + PRE_AUTH_TIMEOUT;
        accepted_tx.send(()).expect("report accepted connection");
        serve_plaintext_connection(
            stream,
            ConnectionContext {
                auth,
                host,
                bots,
                client_connections,
                client_revocations,
                admission,
            },
            PlaintextHandshake {
                expected_websocket_host: None,
                auth_deadline,
            },
        )
        .await
    });
    let mut stream = TcpStream::connect(listen).await.expect("connect gateway");
    accepted_rx.await.expect("connection accepted");

    tokio::time::advance(Duration::from_secs(2)).await;
    stream.write_all(b"G").await.expect("start upgrade");
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    stream
        .write_all(
            format!(
                "ET / HTTP/1.1\r\nHost: {listen}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("finish upgrade");
    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\n") {
        let mut byte = [0_u8; 1];
        let read = stream.read(&mut byte).await.expect("read upgrade response");
        assert_eq!(read, 1, "upgrade response ended early");
        response.push(byte[0]);
    }
    assert!(response.starts_with(b"HTTP/1.1 101"));
    let mut websocket = WebSocketStream::from_raw_socket(stream, Role::Client, None).await;

    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::resume();
    let closed = tokio::time::timeout(Duration::from_secs(1), websocket.next())
        .await
        .expect("authentication deadline must close the socket");

    assert!(matches!(
        closed,
        None | Some(Ok(Message::Close(_))) | Some(Err(_))
    ));
    assert!(matches!(
        serving.await.expect("gateway task"),
        Err(Error::Unauthorized)
    ));
}

#[tokio::test]
async fn bootstrap_owns_the_listener_before_creating_state() {
    let root = tempfile::tempdir().expect("temporary directory");
    let occupied = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("occupied listener");
    let listen = occupied.local_addr().expect("listen address");
    let state = root.path().join("state");

    let result = GatewayServer::bootstrap(state.clone(), listen).await;

    assert!(matches!(result, Err(Error::Io(_))));
    assert!(!state.exists());
}

#[tokio::test]
async fn connected_client_pauses_and_resets_inactivity_shutdown() {
    let root = tempfile::tempdir().expect("temporary directory");
    let (server, grant) = GatewayServer::bootstrap(
        root.path().join("state"),
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .await
    .expect("bootstrap gateway");
    let listen = server.config.listen;
    let serving = tokio::spawn(
        server.serve_until_inactive(std::future::pending(), Duration::from_millis(200)),
    );
    let endpoint = format!("tcp://{listen}")
        .parse::<Endpoint>()
        .expect("endpoint");
    let (connection, _) =
        GatewayClient::pair(&endpoint, grant.code, "inactivity test", ClientKind::Cli)
            .await
            .expect("connect client");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!serving.is_finished());
    drop(connection);
    tokio::time::sleep(Duration::from_millis(75)).await;
    assert!(!serving.is_finished());

    tokio::time::timeout(Duration::from_secs(2), serving)
        .await
        .expect("inactivity shutdown timeout")
        .expect("gateway task")
        .expect("gateway shutdown");
}
