use super::*;
use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
use crate::protocol::Op;

fn scratchpad(store: &Scratchpad) -> Scratchpad {
    store.for_chat()
}

fn management(arguments: impl Into<String>, input: Option<&str>) -> Op {
    Op::CapabilityCommand {
        capability: MANIFEST.id.into(),
        command: "scratchpad".into(),
        arguments: arguments.into(),
        input: input.map(str::to_owned),
        target: None,
    }
}

impl Scratchpad {
    async fn add_global(&self, note: &str) -> Result<FrontendContribution> {
        self.manage(&management("add", Some(note))).await
    }

    async fn edit_global(&self, id: &str, note: &str) -> Result<FrontendContribution> {
        self.manage(&management(format!("edit {id}"), Some(note)))
            .await
    }
}

fn session_context() -> crate::protocol::SessionContext {
    crate::protocol::SessionContext {
        owner_id: "test-bot".into(),
        ..crate::protocol::SessionContext::default()
    }
}

fn entry(note: impl Into<String>) -> Entry {
    Entry {
        id: Uuid::new_v4().to_string(),
        note: note.into(),
        basis: Basis::AgentObservation,
        created_at: "1".into(),
    }
}

fn store() -> (tempfile::TempDir, Scratchpad) {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(temporary.path().join("checkpoints.sqlite3")).expect("checkpoints"),
    );
    (temporary, Scratchpad::new(checkpoints))
}

#[tokio::test]
async fn compaction_discards_projections_before_a_post_hook_stops_or_fails() {
    use crate::agent::{AgentConfig, create_agent};
    use crate::backend::model::{Model, ModelEventSink, ModelOutput, ModelRequest, ModelRouter};
    use crate::backend::sandbox::{ApprovalPolicy, Sandbox, local::LocalSandbox};
    use crate::middleware::{
        CompactContext, MiddlewareStack, compaction::Compaction, messages::Messages, tools::Tools,
    };
    use crate::protocol::MessageSubmission;

    struct CheckpointWriter(std::sync::atomic::AtomicBool);
    impl Model for CheckpointWriter {
        fn respond<'a>(
            &'a self,
            request: ModelRequest<'a>,
            _events: ModelEventSink,
        ) -> BoxFuture<'a, Result<ModelOutput>> {
            Box::pin(async move {
                if matches!(request.tools, [tool] if tool.name == "write_handoff") {
                    assert!(!self.0.swap(true, std::sync::atomic::Ordering::Relaxed));
                    assert!(request.input.iter().any(is_projection_item));
                    ModelOutput::from_output(
                        vec![serde_json::json!({
                            "type": "function_call",
                            "call_id": "checkpoint",
                            "name": "write_handoff",
                            "arguments": "{\"notes\":\"Goal: continue the active task.\"}"
                        })],
                        false,
                        Default::default(),
                    )
                } else {
                    ModelOutput::from_output(Vec::new(), true, Default::default())
                }
            })
        }
    }

    struct StopAfterCompact(bool);
    impl Middleware for StopAfterCompact {
        fn name(&self) -> &'static str {
            "stop_after_compact"
        }

        fn post_compact<'a>(
            &'a self,
            context: &'a mut CompactContext<'_>,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                assert!(!context.input.iter().any(is_projection_item));
                if self.0 {
                    Err(Error::Config("post-compact failure".into()))
                } else {
                    context.stop("post-compact stop")
                }
            })
        }
    }

    for fail in [false, true] {
        let (temporary, store) = store();
        store.add_global("remember this").await.expect("note");
        let middleware = MiddlewareStack::new(vec![
            Arc::new(Messages::default()),
            Arc::new(Tools::new(Vec::new())),
            Arc::new(StopAfterCompact(fail)),
            Arc::new(scratchpad(&store)),
            Arc::new(Compaction::new(1).expect("compaction")),
        ])
        .expect("middleware");
        let mut agent = create_agent(
            AgentConfig::new(
                Arc::new(ModelRouter::new(
                    "model",
                    Arc::new(CheckpointWriter(std::sync::atomic::AtomicBool::new(false))),
                )),
                Arc::new(Sandbox::new(
                    Arc::new(LocalSandbox::new(temporary.path()).expect("sandbox")),
                    ApprovalPolicy::Ask,
                )),
                Arc::clone(&store.store.checkpoints),
                middleware,
                "test",
            )
            .session_id("session")
            .session_context(session_context()),
        )
        .await
        .expect("agent");
        agent
            .sender()
            .submit(Op::Message {
                message: MessageSubmission {
                    author: crate::protocol::MessageAuthor::User,
                    text: "hello".into(),
                    attachments: Vec::new(),
                    reply: None,
                    requested_delivery: None,
                    target_turn_id: None,
                },
            })
            .expect("submit");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if matches!(
                    agent.next_event().await.expect("event").msg,
                    EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
                ) {
                    break;
                }
            }
        })
        .await
        .expect("terminal event");
        let checkpoint = store
            .store
            .checkpoints
            .load("session")
            .await
            .expect("load")
            .expect("checkpoint");
        assert_eq!(checkpoint.context_epoch, u64::from(!fail));
        assert_eq!(
            checkpoint
                .context
                .iter()
                .any(|item| is_projection_item(item)),
            fail
        );
    }
}

#[tokio::test]
async fn shared_scope_budget_rejects_oversized_writes_and_edits_without_changing_state() {
    let (_temporary, store) = store();
    for i in 0..3 {
        store
            .add_global(&format!("{i}{}", "x".repeat(499)))
            .await
            .expect("bounded note");
    }
    store.add_global("short").await.expect("small note");
    let before = store.snapshot().await.expect("before");
    assert!(store.add_global(&"y".repeat(500)).await.is_err());
    assert!(
        store
            .edit_global(&before[3].id, &"z".repeat(500))
            .await
            .is_err()
    );
    assert_eq!(store.snapshot().await.expect("unchanged"), before);
    assert!(store.add_global("").await.is_err());
    assert!(store.add_global(&"é".repeat(251)).await.is_err());
    let escaped = vec![entry(format!("a{}b", "\u{0001}".repeat(400)))];
    assert!(
        validate_scope_budget(&escaped).is_err(),
        "JSON escaping counts toward the visible budget"
    );
}

#[tokio::test]
async fn concurrent_shared_writes_preserve_every_accepted_note_and_the_count_limit() {
    let (_temporary, store) = store();
    let store = Arc::new(store);
    let writes = (0..MAX_NOTES + 5)
        .map(|i| {
            let store = Arc::clone(&store);
            tokio::spawn(async move { store.add_global(&format!("note {i}")).await })
        })
        .collect::<Vec<_>>();
    let mut accepted = 0;
    for write in writes {
        if write.await.expect("write task").is_ok() {
            accepted += 1;
        }
    }
    assert_eq!(accepted, MAX_NOTES);
    assert_eq!(store.snapshot().await.expect("snapshot").len(), MAX_NOTES);
}
