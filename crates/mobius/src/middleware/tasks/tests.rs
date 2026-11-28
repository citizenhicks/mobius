use std::sync::Mutex;

use crate::backend::sandbox::local::LocalSandbox;
use crate::backend::sandbox::{ApprovalPolicy, NetworkAccess, Sandbox, SandboxPermissions};
use crate::middleware::tools::{Tools, execute_batch};
use crate::protocol::ToolCall;

use super::*;

#[tokio::test]
async fn write_todos_persists_session_state_and_updates_the_widget() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let frontend_events = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::clone(&frontend_events);
    let mut runtime = crate::middleware::tests::lifecycle_runtime(temporary.path());
    let checkpoints = Arc::clone(&runtime.checkpoints);
    runtime.session_id = "session-a".parse().expect("session ID");
    runtime.model_route = "default".into();
    runtime.frontend = Arc::new(move |event| {
        events.lock().expect("frontend events").push(event);
        Ok(())
    });
    let tasks = Tasks;
    let mut catalog = Catalog::default();
    tasks.register(&mut catalog, &runtime).expect("register");
    Tools::new(Vec::new())
        .register(&mut catalog, &runtime)
        .expect("register tools_search owner");
    catalog.finalize().expect("finalize catalog");
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(LocalSandbox::new(temporary.path()).expect("sandbox")),
        ApprovalPolicy::Ask,
    ));
    let permissions = SandboxPermissions::restore(
        &"session-a".parse().expect("session ID"),
        crate::backend::sandbox::SandboxMode::WorkspaceWrite,
        NetworkAccess::Denied,
        Vec::new(),
    );

    let searchable = std::collections::BTreeSet::from(["write_todos".into()]);
    let bound = catalog
        .bind_call(
            ToolCall {
                call_id: "write".into(),
                name: "write_todos".into(),
                arguments: serde_json::json!({"todos": [
                    {"content": " inspect seams ", "status": "completed"},
                    {"content": "Implement tasks", "status": "in_progress"}
                ]}),
            },
            &searchable,
            &searchable,
        )
        .expect("bind call");
    let result = execute_batch(
        &catalog,
        [bound],
        Arc::clone(&sandbox),
        &permissions,
        &crate::middleware::tools::ToolStep {
            turn_id: &"turn-a".into(),
            model_step_id: Some(&"step".into()),
            model_route: &"test".into(),
            author: &crate::protocol::MessageAuthor::User.into(),
        },
    )
    .await
    .pop()
    .expect("tool result");

    assert!(!result.is_error, "{}", result.output.text());
    assert_eq!(
        load_todos(&checkpoints, &"session-a".parse().expect("session ID"))
            .await
            .expect("saved todos")[0]
            .content,
        "inspect seams"
    );
    assert!(
        checkpoints
            .load_state("session-b", STATE_KEY)
            .await
            .expect("other session")
            .is_none()
    );
    assert!(matches!(
        frontend_events.lock().expect("frontend events").last(),
        Some(FrontendEvent::Widget { item, .. })
            if item.text == "1/2"
                && item.progress == Some(FrontendProgress { completed: 1, total: 2 })
                && item.action.is_none()
                && matches!(
                    &item.content,
                    Some(FrontendWidgetContent::ActionList { title, items, .. })
                        if title == "Tasks"
                            && items.len() == 2
                            && items[0].text == "inspect seams"
                            && items[0].state == FrontendListItemState::Completed
                            && items[1].state == FrontendListItemState::InProgress
                            && items.iter().all(|item| item.actions.is_empty())
                )
    ));

    let user = Arc::new(crate::backend::model::user_message("continue the work"));
    let mut input = vec![Arc::clone(&user)];
    for source in [SessionStartSource::Startup, SessionStartSource::Resume] {
        tasks
            .session_start(&mut SessionStartContext {
                delivery_once: crate::middleware::delivery_once::DeliveryOnce::testing(),
                runtime: &runtime,
                source,
                queued_messages: Default::default(),
                input: &mut input,
                input_changed: false,
                stop_reason: None,
            })
            .await
            .expect("restore saved task state");
        assert_eq!(input.len(), 2, "unchanged task state must not be repeated");
        assert!(
            input[1]["content"][0]["text"]
                .as_str()
                .expect("task text")
                .contains("Implement tasks")
        );
        crate::backend::model::mark_prompt_cache_breakpoint(Arc::make_mut(&mut input[1]));
    }
    tasks.prepare_compacted_input((&[]).into(), &mut input);
    assert_eq!(input.as_slice(), std::slice::from_ref(&user));
    let previous_events = frontend_events.lock().expect("events").len();
    tasks
        .session_start(&mut SessionStartContext {
            delivery_once: crate::middleware::delivery_once::DeliveryOnce::testing(),
            runtime: &runtime,
            source: SessionStartSource::Compact,
            queued_messages: Default::default(),
            input: &mut input,
            input_changed: false,
            stop_reason: None,
        })
        .await
        .expect("restore after compaction");
    assert_eq!(input.len(), 2);
    assert_eq!(
        frontend_events.lock().expect("events").len(),
        previous_events,
        "compaction does not repeat widgets"
    );

    let clear = catalog
        .bind_call(
            ToolCall {
                call_id: "clear".into(),
                name: "write_todos".into(),
                arguments: serde_json::json!({"todos": []}),
            },
            &searchable,
            &searchable,
        )
        .expect("bind clear");
    let result = execute_batch(
        &catalog,
        [clear],
        sandbox,
        &permissions,
        &crate::middleware::tools::ToolStep {
            turn_id: &"turn-a".into(),
            model_step_id: Some(&"step".into()),
            model_route: &"test".into(),
            author: &crate::protocol::MessageAuthor::User.into(),
        },
    )
    .await
    .pop()
    .expect("clear result");
    assert!(!result.is_error, "{}", result.output.text());
    assert!(matches!(
        frontend_events.lock().expect("events").last(),
        Some(FrontendEvent::RemoveWidget { .. })
    ));
    for source in [SessionStartSource::Resume, SessionStartSource::Compact] {
        if source == SessionStartSource::Compact {
            tasks.prepare_compacted_input((&[]).into(), &mut input);
        }
        tasks
            .session_start(&mut SessionStartContext {
                delivery_once: crate::middleware::delivery_once::DeliveryOnce::testing(),
                runtime: &runtime,
                source,
                queued_messages: Default::default(),
                input: &mut input,
                input_changed: false,
                stop_reason: None,
            })
            .await
            .expect("restore cleared list");
        let latest = input.last().expect("authoritative empty state")["content"][0]["text"]
            .as_str()
            .expect("text");
        assert!(latest.contains("\n[]\n"));
        assert!(!latest.contains("Implement tasks"));
    }
}
