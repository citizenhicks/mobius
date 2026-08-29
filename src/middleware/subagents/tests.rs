use std::sync::Arc;
use std::time::Duration;

use super::runtime::AgentPresentation;
use super::*;
use crate::middleware::MessageQueue;
use crate::protocol::{
    Event, MessageAuthor, MessageDelivery, MessageEvent, ToolCallBeginEvent, ToolCallEndEvent,
    TurnCompleteEvent, TurnStartedEvent,
};

fn test_middleware() -> Subagents {
    Subagents::new(
        1,
        2,
        2,
        Arc::new(|_| Box::pin(async { Err(Error::Stopped("unused".into())) })),
    )
    .expect("subagents middleware")
}

fn preview_messages(events: &[crate::protocol::FrontendPreviewEvent]) -> Vec<EventMsg> {
    events.iter().map(|event| event.event.clone()).collect()
}

async fn append_events(checkpoints: &dyn CheckpointStore, session_id: &str, events: Vec<EventMsg>) {
    for (index, msg) in events.into_iter().enumerate() {
        checkpoints
            .append_event(
                session_id,
                i64::try_from(index).expect("event timestamp"),
                &Event {
                    submission_id: None,
                    msg,
                },
            )
            .await
            .expect("append preview event");
    }
}

#[test]
fn prompt_section_guides_root_to_delegate_parallel_work() {
    let identity = AgentIdentity::read("root", &BTreeMap::new()).expect("root identity");
    let section = test_middleware().section(&identity);

    assert_eq!(
        section.body,
        "Delegate independent work to subagents when it can run in parallel. Spawn with fresh context by default; include recent turns only when the task requires them, and full history only when essential. They share your workspace; continue your own work while they run, and wait only when you need their results."
    );
}

#[test]
fn prompt_section_identifies_child_with_default_instruction() {
    let identity = AgentIdentity {
        root_session_id: "root".into(),
        agent_path: "/root/reviewer".into(),
        depth: 1,
    };
    let section = test_middleware().section(&identity);

    assert_eq!(
        section.body,
        "You are `/root/reviewer`, a child agent.\nComplete the task and report concisely to your parent."
    );
}

#[test]
fn prompt_section_uses_configured_child_instruction() {
    let identity = AgentIdentity {
        root_session_id: "root".into(),
        agent_path: "/root/reviewer".into(),
        depth: 1,
    };
    let middleware = test_middleware()
        .prompt("Review the parser and report findings.")
        .expect("custom child prompt");

    let section = middleware.section(&identity);

    assert_eq!(
        section.body,
        "You are `/root/reviewer`, a child agent.\nReview the parser and report findings."
    );
}

#[test]
fn renders_every_subagent_tool_call() {
    let middleware = Subagents::new(
        1,
        2,
        2,
        Arc::new(|_| Box::pin(async { Err(Error::Stopped("unused".into())) })),
    )
    .expect("subagents middleware");

    for name in [
        "spawn_agent",
        "send_message",
        "followup_task",
        "list_agents",
        "interrupt_agent",
        "wait_agent",
    ] {
        assert!(
            middleware
                .render(
                    &EventMsg::ToolCallBegin(ToolCallBeginEvent {
                        turn_id: "turn".into(),
                        call_id: "call".into(),
                        name: name.into(),
                        arguments: serde_json::json!({}),
                    }),
                    "session"
                )
                .is_some(),
            "missing begin renderer for {name}"
        );
        assert!(
            middleware
                .render(
                    &EventMsg::ToolCallEnd(ToolCallEndEvent {
                        turn_id: "turn".into(),
                        call_id: "call".into(),
                        name: name.into(),
                        output: String::new(),
                        is_error: false,
                    }),
                    "session"
                )
                .is_some(),
            "missing end renderer for {name}"
        );
    }
}

#[test]
fn wait_agent_matches_the_sandbox_timeout_limit() {
    assert_eq!(
        wait_parameters()["properties"]["timeout_ms"]["maximum"],
        serde_json::json!(120_000)
    );
    assert_eq!(
        wait_timeout(Some(120_000)).expect("maximum timeout"),
        Duration::from_secs(120)
    );
    assert!(matches!(
        wait_timeout(Some(120_001)),
        Err(Error::Tool(message))
            if message == "timeout_ms must be between 10000 and 120000"
    ));
}

#[tokio::test]
async fn active_command_emits_a_subagent_transcript_preview() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        crate::backend::checkpoint::sqlite::SqliteCheckpoint::new(
            workspace.path().join("checkpoints.sqlite3"),
        )
        .expect("checkpoint store"),
    );
    let root = Checkpoint::empty("root");
    checkpoints.save(&root, &[], None).await.expect("save root");
    let transcript = serde_json::json!({"role": "user", "content": "review this"});
    let mut child = Checkpoint::empty("child");
    child.sequence = 1;
    child.context.push(transcript.clone());
    checkpoints
        .save(&child, &[transcript], None)
        .await
        .expect("save child");
    append_events(
        checkpoints.as_ref(),
        "child",
        vec![
            EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "turn-1".into(),
                model_context_window: None,
            }),
            EventMsg::Message(MessageEvent {
                author: MessageAuthor::User,
                delivery: MessageDelivery::Turn,
                text: "review this".into(),
                attachments: Vec::new(),
                message_target: None,
            }),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".into(),
            }),
        ],
    )
    .await;
    let middleware = Subagents::new(
        1,
        2,
        2,
        Arc::new(|_| Box::pin(async { Err(Error::Stopped("unused".into())) })),
    )
    .expect("subagents middleware");
    middleware
        .shared
        .session_start(RuntimeContext {
            sender: crate::agent::test_sender(),
            checkpoints,
            session_id: root.session_id.clone(),
            model_route: "test".into(),
            model: "model".into(),
            approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
            session_context: root.session_context.clone(),
            metadata: root.metadata.clone(),
            role: crate::agent::AgentRole::Main,
            frontend: Arc::new(|_| Ok(())),
        })
        .await
        .expect("initialize runtime");
    middleware
        .shared
        .reserve(
            "root",
            "/root/reviewer",
            "/root",
            "child".into(),
            1,
            AgentPresentation {
                model: "test".into(),
                spawn_context: String::new(),
            },
        )
        .await
        .expect("reserve child");
    let mut queued = Vec::new();
    let mut events = Vec::new();

    let result = middleware
        .active_command(&mut ActiveCommandContext {
            submission_id: "preview-1",
            session_id: "root",
            metadata: &root.metadata,
            active_turn_id: "turn-1",
            command: "subagents",
            arguments: "/root/reviewer",
            input: None,
            target: None,
            queued_messages: MessageQueue::new(&mut queued),
            events: &mut events,
        })
        .await
        .expect("active command");

    assert_eq!(result, Some(SubmissionResult::Handled));
    assert!(matches!(
        events.as_slice(),
        [EventMsg::Frontend(FrontendEvent::Preview {
            id,
            title,
            subtitle,
            page_id,
            update: FrontendPreviewUpdate::Replace,
            events,
            next: None,
        })]
            if id == "/root/reviewer"
                && title == "reviewer"
                && subtitle.is_empty()
                && page_id == "/root/reviewer:latest"
                && matches!(preview_messages(events).as_slice(), [
                    EventMsg::TurnStarted(_),
                    EventMsg::Message(message),
                    EventMsg::TurnComplete(_),
                ] if message.text == "review this")
    ));
    assert!(queued.is_empty());
}

#[tokio::test]
async fn preview_continuation_loads_one_older_turn_through_registered_command() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        crate::backend::checkpoint::sqlite::SqliteCheckpoint::new(
            workspace.path().join("checkpoints.sqlite3"),
        )
        .expect("checkpoint store"),
    );
    let root = Checkpoint::empty("root");
    checkpoints.save(&root, &[], None).await.expect("save root");
    let child = Checkpoint::empty("child");
    checkpoints
        .save(&child, &[], None)
        .await
        .expect("save child");
    append_events(
        checkpoints.as_ref(),
        "child",
        vec![
            EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "older".into(),
                model_context_window: None,
            }),
            EventMsg::Message(MessageEvent {
                author: MessageAuthor::User,
                delivery: MessageDelivery::Turn,
                text: "Older question".into(),
                attachments: Vec::new(),
                message_target: None,
            }),
            EventMsg::ContextCompacted,
            EventMsg::AssistantMessage(crate::protocol::AssistantMessageEvent {
                session_id: "child".into(),
                turn_id: "older".into(),
                model_step_id: "older-step".into(),
                content: vec![crate::protocol::ModelStepContent {
                    output_index: 0,
                    part_index: 0,
                    phase: crate::protocol::ModelStepContentPhase::FinalAnswer,
                    text: "Older answer".into(),
                    annotations: Vec::new(),
                }],
                message_target: None,
            }),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "older".into(),
            }),
            EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "latest".into(),
                model_context_window: None,
            }),
            EventMsg::Message(MessageEvent {
                author: MessageAuthor::User,
                delivery: MessageDelivery::Turn,
                text: "Latest question".into(),
                attachments: Vec::new(),
                message_target: None,
            }),
            EventMsg::Message(MessageEvent {
                author: MessageAuthor::User,
                delivery: MessageDelivery::Steer,
                text: "Steer latest".into(),
                attachments: Vec::new(),
                message_target: None,
            }),
            EventMsg::AssistantMessage(crate::protocol::AssistantMessageEvent {
                session_id: "child".into(),
                turn_id: "latest".into(),
                model_step_id: "latest-step".into(),
                content: vec![crate::protocol::ModelStepContent {
                    output_index: 0,
                    part_index: 0,
                    phase: crate::protocol::ModelStepContentPhase::FinalAnswer,
                    text: "Latest answer".into(),
                    annotations: Vec::new(),
                }],
                message_target: None,
            }),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "latest".into(),
            }),
        ],
    )
    .await;
    let middleware = Arc::new(test_middleware());
    middleware
        .shared
        .session_start(RuntimeContext {
            sender: crate::agent::test_sender(),
            checkpoints: Arc::clone(&checkpoints),
            session_id: root.session_id.clone(),
            model_route: "test".into(),
            model: "model".into(),
            approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
            session_context: root.session_context.clone(),
            metadata: root.metadata.clone(),
            role: crate::agent::AgentRole::Main,
            frontend: Arc::new(|_| Ok(())),
        })
        .await
        .expect("initialize runtime");
    middleware
        .shared
        .reserve(
            "root",
            "/root/reviewer",
            "/root",
            "child".into(),
            1,
            AgentPresentation {
                model: "test".into(),
                spawn_context: "Full context".into(),
            },
        )
        .await
        .expect("reserve child");
    let stack =
        crate::middleware::MiddlewareStack::new(vec![middleware]).expect("middleware stack");

    let latest = stack
        .command(
            "subagents",
            MiddlewareCommandContext {
                command: "subagents",
                arguments: "/root/reviewer",
                input: None,
                target: None,
                session_id: "root",
                session_context: &root.session_context,
                checkpoint: &root,
                checkpoints: Arc::clone(&checkpoints),
            },
        )
        .await
        .expect("latest preview");
    let [
        FrontendEvent::Preview {
            update: FrontendPreviewUpdate::Replace,
            events,
            next: Some(Op::CapabilityCommand {
                command, arguments, ..
            }),
            ..
        },
    ] = latest.events.as_slice()
    else {
        panic!("expected latest preview with continuation");
    };
    assert_eq!(command, "subagents");
    assert!(matches!(
        preview_messages(events).as_slice(),
        [
            EventMsg::TurnStarted(_),
            EventMsg::Message(question),
            EventMsg::Message(steering),
            EventMsg::AssistantMessage(answer),
            EventMsg::TurnComplete(_),
        ] if question.text == "Latest question"
            && steering.text == "Steer latest"
            && answer.content[0].text == "Latest answer"
    ));

    let older = stack
        .command(
            "subagents",
            MiddlewareCommandContext {
                command: "subagents",
                arguments,
                input: None,
                target: None,
                session_id: "root",
                session_context: &root.session_context,
                checkpoint: &root,
                checkpoints,
            },
        )
        .await
        .expect("older preview");
    assert!(matches!(
        older.events.as_slice(),
        [FrontendEvent::Preview {
            update: FrontendPreviewUpdate::Prepend,
            events,
            next: None,
            ..
        }] if matches!(
            preview_messages(events).as_slice(),
            [
                EventMsg::TurnStarted(_),
                EventMsg::Message(question),
                EventMsg::ContextCompacted,
                EventMsg::AssistantMessage(answer),
                EventMsg::TurnComplete(_),
            ] if question.text == "Older question" && answer.content[0].text == "Older answer"
        )
    ));
}

#[tokio::test]
async fn preview_cursor_rejects_unknown_fields_and_zero_offsets() {
    let middleware = test_middleware();
    for arguments in [
        r#"{"path":"/root/reviewer","before_sequence":2,"extra":true}"#,
        r#"{"path":" /root/reviewer","before_sequence":2}"#,
        r#"{"path":"/root/reviewer","before_sequence":0}"#,
    ] {
        let error = middleware
            .read_preview_page("root", &BTreeMap::new(), arguments)
            .await
            .err()
            .expect("invalid cursor must fail closed");

        assert!(
            matches!(error, Error::Tool(message) if message == "invalid subagent preview cursor")
        );
    }
}

#[tokio::test]
async fn fork_persists_the_metadata_passed_to_the_child() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        crate::backend::checkpoint::sqlite::SqliteCheckpoint::new(
            workspace.path().join("checkpoints.sqlite3"),
        )
        .expect("checkpoint store"),
    );
    let mut parent = Checkpoint::empty("parent");
    parent.metadata.insert(
        "gateway.chat".into(),
        serde_json::json!({"workspace": "/srv/project"}),
    );
    checkpoints
        .save(&parent, &[], None)
        .await
        .expect("save parent");
    let launched = Arc::new(std::sync::Mutex::new(None));
    let launcher: SubagentLauncher = Arc::new({
        let launched = Arc::clone(&launched);
        move |launch| {
            *launched.lock().expect("launch metadata lock") = Some(launch);
            Box::pin(async { Err(Error::Stopped("test launch stopped".into())) })
        }
    });
    let runtime = RuntimeContext {
        sender: crate::agent::test_sender(),
        checkpoints: Arc::clone(&checkpoints),
        session_id: parent.session_id.clone(),
        model_route: "test".into(),
        model: "model".into(),
        approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
        session_context: parent.session_context.clone(),
        metadata: parent.metadata.clone(),
        role: crate::agent::AgentRole::Main,
        frontend: Arc::new(|_| Ok(())),
    };
    let scope = AgentScope::new(&runtime, launcher).expect("agent scope");

    let result = scope
        .fork(
            "child".into(),
            "/root/child".into(),
            "test".into(),
            None,
            ForkTurns::None,
            "turn".into(),
        )
        .await;
    assert!(matches!(result, Err(Error::Stopped(_))));
    let child = checkpoints
        .load("child")
        .await
        .expect("load child")
        .expect("child checkpoint");
    let launched = launched
        .lock()
        .expect("launch metadata lock")
        .clone()
        .expect("launched metadata");
    let identity = AgentIdentity::read("child", &child.metadata).expect("child identity");

    assert_eq!(child.metadata, launched.metadata);
    assert_eq!(
        launched.role,
        crate::agent::AgentRole::Subagent {
            parent_session_id: "parent".into(),
            parent_turn_id: "turn".into(),
        }
    );
    assert_eq!(
        child.metadata.get("gateway.chat"),
        parent.metadata.get("gateway.chat")
    );
    assert_eq!(identity.root_session_id, "parent");
    assert_eq!(identity.agent_path, "/root/child");
    assert_eq!(identity.depth, 1);
    assert_eq!(
        child.metadata.get(SPAWN_CONTEXT_KEY),
        Some(&Value::String("No context".into()))
    );
}

#[test]
fn cleanup_failures_preserve_both_errors() {
    let error = cleanup_error(
        Error::Tool("launch failed".into()),
        Err(Error::Checkpoint("cleanup failed".into())),
    );

    assert!(matches!(
        error,
        Error::Rollback { primary, rollback }
            if matches!(*primary, Error::Tool(_))
                && matches!(*rollback, Error::Checkpoint(_))
    ));
}

#[test]
fn forked_context_drops_session_owned_attachment_references() {
    let context = vec![serde_json::json!({
        "role": "user",
        "content": [{"type": "input_text", "text": "inspect"}],
        "_mobius_attachments": [{
            "id": "378b8581-e96c-4413-a138-93e74561cb87",
            "name": "photo.png",
            "size": 1,
            "media_type": "image/png"
        }]
    })];

    let fork = fork_context(&context, ForkTurns::All);

    assert!(fork[0].get("_mobius_attachments").is_none());
}

#[tokio::test]
async fn supervised_lifecycle_outlives_a_cancelled_caller() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
    let caller = tokio::spawn(supervise(async move {
        entered_tx.send(()).expect("signal lifecycle start");
        release_rx.await.expect("release lifecycle");
        completed_tx.send(()).expect("signal lifecycle completion");
        Ok(())
    }));

    entered_rx.await.expect("lifecycle started");
    caller.abort();
    assert!(
        caller
            .await
            .expect_err("caller should be cancelled")
            .is_cancelled(),
        "caller should stop before the lifecycle is released"
    );
    release_tx.send(()).expect("release lifecycle");

    tokio::time::timeout(Duration::from_secs(1), completed_rx)
        .await
        .expect("lifecycle continued after caller cancellation")
        .expect("lifecycle completion signal");
}
