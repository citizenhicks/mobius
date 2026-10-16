use super::*;
use base64::Engine as _;

pub(super) fn png() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").expect("PNG")
}

#[tokio::test]
async fn attachment_hydration_survives_plaintext_checkpoint_reset() {
    let workspace = TempDir::new().expect("create workspace");
    let session_id = "attachment-compaction";
    let store = SessionFileStore::new(workspace.path(), None);
    let attachment = upload_attachment(&store, session_id, "photo.png", "image/png", &png()).await;
    let model = Arc::new(
        ScriptedModel::new(vec![
            text_response_with_usage("draft", usage(2_000)),
            super::compaction::checkpoint_response("Checkpoint: inspect the uploaded photo."),
            text_response("done"),
        ])
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

    let requests = model.requests.lock().expect("requests");
    assert_eq!(requests.len(), 3);
    assert_eq!(request_image_count(&requests[1].input), 1);
    assert_eq!(request_image_count(&requests[2].input), 1);
    let final_input = serde_json::to_string(&requests[2].input).expect("serialize final input");
    assert!(final_input.contains("Checkpoint: inspect the uploaded photo"));
    assert!(final_input.contains(&attachment.id));
}

#[tokio::test]
async fn malformed_current_image_fails_but_does_not_poison_later_turns() {
    let workspace = TempDir::new().expect("create workspace");
    let session_id = "oversized-attachment";
    let store = SessionFileStore::new(workspace.path(), None);
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
    assert!(
        !recovery.contains("Unavailable file references"),
        "failed preparation stays provisional"
    );
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
    let files = SessionFileStore::new(workspace.path(), None);
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
