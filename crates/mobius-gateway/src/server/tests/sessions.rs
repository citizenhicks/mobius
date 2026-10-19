use super::*;

#[cfg(unix)]
#[tokio::test]
async fn slow_git_diff_does_not_block_chat_history() {
    use crate::wire::GitDiffScope;
    use std::os::unix::fs::OpenOptionsExt as _;

    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&workspace)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init"]);
    fs::write(workspace.join("tracked.txt"), "before\n").expect("tracked file");
    fs::write(
        workspace.join(".gitattributes"),
        "tracked.txt filter=gate\n",
    )
    .expect("attributes");
    git(&["add", "."]);
    let gate = workspace.join(".git/gate");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&gate)
            .status()
            .expect("fifo")
            .success()
    );
    git(&[
        "config",
        "filter.gate.clean",
        "cat .git/gate >/dev/null; cat",
    ]);
    fs::write(workspace.join("tracked.txt"), "after\n").expect("changed file");

    let (server, grant) = configured_test_server(root.path().join("state")).await;
    let endpoint = format!("tcp://{}", server.config.listen)
        .parse::<Endpoint>()
        .expect("endpoint");
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        signal.await.ok();
    }));
    let (connection, _) = GatewayClient::pair(&endpoint, grant.code, "Git test", ClientKind::Cli)
        .await
        .expect("pair");
    let (sender, mut events) = connection.into_parts();
    wait_gateway_ready(&mut events).await;
    let session_id = create_chat(&sender, &mut events, &workspace).await;
    sender
        .send(ClientMessage::GetGitDiff {
            request_id: "diff".into(),
            session_id: session_id.clone(),
            scope: GitDiffScope::Unstaged,
        })
        .await
        .expect("request diff");

    // Opening the writer proves Git reached the filter; holding it open prevents completion.
    let release = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .custom_flags(nix::libc::O_NONBLOCK)
                .open(&gate)
            {
                Ok(file) => break file,
                Err(error) if error.raw_os_error() == Some(nix::libc::ENXIO) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("open Git gate: {error}"),
            }
        }
    })
    .await
    .expect("Git must reach the filter");
    sender
        .send(ClientMessage::GetSessionHistory {
            request_id: "history".into(),
            session_id: session_id.clone(),
            before_sequence: None,
        })
        .await
        .expect("request history");
    loop {
        match next_gateway_message(&mut events).await {
            ServerMessage::SessionHistory { request_id, .. } if &*request_id == "history" => break,
            ServerMessage::GitDiff { .. } => panic!("Git should still be blocked"),
            ServerMessage::Rejected {
                request_id,
                message,
                ..
            } if request_id.as_ref() == "history" || request_id.as_ref() == "diff" => {
                panic!("{message}")
            }
            _ => {}
        }
    }
    // Git may run the clean filter again, so release later opens as well as this reader.
    let released = gate.with_extension("released");
    fs::write(&released, "").expect("released gate");
    fs::rename(released, &gate).expect("replace Git gate");
    drop(release);
    loop {
        if let ServerMessage::GitDiff {
            request_id,
            session_id: actual,
            scope,
            diff,
        } = next_gateway_message(&mut events).await
        {
            assert_eq!(&*request_id, "diff");
            assert_eq!(actual, session_id);
            assert_eq!(scope, GitDiffScope::Unstaged);
            assert!(diff.contains("+after"));
            break;
        }
    }
    shutdown.send(()).expect("shutdown");
    serving.await.expect("server task").expect("server");
}

#[tokio::test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "the flagged value moves into the gateway task; the lint misses that move"
)]
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
    let (_, grant) =
        AuthStore::initialize(store.auth_path(), config.auth).expect("initialize auth");
    let server = GatewayServer::assemble(store, config, listener)
        .await
        .expect("assemble gateway");
    let listen = server.config.listen;
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        signal.await.ok();
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
    let mut config = crate::wire::AgentComposition::default();
    config.middleware.set_enabled("attachments", true);
    let (session_id, _) =
        create_bot_chat_with_config(&sender, &mut events, &workspace, config).await;

    let missing = SessionFileReference {
        id: Uuid::new_v4().to_string(),
        name: "missing.txt".into(),
        size: 1,
        media_type: "text/plain".into(),
    };
    sender
        .send(ClientMessage::Submit {
            session_id: session_id.as_str().into(),
            submission: Submission {
                id: "invalid-duplicate-attachments".into(),
                op: user_message("invalid", vec![missing.clone(), missing]),
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
            && request_id.as_ref() == "invalid-duplicate-attachments"
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
                request_id: begin_id.as_str().into(),
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
                && request_id.as_ref() == begin_id
            {
                break upload_id;
            }
        };

        open_chat(&sender, &mut events, &other_session_id).await;
        let rejection_id = format!("reject-unselected-{finish}");
        let request = if finish {
            ClientMessage::FinishSessionFileUpload {
                request_id: rejection_id.as_str().into(),
                session_id: session_id.clone(),
                upload_id: upload_id.clone(),
            }
        } else {
            ClientMessage::UploadSessionFileChunk {
                request_id: rejection_id.as_str().into(),
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
                && request_id.as_ref() == rejection_id
            {
                assert_eq!(code, "session_not_selected");
                break;
            }
        }

        open_chat(&sender, &mut events, &session_id).await;
        let retry_id = format!("retry-terminated-{finish}");
        sender
            .send(ClientMessage::FinishSessionFileUpload {
                request_id: retry_id.as_str().into(),
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
                && request_id.as_ref() == retry_id
            {
                assert!(message.contains("not active"));
                break;
            }
        }
    }

    open_chat(&sender, &mut events, &other_session_id).await;
    sender
        .send(ClientMessage::ListSessionFiles {
            request_id: "reject-unselected-file-list".into(),
            session_id: session_id.clone(),
        })
        .await
        .expect("reject file list for unselected chat");
    loop {
        if let ServerMessage::Rejected {
            request_id, code, ..
        } = next_gateway_message(&mut events).await
            && request_id.as_ref() == "reject-unselected-file-list"
        {
            assert_eq!(code, "session_not_selected");
            break;
        }
    }
    open_chat(&sender, &mut events, &session_id).await;

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
            && &*request_id == "begin-doomed-upload"
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
                if request_id.as_ref() == "reject-doomed-chunk"
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
            && request_id.as_ref() == "finish-doomed-upload"
        {
            assert!(message.contains("not active"));
            break;
        }
    }

    let image = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").expect("PNG");
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
            && &*request_id == "begin-upload"
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
                    if *actual == *request_id
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
            && &*request_id == "finish-upload"
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
            && &*request_id == "list-session-files"
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
            && &*request_id == "read-session-file"
        {
            assert_eq!(data, image);
            assert_eq!(next_offset, None);
            break;
        }
    }

    let submission_id = "submit-attachment".to_owned();
    sender
        .send(ClientMessage::Submit {
            session_id: session_id.as_str().into(),
            submission: Submission {
                id: submission_id.as_str().into(),
                op: user_message("describe the image", vec![file.clone()]),
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
        if actual_session.as_ref() != session_id
            || record.event.submission_id.as_deref() != Some(&submission_id)
        {
            continue;
        }
        match record.event.msg {
            EventMsg::Message(message) if matches!(&message.author, MessageAuthor::User) => {
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
    assert_unpairing_disconnects(false).await;
}

async fn assert_unpairing_disconnects(sync_fails: bool) {
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
        signal.await.ok();
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

    if sync_fails {
        crate::publication::fail_next_directory_sync(&root.path().join("state/auth.json"));
    }
    let request_id = Uuid::new_v4().to_string();
    dashboard_sender
        .send(ClientMessage::UnpairClient {
            request_id: request_id.as_str().into(),
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
        if sync_fails
            && let ServerMessage::Rejected {
                request_id: actual,
                message,
                ..
            } = &frame.message
            && **actual == *request_id
        {
            assert!(message.contains("state replacement was applied"));
            break (dashboard_identity.client_id.clone(), Vec::new());
        }
        if let ServerMessage::Clients {
            request_id: actual,
            current_client_id,
            clients,
        } = frame.message
            && actual.as_ref() == request_id
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
    let reconnect =
        GatewayClient::connect(&endpoint, &device_identity.token, ClientKind::Ios).await;

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

#[tokio::test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "the flagged value moves into the gateway task; the lint misses that move"
)]
async fn shutdown_stops_a_blocked_routine_and_fails_its_durable_run() {
    let root = tempfile::tempdir().expect("temporary directory");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let model_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("model listener");
    let model_base_url = format!(
        "http://{}/v1",
        model_listener.local_addr().expect("model address")
    );
    let (model_seen, model_request) = tokio::sync::oneshot::channel();
    let model_server = tokio::spawn(async move {
        let (mut stream, _) = model_listener.accept().await.expect("model request");
        model_seen.send(()).ok();
        let mut buffer = [0; 8 * 1024];
        while stream.read(&mut buffer).await.expect("model connection") != 0 {}
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let listen = listener.local_addr().expect("listen address");
    let (store, config) = ConfigStore::initialize(root.path().join("state"), listen, None)
        .expect("initialize gateway");
    let provider = crate::wire::ProviderConfig {
        tool_discovery: None,
        instance: "blocked-routine".into(),
        provider: "responses".into(),
        model: "local-test".into(),
        base_url: Some(model_base_url.clone()),
        endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
        reasoning_effort: None,
        service_tier: None,
        web_search: mobius::backend::model::provider::HostedWebSearch::Off,
    };
    let composition = crate::wire::AgentComposition {
        provider: provider.clone(),
        ..crate::wire::AgentComposition::default()
    };
    let config = config
        .registering_provider(
            provider,
            "Blocked routine".into(),
            Default::default(),
            vec![crate::wire::ConfiguredModel {
                id: "local-test".into(),
                reasoning_efforts: Some(Vec::new()),
                default_reasoning: None,
                ..Default::default()
            }],
            Vec::new(),
        )
        .expect("register provider");
    store.save(&config).expect("save provider");
    let credentials = CredentialStore::open(store.credentials_path()).expect("credentials");
    credentials
        .set(
            "blocked-routine",
            "responses",
            "test-key",
            Some(&model_base_url),
            None,
        )
        .expect("store provider credential");
    let (_, grant) = AuthStore::initialize(store.auth_path(), config.auth).expect("authentication");
    let server = GatewayServer::assemble(store, config, listener)
        .await
        .expect("gateway");
    let bots = Arc::clone(&server.bots);
    let host = Arc::clone(&server.host);
    let bot = bots
        .create_bot(
            "Blocked routine bot",
            "A blocked routine test bot.",
            composition,
        )
        .expect("Bot");
    let now = Utc::now().timestamp();
    let routine = bots
        .create_routine(
            &bot.id,
            &timer_definition(
                &workspace,
                "wait for shutdown",
                crate::wire::RoutineSchedule {
                    kind: crate::wire::RoutineScheduleKind::Once,
                    at: Some(now - 1),
                    every_seconds: None,
                    expression: None,
                    time_zone: None,
                },
            ),
            None,
        )
        .expect("routine");
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        signal.await.ok();
    }));

    tokio::time::timeout(Duration::from_secs(10), model_request)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "routine did not reach model: {:?}",
                bots.history(Some(&routine.id))
            )
        })
        .expect("routine reached blocked model");
    let run = bots
        .history(Some(&routine.id))
        .expect("routine history")
        .first()
        .cloned()
        .expect("running routine");
    let session_id = run.session_id.clone().expect("routine session");
    let session = host
        .open_session(&session_id)
        .await
        .expect("routine session host");

    shutdown.send(()).expect("shutdown gateway");
    tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .expect("gateway shutdown timeout")
        .expect("gateway task")
        .expect("gateway shutdown");
    tokio::time::timeout(Duration::from_secs(5), session.wait_terminated())
        .await
        .expect("routine session termination timeout");

    let history = bots
        .history(Some(&routine.id))
        .expect("routine history after shutdown");
    let run = history.first().expect("durable routine run");
    assert_eq!(run.status, crate::wire::RoutineRunStatus::Failed);
    assert_eq!(run.session_id.as_deref(), Some(session_id.as_str()));

    tokio::time::timeout(Duration::from_secs(5), model_server)
        .await
        .expect("model server shutdown timeout")
        .expect("model server task");
    drop(grant);
}

async fn request_history(
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    session_id: &str,
) -> std::result::Result<Vec<crate::wire::RecordedEvent>, String> {
    let request_id = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::GetSessionHistory {
            request_id: request_id.as_str().into(),
            session_id: session_id.into(),
            before_sequence: None,
        })
        .await
        .expect("request history");
    loop {
        match next_gateway_message(events).await {
            ServerMessage::SessionHistory {
                request_id: actual,
                records,
                ..
            } if actual.as_ref() == request_id => return Ok(records),
            ServerMessage::Rejected {
                request_id: actual,
                code,
                ..
            } if actual.as_ref() == request_id => return Err(code),
            _ => {}
        }
    }
}

#[tokio::test]
async fn unselected_chat_history_is_read_without_starting_the_chat() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let (server, grant) = configured_test_server(root.path().join("state")).await;
    let host = Arc::clone(&server.host);
    let endpoint = format!("tcp://{}", server.config.listen)
        .parse::<Endpoint>()
        .expect("endpoint");
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        signal.await.ok();
    }));
    let (connection, _) = GatewayClient::pair(&endpoint, grant.code, "Recap test", ClientKind::Cli)
        .await
        .expect("pair");
    let (sender, mut events) = connection.into_parts();
    wait_gateway_ready(&mut events).await;
    let away = create_chat(&sender, &mut events, &workspace).await;
    let selected = create_chat(&sender, &mut events, &workspace).await;
    host.stop_idle_session(&away).await;
    let checkpoints = host.checkpoint_store().await;
    for (recorded_at, msg) in [
        EventMsg::TurnStarted(mobius::protocol::TurnStartedEvent {
            turn_id: "away".into(),
            model_context_window: None,
        }),
        EventMsg::Warning(mobius::protocol::WarningEvent {
            message: "work while away".into(),
        }),
        EventMsg::TurnComplete(mobius::protocol::TurnCompleteEvent {
            turn_id: "away".into(),
        }),
    ]
    .into_iter()
    .enumerate()
    {
        checkpoints
            .append_event(
                &away,
                i64::try_from(recorded_at).expect("timestamp"),
                Event {
                    submission_id: None,
                    msg,
                },
            )
            .await
            .expect("append journal event");
    }

    let records = request_history(&sender, &mut events, &away)
        .await
        .expect("unselected history");
    let warning = records
        .iter()
        .find(|record| {
            matches!(&record.event.msg, EventMsg::Warning(warning) if &*warning.message == "work while away")
        })
        .expect("durable warning");
    assert!(
        warning
            .blocks
            .iter()
            .any(|block| block.block.title == "Warning")
    );
    assert!(!host.is_resident(&away).await);

    request_history(&sender, &mut events, &selected)
        .await
        .expect("selected history");
    assert!(host.is_resident(&selected).await);

    for (session_id, expected) in [
        ("missing-chat", "unknown_session"),
        ("  ", "invalid_session_id"),
    ] {
        assert_eq!(
            request_history(&sender, &mut events, session_id)
                .await
                .err()
                .as_deref(),
            Some(expected)
        );
    }
    sender
        .send(ClientMessage::DeleteSessions {
            request_id: "delete-away".into(),
            session_ids: vec![away.as_str().into()],
            selection: mobius::backend::session_files::SessionFileSelection::All,
        })
        .await
        .expect("delete chat");
    loop {
        match next_gateway_message(&mut events).await {
            ServerMessage::Accepted { request_id } if request_id.as_ref() == "delete-away" => break,
            ServerMessage::Rejected {
                request_id,
                message,
                ..
            } if request_id.as_ref() == "delete-away" => panic!("delete rejected: {message}"),
            _ => {}
        }
    }
    assert_eq!(
        request_history(&sender, &mut events, &away)
            .await
            .err()
            .as_deref(),
        Some("unknown_session")
    );
    assert!(!host.is_resident(&away).await);
    drop(host);
    shutdown.send(()).expect("shutdown");
    serving.await.expect("server task").expect("server");
}
