use super::*;

#[cfg(target_os = "linux")]
#[tokio::test]
async fn encrypted_observers_receive_current_hold_and_release_without_a_desktop_stream() {
    let root = tempfile::tempdir().unwrap();
    let (mut server, grant) = configured_test_server(root.path().join("state")).await;
    server.host.remote_desktop =
        Arc::new(crate::computer_runtime::remote_desktop::RemoteDesktop::new(
            root.path(),
            true,
            crate::computer_runtime::ComputerConfig::default(),
        ));
    let remote = Arc::clone(&server.host.remote_desktop);
    remote.begin_takeover().unwrap();
    let identity = server.auth.pair(&grant.code, "encrypted observer").unwrap();
    let (revocations, _) = broadcast::channel(1);
    let context = ConnectionContext {
        desktop_transport: true,
        local: false,
        auth: Arc::clone(&server.auth),
        host: server.host.clone(),
        bots: Arc::clone(&server.bots),
        client_connections: Arc::new(ClientConnections::default()),
        client_revocations: revocations,
        admission: ConnectionAdmission::new(1, 1).admit().await,
        access_lease: None,
    };
    let (client, stream) = tokio::io::duplex(1024 * 1024);
    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = FrameReader::new(reader);
    let serving = tokio::spawn(serve_connection(
        stream,
        context,
        Instant::now()
            + Duration::from_secs(ConnectionPolicy::default().authentication_timeout_seconds),
        None,
    ));
    write_frame(
        &mut writer,
        &ClientFrame::new(ClientMessage::Authenticate {
            token: identity.token,
            client_kind: ClientKind::Ios,
            catalog: Default::default(),
        }),
    )
    .await
    .unwrap();
    for expected in 0..3 {
        let message = tokio::time::timeout(
            Duration::from_secs(5),
            read_frame::<ServerFrame>(&mut reader),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .message;
        assert!(matches!(
            (expected, message),
            (0, ServerMessage::Authenticated)
                | (1, ServerMessage::Ready { .. })
                | (
                    2,
                    ServerMessage::DesktopControlState {
                        request_id: None,
                        enabled: true,
                        is_owner: false,
                        ..
                    }
                )
        ));
    }
    remote.cancel_takeover().await;
    let message = tokio::time::timeout(
        Duration::from_secs(5),
        read_frame::<ServerFrame>(&mut reader),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap()
    .message;
    assert!(matches!(
        message,
        ServerMessage::DesktopControlState {
            request_id: None,
            enabled: false,
            is_owner: false,
            ..
        }
    ));
    drop(reader);
    drop(writer);
    serving.await.unwrap().unwrap();
    server.host.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn authenticated_viewers_connect_during_desktop_execution_hold() {
    let root = tempfile::tempdir().unwrap();
    let (mut server, grant) = configured_test_server(root.path().join("state")).await;
    server.host.remote_desktop =
        Arc::new(crate::computer_runtime::remote_desktop::RemoteDesktop::new(
            root.path(),
            true,
            crate::computer_runtime::ComputerConfig::default(),
        ));
    let remote = Arc::clone(&server.host.remote_desktop);
    let host = server.host.clone();
    let identity = server.auth.pair(&grant.code, "desktop viewer").unwrap();
    remote.begin_takeover().unwrap();
    assert!(host.begin_mutation().await.is_err());
    let endpoint: Endpoint = format!("tcp://{}", server.listen_addr()).parse().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async {
        let _ = stopped.await;
    }));

    let viewer = tokio::time::timeout(
        Duration::from_secs(5),
        GatewayClient::connect(&endpoint, &identity.token, ClientKind::Macos),
    )
    .await
    .expect("viewer authentication must remain available during takeover")
    .unwrap();
    let (sender, mut events) = viewer.into_parts();
    wait_gateway_ready(&mut events).await;
    sender
        .send(ClientMessage::CreateBot {
            request_id: "blocked-write".into(),
            name: "Blocked".into(),
            description: "Execution is held".into(),
        })
        .await
        .unwrap();
    loop {
        match next_gateway_message(&mut events).await {
            ServerMessage::Rejected {
                request_id,
                message,
                ..
            } if request_id == "blocked-write" => {
                assert!(message.contains("execution is held"));
                break;
            }
            ServerMessage::Bots {
                request_id: Some(request_id),
                ..
            } if request_id == "blocked-write" => panic!("configuration write bypassed takeover"),
            _ => {}
        }
    }
    assert!(remote.check_execution().is_err());
    remote.cancel_takeover().await;
    assert!(host.begin_mutation().await.is_ok());
    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn open_computer_pumps_the_same_local_renderer_connection() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let (mut server, grant) = configured_test_server(root.path().join("state")).await;
    server.host.remote_desktop =
        Arc::new(crate::computer_runtime::remote_desktop::RemoteDesktop::new(
            root.path(),
            true,
            crate::computer_runtime::ComputerConfig::default(),
        ));
    let bot = server
        .host
        .create_bot("Computer", "Same renderer connection")
        .await
        .unwrap();
    let session = server
        .host
        .create_session(&workspace, &bot.id)
        .await
        .unwrap();
    let session_id = session.session_id().to_owned();
    let identity = server.auth.pair(&grant.code, "Mac renderer").unwrap();
    let (revocations, _) = broadcast::channel(1);
    let context = ConnectionContext {
        desktop_transport: false,
        local: true,
        auth: Arc::clone(&server.auth),
        host: server.host.clone(),
        bots: Arc::clone(&server.bots),
        client_connections: Arc::new(ClientConnections::default()),
        client_revocations: revocations,
        admission: ConnectionAdmission::new(1, 1).admit().await,
        access_lease: None,
    };
    let (client, stream) = tokio::io::duplex(1024 * 1024);
    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = FrameReader::new(reader);
    let serving = tokio::spawn(serve_connection(
        stream,
        context,
        Instant::now()
            + Duration::from_secs(ConnectionPolicy::default().authentication_timeout_seconds),
        None,
    ));
    write_frame(
        &mut writer,
        &ClientFrame::new(ClientMessage::Authenticate {
            token: identity.token,
            client_kind: ClientKind::Macos,
            catalog: Default::default(),
        }),
    )
    .await
    .unwrap();
    while !matches!(
        read_frame::<ServerFrame>(&mut reader)
            .await
            .unwrap()
            .unwrap()
            .message,
        ServerMessage::Ready { .. }
    ) {}
    write_frame(
        &mut writer,
        &ClientFrame::new(ClientMessage::SetBrowserRuntime {
            request_id: "renderer".into(),
            enabled: true,
        }),
    )
    .await
    .unwrap();
    while !matches!(
        read_frame::<ServerFrame>(&mut reader)
            .await
            .unwrap()
            .unwrap()
            .message,
        ServerMessage::Accepted { .. }
    ) {}
    write_frame(
        &mut writer,
        &ClientFrame::new(ClientMessage::OpenComputer {
            request_id: "show".into(),
            session_id: session_id.clone(),
        }),
    )
    .await
    .unwrap();
    let page_request = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match read_frame::<ServerFrame>(&mut reader)
                .await
                .unwrap()
                .unwrap()
                .message
            {
                ServerMessage::BrowserPageRequested {
                    request_id,
                    session_id: assigned,
                    foreground,
                } => {
                    assert_eq!(assigned, session_id);
                    assert!(foreground);
                    break request_id;
                }
                ServerMessage::Rejected { message, .. } => panic!("{message}"),
                _ => {}
            }
        }
    })
    .await
    .expect("page request must not block behind its own connection");
    write_frame(
        &mut writer,
        &ClientFrame::new(ClientMessage::BrowserPageReply {
            request_id: page_request,
            endpoint: Some("ws+unix:///tmp/browser.sock:/0123456789abcdef0123456789abcdef".into()),
        }),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match read_frame::<ServerFrame>(&mut reader)
                .await
                .unwrap()
                .unwrap()
                .message
            {
                ServerMessage::ComputerOpened {
                    request_id,
                    session_id: assigned,
                } => {
                    assert_eq!(request_id, "show");
                    assert_eq!(assigned, session_id);
                    break;
                }
                ServerMessage::Rejected { message, .. } => panic!("{message}"),
                _ => {}
            }
        }
    })
    .await
    .expect("computer opened reply");
    drop(reader);
    drop(writer);
    serving.await.unwrap().unwrap();
    server.host.shutdown().await;
}

#[tokio::test]
async fn computer_view_follows_local_and_encrypted_connection_authority() {
    use crate::wire::ComputerView;
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let ready = server.host.ready().await.unwrap();
    for (available, local, encrypted, expected) in [
        (
            ComputerView::EmbeddedBrowser,
            true,
            false,
            ComputerView::EmbeddedBrowser,
        ),
        (
            ComputerView::EmbeddedBrowser,
            false,
            true,
            ComputerView::Unavailable,
        ),
        (
            ComputerView::RemoteDesktop,
            true,
            false,
            ComputerView::Unavailable,
        ),
        (
            ComputerView::RemoteDesktop,
            false,
            true,
            ComputerView::RemoteDesktop,
        ),
    ] {
        let mut view = ClientView::new(CatalogHint::default());
        view.local = local;
        view.desktop_transport = encrypted;
        let mut payload = ready.clone();
        payload.computer_view = available;
        let mut bytes = Vec::new();
        view.write_catalog(
            &mut bytes,
            ServerFrame::new(ServerMessage::Ready { payload }),
        )
        .await
        .unwrap();
        let frame = read_frame::<ServerFrame>(&mut FrameReader::new(bytes.as_slice()))
            .await
            .unwrap()
            .unwrap();
        let ServerMessage::Ready { payload } = frame.message else {
            panic!("ready");
        };
        assert_eq!(payload.computer_view, expected);
    }
    server.host.shutdown().await;
}
