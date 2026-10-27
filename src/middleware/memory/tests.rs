use super::*;
use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
use crate::backend::sandbox::local::LocalSandbox;
use crate::backend::sandbox::{
    ApprovalPolicy, NetworkAccess, Sandbox, SandboxMode, SandboxPermissions,
};
use crate::middleware::tools::{Tool, ToolContext};
use crate::protocol::Op;

fn management(arguments: impl Into<String>, input: Option<&str>) -> Op {
    Op::CapabilityCommand {
        capability: MANIFEST.id.into(),
        command: "memory".into(),
        arguments: arguments.into(),
        input: input.map(str::to_owned),
        target: None,
    }
}

impl Memory {
    async fn add_global(&self, memory: &str) -> Result<FrontendContribution> {
        self.manage(&management("add", Some(memory))).await
    }

    async fn edit_global(&self, id: &str, memory: &str) -> Result<FrontendContribution> {
        self.manage(&management(format!("edit {id}"), Some(memory)))
            .await
    }

    async fn snapshot(&self) -> Result<Vec<Entry>> {
        Ok(self.store.load(None).await?.memories)
    }
}

fn session_context() -> crate::protocol::SessionContext {
    crate::protocol::SessionContext {
        owner_id: "test-bot".into(),
        ..crate::protocol::SessionContext::default()
    }
}

fn owner(bot: &str, project: Option<&str>) -> Arc<MemoryOwner> {
    Arc::new(MemoryOwner::of(&crate::protocol::SessionContext {
        owner_id: bot.into(),
        workspace_id: project.map(str::to_owned),
        workspace_label: project.map(|project| format!("/projects/{project}")),
        ..crate::protocol::SessionContext::default()
    }))
}

fn entry(memory: impl Into<String>) -> Entry {
    Entry {
        id: Uuid::new_v4().to_string(),
        text: memory.into(),
        basis: Basis::AgentObservation,
        updated_at: 1,
        author: None,
    }
}

fn store() -> (tempfile::TempDir, Memory) {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(temporary.path().join("checkpoints.sqlite3")).expect("checkpoints"),
    );
    (temporary, Memory::new(checkpoints))
}

fn tool_context(path: &std::path::Path) -> ToolContext {
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(LocalSandbox::new(path).expect("sandbox")),
        ApprovalPolicy::Ask,
    ));
    let permissions = SandboxPermissions::restore(
        &"session".parse().expect("session ID"),
        SandboxMode::WorkspaceWrite,
        NetworkAccess::Allowed,
        ["memory-call".into()],
    );
    ToolContext::new(sandbox, permissions.for_call("memory-call"), "turn")
}

fn writer(memory: &Memory, owner: &Arc<MemoryOwner>) -> WriteMemory {
    WriteMemory {
        store: Arc::clone(&memory.store),
        owner: Arc::clone(owner),
        frontend: Arc::new(|_| Ok(())),
    }
}

fn visible_texts(partitions: &[Partition; 2]) -> [Vec<&str>; 2] {
    partitions.each_ref().map(|partition| {
        partition
            .memories
            .iter()
            .map(|entry| entry.text.as_str())
            .collect()
    })
}

#[tokio::test]
async fn a_project_label_change_keeps_the_same_memory_scope() {
    let (_temporary, memory) = store();
    let original = owner("alpha", Some("one"));
    let access = memory.store.access.lock().await;
    memory
        .store
        .write_locked(
            Some(&original),
            None,
            "uses pnpm",
            Basis::UserConfirmed,
            &access,
        )
        .await
        .expect("save memory");
    let mut renamed = (*original).clone();
    renamed.project.as_mut().unwrap().label = Some("Renamed project".into());
    let visible = memory
        .store
        .visible(&Arc::new(renamed))
        .await
        .expect("load the same project by its stable id");
    assert_eq!(
        visible_texts(&visible),
        [Vec::<&str>::new(), vec!["uses pnpm"]]
    );
}

#[tokio::test]
async fn written_memories_stay_with_their_bot_and_project_unless_global() {
    let (temporary, memory) = store();
    let (alpha, alpha_elsewhere, beta) = (
        owner("alpha", Some("one")),
        owner("alpha", Some("two")),
        owner("beta", Some("one")),
    );
    let write = writer(&memory, &alpha);
    for arguments in [
        serde_json::json!({"memory": "alpha prefers tabs"}),
        serde_json::json!({"memory": "the office is in Tallinn", "global": true}),
    ] {
        write
            .call(tool_context(temporary.path()), arguments)
            .await
            .expect("write memory");
    }
    let store = &memory.store;
    assert_eq!(
        visible_texts(&store.visible(&alpha).await.expect("alpha")),
        [vec!["the office is in Tallinn"], vec!["alpha prefers tabs"]]
    );
    for other in [&alpha_elsewhere, &beta] {
        let visible = store.visible(other).await.expect("other owner");
        assert_eq!(
            visible_texts(&visible),
            [vec!["the office is in Tallinn"], Vec::<&str>::new()]
        );
        let empty: &[Arc<serde_json::Value>] = &[];
        let projection = next_projection(empty.into(), &visible)
            .expect("projection")
            .expect("memory context");
        let text = projection.to_string();
        assert!(text.contains("the office is in Tallinn") && !text.contains("alpha prefers tabs"));
    }
    let private = &store
        .load(Some(&alpha))
        .await
        .expect("alpha scope")
        .memories[0]
        .id;
    let update = writer(&memory, &beta)
        .call(
            tool_context(temporary.path()),
            serde_json::json!({"memory": "beta took it over", "id": private}),
        )
        .await;
    assert!(update.is_err(), "another Bot cannot reach a scoped memory");

    let listed = memory.list().await.expect("list");
    let summary = listed
        .iter()
        .map(|item| {
            (
                &*item.text,
                item.global,
                item.owner.as_deref() == Some(&*alpha),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        [
            ("alpha prefers tabs", false, true),
            ("the office is in Tallinn", true, true),
        ]
    );
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
        store.add_global("remember this").await.expect("memory");
        let middleware = MiddlewareStack::new(vec![
            Arc::new(Messages::default()),
            Arc::new(Tools::new(Vec::new())),
            Arc::new(StopAfterCompact(fail)),
            Arc::new(store.for_chat()),
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
            .session_id("session".parse().expect("session ID"))
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
            .load(&"session".parse().expect("session ID"))
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
async fn scope_budget_rejects_oversized_writes_and_edits_without_changing_state() {
    let (_temporary, store) = store();
    for i in 0..3 {
        store
            .add_global(&format!("{i}{}", "x".repeat(499)))
            .await
            .expect("bounded memory");
    }
    store.add_global("short").await.expect("small memory");
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
async fn concurrent_writes_preserve_every_accepted_memory_and_the_count_limit() {
    let (_temporary, store) = store();
    let store = Arc::new(store);
    let writes = (0..MAX_MEMORIES + 5)
        .map(|i| {
            let store = Arc::clone(&store);
            tokio::spawn(async move { store.add_global(&format!("memory {i}")).await })
        })
        .collect::<Vec<_>>();
    let mut accepted = 0;
    for write in writes {
        if write.await.expect("write task").is_ok() {
            accepted += 1;
        }
    }
    assert_eq!(accepted, MAX_MEMORIES);
    assert_eq!(
        store.snapshot().await.expect("snapshot").len(),
        MAX_MEMORIES
    );
}
