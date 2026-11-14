use super::*;
use crate::agent::{AgentConfig, create_agent};
use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
use crate::backend::model::{Model, ModelEventSink, ModelOutput, ModelRequest, ModelRouter};
use crate::backend::sandbox::local::LocalSandbox;
use crate::backend::sandbox::{ApprovalPolicy, Sandbox};
use crate::middleware::compaction::Compaction;
use crate::middleware::messages::Messages;
use crate::middleware::tools::Tools;
use crate::middleware::{CompactContext, MiddlewareStack};
use crate::protocol::{MessageSubmission, Op as ClientOp};

fn session_context() -> SessionContext {
    SessionContext {
        owner_id: "test-bot".into(),
        ..SessionContext::default()
    }
}

fn owner(bot: &str, project: Option<&str>) -> Arc<MemoryOwner> {
    Arc::new(MemoryOwner::of(&SessionContext {
        owner_id: bot.into(),
        workspace_id: project.map(str::to_owned),
        workspace_label: project.map(|project| format!("/projects/{project}")),
        ..SessionContext::default()
    }))
}

fn store() -> (tempfile::TempDir, Memory) {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(temporary.path().join("checkpoints.sqlite3")).expect("checkpoints"),
    );
    let memory = Memory::new(checkpoints, temporary.path().join("memory"));
    (temporary, memory)
}

fn chat(memory: &Memory) -> Memory {
    memory
        .for_chat(|id| (id == "dream_minutes").then_some(&FrontendSettingValue::Integer(1)))
        .expect("chat memory")
}

fn set(page: &str, global: bool, body: &str, base: Option<u64>) -> Op {
    Op {
        page: page.into(),
        global,
        change: Change::Set {
            description: format!("about {page}"),
            body: body.into(),
        },
        base_updated_at: base,
    }
}

async fn apply(
    memory: &Memory,
    owner: &Arc<MemoryOwner>,
    ops: Vec<Op>,
    writer: Writer<'_>,
) -> Vec<String> {
    let access = memory.store.access.lock().await;
    memory
        .store
        .apply_locked(owner, ops, Basis::AgentObservation, writer, &access)
        .await
        .expect("apply")
}

fn names(scopes: &[Scope; 2]) -> [Vec<&str>; 2] {
    scopes
        .each_ref()
        .map(|scope| scope.pages.iter().map(|page| page.name.as_str()).collect())
}

#[tokio::test]
async fn pages_stay_with_their_bot_and_project_unless_global() {
    let (_temporary, memory) = store();
    let alpha = owner("alpha", Some("one"));
    apply(
        &memory,
        &alpha,
        vec![
            set(
                "uses-pnpm",
                false,
                "Install with pnpm; see [[office]].",
                None,
            ),
            set("office", true, "The office is in Tallinn.", None),
        ],
        Writer::Chat,
    )
    .await;
    let mut renamed = (*alpha).clone();
    renamed.project.as_mut().expect("project").label = Some("Renamed".into());
    for (reader, visible) in [
        (alpha, [vec!["office"], vec!["uses-pnpm"]]),
        (Arc::new(renamed), [vec!["office"], vec!["uses-pnpm"]]),
        (owner("alpha", Some("two")), [vec!["office"], vec![]]),
        (owner("beta", Some("one")), [vec!["office"], vec![]]),
    ] {
        assert_eq!(
            names(&memory.store.visible(&reader).await.expect("visible")),
            visible
        );
    }
    let listed = memory.list().await.expect("list");
    let page = listed
        .iter()
        .find(|item| &*item.name == "uses-pnpm")
        .expect("page");
    assert_eq!(
        page.links,
        vec![MemoryLink {
            name: "office".into(),
            target: Some("global/office".into())
        }]
    );
    let office = listed
        .iter()
        .find(|item| &*item.name == "office")
        .expect("global");
    assert!(office.global);
    assert_eq!(
        office.owner.as_deref().map(|owner| &*owner.bot),
        Some("alpha")
    );
}

#[tokio::test]
async fn patches_apply_only_against_the_version_they_read() {
    let (temporary, memory) = store();
    let alpha = owner("alpha", None);
    let session: SessionId = "session".parse().expect("session ID");
    let created = apply(
        &memory,
        &alpha,
        vec![set("deploy", false, "make ship", Some(0))],
        Writer::Chat,
    )
    .await;
    assert!(created[0].starts_with("deploy: applied"), "{created:?}");
    let version = memory
        .store
        .pages
        .load(Some(&alpha))
        .await
        .expect("load")
        .pages[0]
        .updated_at;
    for op in [
        set("deploy", false, "again", Some(0)),
        set("deploy", false, "stale", Some(version - 1)),
        set("deploy", false, "unversioned", None),
    ] {
        let access = memory.store.access.lock().await;
        assert!(
            memory
                .store
                .apply_locked(
                    &alpha,
                    vec![op],
                    Basis::AgentObservation,
                    Writer::Dream(&session),
                    &access
                )
                .await
                .is_err()
        );
    }
    apply(
        &memory,
        &alpha,
        vec![set("deploy", false, "make ship --prod", Some(version))],
        Writer::Dream(&session),
    )
    .await;
    let page = memory
        .store
        .pages
        .load(Some(&alpha))
        .await
        .expect("load")
        .pages[0]
        .clone();
    assert_eq!(
        (page.body.as_str(), page.updated_at > version),
        ("make ship --prod", true)
    );
    let delete = Op {
        page: "deploy".into(),
        global: false,
        change: Change::Delete,
        base_updated_at: None,
    };
    apply(&memory, &alpha, vec![delete], Writer::Chat).await;
    assert!(
        memory
            .store
            .pages
            .load(Some(&alpha))
            .await
            .expect("load")
            .pages
            .is_empty()
    );
    let scope = temporary.path().join("memory").join(pages_dir(&alpha));
    assert_eq!(
        std::fs::read_dir(scope.join(".trash"))
            .expect("trash")
            .count(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(scope.join("_log.md"))
            .expect("log")
            .lines()
            .count(),
        3
    );
}

fn pages_dir(owner: &MemoryOwner) -> String {
    Scope {
        owner: Some(Arc::new(owner.clone())),
        pages: Arc::default(),
    }
    .dir_name()
    .expect("dir")
}

#[tokio::test]
async fn failed_replacement_preserves_the_live_page_and_delete_creates_trash() {
    let (temporary, memory) = store();
    let alpha = owner("alpha", None);
    apply(
        &memory,
        &alpha,
        vec![set("fact", true, "original", None)],
        Writer::Chat,
    )
    .await;
    let dir = temporary.path().join("memory/global");
    std::fs::create_dir(dir.join(".fact.md.tmp")).unwrap();
    assert!(
        memory
            .edit("global/fact", "new", "replacement")
            .await
            .is_err()
    );
    assert_eq!(&*memory.list().await.unwrap()[0].body, "original");
    assert_eq!(std::fs::read_dir(dir.join(".trash")).unwrap().count(), 0);
    std::fs::remove_dir(dir.join(".trash")).unwrap();
    memory.forget("global/fact").await.unwrap();
    assert!(memory.list().await.unwrap().is_empty());
    let archived = std::fs::read_dir(dir.join(".trash"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert!(
        std::fs::read_to_string(archived.path())
            .unwrap()
            .contains("original")
    );
}

#[tokio::test]
async fn a_merge_keeps_its_source_until_every_replacement_succeeds() {
    let (temporary, memory) = store();
    let alpha = owner("alpha", None);
    apply(
        &memory,
        &alpha,
        vec![
            set("keep", true, "old", None),
            set("source", true, "unique", None),
        ],
        Writer::Chat,
    )
    .await;
    let pages = memory.list().await.unwrap();
    let keep_version = pages
        .iter()
        .find(|page| &*page.name == "keep")
        .unwrap()
        .updated_at;
    let source_version = pages
        .iter()
        .find(|page| &*page.name == "source")
        .unwrap()
        .updated_at;
    let patch = |base| {
        vec![
            Op {
                page: "source".into(),
                global: true,
                change: Change::Delete,
                base_updated_at: Some(source_version),
            },
            set("keep", true, "old and unique", Some(base)),
        ]
    };
    let session: SessionId = "dream".parse().unwrap();
    let access = memory.store.access.lock().await;
    assert!(
        memory
            .store
            .apply_locked(
                &alpha,
                patch(keep_version - 1),
                Basis::AgentObservation,
                Writer::Dream(&session),
                &access
            )
            .await
            .is_err()
    );
    let blocked = temporary.path().join("memory/global/.keep.md.tmp");
    std::fs::create_dir(&blocked).unwrap();
    assert!(
        memory
            .store
            .apply_locked(
                &alpha,
                patch(keep_version),
                Basis::AgentObservation,
                Writer::Dream(&session),
                &access
            )
            .await
            .is_err()
    );
    assert_eq!(memory.store.pages.load(None).await.unwrap().pages.len(), 2);
    std::fs::remove_dir(blocked).unwrap();
    memory
        .store
        .apply_locked(
            &alpha,
            patch(keep_version),
            Basis::AgentObservation,
            Writer::Dream(&session),
            &access,
        )
        .await
        .unwrap();
    let pages = memory.store.pages.load(None).await.unwrap().pages;
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].body, "old and unique");
}

#[tokio::test]
async fn links_and_backlinks_share_scope_resolution_and_keep_dangling_names() {
    let (_temporary, memory) = store();
    let alpha = owner("alpha", Some("one"));
    apply(
        &memory,
        &alpha,
        vec![
            set("deploy", true, "global", None),
            set("deploy", false, "project", None),
            set("guide", false, "[[deploy]] [[deploy]] [[missing]]", None),
            set("guide", true, "[[deploy]]", None),
        ],
        Writer::Chat,
    )
    .await;
    let pages = memory.pages();
    let scoped: serde_json::Value =
        serde_json::from_str(&pages.read(&alpha, "deploy", false).await.unwrap()).unwrap();
    let global: serde_json::Value =
        serde_json::from_str(&pages.read(&alpha, "deploy", true).await.unwrap()).unwrap();
    assert_eq!(
        scoped["backlinks"],
        serde_json::json!([{"page":"guide", "global":false}])
    );
    assert_eq!(
        global["backlinks"],
        serde_json::json!([{"page":"guide", "global":true}])
    );
    let guide: serde_json::Value =
        serde_json::from_str(&pages.read(&alpha, "guide", false).await.unwrap()).unwrap();
    assert_eq!(
        guide["links"],
        serde_json::json!([
            {"name":"deploy", "target":format!("{}/deploy", pages_dir(&alpha))},
            {"name":"missing", "target":null}
        ])
    );
    let other = pages.documents(&owner("beta", Some("one"))).await.unwrap();
    assert!(other.iter().all(|page| page.global));
}

#[tokio::test]
async fn concurrent_batches_cannot_overfill_a_scope_but_merges_can_reuse_a_slot() {
    let (temporary, memory) = store();
    let alpha = owner("alpha", None);
    let dir = temporary.path().join("memory/global");
    std::fs::create_dir_all(&dir).unwrap();
    for index in 0..pages::MAX_PAGES - 1 {
        std::fs::write(
            dir.join(format!("page-{index}.md")),
            "---\ndescription: fact\nbasis: agent_observation\nupdated_at: 1\n---\nbody\n",
        )
        .unwrap();
    }
    let patch = |name| {
        let memory = &memory;
        let alpha = &alpha;
        async move {
            let access = memory.store.access.lock().await;
            memory
                .store
                .apply_locked(
                    alpha,
                    vec![set(name, true, "new", Some(0))],
                    Basis::AgentObservation,
                    Writer::Chat,
                    &access,
                )
                .await
        }
    };
    let (first, second) = tokio::join!(patch("first"), patch("second"));
    assert_ne!(first.is_ok(), second.is_ok());
    assert_eq!(memory.list().await.unwrap().len(), pages::MAX_PAGES);
    apply(
        &memory,
        &alpha,
        vec![
            set("merged", true, "body and new", Some(0)),
            Op {
                page: "page-0".into(),
                global: true,
                change: Change::Delete,
                base_updated_at: Some(1),
            },
        ],
        Writer::Chat,
    )
    .await;
    let pages = memory.list().await.unwrap();
    assert_eq!(pages.len(), pages::MAX_PAGES);
    assert!(pages.iter().any(|page| &*page.name == "merged"));
    assert!(!pages.iter().any(|page| &*page.name == "page-0"));
}

#[test]
fn patch_ops_must_set_both_fields_or_delete_alone() {
    for invalid in [
        serde_json::json!({"ops": []}),
        serde_json::json!({"ops": [{"page": "Bad Name", "description": "d", "body": "b"}]}),
        serde_json::json!({"ops": [{"page": "a", "description": "d"}]}),
        serde_json::json!({"ops": [{"page": "a", "delete": true, "body": "b"}]}),
        serde_json::json!({"ops": [{"page": "a", "description": "two\nlines", "body": "b"}]}),
    ] {
        assert!(tools::parse_ops(invalid).is_err());
    }
    assert_eq!(
        tools::parse_ops(serde_json::json!({"ops": [{"page": "a-1", "delete": true}]}))
            .expect("ops"),
        [Op {
            page: "a-1".into(),
            global: false,
            change: Change::Delete,
            base_updated_at: None
        }]
    );
}

fn message(text: &str) -> ClientOp {
    ClientOp::Message {
        message: MessageSubmission {
            author: crate::protocol::MessageAuthor::User,
            text: text.into(),
            attachments: Vec::new(),
            reply: None,
            requested_delivery: None,
            target_turn_id: None,
        },
    }
}

async fn until_turn_ends(agent: &mut crate::agent::Agent) {
    loop {
        if matches!(
            agent.next_event().await.expect("event").msg,
            EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
        ) {
            break;
        }
    }
}

#[tokio::test]
async fn compaction_discards_projections_before_a_post_hook_stops_or_fails() {
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
        store
            .manage(&ClientOp::CapabilityCommand {
                capability: MANIFEST.id.into(),
                command: "memory".into(),
                arguments: "add".into(),
                input: Some("remember this".into()),
                target: None,
            })
            .await
            .expect("memory");
        let middleware = MiddlewareStack::new(vec![
            Arc::new(Messages::default()),
            Arc::new(Tools::new(Vec::new())),
            Arc::new(StopAfterCompact(fail)),
            Arc::new(chat(&store)),
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
        agent.sender().submit(message("hello")).expect("submit");
        tokio::time::timeout(Duration::from_secs(5), until_turn_ends(&mut agent))
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

#[tokio::test(start_paused = true)]
async fn an_idle_chat_dreams_its_new_transcript_into_pages_once() {
    struct Dreamer {
        calls: std::sync::atomic::AtomicUsize,
        start: std::sync::Mutex<Option<tokio::time::Instant>>,
    }
    struct EarlierTimer(std::sync::atomic::AtomicUsize);
    impl Middleware for EarlierTimer {
        fn name(&self) -> &'static str {
            "earlier_timer"
        }
        fn idle_after(&self, _: &SessionId) -> Option<Duration> {
            Some(Duration::from_secs(10))
        }
        fn idle<'a>(&'a self, _: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            })
        }
    }
    impl Model for Dreamer {
        fn respond<'a>(
            &'a self,
            request: ModelRequest<'a>,
            _events: ModelEventSink,
        ) -> BoxFuture<'a, Result<ModelOutput>> {
            Box::pin(async move {
                if request.instructions != text::DEFINITION.prompt_dream {
                    *self.start.lock().unwrap() = Some(tokio::time::Instant::now());
                    return ModelOutput::from_output(Vec::new(), true, Default::default());
                }
                assert!(self.start.lock().unwrap().unwrap().elapsed() >= Duration::from_secs(60));
                self.calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                assert!(matches!(request.tools, [tool] if tool.name == "patch_memory"));
                let prompt = request.input.iter().next().expect("prompt").to_string();
                assert!(prompt.contains("we deploy with make ship"), "{prompt}");
                ModelOutput::from_output(
                    vec![serde_json::json!({
                        "type": "function_call",
                        "call_id": "dream",
                        "name": "patch_memory",
                        "arguments": serde_json::json!({"ops": [{
                            "page": "deploy", "description": "How this project deploys",
                            "body": "Run `make ship`.", "base_updated_at": 0
                        }]}).to_string()
                    })],
                    false,
                    Default::default(),
                )
            })
        }
    }

    let (temporary, store) = store();
    let model = Arc::new(Dreamer {
        calls: std::sync::atomic::AtomicUsize::new(0),
        start: std::sync::Mutex::new(None),
    });
    let earlier = Arc::new(EarlierTimer(std::sync::atomic::AtomicUsize::new(0)));
    let middleware = MiddlewareStack::new(vec![
        Arc::new(Messages::default()),
        Arc::new(Tools::new(Vec::new())),
        Arc::new(chat(&store)),
        Arc::clone(&earlier) as Arc<dyn Middleware>,
    ])
    .expect("middleware");
    let mut agent = create_agent(
        AgentConfig::new(
            Arc::new(ModelRouter::new(
                "model",
                Arc::clone(&model) as Arc<dyn Model>,
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
        .submit(message("we deploy with make ship"))
        .expect("submit");
    // Each subscriber runs once at its own deadline, without another user turn.
    for _ in 0..3 {
        tokio::time::timeout(Duration::from_secs(3600), until_turn_ends(&mut agent))
            .await
            .expect("turn");
    }
    let pages = store
        .store
        .visible(&owner("test-bot", None))
        .await
        .expect("visible");
    assert_eq!(pages[1].pages[0].body, "Run `make ship`.");
    assert_eq!(model.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(earlier.0.load(std::sync::atomic::Ordering::Relaxed), 1);
}
