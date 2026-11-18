use super::*;
use mobius::SessionId;

async fn capacity_server(
    state_dir: PathBuf,
    policy: ConnectionPolicy,
) -> (GatewayServer, PairingGrant) {
    configured_test_server_with(state_dir, |config| config.connections = policy).await
}

async fn expect_capacity_accepted(events: &mut GatewayEvents, expected: &str) {
    loop {
        match next_gateway_message(events).await {
            ServerMessage::Accepted { request_id } if request_id.as_ref() == expected => return,
            ServerMessage::Rejected {
                request_id,
                message,
                ..
            } if request_id.as_ref() == expected => {
                panic!("capacity operation rejected: {message}")
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn configured_authenticated_limit_rejects_excess_clients_and_keeps_the_first_alive() {
    let root = tempfile::tempdir().expect("state directory");
    let (server, grant) = capacity_server(
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
    let gateway = RunningGateway::start(server).await;
    let first = GatewayClient::connect(&endpoint, &identity.token, PRESENT_CLIENT)
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
            client: PRESENT_CLIENT,
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
    gateway.shutdown().await;
}

#[tokio::test]
async fn configured_pre_authentication_limit_waits_then_releases_after_disconnect() {
    let root = tempfile::tempdir().expect("state directory");
    let (server, grant) = capacity_server(
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
    let gateway = RunningGateway::start(server).await;
    let stalled = TcpStream::connect(listen)
        .await
        .expect("stalled connection");
    let waiting = GatewayClient::connect(&endpoint, &identity.token, PRESENT_CLIENT);
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
    gateway.shutdown().await;
}

async fn begin_capacity_upload(
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    session_id: &SessionId,
    request_id: &str,
) -> String {
    sender
        .send(ClientMessage::BeginSessionFileUpload {
            request_id: request_id.into(),
            session_id: session_id.share(),
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
            } if *actual == *request_id => return upload_id,
            ServerMessage::Rejected {
                request_id: actual,
                message,
                ..
            } if actual.as_ref() == request_id => panic!("upload rejected: {message}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn configured_pending_upload_limit_is_released_by_cancellation() {
    let root = tempfile::tempdir().expect("state directory");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let (server, grant) = capacity_server(
        root.path().join("state"),
        ConnectionPolicy {
            pending_uploads: 1,
            ..Default::default()
        },
    )
    .await;
    let (sender, mut events, gateway) = paired_gateway(server, grant, REMOTE_VIEWER).await;
    let mut composition = crate::wire::AgentComposition::default();
    composition.middleware.set_enabled("attachments", true);
    let (session_id, _) =
        create_bot_chat_with_config(&sender, &mut events, &workspace, composition).await;
    let upload_id = begin_capacity_upload(&sender, &mut events, &session_id, "first-upload").await;
    sender
        .send(ClientMessage::BeginSessionFileUpload {
            request_id: "excess-upload".into(),
            session_id: session_id.share(),
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
            } if request_id.as_ref() == "excess-upload" => {
                assert_eq!(code, "session_file_rejected");
                assert!(message.contains("more than 1 pending uploads"));
                assert!(!fatal);
                break;
            }
            ServerMessage::SessionFileUploadReady { request_id, .. }
                if &*request_id == "excess-upload" =>
            {
                panic!("configured pending-upload limit bypassed")
            }
            _ => {}
        }
    }
    sender
        .send(ClientMessage::DeleteSessions {
            request_id: "cancel-first-upload".into(),
            session_ids: vec![session_id.share()],
            selection: mobius::backend::session_files::SessionFileSelection::Ids(vec![upload_id]),
        })
        .await
        .expect("cancel pending upload");
    expect_capacity_accepted(&mut events, "cancel-first-upload").await;
    begin_capacity_upload(&sender, &mut events, &session_id, "replacement-upload").await;
    gateway.shutdown().await;
}
