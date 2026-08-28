use super::*;

#[tokio::test]
async fn attachment_hydration_runs_after_native_compaction_replaces_context() {
    let workspace = TempDir::new().expect("create workspace");
    let session_id = "attachment-compaction";
    let store = SessionFileStore::new(workspace.path());
    let attachment = upload_attachment(
        &store,
        session_id,
        "photo.png",
        "image/png",
        b"\x89PNG\r\n\x1a\n",
    )
    .await;
    let model = Arc::new(
        ScriptedModel::with_compaction(
            vec![
                text_response_with_usage("draft", usage(2_000)),
                text_response("done"),
            ],
            vec![
                CompactOutput::from_output(
                    vec![serde_json::json!({
                        "type": "compaction",
                        "encrypted_content": "opaque"
                    })],
                    usage(10),
                )
                .expect("compaction output"),
            ],
        )
        .with_image_input(),
    );
    let config = test_config(
        workspace.path(),
        Arc::clone(&model),
        vec![
            Arc::new(Tools::new(Vec::new())),
            Arc::new(Attachments::new(store)),
            Arc::new(Compaction::new(1_000).expect("compaction")),
        ],
    )
    .session_id(session_id);
    let mut agent = create_agent(config).await.expect("create agent");

    agent
        .sender()
        .submit(Op::UserInput {
            text: "first".into(),
            attachments: Vec::new(),
        })
        .expect("submit first turn");
    assert_eq!(final_message(&mut agent).await, "draft");
    agent
        .sender()
        .submit(Op::UserInput {
            text: "inspect".into(),
            attachments: vec![attachment.clone()],
        })
        .expect("submit attachment turn");
    assert_eq!(final_message(&mut agent).await, "done");

    assert_eq!(
        model
            .compact_requests
            .lock()
            .expect("compact requests")
            .len(),
        1
    );
    let requests = model.requests.lock().expect("requests");
    assert_eq!(requests.len(), 2);
    assert_eq!(request_image_count(&requests[1].input), 1);
    let final_input = serde_json::to_string(&requests[1].input).expect("serialize final input");
    assert!(final_input.contains("opaque"));
    assert!(final_input.contains(&attachment.id));
}

#[tokio::test]
async fn video_attachments_are_exposed_as_workspace_files() {
    let workspace = TempDir::new().expect("create workspace");
    let state = TempDir::new().expect("create state directory");
    let session_id = "video-attachment";
    let store = SessionFileStore::new(state.path());
    let video = b"\0\0\0\x14ftypqt  \xff";
    let attachment =
        upload_attachment(&store, session_id, "clip.mov", "video/quicktime", video).await;
    let model = Arc::new(ScriptedModel::new(vec![
        tool_response(
            "search-attachments",
            mobius::backend::model::TOOLS_SEARCH_NAME,
            serde_json::json!({"query": "attachments"}),
        ),
        tool_response("list-video", "list_attachments", serde_json::json!({})),
        text_response("done"),
    ]));
    let config = test_config(
        workspace.path(),
        Arc::clone(&model),
        vec![
            Arc::new(Tools::new(Vec::new())),
            Arc::new(
                Attachments::new(store.clone())
                    .with_workspace(workspace.path())
                    .expect("configure attachment workspace"),
            ),
        ],
    )
    .session_id(session_id);
    let mut agent = create_agent(config).await.expect("create agent");

    agent
        .sender()
        .submit(Op::UserInput {
            text: "read it".into(),
            attachments: vec![attachment.clone()],
        })
        .expect("submit attachment turn");

    assert_eq!(final_message(&mut agent).await, "done");
    {
        let requests = model.requests.lock().expect("requests");
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[0]
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            [mobius::backend::model::TOOLS_SEARCH_NAME]
        );
        assert_eq!(
            requests[1]
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            [
                mobius::backend::model::TOOLS_SEARCH_NAME,
                "list_attachments"
            ]
        );
        assert_eq!(request_image_count(&requests[0].input), 0);
        let input = serde_json::to_string(&requests[0].input).expect("serialize request");
        assert!(input.contains("User-attached files available"));
        assert!(input.contains(&attachment.id));
        assert!(input.contains("path: .mobius/attachments/"));
        let tool_output = requests[2]
            .input
            .iter()
            .find(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call_output")
                    && item.get("call_id").and_then(Value::as_str) == Some("list-video")
            })
            .and_then(|item| item.get("output"))
            .and_then(Value::as_str)
            .expect("attachment list output");
        assert!(tool_output.contains(".mobius/attachments/"));
    }

    let attachments = workspace.path().join(".mobius/attachments");
    let session = std::fs::read_dir(&attachments)
        .expect("list staged sessions")
        .next()
        .expect("staged session")
        .expect("read staged session")
        .path();
    let attachment_dir = session.join(&attachment.id);
    assert_eq!(
        std::fs::read_dir(&attachment_dir)
            .expect("list staged attachment")
            .count(),
        1
    );
    let staged_file = attachment_dir.join(&attachment.name);
    assert_eq!(
        std::fs::read(&staged_file).expect("read staged video"),
        video
    );
    let blob = std::fs::read_dir(state.path().join("session-files/blobs"))
        .expect("list blobs")
        .next()
        .expect("stored blob")
        .expect("read stored blob")
        .path();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let staged = std::fs::metadata(&staged_file).expect("staged metadata");
        let blob = std::fs::metadata(&blob).expect("blob metadata");
        assert_ne!((staged.dev(), staged.ino()), (blob.dev(), blob.ino()));
    }
    std::fs::write(&staged_file, b"workspace edit").expect("edit staged copy");
    assert_eq!(std::fs::read(&blob).expect("read private blob"), video);

    store
        .delete_session(session_id)
        .await
        .expect("delete session files");
    assert!(!session.exists());
}

#[tokio::test]
async fn materialized_image_keeps_an_exact_prefix_on_later_turns() {
    let workspace = TempDir::new().expect("create workspace");
    let session_id = "stable-image-prefix";
    let store = SessionFileStore::new(workspace.path());
    let attachment = upload_attachment(
        &store,
        session_id,
        "photo.png",
        "image/png",
        b"\x89PNG\r\n\x1a\n",
    )
    .await;
    let model = Arc::new(
        ScriptedModel::new(vec![text_response("first"), text_response("second")])
            .with_image_input(),
    );
    let config = test_config(
        workspace.path(),
        Arc::clone(&model),
        vec![
            Arc::new(Tools::new(Vec::new())),
            Arc::new(Attachments::new(store)),
        ],
    )
    .session_id(session_id);
    let mut agent = create_agent(config).await.expect("create agent");

    agent
        .sender()
        .submit(Op::UserInput {
            text: "inspect".into(),
            attachments: vec![attachment],
        })
        .expect("submit image turn");
    assert_eq!(final_message(&mut agent).await, "first");
    agent
        .sender()
        .submit(Op::UserInput {
            text: "continue".into(),
            attachments: Vec::new(),
        })
        .expect("submit later turn");
    assert_eq!(final_message(&mut agent).await, "second");

    let requests = model.requests.lock().expect("requests");
    assert_eq!(requests.len(), 2);
    assert_eq!(request_image_count(&requests[0].input), 1);
    assert_eq!(request_image_count(&requests[1].input), 1);
    assert_eq!(
        requests[1].input[..requests[0].input.len()],
        requests[0].input
    );
    assert_eq!(
        requests[1]
            .input
            .iter()
            .filter(
                |item| item.get("_mobius_internal").and_then(Value::as_str) == Some("attachments")
            )
            .count(),
        1
    );
}

#[tokio::test]
async fn over_budget_current_image_fails_but_does_not_poison_later_turns() {
    let workspace = TempDir::new().expect("create workspace");
    let session_id = "oversized-attachment";
    let store = SessionFileStore::new(workspace.path());
    let mut oversized_bytes = vec![0_u8; 8 * 1024 * 1024 + 1];
    oversized_bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    let oversized = upload_attachment(
        &store,
        session_id,
        "oversized.png",
        "image/png",
        &oversized_bytes,
    )
    .await;
    let current = upload_attachment(
        &store,
        session_id,
        "current.png",
        "image/png",
        b"\x89PNG\r\n\x1a\n",
    )
    .await;
    let model = Arc::new(
        ScriptedModel::new(vec![
            text_response("text recovered"),
            text_response("image recovered"),
        ])
        .with_image_input(),
    );
    let config = test_config(
        workspace.path(),
        Arc::clone(&model),
        vec![
            Arc::new(Tools::new(Vec::new())),
            Arc::new(Attachments::new(store)),
        ],
    )
    .session_id(session_id);
    let mut agent = create_agent(config).await.expect("create agent");

    agent
        .sender()
        .submit(Op::UserInput {
            text: "too large".into(),
            attachments: vec![oversized.clone()],
        })
        .expect("submit oversized image");
    assert!(failed_turn(&mut agent).await.contains("8 MiB"));
    assert!(model.requests.lock().expect("requests").is_empty());

    agent
        .sender()
        .submit(Op::UserInput {
            text: "continue without it".into(),
            attachments: Vec::new(),
        })
        .expect("submit recovery turn");
    assert_eq!(final_message(&mut agent).await, "text recovered");

    agent
        .sender()
        .submit(Op::UserInput {
            text: "use this smaller image".into(),
            attachments: vec![current.clone()],
        })
        .expect("submit current image");
    assert_eq!(final_message(&mut agent).await, "image recovered");

    let requests = model.requests.lock().expect("requests");
    assert_eq!(request_image_count(&requests[0].input), 0);
    assert_eq!(request_image_count(&requests[1].input), 1);
    let recovery = serde_json::to_string(&requests[1].input).expect("serialize recovery input");
    assert!(recovery.contains(&oversized.id));
    assert!(recovery.contains(&current.id));
    assert!(recovery.contains("Unavailable file references"));
}
