use super::*;
use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
use crate::middleware::tools::{ApprovalRequirement, Tool};
use crate::middleware::{ActiveCommandContext, FrontendEventSink, MessageQueue, SubmissionResult};
use crate::protocol::{
    FrontendEvent, FrontendSlot, FrontendWidgetContent, Op, internal_message_kind,
};

#[derive(Default)]
struct TestBotsBackend {
    scope: std::sync::Mutex<Option<String>>,
    scope_barriers:
        std::sync::Mutex<Option<(Arc<tokio::sync::Barrier>, Arc<tokio::sync::Barrier>)>>,
}

impl TestBotsBackend {
    fn set_scope(&self, scope: Option<&str>) {
        *self.scope.lock().expect("swarm scope") = scope.map(str::to_owned);
    }

    fn block_scope_resolution(
        &self,
        entered: Arc<tokio::sync::Barrier>,
        release: Arc<tokio::sync::Barrier>,
    ) {
        *self.scope_barriers.lock().expect("scope barriers") = Some((entered, release));
    }
}

impl BotsBackend for TestBotsBackend {
    fn active<'a>(&'a self, _bot_id: &'a str) -> BoxFuture<'a, Result<bool>> {
        let active = self.scope.lock().expect("swarm scope").is_some();
        Box::pin(async move { Ok(active) })
    }

    fn scratchpad_scope<'a>(&'a self, _bot_id: &'a str) -> BoxFuture<'a, Result<Option<String>>> {
        let barriers = self.scope_barriers.lock().expect("scope barriers").clone();
        Box::pin(async move {
            if let Some((entered, release)) = barriers {
                entered.wait().await;
                release.wait().await;
            }
            Ok(self.scope.lock().expect("swarm scope").clone())
        })
    }

    fn spawn_bot<'a>(
        &'a self,
        _bot_id: &'a str,
        _name: String,
        _description: String,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async { unreachable!() })
    }

    fn create_routine<'a>(
        &'a self,
        _bot_id: &'a str,
        _bot_handle: Option<String>,
        _workspace: &'a std::path::Path,
        _instructions: String,
        _schedule: serde_json::Value,
        _ends_at: Option<i64>,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async { unreachable!() })
    }

    fn roster<'a>(&'a self, _bot_id: &'a str) -> BoxFuture<'a, Result<String>> {
        Box::pin(async { unreachable!() })
    }

    fn read<'a>(&'a self, _bot_id: &'a str) -> BoxFuture<'a, Result<String>> {
        Box::pin(async { unreachable!() })
    }

    fn swarm_chat_context<'a>(
        &'a self,
        _bot_id: &'a str,
        _session_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async { Ok(None) })
    }

    fn can_reply<'a>(
        &'a self,
        _bot_id: &'a str,
        _message_id: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { unreachable!() })
    }

    fn post<'a>(
        &'a self,
        _bot_id: &'a str,
        _source_session_id: &'a str,
        _text: String,
        _in_reply_to_message_id: Option<String>,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async { unreachable!() })
    }
}

fn scratchpad(store: &ScratchpadStore) -> Scratchpad {
    Scratchpad::new(
        store.clone(),
        Arc::new(TestBotsBackend::default()),
        "test-bot",
    )
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

#[tokio::test]
async fn notes_are_session_scoped_deduplicated_and_exactly_promoted() {
    let (_temporary, store) = store().await;

    assert_eq!(
        store
            .write_session("session-a", "  learned lesson  ")
            .await
            .expect("write"),
        WriteOutcome::Added
    );
    assert_eq!(
        store
            .write_session("session-a", "learned lesson")
            .await
            .expect("deduplicate"),
        WriteOutcome::Existing
    );
    assert!(
        store
            .promote_note("session-b", None, "learned lesson", PromotionTarget::Global,)
            .await
            .is_err()
    );
    let session = store.snapshot("session-a", None).await.expect("session");
    assert_eq!(session.session[0].basis, Basis::AgentObservation);
    assert_eq!(
        store
            .promote_id(
                "session-a",
                None,
                &session.session[0].id,
                PromotionTarget::Global,
            )
            .await
            .expect("promote"),
        WriteOutcome::Added
    );
    store
        .write_session("session-a", "reviewed lesson")
        .await
        .expect("write reviewed note");
    store
        .promote_note(
            "session-a",
            None,
            "reviewed lesson",
            PromotionTarget::Global,
        )
        .await
        .expect("promote reviewed note");
    let session = store.snapshot("session-a", None).await.expect("session");
    let reviewed = session
        .session
        .iter()
        .find(|entry| entry.note == "reviewed lesson")
        .expect("reviewed note");
    assert_eq!(
        store
            .promote_id("session-a", None, &reviewed.id, PromotionTarget::Global,)
            .await
            .expect("confirm reviewed note"),
        WriteOutcome::Updated
    );

    let other = store
        .snapshot("session-b", None)
        .await
        .expect("other session");
    assert!(other.session.is_empty());
    assert_eq!(other.global[0].note, "learned lesson");
    assert_eq!(other.global[0].basis, Basis::UserConfirmed);
    assert!(other.global[0].created_at.parse::<u64>().is_ok());
    assert_eq!(other.global[1].basis, Basis::UserConfirmed);
}

#[tokio::test]
async fn swarm_notes_are_shared_only_with_the_current_swarm() {
    let (_temporary, store) = store().await;
    let swarm_a = Uuid::new_v4().to_string();
    let swarm_b = Uuid::new_v4().to_string();
    store
        .write_session("session-a", "coordinate releases")
        .await
        .expect("write session note");
    let note_id = store
        .snapshot("session-a", Some(&swarm_a))
        .await
        .expect("session snapshot")
        .session[0]
        .id
        .clone();

    assert_eq!(
        store
            .promote_id(
                "session-a",
                Some(&swarm_a),
                &note_id,
                PromotionTarget::Swarm,
            )
            .await
            .expect("promote to swarm"),
        WriteOutcome::Added
    );

    let same_swarm = store
        .snapshot("session-b", Some(&swarm_a))
        .await
        .expect("same swarm");
    assert_eq!(
        same_swarm.swarm.expect("member projection")[0].note,
        "coordinate releases"
    );
    assert!(same_swarm.global.is_empty());
    let source = store
        .snapshot("session-a", Some(&swarm_a))
        .await
        .expect("source session");
    assert_eq!(source.session[0].note, "coordinate releases");
    assert_eq!(
        source.swarm.expect("member projection")[0].basis,
        Basis::UserConfirmed
    );
    assert!(
        store
            .snapshot("session-c", Some(&swarm_b))
            .await
            .expect("other swarm")
            .swarm
            .expect("member projection")
            .is_empty()
    );
    assert!(
        store
            .promote_id("session-a", None, &note_id, PromotionTarget::Swarm,)
            .await
            .expect_err("non-member promotion must fail")
            .to_string()
            .contains("not currently in a swarm")
    );
}

#[tokio::test]
async fn clearing_a_disbanded_swarm_removes_its_collective_notes() {
    let (_temporary, store) = store().await;
    let swarm_id = Uuid::new_v4().to_string();
    store
        .add_swarm(&swarm_id, "temporary collective context")
        .await
        .expect("add swarm note");

    store.clear_swarm(&swarm_id).await.expect("clear swarm");

    assert!(
        store
            .snapshot("session", Some(&swarm_id))
            .await
            .expect("cleared snapshot")
            .swarm
            .expect("swarm scope")
            .is_empty()
    );
}

#[tokio::test]
async fn swarm_scope_is_resolved_for_each_projection() {
    let (_temporary, store) = store().await;
    let swarm_a = Uuid::new_v4().to_string();
    let swarm_b = Uuid::new_v4().to_string();
    store
        .add_swarm(&swarm_a, "alpha context")
        .await
        .expect("seed alpha");
    store
        .add_swarm(&swarm_b, "beta context")
        .await
        .expect("seed beta");
    let backend = Arc::new(TestBotsBackend::default());
    let middleware = Scratchpad::new(store, backend.clone(), "test-bot");

    backend.set_scope(Some(&swarm_a));
    assert_eq!(
        middleware
            .snapshot("session")
            .await
            .expect("alpha snapshot")
            .swarm
            .expect("alpha membership")[0]
            .note,
        "alpha context"
    );
    backend.set_scope(None);
    assert_eq!(
        middleware
            .snapshot("session")
            .await
            .expect("no swarm")
            .swarm,
        None
    );
    backend.set_scope(Some(&swarm_b));
    assert_eq!(
        middleware
            .snapshot("session")
            .await
            .expect("beta snapshot")
            .swarm
            .expect("beta membership")[0]
            .note,
        "beta context"
    );
}

#[tokio::test]
async fn human_management_returns_scoped_action_lists() {
    let (_temporary, store) = store().await;
    let swarm_id = Uuid::new_v4().to_string();
    let initial = store
        .swarm_contribution(&swarm_id)
        .await
        .expect("initial surface");
    let Some(FrontendWidgetContent::ActionList { actions, .. }) = &initial.widgets[0].content
    else {
        panic!("management action list");
    };
    let mut operation = actions[0].op.clone();
    let Op::CapabilityCommand { input, .. } = &mut operation else {
        panic!("add command");
    };
    *input = Some("  initial note  ".into());
    assert_eq!(
        actions[0].editor.as_ref().expect("editor").title,
        "Add collective note"
    );
    let contribution = store
        .management_command(Some(&swarm_id), &operation)
        .await
        .expect("add swarm note");
    let Some(FrontendWidgetContent::ActionList { title, items, .. }) =
        &contribution.widgets[0].content
    else {
        panic!("swarm contribution should be an action list");
    };
    assert_eq!(title, "Collective scratchpad");
    assert_eq!(items[0].text, "initial note");
    let note_id = items[0].id.clone();
    let Op::CapabilityCommand { arguments, .. } = &items[0].actions[0].op else {
        panic!("edit should use a capability command");
    };
    assert_eq!(arguments, &format!("edit swarm {note_id}"));

    let mut edit = items[0].actions[0].op.clone();
    let Op::CapabilityCommand { input, .. } = &mut edit else {
        panic!("edit command");
    };
    *input = Some("revised note".into());
    assert!(store.management_command(None, &edit).await.is_err());
    let contribution = store
        .management_command(Some(&swarm_id), &edit)
        .await
        .expect("edit swarm note");
    let Some(FrontendWidgetContent::ActionList { items, .. }) = &contribution.widgets[0].content
    else {
        panic!("swarm contribution should be an action list");
    };
    assert_eq!(items[0].text, "revised note");
    let contribution = store
        .management_command(Some(&swarm_id), &items[0].actions[1].op)
        .await
        .expect("forget swarm note");
    assert!(matches!(
        &contribution.widgets[0].content,
        Some(FrontendWidgetContent::ActionList { items, .. }) if items.is_empty()
    ));

    let global = store
        .add_global("global note")
        .await
        .expect("add global note");
    assert!(matches!(
        &global.widgets[0].content,
        Some(FrontendWidgetContent::ActionList { title, items, .. })
            if title == "Global Scratchpad" && items[0].text == "global note"
    ));
    assert!(store.swarm_contribution("not-a-uuid").await.is_err());
}

#[test]
fn duplicate_notes_merge_only_stronger_provenance() {
    let mut entries = vec![entry("lesson")];
    assert_eq!(
        insert(&mut entries, "lesson".into(), Basis::UserConfirmed).expect("confirm"),
        WriteOutcome::Updated
    );
    assert_eq!(
        insert(&mut entries, "lesson".into(), Basis::AgentObservation).expect("do not downgrade"),
        WriteOutcome::Existing
    );
    assert_eq!(entries[0].basis, Basis::UserConfirmed);
}

#[tokio::test]
async fn shared_lock_preserves_the_bounded_concurrent_whole_value_writes() {
    let (_temporary, store) = store().await;
    let writes = (0..MAX_NOTES).map(|index| {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .write_session("session", &format!("note {index}"))
                .await
        })
    });
    for write in writes {
        assert_eq!(
            write.await.expect("join").expect("write"),
            WriteOutcome::Added
        );
    }

    assert_eq!(
        store
            .snapshot("session", None)
            .await
            .expect("snapshot")
            .session
            .len(),
        MAX_NOTES
    );
    assert!(
        store
            .write_session("session", "one too many")
            .await
            .is_err()
    );
    assert!(canonical_note(&"é".repeat(MAX_NOTE_BYTES)).is_err());
}

#[tokio::test]
async fn edit_preserves_identity_confirms_provenance_and_rejects_duplicates() {
    let (_temporary, store) = store().await;
    store
        .write_session("session", "first note")
        .await
        .expect("write first note");
    store
        .write_session("session", "second note")
        .await
        .expect("write second note");
    let before = store
        .snapshot("session", None)
        .await
        .expect("snapshot")
        .session[0]
        .clone();

    let middleware = scratchpad(&store);
    let checkpoint = crate::backend::checkpoint::Checkpoint::empty("session");
    let session_context = session_context();
    let arguments = format!("edit session {}", before.id);
    middleware
        .command(MiddlewareCommandContext {
            command: "scratchpad",
            arguments: &arguments,
            input: Some("  revised note  "),
            target: None,
            session_id: "session",
            session_context: &session_context,
            checkpoint: &checkpoint,
            checkpoints: Arc::clone(&store.checkpoints),
        })
        .await
        .expect("edit command");
    let after = store
        .snapshot("session", None)
        .await
        .expect("snapshot")
        .session[0]
        .clone();
    assert_eq!(after.id, before.id);
    assert_eq!(after.created_at, before.created_at);
    assert_eq!(after.note, "revised note");
    assert_eq!(after.basis, Basis::UserConfirmed);
    assert!(
        store
            .edit("session", None, Scope::Session, &after.id, "second note")
            .await
            .is_err()
    );
    assert!(
        store
            .edit("session", None, Scope::Session, &after.id, "   ")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn active_edit_applies_immediately_and_refreshes_widgets() {
    let (_temporary, store) = store().await;
    store
        .write_session("session", "first note")
        .await
        .expect("write note");
    let note_id = store
        .snapshot("session", None)
        .await
        .expect("snapshot")
        .session[0]
        .id
        .clone();
    let middleware = scratchpad(&store);
    let arguments = format!("edit session {note_id}");

    let (result, events) =
        active_command(&middleware, "scratchpad", &arguments, Some("revised note")).await;

    assert_eq!(result, Some(SubmissionResult::Handled));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EventMsg::Frontend(FrontendEvent::Widget { .. })))
    );
    assert_eq!(
        store
            .snapshot("session", None)
            .await
            .expect("snapshot")
            .session[0]
            .note,
        "revised note"
    );
}

#[tokio::test]
async fn active_promote_applies_immediately() {
    let (_temporary, store) = store().await;
    store
        .write_session("session", "promote this")
        .await
        .expect("write note");
    let note_id = store
        .snapshot("session", None)
        .await
        .expect("snapshot")
        .session[0]
        .id
        .clone();
    let middleware = scratchpad(&store);

    let (result, events) = active_command(
        &middleware,
        "scratchpad",
        &format!("promote global {note_id}"),
        None,
    )
    .await;

    assert_eq!(result, Some(SubmissionResult::Handled));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EventMsg::Frontend(FrontendEvent::Widget { .. })))
    );
    let snapshot = store.snapshot("session", None).await.expect("snapshot");
    assert_eq!(snapshot.global[0].note, "promote this");
    assert_eq!(snapshot.global[0].basis, Basis::UserConfirmed);
}

#[tokio::test]
async fn active_command_defers_when_scratchpad_lock_is_busy() {
    let (_temporary, store) = store().await;
    let middleware = scratchpad(&store);
    let _access = store.access.lock().await;

    let (result, events) = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        active_command(&middleware, "scratchpad", "refresh", None),
    )
    .await
    .expect("busy active command must not block");

    assert_eq!(result, None);
    assert!(events.is_empty());
}

#[tokio::test]
async fn swarm_scope_resolution_is_serialized_with_scratchpad_cleanup() {
    let (_temporary, store) = store().await;
    let swarm_id = Uuid::new_v4().to_string();
    store
        .write_session("session", "promote this")
        .await
        .expect("write note");
    store
        .add_swarm(&swarm_id, "existing context")
        .await
        .expect("seed swarm");
    let note_id = store
        .snapshot("session", Some(&swarm_id))
        .await
        .expect("snapshot")
        .session[0]
        .id
        .clone();
    let backend = Arc::new(TestBotsBackend::default());
    backend.set_scope(Some(&swarm_id));
    let entered = Arc::new(tokio::sync::Barrier::new(2));
    let release = Arc::new(tokio::sync::Barrier::new(2));
    backend.block_scope_resolution(Arc::clone(&entered), Arc::clone(&release));
    let middleware = Scratchpad::new(store.clone(), backend.clone(), "test-bot");
    let promotion = tokio::spawn(async move {
        let checkpoint = crate::backend::checkpoint::Checkpoint::empty("session");
        let session_context = session_context();
        middleware
            .command(MiddlewareCommandContext {
                command: "scratchpad",
                arguments: &format!("promote swarm {note_id}"),
                input: None,
                target: None,
                session_id: "session",
                session_context: &session_context,
                checkpoint: &checkpoint,
                checkpoints: Arc::clone(&middleware.store.checkpoints),
            })
            .await
    });

    entered.wait().await;
    backend.set_scope(None);
    let cleanup_store = store.clone();
    let cleanup_swarm_id = swarm_id.clone();
    let mut cleanup =
        tokio::spawn(async move { cleanup_store.clear_swarm(&cleanup_swarm_id).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut cleanup)
            .await
            .is_err(),
        "cleanup must wait for membership resolution"
    );

    release.wait().await;
    assert!(promotion.await.expect("promotion task").is_err());
    cleanup
        .await
        .expect("cleanup task")
        .expect("clear scratchpad");
    assert!(
        store
            .snapshot("session", Some(&swarm_id))
            .await
            .expect("cleared snapshot")
            .swarm
            .expect("swarm scope")
            .is_empty()
    );
}

#[tokio::test]
async fn disabled_agent_keeps_read_only_surfaces_without_prompt_or_tools() {
    let (_temporary, store) = store().await;
    store
        .write_session("session", "historical note")
        .await
        .expect("seed note");
    let note_id = store
        .snapshot("session", None)
        .await
        .expect("snapshot")
        .session[0]
        .id
        .clone();
    let middleware = scratchpad(&store).agent_enabled(false);
    let frontend_events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_events = Arc::clone(&frontend_events);
    let runtime = RuntimeContext {
        sender: crate::agent::test_sender(),
        checkpoints: Arc::clone(&store.checkpoints),
        session_id: "session".into(),
        model_route: "model".into(),
        model: "model".into(),
        approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
        session_context: session_context(),
        metadata: Default::default(),
        role: crate::agent::AgentRole::Main,
        frontend: Arc::new(move |event| {
            captured_events.lock().expect("frontend events").push(event);
            Ok(())
        }),
    };
    let mut catalog = Catalog::default();

    middleware
        .register(&mut catalog, &runtime)
        .expect("disabled catalog");
    assert!(catalog.registered_definitions().is_empty());
    assert_eq!(middleware.prompt_section(&runtime).expect("prompt"), None);
    let contribution = middleware.frontend();
    assert_eq!(contribution.commands.len(), 1);
    assert_eq!(contribution.commands[0].name, "scratchpad");
    assert_eq!(contribution.widgets.len(), 2);
    let mut input = Vec::new();
    let mut start = crate::middleware::SessionStartContext {
        runtime: &runtime,
        source: crate::middleware::SessionStartSource::Startup,
        queued_messages: Default::default(),
        input: &mut input,
        input_changed: false,
        stop_reason: None,
    };
    middleware
        .session_start(&mut start)
        .await
        .expect("session start");
    assert!(
        frontend_events
            .lock()
            .expect("frontend events")
            .iter()
            .any(|event| matches!(
                event,
                FrontendEvent::Widget { item, .. }
                    if matches!(
                        &item.content,
                        Some(FrontendWidgetContent::ActionList { items, .. })
                            if items.iter().any(|item| item.text == "historical note")
                    )
            )),
        "session start should restore historical notes in the management widget"
    );

    let checkpoint = crate::backend::checkpoint::Checkpoint::empty("session");
    let session_context = session_context();
    let stack = crate::middleware::MiddlewareStack::new(vec![Arc::new(middleware)])
        .expect("scratchpad middleware stack");
    assert_eq!(
        stack
            .command(
                MANIFEST.id,
                MiddlewareCommandContext {
                    command: "scratchpad",
                    arguments: "refresh",
                    input: None,
                    target: None,
                    session_id: "session",
                    session_context: &session_context,
                    checkpoint: &checkpoint,
                    checkpoints: Arc::clone(&store.checkpoints),
                },
            )
            .await
            .expect("refresh")
            .events
            .len(),
        2
    );
    let forget = format!("forget session {note_id}");
    assert_eq!(
        stack
            .command(
                MANIFEST.id,
                MiddlewareCommandContext {
                    command: "scratchpad",
                    arguments: &forget,
                    input: None,
                    target: None,
                    session_id: "session",
                    session_context: &session_context,
                    checkpoint: &checkpoint,
                    checkpoints: Arc::clone(&store.checkpoints),
                },
            )
            .await
            .err()
            .expect("mutation must be disabled")
            .to_string(),
        "tool error: scratchpad is disabled for this chat"
    );
    assert_eq!(
        store
            .snapshot("session", None)
            .await
            .expect("retained")
            .session[0]
            .note,
        "historical note"
    );
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
async fn new_session_seeds_one_bounded_baseline_projection() {
    let (_temporary, store) = store().await;
    store
        .write_session("session", "remember this")
        .await
        .expect("write note");
    let middleware = scratchpad(&store);
    let runtime = runtime(&store, "session");
    let mut seeded = Vec::new();
    let mut start = crate::middleware::SessionStartContext {
        runtime: &runtime,
        source: crate::middleware::SessionStartSource::Startup,
        queued_messages: Default::default(),
        input: &mut seeded,
        input_changed: false,
        stop_reason: None,
    };
    middleware
        .session_start(&mut start)
        .await
        .expect("session start");
    assert_eq!(seeded.len(), 1);
    assert_eq!(internal_message_kind(&seeded[0]), Some(BASELINE_KIND));
    assert!(is_projection_item(&seeded[0]));
    assert!(
        seeded[0]["content"][0]["text"]
            .as_str()
            .expect("baseline text")
            .len()
            <= MAX_INJECTION_BYTES
    );
    assert_eq!(
        serde_json::from_value::<Snapshot>(seeded[0][PROJECTION_FIELD].clone())
            .expect("projection"),
        store.snapshot("session", None).await.expect("snapshot")
    );
}

#[tokio::test]
async fn startup_keeps_one_inherited_baseline_projection() {
    let (_temporary, store) = store().await;
    store
        .write_session("session", "remember this")
        .await
        .expect("write note");
    let snapshot = store.snapshot("session", None).await.expect("snapshot");
    let inherited = scratchpad_message(&snapshot).expect("baseline");
    let middleware = scratchpad(&store);
    let runtime = runtime(&store, "session");
    let mut input = vec![inherited.clone()];
    let mut start = crate::middleware::SessionStartContext {
        runtime: &runtime,
        source: crate::middleware::SessionStartSource::Startup,
        queued_messages: Default::default(),
        input: &mut input,
        input_changed: false,
        stop_reason: None,
    };

    middleware
        .session_start(&mut start)
        .await
        .expect("session start");

    assert_eq!(input, [inherited]);
}

#[test]
fn unchanged_projection_is_a_no_op() {
    let snapshot = Snapshot {
        session: vec![entry("same")],
        swarm: None,
        global: Vec::new(),
    };
    let input = vec![scratchpad_message(&snapshot).expect("baseline")];

    assert!(
        next_projection(&input, &snapshot)
            .expect("compare projection")
            .is_none()
    );
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
        store
            .write_session("session", "remember this")
            .await
            .expect("note");
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
        assert_eq!(checkpoint.compaction_count, 1);
        assert!(!checkpoint.context.iter().any(is_projection_item));
    }
}

#[test]
fn disabled_scratchpad_removes_durable_projection_items() {
    let snapshot = Snapshot {
        session: vec![entry("private note")],
        swarm: None,
        global: Vec::new(),
    };
    let user = crate::backend::model::user_message("hello");
    let input = vec![
        scratchpad_message(&snapshot).expect("baseline"),
        user.clone(),
    ];

    let cleaned = without_projection_items(&input).expect("projection cleanup");

    assert_eq!(cleaned, vec![user]);
}

#[test]
fn changed_projection_appends_a_bounded_delta_without_replacing_context() {
    let previous = Snapshot {
        session: vec![entry("old")],
        swarm: None,
        global: Vec::new(),
    };
    let mut current = previous.clone();
    current.session.push(entry("new"));
    let baseline = scratchpad_message(&previous).expect("baseline");
    let input = vec![
        baseline.clone(),
        crate::backend::model::user_message("hello"),
    ];

    let delta = next_projection(&input, &current)
        .expect("compare projection")
        .expect("delta");
    assert_eq!(internal_message_kind(&delta), Some(DELTA_KIND));
    assert!(is_projection_item(&delta));
    assert!(
        delta["content"][0]["text"]
            .as_str()
            .expect("delta text")
            .contains("added")
    );
    assert!(
        delta["content"][0]["text"]
            .as_str()
            .expect("delta text")
            .len()
            <= MAX_INJECTION_BYTES
    );
    assert_eq!(
        input,
        vec![baseline, crate::backend::model::user_message("hello")]
    );
}

#[test]
fn swarm_projection_is_injected_and_hard_rejects_the_old_shape() {
    let previous = Snapshot {
        session: Vec::new(),
        swarm: Some(vec![entry("shared context")]),
        global: Vec::new(),
    };
    let baseline = scratchpad_message(&previous).expect("swarm baseline");
    assert!(
        baseline["content"][0]["text"]
            .as_str()
            .expect("baseline text")
            .contains("Swarm (newest first):")
    );
    let mut current = previous.clone();
    current
        .swarm
        .as_mut()
        .expect("swarm projection")
        .push(entry("new shared context"));
    let delta = next_projection(&[baseline], &current)
        .expect("compare projection")
        .expect("swarm delta");
    let text = delta["content"][0]["text"].as_str().expect("delta text");
    assert!(text.contains("Swarm:\n"));
    assert!(text.contains("new shared context"));
    assert!(
        serde_json::from_value::<Snapshot>(serde_json::json!({
            "session": [],
            "global": []
        }))
        .is_err(),
        "the pre-Swarm projection shape must not be accepted"
    );
}

#[test]
fn surfaces_are_scope_specific_action_lists_without_subtext() {
    let session = entry("Prefer focused tests");
    let mut global = entry("Use generic UI records");
    global.basis = Basis::UserConfirmed;
    let snapshot = Snapshot {
        session: vec![session],
        swarm: None,
        global: vec![global],
    };
    let widgets = surface_widgets(&snapshot);

    let Some(FrontendWidgetContent::ActionList { title, items, .. }) = &widgets[0].content else {
        panic!("navigation should render an action list");
    };
    assert_eq!(title, "Global Scratchpad");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].text, "Use generic UI records");
    assert_eq!(
        items[0]
            .actions
            .iter()
            .map(|action| action.label.as_str())
            .collect::<Vec<_>>(),
        ["Edit", "Delete"]
    );

    let Some(FrontendWidgetContent::ActionList { title, items, .. }) = &widgets[1].content else {
        panic!("chat menu should render an action list");
    };
    assert_eq!(title, "Chat Scratchpad");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].text, "Prefer focused tests");
    assert_eq!(
        items[0]
            .actions
            .iter()
            .map(|action| action.label.as_str())
            .collect::<Vec<_>>(),
        ["Promote Globally", "Edit", "Delete"]
    );
    let Op::CapabilityCommand { input, .. } = &items[0].actions[1].op else {
        panic!("edit should submit a capability command");
    };
    assert_eq!(input.as_deref(), Some("Prefer focused tests"));
    assert_eq!(
        action_list_item(Scope::Session, &entry("Already global"), true, None)
            .actions
            .into_iter()
            .map(|action| action.label)
            .collect::<Vec<_>>(),
        ["Edit", "Delete"]
    );

    let shared = entry("Share release context");
    let widgets = surface_widgets(&Snapshot {
        session: vec![shared.clone()],
        swarm: Some(Vec::new()),
        global: Vec::new(),
    });
    let Some(FrontendWidgetContent::ActionList { items, .. }) = &widgets[1].content else {
        panic!("chat menu should render an action list");
    };
    assert_eq!(
        items[0]
            .actions
            .iter()
            .map(|action| action.label.as_str())
            .collect::<Vec<_>>(),
        ["Promote Globally", "Promote to Swarm", "Edit", "Delete"]
    );
    assert_ne!(items[0].actions[0].id, items[0].actions[1].id);
    let Op::CapabilityCommand { arguments, .. } = &items[0].actions[1].op else {
        panic!("Swarm promotion should submit a capability command");
    };
    assert_eq!(arguments, &format!("promote swarm {}", shared.id));

    let mut confirmed = shared.clone();
    confirmed.basis = Basis::UserConfirmed;
    let widgets = surface_widgets(&Snapshot {
        session: vec![shared],
        swarm: Some(vec![confirmed.clone()]),
        global: vec![confirmed],
    });
    let Some(FrontendWidgetContent::ActionList { items, .. }) = &widgets[1].content else {
        panic!("chat menu should render an action list");
    };
    assert_eq!(
        items[0]
            .actions
            .iter()
            .map(|action| action.label.as_str())
            .collect::<Vec<_>>(),
        ["Edit", "Delete"]
    );

    assert!(surface_widgets(&Snapshot::default()).iter().all(|widget| {
        matches!(
            &widget.content,
            Some(FrontendWidgetContent::ActionList { items, .. }) if items.is_empty()
        )
    }));
}

#[tokio::test]
async fn frontend_is_semantic_and_only_promotion_requires_approval() {
    let (_temporary, store) = store().await;
    let middleware = scratchpad(&store);
    let contribution = middleware.frontend();

    assert_eq!(contribution.widgets[0].slot, FrontendSlot::Navigation);
    assert_eq!(contribution.widgets[1].slot, FrontendSlot::ChatMenu);
    assert!(!contribution.commands[0].requires_idle);
    assert!(
        contribution
            .widgets
            .iter()
            .all(|widget| widget.action.is_some())
    );
    assert_eq!(
        WriteScratchpad {
            store: store.clone(),
            swarm: SwarmScope {
                backend: Arc::new(TestBotsBackend::default()),
                bot_id: "test-bot".into(),
            },
            session_id: "session".into(),
            frontend: frontend_sink(),
        }
        .approval(),
        ApprovalRequirement::Never
    );
    let promote = PromoteScratchpad {
        store,
        swarm: SwarmScope {
            backend: Arc::new(TestBotsBackend::default()),
            bot_id: "test-bot".into(),
        },
        session_id: "session".into(),
        frontend: frontend_sink(),
    };
    assert_eq!(promote.approval(), ApprovalRequirement::Always);
    assert_eq!(
        promote.definition().parameters["properties"]["target"]["enum"],
        serde_json::json!(["global", "swarm"])
    );
    assert_eq!(
        promote.definition().parameters["required"],
        serde_json::json!(["note", "target"])
    );
}
