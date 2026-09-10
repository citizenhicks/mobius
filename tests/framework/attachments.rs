use super::*;
use base64::Engine as _;

pub(super) fn png() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").expect("PNG")
}

#[tokio::test]
async fn attachment_hydration_runs_after_native_compaction_replaces_context() {
    let workspace = TempDir::new().expect("create workspace");
    let session_id = "attachment-compaction";
    let store = SessionFileStore::new(workspace.path());
    let attachment = upload_attachment(&store, session_id, "photo.png", "image/png", &png()).await;
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
        .submit(user_message("first"))
        .expect("submit first turn");
    assert_eq!(final_message(&mut agent).await, "draft");
    agent
        .sender()
        .submit(user_message_with_attachments(
            "inspect",
            vec![attachment.clone()],
        ))
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
    assert_eq!(
        request_image_count(&model.compact_requests.lock().expect("compact requests")[0].input),
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
        .submit(user_message_with_attachments(
            "read it",
            vec![attachment.clone()],
        ))
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
            .and_then(|output| output.get(0))
            .and_then(|part| part.get("text"))
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
    let attachment = upload_attachment(&store, session_id, "photo.png", "image/png", &png()).await;
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
        .submit(user_message_with_attachments("inspect", vec![attachment]))
        .expect("submit image turn");
    assert_eq!(final_message(&mut agent).await, "first");
    agent
        .sender()
        .submit(user_message("continue"))
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
async fn malformed_current_image_fails_but_does_not_poison_later_turns() {
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
    let current = upload_attachment(&store, session_id, "current.png", "image/png", &png()).await;
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
        .submit(user_message_with_attachments(
            "too large",
            vec![oversized.clone()],
        ))
        .expect("submit oversized image");
    assert!(
        failed_turn(&mut agent)
            .await
            .contains("invalid or unsupported image")
    );
    assert!(model.requests.lock().expect("requests").is_empty());

    agent
        .sender()
        .submit(user_message("continue without it"))
        .expect("submit recovery turn");
    assert_eq!(final_message(&mut agent).await, "text recovered");

    agent
        .sender()
        .submit(user_message_with_attachments(
            "use this smaller image",
            vec![current.clone()],
        ))
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

#[tokio::test]
async fn request_budget_failure_records_the_action_without_replaying_on_restart() {
    use mobius::backend::model::ImageInputLimits;
    use mobius::middleware::tools::{Tool, ToolContext};
    use mobius::protocol::{ContentPart, ImageDetail, ToolContent, ToolResponse};
    struct Capture {
        content: ToolContent,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Tool for Capture {
        fn exposure(&self) -> mobius::middleware::tools::ToolExposure {
            mobius::middleware::tools::ToolExposure::Direct
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "capture".into(),
                description: "Capture once".into(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            }
        }
        fn call<'a>(&'a self, _: ToolContext, _: Value) -> BoxFuture<'a, Result<ToolResponse>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(ToolResponse {
                    content: self.content.clone(),
                    is_error: false,
                })
            })
        }
    }
    let workspace = TempDir::new().expect("workspace");
    let files = SessionFileStore::new(workspace.path());
    let image = files
        .ingest_image("budget", "screen.png".into(), png(), ImageDetail::High)
        .await
        .expect("image");
    let content = ToolContent(vec![
        ContentPart::Image {
            image: image.clone(),
        },
        ContentPart::Image { image },
    ]);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let model = Arc::new(
        ScriptedModel::new(vec![
            tool_response("action", "capture", serde_json::json!({})),
            text_response("done"),
        ])
        .with_image_input(),
    );
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("history.sqlite")).expect("checkpoints"),
    );
    let config = |max_images| {
        let route: Arc<dyn Model> = model.clone();
        AgentConfig::new(
            Arc::new(
                ModelRouter::new("test", route)
                    .session_files(files.clone())
                    .image_input_limits(ImageInputLimits {
                        max_images,
                        ..ImageInputLimits::default()
                    })
                    .expect("limits"),
            ),
            Arc::new(Sandbox::new(
                Arc::new(LocalSandbox::new(workspace.path()).expect("sandbox")),
                ApprovalPolicy::Ask,
            )),
            checkpoints.clone(),
            MiddlewareStack::new(vec![
                Arc::new(Messages::default()),
                Arc::new(Tools::new(vec![Arc::new(Capture {
                    content: content.clone(),
                    calls: calls.clone(),
                })])),
            ])
            .expect("middleware"),
            "test",
        )
        .session_id("budget")
        .session_context(test_session_context())
    };
    let mut first = create_agent(config(1)).await.expect("agent");
    first
        .sender()
        .submit(user_message("capture"))
        .expect("message");
    loop {
        if let EventMsg::Error(error) = first.next_event().await.expect("event").msg {
            assert!(
                error.message.contains("request budget"),
                "{}",
                error.message
            );
            break;
        }
    }
    let saved = checkpoints
        .load("budget")
        .await
        .expect("load")
        .expect("checkpoint");
    let result = saved
        .context
        .iter()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
        .expect("recorded result");
    assert_eq!(
        result["output"],
        serde_json::to_value(&content).expect("content")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let (sender, mut events) = first.into_parts();
    drop(sender);
    while events.recv().await.is_some() {}
    let mut restarted = create_agent(config(2)).await.expect("restart");
    restarted
        .sender()
        .submit(user_message("continue"))
        .expect("message");
    assert_eq!(final_message(&mut restarted).await, "done");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
