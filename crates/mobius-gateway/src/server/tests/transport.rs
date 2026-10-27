use super::*;
use mobius::SessionId;

#[tokio::test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "the flagged value moves into the gateway task; the lint misses that move"
)]
async fn queued_client_requests_do_not_starve_gateway_broadcasts() {
    let root = tempfile::tempdir().unwrap();
    let (server, grant) = configured_test_server(root.path().join("state")).await;
    let identity = server.auth.pair(&grant.code, "busy client").unwrap();
    let (revocations, _) = broadcast::channel(1);
    let connection = ConnectionContext {
        shared: Arc::new(ConnectionShared {
            auth: Arc::clone(&server.auth),
            host: Arc::clone(&server.host),
            bots: Arc::clone(&server.bots),
            client_connections: Arc::new(ClientConnections::default()),
            client_revocations: revocations,
            tls: None,
            websocket_host: None,
        }),
        desktop_transport: false,
        local: true,
        admission: ConnectionAdmission::new(1, 1).admit().await,
        access_lease: None,
    };
    let (client, stream) = tokio::io::duplex(1024 * 1024);
    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = FrameReader::new(reader);
    write_frame(
        &mut writer,
        &ClientFrame::new(ClientMessage::Authenticate {
            token: identity.token,
            client: crate::wire::ClientCapabilities {
                presence: true,
                ..Default::default()
            },
            catalog: Default::default(),
        }),
    )
    .await
    .unwrap();
    let serving = serve_connection(
        stream,
        connection,
        Instant::now()
            + Duration::from_secs(ConnectionPolicy::default().authentication_timeout_seconds),
        None,
    );
    tokio::pin!(serving);
    tokio::select! {
        result = &mut serving => panic!("connection stopped: {result:?}"),
        () = async {
            while !matches!(read_frame::<ServerFrame>(&mut reader).await.unwrap().unwrap().message, ServerMessage::Ready { .. }) {}
        } => {}
    }
    // Pause connection polling so both directions are ready before testing fairness.
    for id in 0..128 {
        write_frame(
            &mut writer,
            &ClientFrame::new(ClientMessage::SetNotifications {
                request_id: id.to_string().into(),
                disabled: BTreeSet::new(),
            }),
        )
        .await
        .unwrap();
    }
    server
        .host
        .create_bot(
            crate::wire::HarnessKind::Mobius,
            "Fairness",
            "Transport test",
        )
        .await
        .unwrap();
    tokio::select! {
        result = &mut serving => panic!("connection stopped: {result:?}"),
        () = async {
            let mut responses = 0;
            loop {
                match read_frame::<ServerFrame>(&mut reader).await.unwrap().unwrap().message {
                    ServerMessage::Bots { .. } => break,
                    ServerMessage::Accepted { .. } => responses += 1,
                    _ => {}
                }
                assert!(responses < 128, "input drained before an already-ready broadcast");
            }
        } => {}
    }
    server.host.shutdown().await;
}

#[tokio::test(start_paused = true)]
#[expect(
    clippy::significant_drop_tightening,
    reason = "the flagged value moves into the gateway task; the lint misses that move"
)]
async fn non_reading_client_releases_its_authenticated_connection_slot() {
    let root = tempfile::tempdir().unwrap();
    let (server, grant) = configured_test_server(root.path().join("state")).await;
    let identity = server.auth.pair(&grant.code, "stalled").unwrap();
    let admission = ConnectionAdmission::new(1, 1);
    let (revocations, _) = broadcast::channel(1);
    let connection = ConnectionContext {
        shared: Arc::new(ConnectionShared {
            auth: Arc::clone(&server.auth),
            host: Arc::clone(&server.host),
            bots: Arc::clone(&server.bots),
            client_connections: Arc::new(ClientConnections::default()),
            client_revocations: revocations,
            tls: None,
            websocket_host: None,
        }),
        desktop_transport: false,
        local: true,
        admission: admission.admit().await,
        access_lease: None,
    };
    let (mut client, stream) = tokio::io::duplex(64);
    let serving = tokio::spawn(serve_connection(
        stream,
        connection,
        Instant::now()
            + Duration::from_secs(ConnectionPolicy::default().authentication_timeout_seconds),
        None,
    ));
    write_frame(
        &mut client,
        &ClientFrame::new(ClientMessage::Authenticate {
            token: identity.token,
            client: crate::wire::ClientCapabilities {
                presence: true,
                ..Default::default()
            },
            catalog: Default::default(),
        }),
    )
    .await
    .unwrap();
    let error = serving
        .await
        .unwrap()
        .expect_err("stalled write closes the connection");
    assert!(matches!(error, Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut));
    assert!(admission.admit().await.promote().is_some());
    server.host.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn access_lease_stops_the_server_and_closes_existing_clients() {
    let root = tempfile::tempdir().expect("temporary directory");
    let (mut server, grant) = GatewayServer::bootstrap(
        root.path().join("state"),
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .await
    .expect("bootstrap gateway");
    let listen = server.listen_addr();
    server.access_lease = Some(AccessLease {
        expires_at: SystemTime::now() + Duration::from_secs(30),
        deadline: Instant::now() + Duration::from_secs(30),
    });
    let serving = tokio::spawn(server.serve_until(std::future::pending()));
    let endpoint = format!("tcp://{listen}")
        .parse::<Endpoint>()
        .expect("endpoint");
    let (client, _) = GatewayClient::pair(
        &endpoint,
        grant.code,
        "lease test",
        crate::wire::ClientCapabilities {
            presence: true,
            remote_desktop_viewer: true,
            ..Default::default()
        },
    )
    .await
    .expect("pair client");
    let (_, mut events) = client.into_parts();
    wait_gateway_ready(&mut events).await;

    tokio::time::advance(Duration::from_secs(30)).await;
    // Completion drops the owned listener; reconnecting can hit another test's reused port.
    serving
        .await
        .expect("gateway task")
        .expect("lease shutdown");
    assert!(events.next().await.expect("client disconnect").is_none());
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
fn websocket_upgrade_waits_for_the_tunnel_hostname() {
    let request = Request::builder()
        .uri("/")
        .header(HOST, "bright-river.trycloudflare.com")
        .header(SEC_WEBSOCKET_PROTOCOL, crate::channel::SUBPROTOCOL)
        .body(())
        .expect("request");
    let hostname = std::sync::OnceLock::new();

    let rejection = WebSocketUpgradePolicy {
        expected_host: Some(&hostname),
    }
    .on_request(&request, Response::new(()))
    .expect_err("an upgrade before the tunnel has a hostname must fail");
    assert_eq!(rejection.status(), StatusCode::FORBIDDEN);

    hostname
        .set("bright-river.trycloudflare.com".to_owned())
        .expect("hostname");
    WebSocketUpgradePolicy {
        expected_host: Some(&hostname),
    }
    .on_request(&request, Response::new(()))
    .expect("the tunnel hostname is accepted once assigned");
}

#[tokio::test]
async fn websocket_preserves_a_pipelined_bulk_frame_across_authentication() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
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
        signal.await.ok();
    }));
    let mut request = format!("ws://{listen}").into_client_request().unwrap();
    request.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        crate::channel::SUBPROTOCOL.parse().unwrap(),
    );
    let (mut websocket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let mut state = crate::channel::client_handshake(&mut websocket, &grant.code)
        .await
        .unwrap();
    let mut pairing = Vec::new();
    write_frame(
        &mut pairing,
        &ClientFrame::new(ClientMessage::Pair {
            code: grant.code,
            client_label: "WebSocket test".into(),
            client: crate::wire::ClientCapabilities {
                presence: true,
                remote_desktop_viewer: true,
                ..Default::default()
            },
        }),
    )
    .await
    .unwrap();
    let request_id = "post-auth-list";
    let mut post_auth = Vec::new();
    write_frame(
        &mut post_auth,
        &ClientFrame::new(ClientMessage::UploadSessionFileChunk {
            request_id: request_id.into(),
            session_id: SessionId::new("missing-session").unwrap(),
            upload_id: "missing-upload".into(),
            offset: 0,
            data: vec![0; MAX_PRE_AUTH_FRAME_BYTES],
        }),
    )
    .await
    .unwrap();
    assert!(post_auth.len() > MAX_PRE_AUTH_FRAME_BYTES);
    let mut pipelined = Vec::new();
    for plaintext in [pairing, post_auth] {
        let mut encrypted = vec![0; plaintext.len() + crate::channel::TAG_BYTES];
        let length = state.write_message(&plaintext, &mut encrypted).unwrap();
        append_masked_binary_frame(&mut pipelined, &encrypted[..length]);
    }
    websocket
        .get_mut()
        .write_all(&pipelined)
        .await
        .expect("pipeline encrypted pairing and post-auth records");
    let (transport, stream) = tokio::io::duplex(16 * 1024);
    let bridge = tokio::spawn(crate::channel::bridge(websocket, state, stream));
    let mut reader = FrameReader::new(transport);
    let paired = read_frame::<ServerFrame>(&mut reader)
        .await
        .unwrap()
        .unwrap();
    let authenticated = read_frame::<ServerFrame>(&mut reader)
        .await
        .unwrap()
        .unwrap();
    let ready = read_frame::<ServerFrame>(&mut reader)
        .await
        .unwrap()
        .unwrap();
    let rejection = read_frame::<ServerFrame>(&mut reader)
        .await
        .unwrap()
        .unwrap();

    assert!(matches!(
        (
            paired.message,
            authenticated.message,
            ready.message,
            rejection.message
        ),
        (
            ServerMessage::Paired { .. },
            ServerMessage::Authenticated,
            ServerMessage::Ready { .. },
            ServerMessage::Rejected { request_id: actual, .. }
        ) if actual.as_ref() == request_id
    ));
    bridge.abort();
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
    let (client_revocations, _) = broadcast::channel(ConnectionPolicy::default().total());
    let admission = ConnectionAdmission::new(1, 1).admit().await;
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept connection");
        let auth_deadline = Instant::now()
            + Duration::from_secs(ConnectionPolicy::default().authentication_timeout_seconds);
        accepted_tx.send(()).expect("report accepted connection");
        serve_plaintext_connection(
            stream,
            ConnectionContext {
                shared: Arc::new(ConnectionShared {
                    auth,
                    host,
                    bots,
                    client_connections,
                    client_revocations,
                    tls: None,
                    websocket_host: None,
                }),
                desktop_transport: false,
                local: true,
                admission,
                access_lease: None,
            },
            PlaintextHandshake {
                verify_websocket_host: false,
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
                "ET / HTTP/1.1\r\nHost: {listen}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: mobius-noise-v1\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
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
        None | Some(Ok(Message::Close(_)) | Err(_))
    ));
    assert!(matches!(
        serving.await.expect("gateway task"),
        Err(Error::Unauthorized)
    ));
}

#[test]
fn only_presence_connections_count_and_announce_the_client() {
    let root = tempfile::tempdir().expect("temporary directory");
    let bots = Arc::new(BotStore::open(root.path()).expect("Bots"));
    bots.create_bot(
        "fixture",
        "Fixture",
        crate::wire::AgentComposition::default().into(),
    )
    .expect("Bot");
    let connections = Arc::new(ClientConnections::default());
    let identity = |id: &str| crate::auth::ClientIdentity {
        id: id.into(),
        label: id.into(),
    };

    let tool = connections
        .register("tool".into(), false, Arc::clone(&bots))
        .expect("register tool");
    assert_eq!(connections.presence_count().expect("count"), 0);
    assert!(bots.unpublished_events(10).expect("events").is_empty());

    let app = connections
        .register("app".into(), true, Arc::clone(&bots))
        .expect("register app");
    assert_eq!(connections.presence_count().expect("count"), 1);
    let clients = connections
        .snapshot(vec![identity("tool"), identity("app")])
        .expect("snapshot");
    assert_eq!(clients[0].connections, 0);
    assert_eq!(clients[1].connections, 1);
    let events = bots.unpublished_events(10).expect("events");
    assert!(matches!(
        &events[..],
        [crate::wire::HookEvent { data: crate::wire::HookData::ClientConnected { client_id }, .. }]
            if &**client_id == "app"
    ));
    drop((tool, app));
}
