use super::*;

async fn capacity_server(
    state_dir: PathBuf,
    policy: ConnectionPolicy,
) -> (GatewayServer, PairingGrant) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let listen = listener.local_addr().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(state_dir, listen, None).expect("initialize gateway");
    let mut config = config
        .registering_provider(
            crate::wire::AgentComposition::default().provider,
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .expect("register provider");
    config.connections = policy;
    store.save(&config).expect("save capacity policy");
    let (_, grant) =
        AuthStore::initialize(store.auth_path(), config.auth).expect("initialize auth");
    let server = GatewayServer::assemble(store, config, listener)
        .await
        .expect("assemble gateway with capacity policy");
    (server, grant)
}

async fn expect_capacity_accepted(events: &mut GatewayEvents, expected: &str) {
    loop {
        match next_gateway_message(events).await {
            ServerMessage::Accepted { request_id } if request_id == expected => return,
            ServerMessage::Rejected {
                request_id,
                message,
                ..
            } if request_id == expected => {
                panic!("capacity operation rejected: {message}")
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn configured_authenticated_limit_rejects_excess_clients_and_keeps_the_first_alive() {
    let root = tempfile::tempdir().expect("state directory");
    let (mut server, grant) = capacity_server(
        root.path().join("state"),
        ConnectionPolicy {
            authenticated: 1,
            ..Default::default()
        },
    )
    .await;
    let identity = server
        .auth
        .pair(&grant.code, "capacity client")
        .expect("pair client");
    let listen = server.listen_addr();
    let endpoint: Endpoint = format!("tcp://{listen}").parse().expect("endpoint");
    let ready = server.notify_ready();
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async {
        let _ = stopped.await;
    }));
    ready.await.expect("listener ready");
    let first = GatewayClient::connect(&endpoint, &identity.token, ClientKind::Cli)
        .await
        .expect("first connection");
    let (sender, mut events) = first.into_parts();
    wait_gateway_ready(&mut events).await;

    let stream = TcpStream::connect(listen).await.expect("second connection");
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = FrameReader::new(reader);
    write_frame(
        &mut writer,
        &ClientFrame::new(ClientMessage::Authenticate {
            token: identity.token,
            client_kind: ClientKind::Cli,
            catalog: Default::default(),
        }),
    )
    .await
    .expect("second authentication");
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        read_frame::<ServerFrame>(&mut reader),
    )
    .await
    .expect("capacity response deadline")
    .expect("capacity response")
    .expect("error frame");
    assert!(matches!(response.message,
        ServerMessage::Error { code, message, fatal: true }
            if code == "server_busy" && message.contains("authenticated connection limit")
    ));
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            read_frame::<ServerFrame>(&mut reader)
        )
        .await
        .expect("rejected connection closes")
        .expect("closed connection")
        .is_none()
    );
    sender
        .send(ClientMessage::SetNotifications {
            request_id: "first-still-alive".into(),
            disabled: BTreeSet::new(),
        })
        .await
        .expect("first client remains usable");
    expect_capacity_accepted(&mut events, "first-still-alive").await;
    shutdown.send(()).expect("shutdown");
    serving
        .await
        .expect("server task")
        .expect("server shutdown");
}

#[tokio::test]
async fn configured_pre_authentication_limit_waits_then_releases_after_disconnect() {
    let root = tempfile::tempdir().expect("state directory");
    let (mut server, grant) = capacity_server(
        root.path().join("state"),
        ConnectionPolicy {
            pre_authentication: 1,
            ..Default::default()
        },
    )
    .await;
    let identity = server
        .auth
        .pair(&grant.code, "waiting client")
        .expect("pair client");
    let listen = server.listen_addr();
    let endpoint: Endpoint = format!("tcp://{listen}").parse().expect("endpoint");
    let ready = server.notify_ready();
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async {
        let _ = stopped.await;
    }));
    ready.await.expect("listener ready");
    let stalled = TcpStream::connect(listen)
        .await
        .expect("stalled connection");
    let waiting = GatewayClient::connect(&endpoint, &identity.token, ClientKind::Cli);
    tokio::pin!(waiting);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut waiting)
            .await
            .is_err(),
        "a second authentication cannot bypass the configured pre-authentication capacity"
    );
    drop(stalled);
    let client = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("released capacity deadline")
        .expect("waiting client admitted");
    let (_sender, mut events) = client.into_parts();
    wait_gateway_ready(&mut events).await;
    shutdown.send(()).expect("shutdown");
    serving
        .await
        .expect("server task")
        .expect("server shutdown");
}

async fn begin_capacity_upload(
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    session_id: &str,
    request_id: &str,
) -> String {
    sender
        .send(ClientMessage::BeginSessionFileUpload {
            request_id: request_id.into(),
            session_id: session_id.into(),
            name: format!("{request_id}.txt"),
            size: 1,
            media_type: "text/plain".into(),
        })
        .await
        .expect("begin upload");
    loop {
        match next_gateway_message(events).await {
            ServerMessage::SessionFileUploadReady {
                request_id: actual,
                upload_id,
                ..
            } if actual == request_id => return upload_id,
            ServerMessage::Rejected {
                request_id: actual,
                message,
                ..
            } if actual == request_id => panic!("upload rejected: {message}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn configured_pending_upload_limit_is_released_by_cancellation() {
    let root = tempfile::tempdir().expect("state directory");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let (mut server, grant) = capacity_server(
        root.path().join("state"),
        ConnectionPolicy {
            pending_uploads: 1,
            ..Default::default()
        },
    )
    .await;
    let endpoint: Endpoint = format!("tcp://{}", server.listen_addr())
        .parse()
        .expect("endpoint");
    let ready = server.notify_ready();
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async {
        let _ = stopped.await;
    }));
    ready.await.expect("listener ready");
    let (client, _) =
        GatewayClient::pair(&endpoint, grant.code, "upload capacity", ClientKind::Ios)
            .await
            .expect("pair client");
    let (sender, mut events) = client.into_parts();
    wait_gateway_ready(&mut events).await;
    let mut composition = crate::wire::AgentComposition::default();
    composition.middleware.set_enabled("attachments", true);
    let (session_id, _) =
        create_bot_chat_with_config(&sender, &mut events, &workspace, composition).await;
    let upload_id = begin_capacity_upload(&sender, &mut events, &session_id, "first-upload").await;
    sender
        .send(ClientMessage::BeginSessionFileUpload {
            request_id: "excess-upload".into(),
            session_id: session_id.as_str().into(),
            name: "second.txt".into(),
            size: 1,
            media_type: "text/plain".into(),
        })
        .await
        .expect("request excess upload");
    loop {
        match next_gateway_message(&mut events).await {
            ServerMessage::Rejected {
                request_id,
                code,
                message,
                fatal,
            } if request_id == "excess-upload" => {
                assert_eq!(code, "session_file_rejected");
                assert!(message.contains("more than 1 pending uploads"));
                assert!(!fatal);
                break;
            }
            ServerMessage::SessionFileUploadReady { request_id, .. }
                if request_id == "excess-upload" =>
            {
                panic!("configured pending-upload limit bypassed")
            }
            _ => {}
        }
    }
    sender
        .send(ClientMessage::DeleteSessions {
            request_id: "cancel-first-upload".into(),
            session_ids: vec![session_id.as_str().into()],
            selection: mobius::backend::session_files::SessionFileSelection::Ids(vec![upload_id]),
        })
        .await
        .expect("cancel pending upload");
    expect_capacity_accepted(&mut events, "cancel-first-upload").await;
    begin_capacity_upload(&sender, &mut events, &session_id, "replacement-upload").await;
    shutdown.send(()).expect("shutdown");
    serving
        .await
        .expect("server task")
        .expect("server shutdown");
}
