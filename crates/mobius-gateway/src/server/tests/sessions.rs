use super::*;

async fn expect_accepted(events: &mut GatewayEvents, expected_request_id: &str) {
    loop {
        match next_gateway_message(events).await {
            ServerMessage::Accepted { request_id } if request_id == expected_request_id => return,
            ServerMessage::Rejected {
                request_id,
                code,
                message,
                ..
            } if request_id == expected_request_id => {
                panic!("request rejected ({code}): {message}")
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn catalogue_mutations_do_not_require_selecting_the_target_chat() {
    let root = tempfile::tempdir().expect("temporary directory");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let listen = listener.local_addr().expect("listen address");
    let (store, config) = ConfigStore::initialize(root.path().join("state"), listen, None)
        .expect("initialize gateway");
    let checkpoints_path = store.checkpoints_path();
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
    let (connection, _) = GatewayClient::pair(
        &endpoint,
        grant.code,
        "catalogue mutation test",
        ClientKind::Ios,
    )
    .await
    .expect("pair frontend");
    let (sender, mut events) = connection.into_parts();
    wait_gateway_ready(&mut events).await;
    let inactive_session_id = create_chat(&sender, &mut events, &workspace).await;
    let selected_session_id = create_chat(&sender, &mut events, &workspace).await;

    sender
        .send(ClientMessage::RenameSession {
            request_id: "rename-inactive".into(),
            session_id: inactive_session_id.clone(),
            title: "Renamed without opening".into(),
        })
        .await
        .expect("rename inactive chat");
    expect_accepted(&mut events, "rename-inactive").await;

    sender
        .send(ClientMessage::SetSessionPinned {
            request_id: "pin-inactive".into(),
            session_id: inactive_session_id.clone(),
            pinned: true,
        })
        .await
        .expect("pin inactive chat");
    expect_accepted(&mut events, "pin-inactive").await;

    sender
        .send(ClientMessage::DeleteSessions {
            request_id: "delete-inactive".into(),
            session_ids: vec![inactive_session_id],
        })
        .await
        .expect("delete inactive chat");
    expect_accepted(&mut events, "delete-inactive").await;

    sender
        .send(ClientMessage::GetSessionHistory {
            request_id: "selected-history".into(),
            session_id: selected_session_id.clone(),
            before_sequence: None,
        })
        .await
        .expect("read selected chat history");
    loop {
        match next_gateway_message(&mut events).await {
            ServerMessage::SessionHistory { request_id, .. }
                if request_id == "selected-history" =>
            {
                break;
            }
            ServerMessage::Rejected {
                request_id,
                code,
                message,
                ..
            } if request_id == "selected-history" => {
                panic!("selected chat was cleared ({code}): {message}")
            }
            _ => {}
        }
    }

    let checkpoints = SqliteCheckpoint::new(checkpoints_path).expect("open checkpoints");
    let selected = checkpoints
        .load(&selected_session_id)
        .await
        .expect("load selected")
        .expect("selected checkpoint");
    let mut hidden = Checkpoint::empty("hidden-child");
    hidden.session_context = selected.session_context.clone();
    hidden.catalog_visible = false;
    checkpoints
        .fork(&selected_session_id, selected.sequence, &hidden)
        .await
        .expect("fork hidden child");
    sender
        .send(ClientMessage::RenameSession {
            request_id: "rename-hidden".into(),
            session_id: "hidden-child".into(),
            title: "Hidden".into(),
        })
        .await
        .expect("rename hidden chat");
    loop {
        match next_gateway_message(&mut events).await {
            ServerMessage::Rejected {
                request_id, code, ..
            } if request_id == "rename-hidden" => {
                assert_eq!(code, "unknown_session");
                break;
            }
            ServerMessage::Accepted { request_id } if request_id == "rename-hidden" => {
                panic!("hidden chat was renamed")
            }
            _ => {}
        }
    }

    let parent_session_id = create_chat(&sender, &mut events, &workspace).await;
    let parent = checkpoints
        .load(&parent_session_id)
        .await
        .expect("load parent")
        .expect("parent checkpoint");
    let child_session_id = "selected-child";
    let mut child = Checkpoint::empty(child_session_id);
    child.session_context.clone_from(&parent.session_context);
    child.metadata.clone_from(&parent.metadata);
    child.model_route.clone_from(&parent.model_route);
    checkpoints
        .fork(&parent_session_id, parent.sequence, &child)
        .await
        .expect("fork child");
    open_chat(&sender, &mut events, child_session_id).await;

    sender
        .send(ClientMessage::DeleteSessions {
            request_id: "delete-selected-child-parent".into(),
            session_ids: vec![parent_session_id],
        })
        .await
        .expect("delete selected child's parent");
    expect_accepted(&mut events, "delete-selected-child-parent").await;

    sender
        .send(ClientMessage::GetSessionHistory {
            request_id: "deleted-child-history".into(),
            session_id: child_session_id.into(),
            before_sequence: None,
        })
        .await
        .expect("read deleted child history");
    loop {
        match next_gateway_message(&mut events).await {
            ServerMessage::Rejected {
                request_id, code, ..
            } if request_id == "deleted-child-history" => {
                assert_eq!(code, "session_required");
                break;
            }
            ServerMessage::SessionHistory { request_id, .. }
                if request_id == "deleted-child-history" =>
            {
                panic!("deleted child remained selected")
            }
            _ => {}
        }
    }

    shutdown.send(()).expect("stop gateway");
    serving.await.expect("gateway task").expect("gateway stop");
}

#[tokio::test]
async fn paired_client_cancels_and_deletes_session_uploads() {
    let root = tempfile::tempdir().expect("temporary directory");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let (server, grant) = configured_test_server(root.path().join("state")).await;
    let listen = server.config.listen;
    let files = server.host.session_file_store().await;
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    let endpoint = format!("tcp://{listen}")
        .parse::<Endpoint>()
        .expect("endpoint");
    let (connection, _) =
        GatewayClient::pair(&endpoint, grant.code, "file deletion", ClientKind::Ios)
            .await
            .expect("pair frontend");
    let (sender, mut events) = connection.into_parts();
    wait_gateway_ready(&mut events).await;
    let mut config = crate::wire::AgentComposition::default();
    config.middleware.set_enabled("attachments", true);
    let (session_id, _) =
        create_bot_chat_with_config(&sender, &mut events, &workspace, config).await;

    sender
        .send(ClientMessage::BeginSessionFileUpload {
            request_id: "begin-cancelled".into(),
            session_id: session_id.clone(),
            name: "cancelled.txt".into(),
            size: 1,
            media_type: "text/plain".into(),
        })
        .await
        .expect("begin cancelled upload");
    let cancelled_id = loop {
        if let ServerMessage::SessionFileUploadReady {
            request_id,
            upload_id,
            ..
        } = next_gateway_message(&mut events).await
            && request_id == "begin-cancelled"
        {
            break upload_id;
        }
    };
    for request_id in ["cancel-upload", "repeat-cancel"] {
        sender
            .send(ClientMessage::DeleteSessionFile {
                request_id: request_id.into(),
                session_id: session_id.clone(),
                file_id: cancelled_id.clone(),
            })
            .await
            .expect("cancel upload");
        expect_accepted(&mut events, request_id).await;
    }
    sender
        .send(ClientMessage::FinishSessionFileUpload {
            request_id: "finish-cancelled".into(),
            session_id: session_id.clone(),
            upload_id: cancelled_id,
        })
        .await
        .expect("finish cancelled upload");
    loop {
        if let ServerMessage::Rejected {
            request_id,
            message,
            ..
        } = next_gateway_message(&mut events).await
            && request_id == "finish-cancelled"
        {
            assert!(message.contains("not active"));
            break;
        }
    }

    sender
        .send(ClientMessage::BeginSessionFileUpload {
            request_id: "begin-deleted".into(),
            session_id: session_id.clone(),
            name: "deleted.txt".into(),
            size: 1,
            media_type: "text/plain".into(),
        })
        .await
        .expect("begin deleted upload");
    let deleted_id = loop {
        if let ServerMessage::SessionFileUploadReady {
            request_id,
            upload_id,
            ..
        } = next_gateway_message(&mut events).await
            && request_id == "begin-deleted"
        {
            break upload_id;
        }
    };
    sender
        .send(ClientMessage::UploadSessionFileChunk {
            request_id: "write-deleted".into(),
            session_id: session_id.clone(),
            upload_id: deleted_id.clone(),
            offset: 0,
            data: b"x".to_vec(),
        })
        .await
        .expect("write deleted upload");
    loop {
        if matches!(
            next_gateway_message(&mut events).await,
            ServerMessage::SessionFileUploadChunkAccepted { request_id, .. }
                if request_id == "write-deleted"
        ) {
            break;
        }
    }
    sender
        .send(ClientMessage::FinishSessionFileUpload {
            request_id: "finish-deleted".into(),
            session_id: session_id.clone(),
            upload_id: deleted_id.clone(),
        })
        .await
        .expect("finish deleted upload");
    loop {
        if matches!(
            next_gateway_message(&mut events).await,
            ServerMessage::SessionFileUploadCompleted { request_id, .. }
                if request_id == "finish-deleted"
        ) {
            break;
        }
    }
    sender
        .send(ClientMessage::DeleteSessionFile {
            request_id: "delete-completed".into(),
            session_id: session_id.clone(),
            file_id: deleted_id,
        })
        .await
        .expect("delete completed upload");
    expect_accepted(&mut events, "delete-completed").await;
    assert!(
        files
            .list_uploads(&session_id)
            .await
            .expect("list uploads")
            .is_empty()
    );

    let artifact = files
        .publish_artifact(
            &session_id,
            "result.txt".into(),
            "text/plain".into(),
            b"result",
        )
        .await
        .expect("publish artifact");
    sender
        .send(ClientMessage::DeleteSessionFile {
            request_id: "reject-artifact".into(),
            session_id: session_id.clone(),
            file_id: artifact.id.clone(),
        })
        .await
        .expect("reject artifact deletion");
    loop {
        if let ServerMessage::Rejected {
            request_id, code, ..
        } = next_gateway_message(&mut events).await
            && request_id == "reject-artifact"
        {
            assert_eq!(code, "session_file_rejected");
            break;
        }
    }
    assert_eq!(
        files
            .list_artifacts(&session_id)
            .await
            .expect("list artifacts"),
        [artifact]
    );

    create_chat(&sender, &mut events, &workspace).await;
    sender
        .send(ClientMessage::DeleteSessionFile {
            request_id: "reject-unselected-delete".into(),
            session_id,
            file_id: Uuid::new_v4().to_string(),
        })
        .await
        .expect("reject unselected deletion");
    loop {
        if let ServerMessage::Rejected {
            request_id, code, ..
        } = next_gateway_message(&mut events).await
            && request_id == "reject-unselected-delete"
        {
            assert_eq!(code, "session_not_selected");
            break;
        }
    }

    shutdown.send(()).expect("stop gateway");
    serving.await.expect("gateway task").expect("gateway stop");
}

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
            session_id: session_id.clone(),
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
            && request_id == "reject-unselected-file-list"
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
        if actual_session != session_id
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
async fn paired_client_deletes_routine_sessions_with_the_routine() {
    let root = tempfile::tempdir().expect("temporary directory");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let (server, grant) = configured_test_server(root.path().join("state")).await;
    let listen = server.config.listen;
    let host = server.host.clone();
    let bots = Arc::clone(&server.bots);
    let files = host.session_file_store().await;
    let checkpoints = SqliteCheckpoint::new(root.path().join("state/checkpoints.sqlite3"))
        .expect("checkpoint store");
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    let endpoint = format!("tcp://{listen}")
        .parse::<Endpoint>()
        .expect("endpoint");
    let (connection, _) =
        GatewayClient::pair(&endpoint, grant.code, "routine files", ClientKind::Ios)
            .await
            .expect("pair frontend");
    let (sender, mut events) = connection.into_parts();
    wait_gateway_ready(&mut events).await;
    let (_, bot_id) = create_bot_chat(&sender, &mut events, &workspace).await;
    let routine = bots
        .create_routine(
            &bot_id,
            &workspace,
            "produce a report",
            crate::wire::RoutineSchedule {
                kind: crate::wire::RoutineScheduleKind::Interval,
                at: None,
                every_seconds: Some(600),
                expression: None,
                time_zone: None,
            },
            None,
        )
        .expect("create routine");

    let _ = host.run_routine(routine.id.clone()).await;
    let execution_session_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(run) = bots
                .history(Some(&routine.id))
                .expect("run history")
                .first()
                && run.status != crate::wire::RoutineRunStatus::Running
                && let Some(session_id) = &run.session_id
            {
                break session_id.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("routine completion");
    let contents = b"routine report";
    files
        .publish_artifact(
            &execution_session_id,
            "report.txt".into(),
            "text/plain".into(),
            contents,
        )
        .await
        .expect("publish routine artifact");
    assert!(
        checkpoints
            .load(&execution_session_id)
            .await
            .expect("load routine checkpoint")
            .is_some()
    );
    sender
        .send(ClientMessage::DeleteRoutine {
            request_id: "delete-routine".into(),
            id: routine.id.clone(),
        })
        .await
        .expect("delete routine");
    expect_accepted(&mut events, "delete-routine").await;

    assert!(bots.routine(&routine.id).is_err());
    assert!(bots.history(None).expect("empty history").is_empty());
    assert!(
        checkpoints
            .load(&execution_session_id)
            .await
            .expect("load deleted routine checkpoint")
            .is_none()
    );
    assert!(
        files
            .list_files(&execution_session_id)
            .await
            .expect("deleted routine files")
            .is_empty()
    );

    shutdown.send(()).expect("send shutdown");
    serving
        .await
        .expect("gateway task")
        .expect("gateway shutdown");
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
async fn running_one_shot_routine_disables_inactivity_shutdown() {
    let root = tempfile::tempdir().expect("temporary directory");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let (server, _) = GatewayServer::bootstrap(
        root.path().join("state"),
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .await
    .expect("bootstrap gateway");
    let bots = Arc::clone(&server.bots);
    let bot = bots
        .create_bot(
            "keepalive",
            "Keepalive",
            crate::wire::AgentComposition::default(),
        )
        .expect("Bot");
    let now = Utc::now().timestamp();
    bots.create_routine(
        &bot.id,
        &workspace,
        "hold the gateway open",
        crate::wire::RoutineSchedule {
            kind: crate::wire::RoutineScheduleKind::Once,
            at: Some(now - 1),
            every_seconds: None,
            expression: None,
            time_zone: None,
        },
        None,
    )
    .expect("one-shot routine");
    let (_, active_run) = bots
        .take_due(now)
        .expect("due routines")
        .pop()
        .expect("due one-shot");
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until_inactive(
        async move {
            let _ = signal.await;
        },
        Duration::from_millis(50),
    ));
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(75)).await;
    assert!(
        !serving.is_finished(),
        "a running one-shot routine must keep the gateway alive after its schedule is consumed"
    );
    shutdown.send(()).expect("send shutdown");
    serving
        .await
        .expect("gateway task")
        .expect("gateway shutdown");
    drop(active_run);
}

#[tokio::test]
async fn frontends_select_independent_chats_and_can_share_one_chat() {
    let root = tempfile::tempdir().expect("temporary directory");
    let first_workspace = root.path().join("first");
    let second_workspace = root.path().join("second");
    fs::create_dir(&first_workspace).expect("first workspace");
    fs::create_dir(&second_workspace).expect("second workspace");
    let (server, grant) = configured_test_server(root.path().join("state")).await;
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
                op: user_message("hello", Vec::new()),
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
                op: user_message("shared", Vec::new()),
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
    let (server, grant) = configured_test_server(root.path().join("state")).await;
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
