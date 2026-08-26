use super::*;

#[tokio::test]
async fn paired_client_uploads_lists_reads_and_submits_a_session_file() {
    let root = tempfile::tempdir().expect("temporary directory");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let listen = listener.local_addr().expect("listen address");
    let (store, config) = ConfigStore::initialize(root.path().join("state"), listen, None)
        .expect("initialize gateway");
    let config = config
        .registering_provider(
            crate::wire::AgentComposition::default().provider,
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("register provider");
    store.save(&config).expect("save provider");
    let (_, grant) = AuthStore::initialize(store.auth_path()).expect("initialize auth");
    let server = GatewayServer::assemble(store, config, listener)
        .await
        .expect("assemble gateway");
    let listen = server.config.listen;
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    let endpoint = format!("tcp://{listen}")
        .parse::<Endpoint>()
        .expect("endpoint");
    let (connection, _) =
        GatewayClient::pair(&endpoint, grant.code, "attachment test", ClientKind::Ios)
            .await
            .expect("pair frontend");
    let (sender, mut events) = connection.into_parts();
    wait_gateway_ready(&mut events).await;
    let session_id = create_chat(&sender, &mut events, &workspace).await;

    let mut config = crate::wire::AgentComposition::default();
    config.middleware.set_enabled("attachments", true);
    sender
        .send(ClientMessage::ConfigureSession {
            request_id: "configure-attachments".into(),
            session_id: session_id.clone(),
            expected_revision: 1,
            config,
        })
        .await
        .expect("enable attachments");
    loop {
        match next_gateway_message(&mut events).await {
            ServerMessage::Accepted { request_id } if request_id == "configure-attachments" => {
                break;
            }
            ServerMessage::Rejected {
                request_id,
                code,
                message,
                ..
            } if request_id == "configure-attachments" => {
                panic!("attachment configuration rejected ({code}): {message}")
            }
            _ => {}
        }
    }

    let missing = SessionFileReference {
        id: Uuid::new_v4().to_string(),
        name: "missing.txt".into(),
        size: 1,
        media_type: "text/plain".into(),
    };
    sender
        .send(ClientMessage::Submit {
            session_id: session_id.clone(),
            submission: Submission {
                id: "invalid-duplicate-attachments".into(),
                op: Op::UserInput {
                    text: "invalid".into(),
                    attachments: vec![missing.clone(), missing],
                },
            },
        })
        .await
        .expect("submit invalid attachment references");
    loop {
        if let ServerMessage::Rejected {
            request_id,
            code,
            message,
            ..
        } = next_gateway_message(&mut events).await
            && request_id == "invalid-duplicate-attachments"
        {
            assert_eq!(code, "invalid_submission");
            assert!(message.contains("unique"));
            break;
        }
    }

    let other_session_id = create_chat(&sender, &mut events, &workspace).await;
    for finish in [false, true] {
        open_chat(&sender, &mut events, &session_id).await;
        let begin_id = format!("begin-unselected-{finish}");
        sender
            .send(ClientMessage::BeginSessionFileUpload {
                request_id: begin_id.clone(),
                session_id: session_id.clone(),
                name: format!("unselected-{finish}.bin"),
                size: 1,
                media_type: "application/octet-stream".into(),
            })
            .await
            .expect("begin upload before switching chat");
        let upload_id = loop {
            if let ServerMessage::SessionFileUploadReady {
                request_id,
                upload_id,
                ..
            } = next_gateway_message(&mut events).await
                && request_id == begin_id
            {
                break upload_id;
            }
        };

        open_chat(&sender, &mut events, &other_session_id).await;
        let rejection_id = format!("reject-unselected-{finish}");
        let request = if finish {
            ClientMessage::FinishSessionFileUpload {
                request_id: rejection_id.clone(),
                session_id: session_id.clone(),
                upload_id: upload_id.clone(),
            }
        } else {
            ClientMessage::UploadSessionFileChunk {
                request_id: rejection_id.clone(),
                session_id: session_id.clone(),
                upload_id: upload_id.clone(),
                offset: 0,
                data: vec![0],
            }
        };
        sender
            .send(request)
            .await
            .expect("reject upload for unselected chat");
        loop {
            if let ServerMessage::Rejected {
                request_id, code, ..
            } = next_gateway_message(&mut events).await
                && request_id == rejection_id
            {
                assert_eq!(code, "session_not_selected");
                break;
            }
        }

        open_chat(&sender, &mut events, &session_id).await;
        let retry_id = format!("retry-terminated-{finish}");
        sender
            .send(ClientMessage::FinishSessionFileUpload {
                request_id: retry_id.clone(),
                session_id: session_id.clone(),
                upload_id,
            })
            .await
            .expect("retry terminated upload");
        loop {
            if let ServerMessage::Rejected {
                request_id,
                message,
                ..
            } = next_gateway_message(&mut events).await
                && request_id == retry_id
            {
                assert!(message.contains("not active"));
                break;
            }
        }
    }

    sender
        .send(ClientMessage::BeginSessionFileUpload {
            request_id: "begin-doomed-upload".into(),
            session_id: session_id.clone(),
            name: "doomed.bin".into(),
            size: 1,
            media_type: "application/octet-stream".into(),
        })
        .await
        .expect("begin doomed upload");
    let doomed_upload_id = loop {
        if let ServerMessage::SessionFileUploadReady {
            request_id,
            upload_id,
            ..
        } = next_gateway_message(&mut events).await
            && request_id == "begin-doomed-upload"
        {
            break upload_id;
        }
    };
    sender
        .send(ClientMessage::UploadSessionFileChunk {
            request_id: "reject-doomed-chunk".into(),
            session_id: session_id.clone(),
            upload_id: doomed_upload_id.clone(),
            offset: 1,
            data: vec![0],
        })
        .await
        .expect("append invalid chunk");
    loop {
        if matches!(
            next_gateway_message(&mut events).await,
            ServerMessage::Rejected { request_id, .. }
                if request_id == "reject-doomed-chunk"
        ) {
            break;
        }
    }
    sender
        .send(ClientMessage::FinishSessionFileUpload {
            request_id: "finish-doomed-upload".into(),
            session_id: session_id.clone(),
            upload_id: doomed_upload_id,
        })
        .await
        .expect("finish terminated upload");
    loop {
        if let ServerMessage::Rejected {
            request_id,
            message,
            ..
        } = next_gateway_message(&mut events).await
            && request_id == "finish-doomed-upload"
        {
            assert!(message.contains("not active"));
            break;
        }
    }

    let image = b"\x89PNG\r\n\x1a\npayload";
    sender
        .send(ClientMessage::BeginSessionFileUpload {
            request_id: "begin-upload".into(),
            session_id: session_id.clone(),
            name: "image.png".into(),
            size: image.len() as u64,
            media_type: "image/png".into(),
        })
        .await
        .expect("begin upload");
    let upload_id = loop {
        if let ServerMessage::SessionFileUploadReady {
            request_id,
            upload_id,
            ..
        } = next_gateway_message(&mut events).await
            && request_id == "begin-upload"
        {
            break upload_id;
        }
    };

    for (request_id, offset, data) in [
        ("upload-chunk-1", 0_u64, image[..8].to_vec()),
        ("upload-chunk-2", 8_u64, image[8..].to_vec()),
    ] {
        sender
            .send(ClientMessage::UploadSessionFileChunk {
                request_id: request_id.into(),
                session_id: session_id.clone(),
                upload_id: upload_id.clone(),
                offset,
                data,
            })
            .await
            .expect("append upload chunk");
        loop {
            if matches!(
                next_gateway_message(&mut events).await,
                ServerMessage::SessionFileUploadChunkAccepted { request_id: actual, .. }
                    if actual == request_id
            ) {
                break;
            }
        }
    }

    sender
        .send(ClientMessage::FinishSessionFileUpload {
            request_id: "finish-upload".into(),
            session_id: session_id.clone(),
            upload_id,
        })
        .await
        .expect("finish upload");
    let file = loop {
        if let ServerMessage::SessionFileUploadCompleted {
            request_id, file, ..
        } = next_gateway_message(&mut events).await
            && request_id == "finish-upload"
        {
            break file;
        }
    };

    sender
        .send(ClientMessage::ListSessionFiles {
            request_id: "list-session-files".into(),
            session_id: session_id.clone(),
        })
        .await
        .expect("list session files");
    loop {
        if let ServerMessage::SessionFiles {
            request_id, files, ..
        } = next_gateway_message(&mut events).await
            && request_id == "list-session-files"
        {
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].origin, mobius::protocol::SessionFileOrigin::User);
            assert_eq!(files[0].file, file);
            break;
        }
    }

    sender
        .send(ClientMessage::ReadSessionFile {
            request_id: "read-session-file".into(),
            session_id: session_id.clone(),
            file_id: file.id.clone(),
            offset: 0,
            max_bytes: image.len(),
        })
        .await
        .expect("read session file");
    loop {
        if let ServerMessage::SessionFileChunk {
            request_id,
            data,
            next_offset,
            ..
        } = next_gateway_message(&mut events).await
            && request_id == "read-session-file"
        {
            assert_eq!(data, image);
            assert_eq!(next_offset, None);
            break;
        }
    }

    let submission_id = "submit-attachment".to_string();
    sender
        .send(ClientMessage::Submit {
            session_id: session_id.clone(),
            submission: Submission {
                id: submission_id.clone(),
                op: Op::UserInput {
                    text: "describe the image".into(),
                    attachments: vec![file.clone()],
                },
            },
        })
        .await
        .expect("submit attachment");
    let mut saw_user_message = false;
    let model_error = loop {
        let ServerMessage::AgentEvent {
            session_id: actual_session,
            record,
            ..
        } = next_gateway_message(&mut events).await
        else {
            continue;
        };
        if actual_session != session_id
            || record.event.submission_id.as_deref() != Some(&submission_id)
        {
            continue;
        }
        match record.event.msg {
            EventMsg::UserMessage(message) => {
                assert_eq!(message.attachments, std::slice::from_ref(&file));
                saw_user_message = true;
            }
            EventMsg::Error(error) => break error.message,
            _ => {}
        }
    };

    assert!(saw_user_message);
    assert!(
        model_error.contains("selected provider is not configured"),
        "valid image must reach the model after attachment middleware: {model_error}"
    );
    shutdown.send(()).expect("stop gateway");
    serving.await.expect("gateway task").expect("gateway stop");
}

#[tokio::test]
async fn unpairing_disconnects_the_client_and_rejects_its_token() {
    let root = tempfile::tempdir().expect("temporary directory");
    let (server, grant) = GatewayServer::bootstrap(
        root.path().join("state"),
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .await
    .expect("bootstrap gateway");
    let listen = server.config.listen;
    let auth = Arc::clone(&server.auth);
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    let endpoint = format!("tcp://{listen}")
        .parse::<Endpoint>()
        .expect("endpoint");
    let (dashboard, dashboard_identity) = GatewayClient::pair(
        &endpoint,
        grant.code,
        "dashboard",
        ClientKind::GatewayDashboard,
    )
    .await
    .expect("pair dashboard");
    let (dashboard_sender, mut dashboard_events) = dashboard.into_parts();
    wait_gateway_ready(&mut dashboard_events).await;
    let device_grant = auth.create_pairing_code().expect("device pairing code");
    let (device, device_identity) =
        GatewayClient::pair(&endpoint, device_grant.code, "iPhone", ClientKind::Ios)
            .await
            .expect("pair device");
    let (_device_sender, mut device_events) = device.into_parts();
    wait_gateway_ready(&mut device_events).await;

    let request_id = Uuid::new_v4().to_string();
    dashboard_sender
        .send(ClientMessage::UnpairClient {
            request_id: request_id.clone(),
            client_id: device_identity.client_id.clone(),
        })
        .await
        .expect("unpair device");
    let (current_client_id, clients) = loop {
        let frame = dashboard_events
            .next()
            .await
            .expect("inventory frame")
            .expect("dashboard open");
        if let ServerMessage::Clients {
            request_id: actual,
            current_client_id,
            clients,
        } = frame.message
            && actual == request_id
        {
            break (current_client_id, clients);
        }
    };
    while tokio::time::timeout(Duration::from_secs(2), device_events.next())
        .await
        .expect("device disconnect timeout")
        .expect("device frame")
        .is_some()
    {}
    let reconnect = GatewayClient::connect(&endpoint, device_identity.token, ClientKind::Ios).await;

    assert_eq!(
        (
            current_client_id == dashboard_identity.client_id,
            clients
                .iter()
                .all(|client| client.client_id != device_identity.client_id),
            matches!(reconnect, Err(Error::Unauthorized)),
        ),
        (true, true, true)
    );
    shutdown.send(()).expect("send shutdown");
    serving
        .await
        .expect("gateway task")
        .expect("gateway shutdown");
}

#[tokio::test(start_paused = true)]
async fn scheduled_task_disables_inactivity_shutdown() {
    let root = tempfile::tempdir().expect("temporary directory");
    let (server, _) = GatewayServer::bootstrap(
        root.path().join("state"),
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .await
    .expect("bootstrap gateway");
    let cron = Arc::clone(&server.cron);
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until_inactive(
        async move {
            let _ = signal.await;
        },
        Duration::from_millis(50),
    ));
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(25)).await;
    cron.add_for_test(
        "source-chat",
        "do work",
        crate::wire::CronSchedule {
            kind: crate::wire::CronScheduleKind::Cron,
            at: None,
            every_seconds: None,
            expression: Some("0 9 * * *".into()),
            time_zone: Some("UTC".into()),
        },
        None,
    )
    .expect("schedule task");

    tokio::time::advance(Duration::from_millis(75)).await;
    assert!(
        !serving.is_finished(),
        "scheduled task must keep gateway alive"
    );
    shutdown.send(()).expect("send shutdown");
    serving
        .await
        .expect("gateway task")
        .expect("gateway shutdown");
}

#[tokio::test]
async fn frontends_select_independent_chats_and_can_share_one_chat() {
    let root = tempfile::tempdir().expect("temporary directory");
    let first_workspace = root.path().join("first");
    let second_workspace = root.path().join("second");
    fs::create_dir(&first_workspace).expect("first workspace");
    fs::create_dir(&second_workspace).expect("second workspace");
    let (server, grant) = GatewayServer::bootstrap(
        root.path().join("state"),
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .await
    .expect("bootstrap gateway");
    let listen = server.config.listen;
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    let endpoint = format!("tcp://{listen}")
        .parse::<Endpoint>()
        .expect("endpoint");
    let (first, paired) = GatewayClient::pair(&endpoint, grant.code, "first", ClientKind::Cli)
        .await
        .expect("pair first frontend");
    let second = GatewayClient::connect(&endpoint, paired.token, ClientKind::Macos)
        .await
        .expect("connect second frontend");
    let (first_sender, mut first_events) = first.into_parts();
    let (second_sender, mut second_events) = second.into_parts();
    wait_gateway_ready(&mut first_events).await;
    wait_gateway_ready(&mut second_events).await;
    let clients_request = Uuid::new_v4().to_string();
    first_sender
        .send(ClientMessage::ListClients {
            request_id: clients_request.clone(),
        })
        .await
        .expect("list clients");
    let clients = loop {
        let frame = first_events
            .next()
            .await
            .expect("client-inventory frame")
            .expect("gateway open");
        if let ServerMessage::Clients {
            request_id,
            current_client_id: _,
            clients,
        } = frame.message
            && request_id == clients_request
        {
            break clients;
        }
    };
    assert_eq!(
        (clients[0].kinds.as_slice(), clients[0].connections),
        ([ClientKind::Cli, ClientKind::Macos].as_slice(), 2)
    );
    let first_session = create_chat(&first_sender, &mut first_events, &first_workspace).await;
    let second_session = create_chat(&second_sender, &mut second_events, &second_workspace).await;
    assert_ne!(first_session, second_session);
    drain_ready_replay(&mut first_events).await;
    drain_ready_replay(&mut second_events).await;

    let first_submission = Uuid::new_v4().to_string();
    first_sender
        .send(ClientMessage::Submit {
            session_id: first_session.clone(),
            submission: Submission {
                id: first_submission.clone(),
                op: Op::UserInput {
                    text: "hello".into(),
                    attachments: Vec::new(),
                },
            },
        })
        .await
        .expect("submit first chat");
    wait_submission(&mut first_events, &first_submission).await;
    let running = wait_session_activity(
        &mut second_events,
        &first_session,
        SessionActivityState::Running,
    )
    .await;
    let finished = wait_session_activity(
        &mut second_events,
        &first_session,
        SessionActivityState::Idle,
    )
    .await;
    assert!(running.turn_id.is_some());
    assert!(finished.last_outcome.is_some());

    open_chat(&second_sender, &mut second_events, &first_session).await;
    drain_ready_replay(&mut second_events).await;
    let shared_submission = Uuid::new_v4().to_string();
    first_sender
        .send(ClientMessage::Submit {
            session_id: first_session,
            submission: Submission {
                id: shared_submission.clone(),
                op: Op::UserInput {
                    text: "shared".into(),
                    attachments: Vec::new(),
                },
            },
        })
        .await
        .expect("submit shared chat");
    wait_submission(&mut first_events, &shared_submission).await;
    wait_submission(&mut second_events, &shared_submission).await;

    shutdown.send(()).expect("stop gateway");
    serving.await.expect("gateway task").expect("gateway stop");
}

#[tokio::test]
async fn branch_switch_is_acknowledged_and_broadcasts_fresh_status() {
    let root = tempfile::tempdir().expect("temporary directory");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    run_git(&workspace, &["init", "--quiet", "--initial-branch", "main"]);
    run_git(
        &workspace,
        &["config", "user.email", "mobius@example.invalid"],
    );
    run_git(&workspace, &["config", "user.name", "möbius Test"]);
    fs::write(workspace.join("tracked.txt"), b"main").expect("tracked file");
    run_git(&workspace, &["add", "--", "tracked.txt"]);
    run_git(&workspace, &["commit", "--quiet", "-m", "initial"]);
    run_git(&workspace, &["branch", "feature"]);
    let (server, grant) = GatewayServer::bootstrap(
        root.path().join("state"),
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .await
    .expect("bootstrap gateway");
    let listen = server.config.listen;
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    let endpoint = format!("tcp://{listen}")
        .parse::<Endpoint>()
        .expect("endpoint");
    let (connection, _) =
        GatewayClient::pair(&endpoint, grant.code, "branch test", ClientKind::Cli)
            .await
            .expect("pair frontend");
    let (sender, mut events) = connection.into_parts();
    wait_gateway_ready(&mut events).await;
    let session_id = create_chat(&sender, &mut events, &workspace).await;
    drain_ready_replay(&mut events).await;
    let request_id = Uuid::new_v4().to_string();

    sender
        .send(ClientMessage::SwitchGitBranch {
            request_id: request_id.clone(),
            session_id,
            branch: "feature".into(),
        })
        .await
        .expect("switch branch");
    let mut accepted = false;
    let mut changed = false;
    while !accepted || !changed {
        let frame = tokio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .expect("branch response timeout")
            .expect("gateway frame")
            .expect("gateway open");
        match frame.message {
            ServerMessage::Accepted { request_id: actual } if actual == request_id => {
                accepted = true;
            }
            ServerMessage::SessionChanged { payload } => {
                changed = payload.git.is_some_and(|git| {
                    git.current_branch == "feature"
                        && git.branches == ["feature".to_string(), "main".to_string()]
                });
            }
            _ => {}
        }
    }

    shutdown.send(()).expect("stop gateway");
    serving.await.expect("gateway task").expect("gateway stop");
}
