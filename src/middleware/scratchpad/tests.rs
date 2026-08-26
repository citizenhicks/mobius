use super::*;
use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
use crate::middleware::tools::{ApprovalRequirement, Tool};
use crate::middleware::{
    ActiveCommandContext, ActiveSubmissionResult, FrontendEventSink, QueuedInputBaseline,
    QueuedInputQueue,
};
use crate::protocol::{
    FrontendEvent, FrontendSlot, FrontendWidgetContent, Op, internal_message_kind,
};

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
) -> (Option<ActiveSubmissionResult>, Vec<EventMsg>) {
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
        queued_input: QueuedInputQueue::new(&mut queued, QueuedInputBaseline::default()),
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
            .promote_note("session-b", "learned lesson")
            .await
            .is_err()
    );
    let session = store.snapshot("session-a").await.expect("session");
    assert_eq!(session.session[0].basis, Basis::AgentObservation);
    assert_eq!(
        store
            .promote_id("session-a", &session.session[0].id)
            .await
            .expect("promote"),
        WriteOutcome::Added
    );
    store
        .write_session("session-a", "reviewed lesson")
        .await
        .expect("write reviewed note");
    store
        .promote_note("session-a", "reviewed lesson")
        .await
        .expect("promote reviewed note");
    let session = store.snapshot("session-a").await.expect("session");
    let reviewed = session
        .session
        .iter()
        .find(|entry| entry.note == "reviewed lesson")
        .expect("reviewed note");
    assert_eq!(
        store
            .promote_id("session-a", &reviewed.id)
            .await
            .expect("confirm reviewed note"),
        WriteOutcome::Updated
    );

    let other = store.snapshot("session-b").await.expect("other session");
    assert!(other.session.is_empty());
    assert_eq!(other.global[0].note, "learned lesson");
    assert_eq!(other.global[0].basis, Basis::UserConfirmed);
    assert!(other.global[0].created_at.parse::<u64>().is_ok());
    assert_eq!(other.global[1].basis, Basis::UserConfirmed);
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
            .snapshot("session")
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
    let before = store.snapshot("session").await.expect("snapshot").session[0].clone();

    let middleware = Scratchpad::new(store.clone());
    let checkpoint = crate::backend::checkpoint::Checkpoint::empty("session");
    let session_context = crate::protocol::SessionContext::default();
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
    let after = store.snapshot("session").await.expect("snapshot").session[0].clone();
    assert_eq!(after.id, before.id);
    assert_eq!(after.created_at, before.created_at);
    assert_eq!(after.note, "revised note");
    assert_eq!(after.basis, Basis::UserConfirmed);
    assert!(
        store
            .edit("session", Scope::Session, &after.id, "second note")
            .await
            .is_err()
    );
    assert!(
        store
            .edit("session", Scope::Session, &after.id, "   ")
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
    let note_id = store.snapshot("session").await.expect("snapshot").session[0]
        .id
        .clone();
    let middleware = Scratchpad::new(store.clone());
    let arguments = format!("edit session {note_id}");

    let (result, events) =
        active_command(&middleware, "scratchpad", &arguments, Some("revised note")).await;

    assert_eq!(result, Some(ActiveSubmissionResult::Handled));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EventMsg::Frontend(FrontendEvent::Widget { .. })))
    );
    assert_eq!(
        store.snapshot("session").await.expect("snapshot").session[0].note,
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
    let note_id = store.snapshot("session").await.expect("snapshot").session[0]
        .id
        .clone();
    let middleware = Scratchpad::new(store.clone());

    let (result, events) = active_command(
        &middleware,
        "scratchpad",
        &format!("promote {note_id}"),
        None,
    )
    .await;

    assert_eq!(result, Some(ActiveSubmissionResult::Handled));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EventMsg::Frontend(FrontendEvent::Widget { .. })))
    );
    let snapshot = store.snapshot("session").await.expect("snapshot");
    assert_eq!(snapshot.global[0].note, "promote this");
    assert_eq!(snapshot.global[0].basis, Basis::UserConfirmed);
}

#[tokio::test]
async fn active_command_defers_when_scratchpad_lock_is_busy() {
    let (_temporary, store) = store().await;
    let middleware = Scratchpad::new(store.clone());
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
async fn disabled_agent_keeps_read_only_surfaces_without_prompt_or_tools() {
    let (_temporary, store) = store().await;
    store
        .write_session("session", "historical note")
        .await
        .expect("seed note");
    let note_id = store.snapshot("session").await.expect("snapshot").session[0]
        .id
        .clone();
    let middleware = Scratchpad::new(store.clone()).agent_enabled(false);
    let frontend_events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_events = Arc::clone(&frontend_events);
    let runtime = RuntimeContext {
        checkpoints: Arc::clone(&store.checkpoints),
        session_id: "session".into(),
        model_route: "model".into(),
        model: "model".into(),
        approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
        session_context: crate::protocol::SessionContext::default(),
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
    assert!(catalog.definitions().is_empty());
    assert_eq!(middleware.prompt_section(&runtime).expect("prompt"), None);
    let contribution = middleware.frontend();
    assert_eq!(contribution.commands.len(), 1);
    assert_eq!(contribution.commands[0].name, "scratchpad");
    assert_eq!(contribution.widgets.len(), 2);
    let mut input = Vec::new();
    let mut start = crate::middleware::SessionStartContext {
        runtime: &runtime,
        source: crate::middleware::SessionStartSource::Startup,
        queued_input: Default::default(),
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
    let session_context = crate::protocol::SessionContext::default();
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
        store.snapshot("session").await.expect("retained").session[0].note,
        "historical note"
    );
}

fn runtime(store: &ScratchpadStore, session_id: &str) -> RuntimeContext {
    RuntimeContext {
        checkpoints: Arc::clone(&store.checkpoints),
        session_id: session_id.into(),
        model_route: "model".into(),
        model: "model".into(),
        approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
        session_context: crate::protocol::SessionContext::default(),
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
    let middleware = Scratchpad::new(store.clone());
    let runtime = runtime(&store, "session");
    let mut seeded = Vec::new();
    let mut start = crate::middleware::SessionStartContext {
        runtime: &runtime,
        source: crate::middleware::SessionStartSource::Startup,
        queued_input: Default::default(),
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
        store.snapshot("session").await.expect("snapshot")
    );
}

#[tokio::test]
async fn startup_keeps_one_inherited_baseline_projection() {
    let (_temporary, store) = store().await;
    store
        .write_session("session", "remember this")
        .await
        .expect("write note");
    let snapshot = store.snapshot("session").await.expect("snapshot");
    let inherited = scratchpad_message(&snapshot).expect("baseline");
    let middleware = Scratchpad::new(store.clone());
    let runtime = runtime(&store, "session");
    let mut input = vec![inherited.clone()];
    let mut start = crate::middleware::SessionStartContext {
        runtime: &runtime,
        source: crate::middleware::SessionStartSource::Startup,
        queued_input: Default::default(),
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
        global: Vec::new(),
    };
    let input = vec![scratchpad_message(&snapshot).expect("baseline")];

    assert!(
        next_projection(&input, &snapshot)
            .expect("compare projection")
            .is_none()
    );
}

#[test]
fn disabled_scratchpad_removes_durable_projection_items() {
    let snapshot = Snapshot {
        session: vec![entry("private note")],
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
fn surfaces_are_scope_specific_action_lists_without_subtext() {
    let session = entry("Prefer focused tests");
    let mut global = entry("Use generic UI records");
    global.basis = Basis::UserConfirmed;
    let snapshot = Snapshot {
        session: vec![session],
        global: vec![global],
    };
    let widgets = surface_widgets(&snapshot);

    let Some(FrontendWidgetContent::ActionList { title, items }) = &widgets[0].content else {
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

    let Some(FrontendWidgetContent::ActionList { title, items }) = &widgets[1].content else {
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
        ["Promote", "Edit", "Delete"]
    );
    let Op::CapabilityCommand { input, .. } = &items[0].actions[1].op else {
        panic!("edit should submit a capability command");
    };
    assert_eq!(input.as_deref(), Some("Prefer focused tests"));
    assert_eq!(
        action_list_item(Scope::Session, &entry("Already global"), true)
            .actions
            .into_iter()
            .map(|action| action.label)
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
    let middleware = Scratchpad::new(store.clone());
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
            session_id: "session".into(),
            frontend: frontend_sink(),
        }
        .approval(),
        ApprovalRequirement::Never
    );
    assert_eq!(
        PromoteScratchpad {
            store,
            session_id: "session".into(),
            frontend: frontend_sink(),
        }
        .approval(),
        ApprovalRequirement::Always
    );
}
