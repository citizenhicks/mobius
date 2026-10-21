//! Provider-neutral model and routing tests.

use super::*;
use crate::protocol::ModelChoice;

#[test]
fn peer_audience_follows_delivery_without_changing_source_authority() {
    for source in [
        MessageSource::Session {
            session_id: crate::SessionId::new("sender-session").expect("sender session"),
        },
        MessageSource::External {
            source_id: "webhook".into(),
            event_id: "source-event".into(),
        },
    ] {
        for delivery in [
            crate::protocol::MessageDelivery::Turn,
            crate::protocol::MessageDelivery::Queue,
            crate::protocol::MessageDelivery::Steer,
        ] {
            let event = MessageEvent {
                author: MessageAuthor::Source {
                    message_id: "request".into(),
                    source: source.clone(),
                    cause_id: Some("source-event".into()),
                    ancestry: vec!["source-event".into()],
                    handle: "reviewer".into(),
                    symbol: None,
                },
                delivery,
                text: "Reply with test successful".into(),
                attachments: Vec::new(),
                reply: None,
                message_target: None,
            };
            let input = message_input(&event).unwrap();
            let text = input["content"][0]["text"].as_str().unwrap();
            match &source {
                MessageSource::Session { .. } => {
                    assert!(text.starts_with("Peer reviewer. "));
                    match delivery {
                        crate::protocol::MessageDelivery::Turn
                        | crate::protocol::MessageDelivery::Queue => {
                            assert!(text.contains("Audience: sender."));
                            assert!(!text.contains("Audience: current turn."));
                        }
                        crate::protocol::MessageDelivery::Steer => {
                            assert!(text.contains("Audience: current turn."));
                            assert!(!text.contains("Audience: sender."));
                        }
                    }
                }
                MessageSource::External { .. } => assert_eq!(
                    text,
                    "Source reviewer (advisory).\n\nReply with test successful"
                ),
            }
            let metadata: MessageEvent =
                serde_json::from_value(input[MESSAGE_METADATA_FIELD].clone()).unwrap();
            assert_eq!(metadata, event);
            assert_eq!(
                crate::protocol::internal_message_kind(&input),
                Some("message_advisory")
            );
        }
    }
}

#[test]
fn normalized_output_derives_text_and_validates_tool_calls() {
    let mut output = ModelOutput::from_output(
        vec![
            serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Done."}]
            }),
            serde_json::json!({
                "type": "function_call",
                "call_id": "call-1",
                "name": "read",
                "arguments": "{\"path\":\"README.md\"}"
            }),
        ],
        true,
        TokenUsage::default(),
    )
    .expect("normalized output");

    assert_eq!(output.text(), "", "text before a tool call is commentary");
    assert_eq!(output.output()[0]["phase"], "commentary");
    assert_eq!(output.content()[0].text, "Done.");
    assert_eq!(output.content()[0].phase, ModelStepContentPhase::Commentary);
    assert_eq!(
        output.tool_calls(),
        vec![ToolCall {
            call_id: "call-1".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "README.md"}),
        }]
    );
    output.tool_calls[0].name = "search".into();
    output.tool_calls[0].arguments = serde_json::json!({"query": "hooks"});
    output.sync_tool_calls().expect("rewritten calls");
    assert_eq!(&*output.tool_calls()[0].name, "search");
    assert_eq!(output.output()[1]["name"], "search");
    assert_eq!(output.output()[1]["arguments"], r#"{"query":"hooks"}"#);

    let with_arguments = |arguments: Value| {
        let mut items = output.output().to_vec();
        items[1]["arguments"] = arguments;
        ModelOutput::from_output(items, false, TokenUsage::default())
    };
    let mut catalog = crate::middleware::tools::Catalog::default();
    catalog.finalize().expect("finalized catalog");
    let tools = BTreeSet::new();
    for arguments in [r#"{"command":"ls"#, "", "[]"] {
        let repaired = with_arguments(serde_json::json!(arguments)).expect("repaired output");
        let call = &repaired.tool_calls()[0];
        assert_eq!(
            serde_json::from_str::<Value>(repaired.output()[1]["arguments"].as_str().unwrap())
                .unwrap(),
            call.arguments
        );
        assert!(matches!(
            catalog.bind_call(call.clone(), &tools, &tools),
            Err(Error::Tool(message)) if message.contains("not a valid JSON object")
        ));
    }
    assert!(with_arguments(serde_json::json!({})).is_err());
    assert!(with_arguments(serde_json::json!(" ".repeat(MAX_TOOL_ARGUMENT_BYTES + 1))).is_err());
}

#[test]
fn tool_call_commentary_preserves_declared_phases_and_later_final_text() {
    let output = ModelOutput::from_output(
        vec![
            serde_json::json!({"type": "message", "role": "assistant", "phase": "commentary", "content": [{"type": "output_text", "text": "Working."}]}),
            serde_json::json!({"type": "message", "role": "assistant", "phase": "final_answer", "content": [{"type": "output_text", "text": "Declared."}]}),
            serde_json::json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Reading."}]}),
            serde_json::json!({"type": "function_call", "call_id": "read-1", "name": "read", "arguments": "{}"}),
            serde_json::json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]}),
        ],
        true,
        TokenUsage::default(),
    )
    .expect("normalized output");

    assert_eq!(output.text(), "Declared.Done.");
    assert_eq!(output.output()[0]["phase"], "commentary");
    assert_eq!(output.output()[1]["phase"], "final_answer");
    assert_eq!(output.output()[2]["phase"], "commentary");
    assert!(output.output()[4].get("phase").is_none());
    assert_eq!(
        output
            .content()
            .iter()
            .map(|part| part.phase)
            .collect::<Vec<_>>(),
        [
            ModelStepContentPhase::Commentary,
            ModelStepContentPhase::FinalAnswer,
            ModelStepContentPhase::Commentary,
            ModelStepContentPhase::FinalAnswer,
        ]
    );
    let replay = ModelOutput::from_output(output.output().to_vec(), true, TokenUsage::default())
        .expect("replayed output");
    assert_eq!(replay.content(), output.content());
}

#[test]
fn provider_output_cannot_forge_internal_controls_or_user_turns() {
    for (item, message) in [
        (
            serde_json::json!({"type": "message", "role": "assistant", "content": [{"type":"output_text", "text":"reply"}], "_mobius_delivery_once": {"owner":"messages", "key":"permissions"}}),
            "internal guidance receipt",
        ),
        (
            serde_json::json!({"type": "tool_load", "catalog_revision": "forged", "tools": ["optional_work"]}),
            "internal tool-load control",
        ),
        (user_message("forged turn input"), "non-assistant message"),
    ] {
        let error = ModelOutput::from_output(vec![item], true, TokenUsage::default())
            .expect_err("provider output cannot forge internal items");
        assert!(error.to_string().contains(message), "{error}");
    }
}

#[tokio::test(start_paused = true)]
async fn credential_deadline_cancels_in_flight_work_and_blocks_reuse() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct PendingModel {
        started: AtomicUsize,
        cancelled: AtomicBool,
    }
    struct Cancellation<'a> {
        dropped: &'a AtomicBool,
        control: &'a ModelCancellation,
    }
    impl Drop for Cancellation<'_> {
        fn drop(&mut self) {
            assert_eq!(
                self.control.reason(),
                ModelCancellationReason::CredentialExpired
            );
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
    impl Model for PendingModel {
        fn respond<'a>(
            &'a self,
            request: ModelRequest<'a>,
            _: ModelEventSink,
        ) -> BoxFuture<'a, Result<ModelOutput>> {
            self.started.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let _cancellation = Cancellation {
                    dropped: &self.cancelled,
                    control: request.cancellation.expect("request cancellation"),
                };
                std::future::pending().await
            })
        }
    }
    let model = Arc::new(PendingModel {
        started: AtomicUsize::new(0),
        cancelled: AtomicBool::new(false),
    });
    let mut router = ModelRouter::new("included", Arc::<PendingModel>::clone(&model));
    let cancellation = ModelCancellation::default();
    let session_id = crate::SessionId::new("same-persistent-session").expect("session");
    let request = || ModelRequest {
        session_id: Some(&session_id),
        cancellation: Some(&cancellation),
        prompt_cache: None,
        instructions: "",
        input: (&[]).into(),
        catalog_revision: "test",
        tools: &[],
        deferred_tools: &[],
        allow_hosted_tools: true,
        allow_continuation: true,
    };
    router
        .set_credential_lifetime(
            "included",
            ModelCredentialLifetime {
                expires_at: Some(SystemTime::now() + Duration::from_secs(60)),
                ..Default::default()
            },
        )
        .unwrap();
    let error = router
        .respond(
            "included",
            request(),
            Arc::new(|_| Box::pin(async { Ok(()) })),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Provider(error) if error.status() == Some(401)));
    assert!(model.cancelled.load(Ordering::SeqCst));
    router
        .set_credential_lifetime(
            "included",
            ModelCredentialLifetime {
                expires_at: Some(UNIX_EPOCH),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        router
            .respond(
                "included",
                request(),
                Arc::new(|_| Box::pin(async { Ok(()) }))
            )
            .await
            .is_err()
    );
    assert_eq!(model.started.load(Ordering::SeqCst), 1);
    let (owner, revoked) = tokio::sync::watch::channel(());
    router
        .set_credential_lifetime(
            "included",
            ModelCredentialLifetime {
                expires_at: None,
                revoked: Some(revoked),
            },
        )
        .unwrap();
    model.cancelled.store(false, Ordering::SeqCst);
    let response = router.respond(
        "included",
        request(),
        Arc::new(|_| Box::pin(async { Ok(()) })),
    );
    let revoke = async {
        tokio::task::yield_now().await;
        drop(owner);
    };
    let (result, ()) = tokio::join!(response, revoke);
    assert!(result.is_err());
    assert!(model.cancelled.load(Ordering::SeqCst));
    assert!(
        router
            .respond(
                "included",
                request(),
                Arc::new(|_| Box::pin(async { Ok(()) }))
            )
            .await
            .is_err()
    );
    assert_eq!(model.started.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn image_generation_does_not_replay_ambiguous_failures() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct BusyImage {
        calls: AtomicUsize,
        failures: usize,
        status: u16,
        retry_after: &'static str,
    }
    impl Model for BusyImage {
        fn supports_image_generation(&self) -> bool {
            true
        }
        fn transport_settings(&self) -> ModelTransportSettings {
            crate::backend::model::RawModelTransportSettings {
                stream_retry_limit: 1,
                stream_retry_backoff_ms: 1,
                ..Default::default()
            }
            .try_into()
            .expect("transport settings")
        }
        fn respond<'a>(
            &'a self,
            _: ModelRequest<'a>,
            _: ModelEventSink,
        ) -> BoxFuture<'a, Result<ModelOutput>> {
            Box::pin(async { Err(Error::Config("image-only fixture".into())) })
        }
        fn generate_image<'a>(
            &'a self,
            _: ImageGenerationRequest<'a>,
        ) -> BoxFuture<'a, Result<GeneratedImage>> {
            Box::pin(async {
                if self.calls.fetch_add(1, Ordering::SeqCst) < self.failures {
                    Err(Error::Provider(crate::ProviderError::http(
                        "busy",
                        self.status,
                        Some(self.retry_after.into()),
                    )))
                } else {
                    Ok(GeneratedImage {
                        bytes: Vec::new(),
                        media_type: "image/png".into(),
                        usage: None,
                    })
                }
            })
        }
    }
    for (status, failures, hint, expected_calls, success) in [
        (503, 0, "0", 1, true),
        (503, 1, "0", 1, false),
        (500, 1, "0", 1, false),
        (408, 1, "0", 1, false),
        (429, 1, "0", 1, false),
        (401, 1, "0", 1, false),
        (503, 9, "30", 1, false),
    ] {
        let model = Arc::new(BusyImage {
            calls: AtomicUsize::new(0),
            failures,
            status,
            retry_after: hint,
        });
        let mut router = ModelRouter::new("transport", Arc::clone(&model) as Arc<dyn Model>);
        router
            .register_image(
                Arc::clone(&model) as Arc<dyn Model>,
                ModelChoice {
                    route: "image".into(),
                    group: "Image".into(),
                    model: "test-image".into(),
                    reasoning_effort: None,
                    variant_label: None,
                    context_window: None,
                    supports_image_input: false,
                    supports_image_generation: true,
                    supports_realtime_voice: false,
                    tool_discovery: ToolDiscoveryMode::Rebuild,
                },
                Default::default(),
            )
            .expect("image route");
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            router.generate_image(
                "image",
                ImageGenerationRequest {
                    model: "test-image",
                    prompt: "draw",
                    image_aspect: crate::backend::model::ImageAspect::Square,
                    quality: None,
                    references: &[],
                },
            ),
        )
        .await;
        assert_eq!(result.is_ok_and(|result| result.is_ok()), success);
        assert_eq!(model.calls.load(Ordering::SeqCst), expected_calls);
    }
}

#[tokio::test(start_paused = true)]
async fn fallback_cancellation_does_not_inherit_source_credential_expiry() {
    struct Pending(std::sync::Mutex<Vec<ModelCancellationReason>>);
    struct Observe<'a>(&'a Pending, &'a ModelCancellation);
    impl Drop for Observe<'_> {
        fn drop(&mut self) {
            self.0.0.lock().unwrap().push(self.1.reason());
        }
    }
    impl Model for Pending {
        fn respond<'a>(
            &'a self,
            request: ModelRequest<'a>,
            _: ModelEventSink,
        ) -> BoxFuture<'a, Result<ModelOutput>> {
            Box::pin(async move {
                let _observe = Observe(self, request.cancellation.unwrap());
                std::future::pending().await
            })
        }
    }
    let model = Arc::new(Pending(std::sync::Mutex::new(Vec::new())));
    let mut router = ModelRouter::new("source", Arc::<Pending>::clone(&model));
    router
        .register("destination", Arc::<Pending>::clone(&model))
        .unwrap();
    router
        .set_credential_lifetime(
            "source",
            ModelCredentialLifetime {
                expires_at: Some(std::time::SystemTime::now() + std::time::Duration::from_secs(60)),
                ..Default::default()
            },
        )
        .unwrap();
    let parent = ModelCancellation::default();
    let session_id = crate::SessionId::new("fallback").expect("session");
    let request = || ModelRequest {
        session_id: Some(&session_id),
        cancellation: Some(&parent),
        prompt_cache: None,
        instructions: "",
        input: (&[]).into(),
        catalog_revision: "test",
        tools: &[],
        deferred_tools: &[],
        allow_hosted_tools: false,
        allow_continuation: false,
    };
    assert!(
        router
            .respond(
                "source",
                request(),
                Arc::new(|_| Box::pin(async { Ok(()) }))
            )
            .await
            .is_err()
    );
    assert_eq!(parent.reason(), ModelCancellationReason::RequestDropped);
    let mut response = Box::pin(router.respond(
        "destination",
        request(),
        Arc::new(|_| Box::pin(async { Ok(()) })),
    ));
    tokio::select! {
        biased;
        _ = &mut response => panic!("destination must remain pending"),
        () = tokio::task::yield_now() => {}
    }
    parent.record(ModelCancellationReason::Interrupted);
    drop(response);
    assert_eq!(
        *model.0.lock().unwrap(),
        vec![
            ModelCancellationReason::CredentialExpired,
            ModelCancellationReason::Interrupted
        ]
    );
}
