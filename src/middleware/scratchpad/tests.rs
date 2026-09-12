use super::*;
use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
use crate::middleware::tools::{ApprovalRequirement, Tool, ToolContext};
use crate::middleware::{ActiveCommandContext, FrontendEventSink, MessageQueue, SubmissionResult};
use crate::protocol::{FrontendSlot, FrontendWidgetContent, Op};

fn scratchpad(store: &ScratchpadStore) -> Scratchpad {
    Scratchpad::new(store.clone())
}

fn session_context() -> crate::protocol::SessionContext {
    crate::protocol::SessionContext {
        bot_id: "test-bot".into(),
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

async fn store() -> (tempfile::TempDir, ScratchpadStore) {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(temporary.path().join("checkpoints.sqlite3")).expect("checkpoints"),
    );
    (temporary, ScratchpadStore::new(checkpoints))
}

fn frontend_sink() -> FrontendEventSink {
    Arc::new(|_| Ok(()))
}

async fn active_command(
    middleware: &Scratchpad,
    command: &str,
    arguments: &str,
    input: Option<&str>,
) -> (Option<SubmissionResult>, Vec<EventMsg>) {
    let metadata = std::collections::BTreeMap::new();
    let mut queued = Vec::new();
    let mut events = Vec::new();
    let mut context = ActiveCommandContext {
        checkpoints: middleware.store.checkpoints.as_ref(),
        submission_id: "active-command",
        session_id: "session",
        metadata: &metadata,
        active_turn_id: "turn",
        command,
        arguments,
        input,
        target: None,
        queued_messages: MessageQueue::new(&mut queued),
        events: &mut events,
    };
    let result = middleware
        .active_command(&mut context)
        .await
        .expect("active command");
    (result, events)
}

fn runtime(store: &ScratchpadStore, session_id: &str) -> RuntimeContext {
    RuntimeContext {
        sender: crate::agent::test_sender(),
        checkpoints: Arc::clone(&store.checkpoints),
        session_id: session_id.into(),
        model_route: "model".into(),
        model: "model".into(),
        approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
        session_context: session_context(),
        metadata: Default::default(),
        role: crate::agent::AgentRole::Main,
        frontend: frontend_sink(),
    }
}

#[tokio::test]
async fn compaction_discards_projections_before_a_post_hook_stops_or_fails() {
    use crate::agent::{AgentConfig, create_agent};
    use crate::backend::model::{
        CompactOutput, CompactRequest, Model, ModelEventSink, ModelOutput, ModelRequest,
        ModelRouter,
    };
    use crate::backend::sandbox::{ApprovalPolicy, Sandbox, local::LocalSandbox};
    use crate::middleware::{
        CompactContext, MiddlewareStack, compaction::Compaction, messages::Messages, tools::Tools,
    };
    use crate::protocol::MessageSubmission;

    struct RetainingCompactor;
    impl Model for RetainingCompactor {
        fn respond<'a>(
            &'a self,
            _request: ModelRequest<'a>,
            _events: ModelEventSink,
        ) -> BoxFuture<'a, Result<ModelOutput>> {
            Box::pin(async { Err(Error::Config("unexpected model request".into())) })
        }

        fn compaction_endpoint(&self) -> bool {
            true
        }

        fn compact<'a>(
            &'a self,
            request: CompactRequest<'a>,
        ) -> BoxFuture<'a, Result<CompactOutput>> {
            Box::pin(async move {
                assert!(request.input.iter().any(is_projection_item));
                CompactOutput::from_output(request.input.to_vec(), Default::default())
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
        let (temporary, store) = store().await;
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
                Arc::new(ModelRouter::new("model", Arc::new(RetainingCompactor))),
                Arc::new(Sandbox::new(
                    Arc::new(LocalSandbox::new(temporary.path()).expect("sandbox")),
                    ApprovalPolicy::Ask,
                )),
                Arc::clone(&store.checkpoints),
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
            .checkpoints
            .load("session")
            .await
            .expect("load")
            .expect("checkpoint");
        assert_eq!(checkpoint.compaction_count, u64::from(!fail));
        assert_eq!(checkpoint.context.iter().any(is_projection_item), fail);
    }
}

fn tool_context() -> ToolContext {
    use crate::backend::sandbox::{
        ApprovalPolicy, NetworkAccess, Sandbox, SandboxMode, SandboxPermissions,
    };
    ToolContext::new(
        Arc::new(Sandbox::new(
            Arc::new(crate::backend::sandbox::local::LocalSandbox::new(".").expect("sandbox")),
            ApprovalPolicy::Ask,
        )),
        SandboxPermissions::restore(
            "chat",
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            ["call".into()],
        )
        .for_call("call"),
        "turn",
    )
}

fn write_tool(store: &ScratchpadStore) -> WriteScratchpad {
    WriteScratchpad {
        store: store.clone(),
        frontend: frontend_sink(),
    }
}

#[tokio::test]
async fn shared_management_adds_edits_and_forgets_global_notes() {
    let (_temporary, store) = store().await;
    let contribution = store
        .global_contribution()
        .await
        .expect("management surface");
    let Some(FrontendWidgetContent::ActionList { actions, .. }) = &contribution.widgets[0].content
    else {
        panic!("semantic shared management");
    };
    assert_eq!(actions.len(), 1);
    let mut add = actions[0].op.clone();
    let Op::CapabilityCommand { input, .. } = &mut add else {
        panic!("add command")
    };
    *input = Some("shared fact".into());
    store
        .management_command(&add)
        .await
        .expect("add shared fact");
    let snapshot = store.snapshot().await.expect("snapshot");
    let entry = &snapshot.global[0];
    assert_eq!(entry.basis, Basis::UserConfirmed);
    let item = action_list_item(entry);
    assert_eq!(item.actions.len(), 2);
    let mut edit = item.actions[0].op.clone();
    let Op::CapabilityCommand { input, .. } = &mut edit else {
        panic!("edit command")
    };
    *input = Some("revised fact".into());
    store.management_command(&edit).await.expect("edit");
    assert_eq!(
        store.snapshot().await.expect("edited").global[0].note,
        "revised fact"
    );
    store
        .management_command(&item.actions[1].op)
        .await
        .expect("forget");
    assert!(store.snapshot().await.expect("snapshot").global.is_empty());
}

#[tokio::test]
async fn agent_writes_shared_notes_with_approval_without_session_staging() {
    let (temporary, store) = store().await;
    let tool = write_tool(&store);
    assert_eq!(tool.approval(), ApprovalRequirement::Always);
    assert_eq!(
        tool.definition().parameters["required"],
        serde_json::json!(["note"])
    );
    assert!(
        tool.call(
            tool_context(),
            serde_json::json!({"scope":"session", "note":"no"})
        )
        .await
        .is_err()
    );
    tool.call(
        tool_context(),
        serde_json::json!({"note":"  shared fact  "}),
    )
    .await
    .expect("shared write");
    tool.call(tool_context(), serde_json::json!({"note":"shared fact"}))
        .await
        .expect("deduplicated write");
    let snapshot = store.snapshot().await.expect("snapshot");
    assert_eq!(snapshot.global.len(), 1);
    assert_eq!(snapshot.global[0].basis, Basis::AgentObservation);
    let id = snapshot.global[0].id.clone();
    store
        .add_global("shared fact")
        .await
        .expect("human confirmation");
    let reopened = ScratchpadStore::new(Arc::new(
        SqliteCheckpoint::new(temporary.path().join("checkpoints.sqlite3")).expect("reopen"),
    ));
    let saved = reopened.snapshot().await.expect("durable state");
    assert_eq!(saved.global[0].id, id);
    assert_eq!(saved.global[0].basis, Basis::UserConfirmed);
}

#[tokio::test]
async fn shared_scope_budget_rejects_oversized_writes_and_edits_without_changing_state() {
    let (_temporary, store) = store().await;
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
            .edit_global(&before.global[3].id, &"z".repeat(500))
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
    let (_temporary, store) = store().await;
    let writes = (0..MAX_NOTES + 5)
        .map(|i| {
            let store = store.clone();
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
    assert_eq!(
        store.snapshot().await.expect("snapshot").global.len(),
        MAX_NOTES
    );
}

#[test]
fn projections_include_all_notes_and_append_complete_changes_without_hidden_metadata() {
    let notes = (0..MAX_NOTES)
        .map(|i| entry(format!("note {i} {}", "x".repeat(65))))
        .collect::<Vec<_>>();
    let previous = Snapshot { global: notes };
    let mut baseline = next_projection(&[], &previous)
        .expect("projection")
        .expect("baseline");
    let text = baseline["content"][0]["text"].as_str().expect("text");
    assert!(text.len() <= MAX_INJECTION_BYTES);
    assert!(text.contains(&previous.global[0].note));
    assert!(text.contains(&previous.global[MAX_NOTES - 1].note));
    assert!(baseline.get("_mobius_scratchpad_projection").is_none());
    crate::backend::model::mark_prompt_cache_breakpoint(&mut baseline);
    let user = crate::backend::model::user_message("hello");
    let input = vec![baseline.clone(), user.clone()];
    assert!(next_projection(&input, &previous).expect("same").is_none());
    let mut current = previous.clone();
    current.global[0].note = "edited first note".into();
    current.global.pop();
    let update = next_projection(&input, &current)
        .expect("update")
        .expect("changed");
    let text = update["content"][0]["text"].as_str().expect("text");
    assert!(text.contains("edited first note"));
    assert!(text.contains("replace all prior scratchpad context"));
    assert_eq!(input, [baseline, user.clone()]);
    let cleared = next_projection(&[update], &Snapshot::default())
        .expect("clear")
        .expect("clear projection");
    assert!(
        !cleared["content"][0]["text"]
            .as_str()
            .expect("clear text")
            .contains("edited first note")
    );
    assert_eq!(
        without_projection_items(&[cleared, user.clone()]).expect("remove projection"),
        [user]
    );
    assert!(
        next_projection(&[], &Snapshot::default())
            .expect("empty start")
            .is_none()
    );
}

#[tokio::test]
async fn startup_and_compaction_restore_shared_notes_without_chat_menu_or_duplicate_input() {
    let (_temporary, store) = store().await;
    store.add_global("global context").await.expect("note");
    let middleware = scratchpad(&store);
    let runtime = runtime(&store, "session");
    let mut input = Vec::new();
    for source in [
        SessionStartSource::Startup,
        SessionStartSource::Startup,
        SessionStartSource::Compact,
    ] {
        let mut start = SessionStartContext {
            runtime: &runtime,
            source,
            queued_messages: Default::default(),
            input: &mut input,
            input_changed: false,
            stop_reason: None,
        };
        middleware.session_start(&mut start).await.expect("start");
        assert_eq!(input.len(), 1);
    }
    assert_eq!(middleware.frontend().widgets.len(), 1);
    assert_eq!(
        middleware.frontend().widgets[0].slot,
        FrontendSlot::Navigation
    );
    assert!(!middleware.retain_compacted_input(&input[0]));
}

#[tokio::test]
async fn disabled_agent_keeps_shared_management_without_prompt_or_tools() {
    let (_temporary, store) = store().await;
    store.add_global("historical note").await.expect("seed");
    let before = store.snapshot().await.expect("snapshot");
    let middleware = scratchpad(&store).agent_enabled(false);
    let runtime = runtime(&store, "session");
    let mut catalog = Catalog::default();
    middleware
        .register(&mut catalog, &runtime)
        .expect("register");
    assert!(catalog.registered_definitions().is_empty());
    assert_eq!(middleware.prompt_section(&runtime).expect("prompt"), None);
    assert_eq!(middleware.frontend().widgets.len(), 1);
    let mut input = Vec::new();
    let mut start = SessionStartContext {
        runtime: &runtime,
        source: SessionStartSource::Startup,
        queued_messages: Default::default(),
        input: &mut input,
        input_changed: false,
        stop_reason: None,
    };
    middleware.session_start(&mut start).await.expect("start");
    assert!(input.is_empty());
    let (handled, events) = active_command(&middleware, "scratchpad", "refresh", None).await;
    assert_eq!(handled, Some(SubmissionResult::Handled));
    assert_eq!(events.len(), 1);
    let access = store.lock_access().await;
    assert!(
        middleware
            .execute_command_locked(
                "scratchpad",
                &format!("edit {}", before.global[0].id),
                Some("no"),
                access,
            )
            .await
            .is_err()
    );
    assert_eq!(store.snapshot().await.expect("unchanged"), before);
    assert!(parse_command("edit session unused", Some("no")).is_none());
    assert!(parse_command("promote global unused", None).is_none());
}

#[tokio::test]
async fn active_shared_edit_updates_state_and_defers_when_lock_is_busy() {
    let (_temporary, store) = store().await;
    store.add_global("before").await.expect("seed");
    let id = store.snapshot().await.expect("snapshot").global[0]
        .id
        .clone();
    let middleware = scratchpad(&store);
    let (handled, events) = active_command(
        &middleware,
        "scratchpad",
        &format!("edit {id}"),
        Some("after"),
    )
    .await;
    assert_eq!(handled, Some(SubmissionResult::Handled));
    assert!(!events.is_empty());
    assert_eq!(
        store.snapshot().await.expect("after").global[0].note,
        "after"
    );
    let _access = store.lock_access().await;
    let (result, events) = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        active_command(&middleware, "scratchpad", "refresh", None),
    )
    .await
    .expect("must not block");
    assert_eq!(result, None);
    assert!(events.is_empty());
}

#[tokio::test]
async fn oversized_saved_shared_scope_stays_manageable_and_is_never_silently_clipped() {
    let (_temporary, store) = store().await;
    let notes = (0..5)
        .map(|i| entry(format!("{i}{}", "x".repeat(499))))
        .collect::<Vec<_>>();
    store
        .checkpoints
        .save_state(
            GLOBAL_SCOPE,
            GLOBAL_STATE_KEY,
            &serde_json::to_value(&notes).expect("notes"),
        )
        .await
        .expect("saved state");
    let snapshot = store.snapshot().await.expect("management can load notes");
    let error = next_projection(&[], &snapshot).expect_err("projection must not clip");
    assert!(error.to_string().contains("shorten or remove a note"));
    assert_eq!(store.snapshot().await.expect("unchanged"), snapshot);
    store
        .global_contribution()
        .await
        .expect("management can show notes");
    store
        .edit_global(&notes[0].id, "shortened")
        .await
        .expect("can reduce an oversized scope");
    store
        .forget_global(&notes[1].id)
        .await
        .expect("can remove a note");
    assert!(
        next_projection(&[], &store.snapshot().await.expect("reduced"))
            .expect("all remaining notes fit")
            .is_some()
    );
}
