use super::*;

pub(super) fn checkpoint_response(notes: &str) -> ModelOutput {
    tool_response(
        "checkpoint",
        "write_handoff",
        &serde_json::json!({"notes": notes}),
    )
}

fn config_with_store<M: Model + 'static>(
    workspace: &std::path::Path,
    model: Arc<M>,
    checkpoints: Arc<dyn CheckpointStore>,
    session: &str,
) -> AgentConfig {
    AgentConfig::new(
        Arc::new(ModelRouter::new("test", model)),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace).expect("sandbox")),
            ApprovalPolicy::Ask,
        )),
        checkpoints,
        MiddlewareStack::new(vec![
            Arc::new(Messages::default()),
            Arc::new(Compaction::new(1_000).expect("policy")),
        ])
        .expect("middleware"),
        "test system prompt",
    )
    .session_id(SessionId::new(session).unwrap())
    .session_context(test_session_context())
}

#[tokio::test(start_paused = true)]
async fn checkpoint_preparation_retries_are_bounded_and_interruptible() {
    use mobius::backend::model::{ModelTransportSettings, RawModelTransportSettings};
    use std::time::Duration;
    use tokio::time::Instant;

    struct RetryingModel {
        inner: ScriptedModel,
        failures: usize,
        failure_kind: &'static str,
        retry_after: Option<&'static str>,
        attempts: Mutex<Vec<Instant>>,
        fallbacks: AtomicUsize,
        entered: Notify,
    }

    impl Model for RetryingModel {
        fn transport_settings(&self) -> ModelTransportSettings {
            RawModelTransportSettings {
                stream_retry_limit: 1,
                stream_retry_backoff_ms: 10,
                stream_retry_max_backoff_ms: 100,
                ..Default::default()
            }
            .try_into()
            .unwrap()
        }

        fn respond<'a>(
            &'a self,
            request: ModelRequest<'a>,
            events: ModelEventSink,
        ) -> BoxFuture<'a, Result<ModelOutput>> {
            Box::pin(async move {
                if request.tools.len() == 1 && request.tools[0].name == "write_handoff" {
                    assert!(!request.allow_hosted_tools);
                    assert!(!request.allow_continuation);
                    let attempt = {
                        let mut attempts = self.attempts.lock().expect("attempts");
                        attempts.push(Instant::now());
                        attempts.len()
                    };
                    self.entered.notify_one();
                    if attempt <= self.failures {
                        return Err(Error::Provider(match self.failure_kind {
                            "stream" => mobius::ProviderError::stream_interrupted(
                                self.retry_after.map(str::to_owned),
                            ),
                            "retryable" => {
                                mobius::ProviderError::retryable("checkpoint unavailable")
                            }
                            _ => mobius::ProviderError::new("checkpoint unavailable"),
                        }));
                    }
                }
                self.inner.respond(request, events).await
            })
        }

        fn fallback_transport<'a>(&'a self, _: &'a SessionId) -> BoxFuture<'a, Result<bool>> {
            self.fallbacks.fetch_add(1, Ordering::SeqCst);
            // Even a provider that always offers fallback must not create an unbounded retry loop.
            Box::pin(async { Ok(true) })
        }
    }

    for (
        failures,
        failure_kind,
        hint,
        interrupt,
        expected_attempts,
        expected_fallbacks,
        succeeds,
    ) in [
        (1, "stream", Some("3600"), false, 2, 0, true),
        (1, "retryable", None, false, 2, 0, true),
        (2, "stream", None, false, 3, 1, true),
        (9, "stream", None, false, 4, 1, false),
        (1, "permanent", None, false, 1, 0, false),
        (9, "stream", Some("3600"), true, 1, 0, false),
    ] {
        let workspace = TempDir::new().expect("workspace");
        let model = Arc::new(RetryingModel {
            inner: ScriptedModel::new(vec![
                text_response_with_usage("draft", usage(2_000)),
                checkpoint_response("Goal: finish the original task."),
                text_response("done"),
            ]),
            failures,
            failure_kind,
            retry_after: hint,
            attempts: Mutex::new(Vec::new()),
            fallbacks: AtomicUsize::new(0),
            entered: Notify::new(),
        });
        let checkpoints = Arc::new(MemoryCheckpoints::default());
        let mut agent = create_agent(config_with_store(
            workspace.path(),
            Arc::clone(&model),
            Arc::<MemoryCheckpoints>::clone(&checkpoints),
            "checkpoint-retry",
        ))
        .await
        .expect("agent");
        agent
            .sender()
            .submit(user_message("original task"))
            .expect("first input");
        assert_eq!(final_message(&mut agent).await, "draft");
        agent
            .sender()
            .submit(user_message("pending request"))
            .expect("next input");
        if interrupt {
            let turn_id = loop {
                if let EventMsg::TurnStarted(turn) = agent.next_event().await.expect("event").msg {
                    break turn.turn_id;
                }
            };
            model.entered.notified().await;
            agent
                .sender()
                .submit(Op::Interrupt { turn_id })
                .expect("interrupt retry wait");
            loop {
                match agent.next_event().await.expect("interrupt event").msg {
                    EventMsg::TurnAborted(_) => break,
                    EventMsg::TurnComplete(_) => panic!("interrupted preparation completed"),
                    _ => {}
                }
            }
            tokio::time::advance(Duration::from_secs(1)).await;
        } else if succeeds {
            assert_eq!(final_message(&mut agent).await, "done");
        } else {
            failed_turn(&mut agent).await;
        }
        {
            let attempts = model.attempts.lock().expect("attempts");
            assert_eq!(attempts.len(), expected_attempts);
            assert_eq!(model.fallbacks.load(Ordering::SeqCst), expected_fallbacks);
            if hint.is_some() && !interrupt {
                assert_eq!(attempts[1] - attempts[0], Duration::from_millis(100));
            }
            if expected_fallbacks == 1 {
                assert_eq!(attempts[2] - attempts[1], Duration::ZERO);
            }
        }
        if !succeeds {
            let saved = checkpoints
                .load(&SessionId::new("checkpoint-retry").unwrap())
                .await
                .expect("load")
                .expect("saved checkpoint");
            assert_eq!(saved.context_epoch, 0);
            let input = serde_json::to_string(&saved.context).expect("context");
            assert!(input.contains("original task"));
            assert!(input.contains("pending request"));
            assert!(!input.contains("handoff_notes"));
        }
    }
}

#[tokio::test]
async fn automatic_plaintext_checkpoint_survives_recreation_and_keeps_the_prefix() {
    let workspace = TempDir::new().expect("workspace");
    let model = Arc::new(ScriptedModel::new(vec![
        text_response_with_usage("draft", usage(2_000)),
        checkpoint_response("Goal: continue the task. Verified: the first step finished."),
        text_response("done"),
        text_response("resumed"),
    ]));
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoint.sqlite3")).expect("store"),
    );
    let config = || {
        config_with_store(
            workspace.path(),
            Arc::clone(&model),
            Arc::clone(&checkpoints),
            "portable",
        )
    };
    let mut agent = create_agent(config()).await.expect("agent");
    agent.sender().submit(user_message("first")).expect("first");
    assert_eq!(final_message(&mut agent).await, "draft");
    agent
        .sender()
        .submit(user_message("continue exactly"))
        .expect("second");
    assert_eq!(final_message(&mut agent).await, "done");
    let (sender, mut events) = agent.into_parts();
    drop(sender);
    while events.recv().await.is_some() {}
    let mut agent = create_agent(config()).await.expect("resume");
    agent.sender().submit(user_message("next")).expect("next");
    assert_eq!(final_message(&mut agent).await, "resumed");
    let requests = model.requests.lock().expect("requests").clone();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[1].tools.len(), 1);
    assert_eq!(requests[1].tools[0].name, "write_handoff");
    assert!(!requests[1].allow_hosted_tools);
    let fresh = serde_json::to_string(&requests[2].input).expect("fresh");
    assert!(fresh.contains("Goal: continue the task"));
    assert!(fresh.contains("continue exactly"));
    assert!(!fresh.contains("encrypted_content"));
    assert!(
        requests[2]
            .tools
            .iter()
            .all(|tool| tool.name != "new_context")
    );
    assert!(requests[3].input.starts_with(&requests[2].input));
}

#[tokio::test]
async fn checkpoint_model_override_keeps_the_conversation_on_its_selected_route() {
    let workspace = TempDir::new().expect("workspace");
    let conversation = Arc::new(ScriptedModel::new(vec![
        ModelOutput::from_output(
            vec![
                serde_json::json!({"type": "reasoning", "encrypted_content": "private"}),
                serde_json::json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "draft"}]}),
            ],
            true,
            usage(2_000),
        )
        .expect("draft"),
        text_response("done"),
    ]));
    let writer = Arc::new(ScriptedModel::new(vec![checkpoint_response(
        "Goal: finish the original task. Progress: draft prepared.",
    )]));
    let mut router = ModelRouter::new("test", Arc::<ScriptedModel>::clone(&conversation));
    router
        .register("checkpoint", Arc::<ScriptedModel>::clone(&writer))
        .expect("route");
    let settings = BTreeMap::from([
        (
            "model_route",
            mobius::protocol::FrontendSettingValue::String("checkpoint".into()),
        ),
        (
            "at_tokens",
            mobius::protocol::FrontendSettingValue::Integer(1_000),
        ),
    ]);
    let config = AgentConfig::new(
        Arc::new(router),
        Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("sandbox")),
            ApprovalPolicy::Ask,
        )),
        Arc::new(SqliteCheckpoint::new(workspace.path().join("state.sqlite3")).expect("store")),
        MiddlewareStack::new(vec![
            Arc::new(Messages::default()),
            Arc::new(Compaction::from_settings(|id| settings.get(id)).expect("policy")),
        ])
        .expect("middleware"),
        "test system prompt",
    )
    .session_id(SessionId::new("separate-writer").unwrap())
    .session_context(test_session_context());
    let mut agent = create_agent(config).await.expect("agent");
    agent
        .sender()
        .submit(user_message("original task"))
        .expect("first");
    assert_eq!(final_message(&mut agent).await, "draft");
    agent
        .sender()
        .submit(user_message("continue"))
        .expect("next");
    assert_eq!(final_message(&mut agent).await, "done");
    let requests = conversation
        .requests
        .lock()
        .expect("conversation requests")
        .clone();
    assert_eq!(requests.len(), 2);
    assert!(
        serde_json::to_string(&requests[1].input)
            .expect("input")
            .contains("draft prepared")
    );
    let summaries = writer.requests.lock().expect("checkpoint requests").clone();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].tools.len(), 1);
    assert_eq!(summaries[0].tools[0].name, "write_handoff");
    assert!(
        !serde_json::to_string(&summaries[0].input)
            .expect("writer input")
            .contains("encrypted_content")
    );
}

#[tokio::test]
async fn invalid_checkpoint_preserves_original_and_pending_input() {
    let workspace = TempDir::new().expect("workspace");
    let model = Arc::new(ScriptedModel::new(vec![
        text_response_with_usage("first result", usage(2_000)),
        text_response("I answered instead of saving notes"),
    ]));
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoint.sqlite3")).expect("store"),
    );
    let mut agent = create_agent(config_with_store(
        workspace.path(),
        Arc::clone(&model),
        Arc::clone(&checkpoints),
        "failed-checkpoint",
    ))
    .await
    .expect("agent");
    agent
        .sender()
        .submit(user_message("original task"))
        .expect("original");
    assert_eq!(final_message(&mut agent).await, "first result");
    agent
        .sender()
        .submit(user_message("pending request"))
        .expect("pending");
    assert!(failed_turn(&mut agent).await.contains("one write_handoff"));
    let checkpoints =
        SqliteCheckpoint::new(workspace.path().join("checkpoint.sqlite3")).expect("store");
    let saved = checkpoints
        .load(&SessionId::new("failed-checkpoint").unwrap())
        .await
        .expect("load")
        .expect("saved");
    let input = serde_json::to_string(&saved.context).expect("input");
    assert!(input.contains("original task"));
    assert!(input.contains("pending request"));
    assert!(input.contains("first result"));
    assert_eq!(saved.context_epoch, 0);
}

#[tokio::test]
async fn oversized_history_summarizes_a_complete_prefix_and_preserves_parallel_results() {
    let workspace = TempDir::new().expect("workspace");
    let model = Arc::new(ScriptedModel::new(vec![
        checkpoint_response("old task completed"),
        text_response("continued"),
    ]));
    let checkpoints = Arc::new(MemoryCheckpoints::default());
    let mut checkpoint = Checkpoint::empty(SessionId::new("over-budget").unwrap());
    checkpoint.session_context = test_session_context().into();
    checkpoint.model_route = Some("test".into());
    checkpoint.context_model_route = Some("test".into());
    let first_result = "parallel-a ".repeat(350);
    let second_result = "parallel-b ".repeat(350);
    checkpoint.context = Arc::new(vec![
        mobius::backend::model::user_message(&"old context ".repeat(2_000)),
        serde_json::json!({"type":"function_call", "call_id":"a", "name":"read_file", "arguments":"{}"}),
        serde_json::json!({"type":"function_call", "call_id":"b", "name":"read_file", "arguments":"{}"}),
        mobius::backend::model::tool_output("a", &first_result.as_str().into(), false),
        mobius::backend::model::tool_output("b", &second_result.as_str().into(), false),
    ].into_iter().map(Arc::new).collect());
    checkpoints
        .sessions
        .lock()
        .expect("sessions")
        .insert(checkpoint.session_id.share(), checkpoint);
    let mut agent = create_agent(
        config_with_store(
            workspace.path(),
            Arc::clone(&model),
            checkpoints,
            "over-budget",
        )
        .context_window(8_000)
        .initial_replay_batches(0),
    )
    .await
    .expect("agent");
    agent
        .sender()
        .submit(user_message("continue"))
        .expect("input");
    assert_eq!(final_message(&mut agent).await, "continued");
    let requests = model.requests.lock().expect("requests").clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools.len(), 1);
    assert!(!requests[0].input.iter().any(|item| item["call_id"] == "a"));
    let output = serde_json::to_string(&requests[1].input).expect("request");
    assert!(output.contains(&first_result));
    assert!(output.contains(&second_result));
}
