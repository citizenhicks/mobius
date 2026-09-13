use super::*;

#[test]
fn tls_loader_rejects_empty_pem_files() {
    let file = tempfile::NamedTempFile::new().expect("temporary PEM");

    let error = load_certificates(file.path()).expect_err("empty PEM must fail");

    assert!(error.to_string().contains("certificate file is empty"));
    let error = load_private_key(file.path()).expect_err("empty key must fail");
    assert!(error.to_string().contains("private-key file is empty"));
}

#[test]
fn tls_loaders_select_certificates_and_the_first_private_key() {
    let file = tempfile::NamedTempFile::new().expect("temporary PEM");
    for label in ["PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY"] {
        fs::write(
            file.path(),
            format!(
                "-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n\
                 -----BEGIN {label}-----\nBAUG\n-----END {label}-----\n\
                 -----BEGIN CERTIFICATE-----\nBwgJ\n-----END CERTIFICATE-----\n\
                 -----BEGIN PRIVATE KEY-----\nCgsM\n-----END PRIVATE KEY-----\n"
            ),
        )
        .expect("write PEM");
        let certificates = load_certificates(file.path()).expect("certificate chain");
        assert_eq!(certificates.len(), 2);
        assert_eq!(certificates[0].as_ref(), [1, 2, 3]);
        assert_eq!(certificates[1].as_ref(), [7, 8, 9]);
        assert_eq!(
            load_private_key(file.path())
                .expect("first key")
                .secret_der(),
            [4, 5, 6]
        );
    }
}

#[test]
fn tls_loaders_reject_malformed_pem() {
    let file = tempfile::NamedTempFile::new().expect("temporary PEM");
    fs::write(
        file.path(),
        "-----BEGIN CERTIFICATE-----\n!\n-----END CERTIFICATE-----\n",
    )
    .expect("write malformed PEM");
    for error in [
        load_certificates(file.path()).expect_err("invalid certificate"),
        load_private_key(file.path()).expect_err("invalid section before key"),
    ] {
        assert!(
            matches!(error, Error::Io(error) if error.kind() == std::io::ErrorKind::InvalidData)
        );
    }
}

#[tokio::test]
async fn paired_client_creates_workspace_directory_and_receives_its_listing() {
    let root = tempfile::tempdir().expect("root");
    let parent = root.path().join("projects");
    fs::create_dir(&parent).expect("parent");
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
    let (connection, _) =
        GatewayClient::pair(&endpoint, grant.code, "directory test", ClientKind::Ios)
            .await
            .expect("connect client");
    let (sender, mut events) = connection.into_parts();
    wait_gateway_ready(&mut events).await;

    let request_id = "create-directory".to_string();
    sender
        .send(ClientMessage::CreateWorkspaceDirectory {
            request_id: request_id.clone(),
            parent: parent.clone(),
            name: "new-project".into(),
        })
        .await
        .expect("create workspace directory");
    loop {
        match next_gateway_message(&mut events).await {
            ServerMessage::Directories {
                request_id: actual,
                listing,
            } if actual == request_id => {
                assert_eq!(
                    listing.path,
                    fs::canonicalize(parent.join("new-project")).expect("created path")
                );
                assert!(listing.entries.is_empty());
                break;
            }
            ServerMessage::Rejected {
                request_id: actual,
                code,
                message,
                ..
            } if actual == request_id => {
                panic!("directory creation rejected ({code}): {message}");
            }
            _ => {}
        }
    }
    assert!(parent.join("new-project").is_dir());

    shutdown.send(()).expect("stop gateway");
    serving.await.expect("gateway task").expect("gateway stop");
}

#[test]
fn directory_listing_is_sorted_and_excludes_files() {
    let root = tempfile::tempdir().expect("temporary directory");
    fs::create_dir(root.path().join("zeta")).expect("create directory");
    fs::create_dir(root.path().join("Alpha")).expect("create directory");
    fs::write(root.path().join("notes.txt"), b"not a folder").expect("create file");

    let listing = list_directories(root.path(), false).expect("list directories");

    assert_eq!(
        listing
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["Alpha", "zeta"]
    );
}

#[test]
fn directory_listing_can_include_files() {
    let root = tempfile::tempdir().expect("temporary directory");
    fs::create_dir(root.path().join("tasks")).expect("create directory");
    fs::create_dir(root.path().join(".git")).expect("create Git metadata");
    fs::write(root.path().join("daily.md"), b"task").expect("create file");

    let listing = list_directories(root.path(), true).expect("list directory entries");

    assert_eq!(listing.entries.len(), 2);
    assert!(
        listing
            .entries
            .iter()
            .any(|entry| { entry.name == "tasks" && entry.is_directory })
    );
    assert!(
        listing
            .entries
            .iter()
            .any(|entry| { entry.name == "daily.md" && !entry.is_directory })
    );
}

#[test]
fn history_frame_bound_rejects_one_oversized_turn() {
    let frame = ServerFrame::new(ServerMessage::SessionHistory {
        request_id: "history".into(),
        session_id: "session".into(),
        records: vec![crate::wire::RecordedEvent {
            sequence: 1,
            recorded_at_ms: 1,
            event: Event {
                submission_id: None,
                msg: EventMsg::Warning(mobius::protocol::WarningEvent {
                    message: "x".repeat(MAX_FRAME_BYTES),
                }),
            },
            stream_metrics: Vec::new(),
            blocks: Vec::new(),
            preview: None,
        }],
        next_before_sequence: None,
    });

    assert!(!encoded_frame_fits(&frame).expect("measure history frame"));
}

#[tokio::test]
async fn rejection_frames_preserve_request_correlation() {
    let (mut writer, reader) = tokio::io::duplex(1024);
    let mut reader = FrameReader::new(reader);
    write_rejection(
        &mut writer,
        "request-7".into(),
        Rejection {
            code: "agent_busy",
            message: "busy".into(),
            fatal: false,
        },
    )
    .await
    .expect("write rejection");

    let frame = read_frame::<ServerFrame>(&mut reader)
        .await
        .expect("read rejection")
        .expect("frame");

    assert!(matches!(
        frame.message,
        ServerMessage::Rejected { request_id, .. } if request_id == "request-7"
    ));
}
