use super::*;

#[tokio::test]
async fn bot_catalog_broadcasts_do_not_reintroduce_a_deleted_bot() {
    let root = tempfile::tempdir().expect("root");
    let (server, grant) = configured_test_server(root.path().join("state")).await;
    let bot = server
        .host
        .create_bot(
            crate::wire::HarnessKind::Mobius,
            "Original",
            "Bot catalog ordering test",
        )
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
            client: crate::wire::ClientCapabilities {
                presence: true,
                remote_desktop_viewer: true,
                ..Default::default()
            },
        },
        ClientMessage::UpdateBot {
            request_id: "rename".into(),
            id: Arc::clone(&bot.id),
            expected_revision: bot.config.revision,
            name: "Renamed".into(),
            description: bot.description,
            tint: bot.tint,
            shape: bot.shape,
            config: bot.config.config.clone(),
        },
        ClientMessage::DeleteBot {
            request_id: "delete".into(),
            id: Arc::clone(&bot.id),
            expected_revision: bot.config.revision + 1,
        },
        ClientMessage::ListBots {
            request_id: "after".into(),
        },
    ] {
        write_frame(&mut writer, &ClientFrame::new(message))
            .await
            .expect("queue request");
    }
    let (client_revocations, _) = broadcast::channel(ConnectionPolicy::default().total());
    let serving = tokio::spawn(serve_connection(
        stream,
        ConnectionContext {
            shared: Arc::new(ConnectionShared {
                auth: server.auth,
                host: server.host,
                bots: server.bots,
                client_connections: Arc::new(ClientConnections::default()),
                client_revocations,
                tls: None,
                websocket_host: None,
            }),
            desktop_transport: false,
            local: true,
            admission: ConnectionAdmission::new(1, 1).admit().await,
            access_lease: None,
        },
        Instant::now()
            + Duration::from_secs(ConnectionPolicy::default().authentication_timeout_seconds),
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
                    if request_id.as_deref() == Some("after") {
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
