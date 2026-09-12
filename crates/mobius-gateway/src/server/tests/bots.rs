use super::*;

#[tokio::test]
async fn chat_creation_requires_an_existing_bot_after_workspace_selection() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
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
    let endpoint = format!("tcp://{listen}")
        .parse::<Endpoint>()
        .expect("endpoint");
    let (connection, _) = GatewayClient::pair(&endpoint, grant.code, "Bot test", ClientKind::Ios)
        .await
        .expect("connect");
    let (sender, mut events) = connection.into_parts();
    wait_gateway_ready(&mut events).await;

    sender
        .send(ClientMessage::CreateSession {
            request_id: "create".into(),
            workspace,
            bot_ids: vec![Uuid::new_v4().to_string()],
        })
        .await
        .expect("send create");
    loop {
        if let ServerMessage::Rejected {
            request_id, code, ..
        } = next_gateway_message(&mut events).await
            && request_id == "create"
        {
            assert_eq!(code, "invalid_bot");
            break;
        }
    }

    shutdown.send(()).expect("stop gateway");
    serving.await.expect("gateway task").expect("gateway stop");
}

#[tokio::test]
async fn deleting_a_bot_clears_its_selected_chat_on_the_requesting_connection() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let (server, grant) = configured_test_server(root.path().join("state")).await;
    let listen = server.listen_addr();
    let bots = Arc::clone(&server.bots);
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    let endpoint = format!("tcp://{listen}")
        .parse::<Endpoint>()
        .expect("endpoint");
    let (connection, _) =
        GatewayClient::pair(&endpoint, grant.code, "Bot deletion test", ClientKind::Ios)
            .await
            .expect("connect");
    let (sender, mut events) = connection.into_parts();
    wait_gateway_ready(&mut events).await;
    let (session_id, bot_id) = create_bot_chat(&sender, &mut events, &workspace).await;
    let bot = bots.bot(&bot_id).expect("created Bot");

    sender
        .send(ClientMessage::DeleteBot {
            request_id: "delete-bot".into(),
            id: bot.id,
            expected_revision: bot.config.revision,
        })
        .await
        .expect("delete Bot");
    loop {
        match next_gateway_message(&mut events).await {
            ServerMessage::Bots {
                request_id: Some(request_id),
                ..
            } if request_id == "delete-bot" => break,
            ServerMessage::Rejected {
                request_id,
                code,
                message,
                ..
            } if request_id == "delete-bot" => {
                panic!("Bot deletion rejected ({code}): {message}")
            }
            _ => {}
        }
    }

    sender
        .send(ClientMessage::GetSessionHistory {
            request_id: "deleted-history".into(),
            session_id,
            before_sequence: None,
        })
        .await
        .expect("request deleted history");
    loop {
        if let ServerMessage::Rejected {
            request_id, code, ..
        } = next_gateway_message(&mut events).await
            && request_id == "deleted-history"
        {
            assert_eq!(code, "session_required");
            break;
        }
    }

    shutdown.send(()).expect("stop gateway");
    serving.await.expect("gateway task").expect("gateway stop");
}

#[tokio::test]
async fn bot_catalog_broadcasts_do_not_reintroduce_a_deleted_bot() {
    let root = tempfile::tempdir().expect("root");
    let (server, grant) = configured_test_server(root.path().join("state")).await;
    let bot = server
        .host
        .create_bot("Original", "Bot catalog ordering test")
        .await
        .expect("create Bot");
    let (client, stream) = tokio::io::duplex(1024 * 1024);
    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = FrameReader::new(reader);
    // Queue both mutations before polling the connection to keep the rename broadcast pending.
    for message in [
        ClientMessage::Pair {
            code: grant.code,
            client_label: "Bot catalog test".into(),
            client_kind: ClientKind::Ios,
        },
        ClientMessage::UpdateBot {
            request_id: "rename".into(),
            id: bot.id.clone(),
            expected_revision: bot.config.revision,
            name: "Renamed".into(),
            description: bot.description,
            tint: bot.tint,
            config: bot.config.config,
        },
        ClientMessage::DeleteBot {
            request_id: "delete".into(),
            id: bot.id.clone(),
            expected_revision: bot.config.revision + 1,
        },
    ] {
        write_frame(&mut writer, &ClientFrame::new(message))
            .await
            .expect("queue request");
    }
    let (client_revocations, _) = broadcast::channel(MAX_CONNECTIONS);
    let serving = tokio::spawn(serve_connection(
        stream,
        ConnectionContext {
            local: true,
            auth: server.auth,
            host: server.host,
            bots: server.bots,
            client_connections: Arc::new(ClientConnections::default()),
            client_revocations,
            admission: ConnectionAdmission::new(1, 1).admit().await,
        },
        Instant::now() + PRE_AUTH_TIMEOUT,
        None,
    ));
    let mut deleted = false;
    loop {
        let frame = tokio::time::timeout(
            Duration::from_secs(5),
            read_frame::<ServerFrame>(&mut reader),
        )
        .await
        .expect("response timeout")
        .expect("read response")
        .expect("connection open");
        match frame.message {
            ServerMessage::Bots { request_id, bots } => {
                deleted |= request_id.as_deref() == Some("delete");
                if deleted {
                    assert!(
                        bots.iter().all(|candidate| candidate.id != bot.id),
                        "a stale catalog reintroduced the deleted Bot after its deletion response"
                    );
                    if request_id.is_none() {
                        break;
                    }
                }
            }
            ServerMessage::Rejected { code, message, .. } => {
                panic!("Bot mutation rejected ({code}): {message}")
            }
            _ => {}
        }
    }
    drop(reader);
    drop(writer);
    serving
        .await
        .expect("connection task")
        .expect("connection stop");
}
