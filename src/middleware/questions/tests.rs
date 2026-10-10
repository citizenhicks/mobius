use super::*;
use crate::agent::{Agent, AgentConfig, create_agent};
use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
use crate::backend::model::{Model, ModelEventSink, ModelOutput, ModelRequest, ModelRouter};
use crate::backend::sandbox::local::LocalSandbox;
use crate::backend::sandbox::{
    ApprovalPolicy, NetworkAccess, Sandbox, SandboxMode, SandboxPermissions,
};
use crate::middleware::{MiddlewareStack, messages::Messages};
use crate::protocol::{MessageDelivery, SessionContext, TokenUsage};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

fn question(id: &str, at: i64) -> Question {
    Question {
        id: id.into(),
        title: "Choose {answer}?".into(),
        options: vec!["A".into(), "B".into()],
        asked_at: at,
        outcome: None,
    }
}

struct TestModel {
    block: AtomicBool,
    release: Notify,
}
impl Model for TestModel {
    fn respond<'a>(
        &'a self,
        _: ModelRequest,
        _: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(async move {
            if self.block.swap(false, Ordering::SeqCst) {
                self.release.notified().await;
            }
            ModelOutput::from_output(
                vec![serde_json::json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Done."}]
                })],
                true,
                TokenUsage::default(),
            )
        })
    }
}

async fn next(agent: &mut Agent, predicate: impl Fn(&EventMsg) -> bool) -> EventMsg {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = agent.next_event().await.expect("agent event");
            assert!(
                !matches!(event.msg, EventMsg::Error(_) | EventMsg::TurnAborted(_)),
                "unexpected event: {:?}",
                event.msg
            );
            if predicate(&event.msg) {
                return event.msg;
            }
        }
    })
    .await
    .expect("event deadline")
}

async fn agent(
    path: &std::path::Path,
    checkpoints: Arc<dyn CheckpointStore>,
    questions: Arc<Questions>,
    model: Arc<TestModel>,
) -> Agent {
    create_agent(
        AgentConfig::new(
            Arc::new(ModelRouter::new("test", model)),
            Arc::new(Sandbox::new(
                Arc::new(LocalSandbox::new(path).expect("sandbox")),
                ApprovalPolicy::Allow,
            )),
            checkpoints,
            MiddlewareStack::new(vec![Arc::new(Messages::default()), questions])
                .expect("middleware"),
            "test",
        )
        .session_id("session")
        .session_context(SessionContext {
            owner_id: "test-owner".into(),
            ..SessionContext::default()
        }),
    )
    .await
    .expect("agent")
}

#[tokio::test]
async fn answers_are_user_messages_during_active_and_idle_turns() {
    for active in [false, true] {
        let temp = tempfile::tempdir().expect("temp");
        let store: Arc<dyn CheckpointStore> =
            Arc::new(SqliteCheckpoint::new(temp.path().join("db")).expect("store"));
        store
            .save_state(
                "session",
                STATE_KEY,
                &serde_json::to_value(vec![question("q", 1)]).expect("json"),
            )
            .await
            .expect("seed");
        let questions = Arc::new(Questions::default());
        let model = Arc::new(TestModel {
            block: AtomicBool::new(active),
            release: Notify::new(),
        });
        let mut agent = agent(
            temp.path(),
            Arc::clone(&store),
            Arc::clone(&questions),
            Arc::clone(&model),
        )
        .await;
        if active {
            agent
                .sender()
                .send(Submission::message(MessageSubmission {
                    author: MessageAuthor::User,
                    text: "start".into(),
                    attachments: Vec::new(),
                    reply: None,
                    requested_delivery: None,
                    target_turn_id: None,
                }))
                .expect("start");
            next(&mut agent, |event| {
                matches!(event, EventMsg::TurnStarted(_))
            })
            .await;
        }
        agent
            .sender()
            .submit(Op::CapabilityCommand {
                capability: "questions".into(),
                command: "answer_question".into(),
                arguments: "q".into(),
                input: Some("A".into()),
                target: None,
            })
            .expect("answer");
        if active {
            next(&mut agent, |event| {
                matches!(event, EventMsg::Frontend(FrontendEvent::Widget { capability, item })
                    if capability == "messages" && item.text == "> Choose {answer}?\n\nA")
            })
            .await;
            model.release.notify_one();
        }
        let event = next(
            &mut agent,
            |event| matches!(event, EventMsg::Message(message) if message.text.starts_with("> ")),
        )
        .await;
        let EventMsg::Message(message) = event else {
            unreachable!()
        };
        assert_eq!(message.text, "> Choose {answer}?\n\nA");
        assert_eq!(message.author, MessageAuthor::User);
        assert_eq!(
            message.delivery,
            if active {
                MessageDelivery::Steer
            } else {
                MessageDelivery::Turn
            }
        );
        let session = questions.session("session").expect("session");
        let saved = session.load().await.expect("load");
        assert!(matches!(&saved[0].outcome, Some(Outcome::Answered { .. })));
        assert!(
            session
                .command("answer_question", "q", Some("B"), None)
                .await
                .is_err()
        );
        let Some(FrontendWidgetContent::ActionList { items, .. }) = widget(saved).content else {
            panic!("list")
        };
        assert_eq!(items[0].state, FrontendListItemState::Completed);
        assert!(items[0].actions.is_empty());
        next(&mut agent, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
    }
}

#[tokio::test]
async fn concurrent_answers_close_a_question_once() {
    let temp = tempfile::tempdir().expect("temp");
    let store: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(temp.path().join("db")).expect("store"));
    store
        .save_state("session", STATE_KEY, &serde_json::json!([question("q", 1)]))
        .await
        .expect("seed");
    let questions = Arc::new(Questions::default());
    let mut agent = agent(
        temp.path(),
        Arc::clone(&store),
        Arc::clone(&questions),
        Arc::new(TestModel {
            block: AtomicBool::new(false),
            release: Notify::new(),
        }),
    )
    .await;
    let checkpoint = crate::backend::checkpoint::Checkpoint::empty("session");
    let context = SessionContext {
        owner_id: "test-owner".into(),
        ..SessionContext::default()
    };
    let answer = |input| {
        questions.command(MiddlewareCommandContext {
            command: "answer_question",
            arguments: "q",
            input: Some(input),
            target: None,
            session_id: "session",
            session_context: &context,
            checkpoint: &checkpoint,
            checkpoints: Arc::clone(&store),
        })
    };
    let (first, second) = tokio::join!(answer("A"), answer("B"));
    assert_ne!(first.is_ok(), second.is_ok());
    let saved = questions
        .session("session")
        .expect("session")
        .load()
        .await
        .expect("load");
    let Some(Outcome::Answered { text, .. }) = &saved[0].outcome else {
        panic!("answered once")
    };
    assert_eq!(text, if first.is_ok() { "A" } else { "B" });
    let mut messages = Vec::new();
    loop {
        match next(&mut agent, |event| {
            matches!(event, EventMsg::Message(_) | EventMsg::TurnComplete(_))
        })
        .await
        {
            EventMsg::Message(message) => messages.push(message.text),
            EventMsg::TurnComplete(_) => break,
            _ => unreachable!(),
        }
    }
    assert_eq!(messages, [format!("> Choose {{answer}}?\n\n{text}")]);
}

#[tokio::test]
async fn startup_restores_open_answered_and_skipped_questions() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("db");
    let mut answered = question("answered", 2);
    answered.outcome = Some(Outcome::Answered {
        text: "A".into(),
        at: 4,
    });
    let mut skipped = question("skipped", 3);
    skipped.outcome = Some(Outcome::Skipped { at: 5 });
    let store = SqliteCheckpoint::new(&path).expect("store");
    store
        .save_state(
            "session",
            STATE_KEY,
            &serde_json::json!([question("open", 1), answered, skipped]),
        )
        .await
        .expect("seed");
    drop(store);
    let mut agent = agent(
        temp.path(),
        Arc::new(SqliteCheckpoint::new(path).expect("reopen store")),
        Arc::new(Questions::default()),
        Arc::new(TestModel {
            block: AtomicBool::new(false),
            release: Notify::new(),
        }),
    )
    .await;
    let EventMsg::Frontend(FrontendEvent::Widget { item, .. }) = next(&mut agent, |event| {
        matches!(event, EventMsg::Frontend(FrontendEvent::Widget { capability, .. }) if capability == "questions")
    }).await else { unreachable!() };
    assert_eq!(item.text, "1");
    let Some(FrontendWidgetContent::ActionList { items, .. }) = item.content else {
        panic!("list")
    };
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].id, "open");
    assert_eq!(items[0].actions.len(), 4);
    assert!(items[1..].iter().all(|item| item.actions.is_empty()));
}

#[tokio::test]
async fn ask_skip_restore_and_bounds_are_durable() {
    let temp = tempfile::tempdir().expect("temp");
    let store: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(temp.path().join("db")).expect("store"));
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let session = Arc::new(QuestionSession {
        id: "session".into(),
        checkpoints: store,
        sender: crate::agent::test_sender(),
        frontend: Arc::new(move |event| {
            captured.lock().expect("events").push(event);
            Ok(())
        }),
        access: tokio::sync::Mutex::new(()),
    });
    let tool = AskUser(Arc::clone(&session));
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(LocalSandbox::new(temp.path()).expect("sandbox")),
        ApprovalPolicy::Allow,
    ));
    let permissions = SandboxPermissions::restore(
        "session",
        SandboxMode::WorkspaceWrite,
        NetworkAccess::Denied,
        Vec::new(),
    );
    for i in 0..=MAX_OPEN {
        let context = ToolContext::new(
            Arc::clone(&sandbox),
            permissions.for_call(&i.to_string()),
            "turn",
        )
        .with_call_id(&i.to_string());
        let result = tool
            .call(
                context,
                serde_json::json!({"title":"Choose", "options":["A","B"]}),
            )
            .await;
        assert_eq!(result.is_ok(), i < MAX_OPEN);
    }
    let saved = session.load().await.expect("load");
    assert_eq!(saved.len(), MAX_OPEN);
    assert_eq!(
        saved
            .iter()
            .filter(|question| question.outcome.is_none())
            .count(),
        MAX_OPEN
    );
    let Some(FrontendWidgetContent::ActionList { items, .. }) = widget(saved).content else {
        panic!("list")
    };
    assert_eq!(items[0].actions.len(), 4);
    assert!(!tool.cancel_on_input());
    assert!(
        session
            .command(
                "answer_question",
                "0",
                Some(&"a".repeat(MAX_ANSWER_BYTES + 1)),
                None
            )
            .await
            .is_err()
    );
    // A dead sender must leave the question open for another client to retry.
    assert!(
        session
            .command("answer_question", "0", Some("A"), None)
            .await
            .is_err()
    );
    assert!(session.load().await.expect("load")[0].outcome.is_none());
    session
        .command("skip_question", "0", None, None)
        .await
        .expect("skip");
    assert!(matches!(
        session.load().await.expect("restore")[0].outcome,
        Some(Outcome::Skipped { .. })
    ));
}

#[test]
fn validates_bounds_and_prunes_only_old_closed_items() {
    for args in [
        AskArgs {
            title: "".into(),
            options: vec![],
        },
        AskArgs {
            title: "x".into(),
            options: vec!["a".into(); 7],
        },
        AskArgs {
            title: "x".into(),
            options: vec!["a".into(), "a".into()],
        },
        AskArgs {
            title: "x".repeat(1025),
            options: vec![],
        },
    ] {
        assert!(validate(&args).is_err());
    }
    let mut questions: Vec<_> = (0..52)
        .map(|i| {
            let mut q = question(&i.to_string(), i);
            q.outcome = Some(Outcome::Skipped { at: i });
            q
        })
        .collect();
    questions.insert(0, question("open", -1));
    prune(&mut questions);
    assert_eq!(questions.len(), 51);
    assert_eq!(questions[0].id, "open");
    assert_eq!(questions[1].id, "2");
}

#[test]
fn subagents_get_neither_tool_nor_prompt() {
    let temp = tempfile::tempdir().expect("temp");
    let runtime = RuntimeContext {
        children: crate::agent::ChildAgents::default(),
        sender: crate::agent::test_sender(),
        checkpoints: Arc::new(SqliteCheckpoint::new(temp.path().join("db")).expect("store")),
        session_id: "child".into(),
        model_route: "test".into(),
        model: "test".into(),
        approval_policy: ApprovalPolicy::Allow,
        session_context: SessionContext::default(),
        metadata: BTreeMap::new(),
        role: AgentRole::Subagent {
            parent_session_id: "parent".into(),
            parent_turn_id: "turn".into(),
        },
        frontend: Arc::new(|_| Ok(())),
    };
    let questions = Questions::default();
    let mut catalog = Catalog::default();
    questions
        .register(&mut catalog, &runtime)
        .expect("register");
    assert!(
        questions
            .prompt_section(&runtime)
            .expect("prompt")
            .is_none()
    );
    assert!(questions.session("child").is_err());
}

#[test]
fn option_label_is_stored_once_and_moves_into_the_submitted_operation() {
    let widget = widget(vec![question("q", 1)]);
    assert_eq!(widget.pending_attention_items().count(), 1);
    let Some(FrontendWidgetContent::ActionList { items, .. }) = widget.content else {
        panic!("list")
    };
    let action = items
        .into_iter()
        .next()
        .expect("question")
        .actions
        .into_iter()
        .next()
        .expect("option");
    let wire = serde_json::to_value(&action).expect("serialize");
    assert_eq!(wire["label"], "A");
    assert_eq!(wire["input_from_label"], true);
    assert!(wire["op"]["input"].is_null());
    let label_pointer = action.label.as_ptr();
    let Op::CapabilityCommand {
        input: Some(input), ..
    } = action.into_operation()
    else {
        panic!("answer")
    };
    assert_eq!(input, "A");
    assert_eq!(
        input.as_ptr(),
        label_pointer,
        "the label allocation is moved, not cloned"
    );
}

#[test]
fn pending_attention_items_ignore_other_slots_and_closed_questions() {
    let mut closed = question("closed", 2);
    closed.outcome = Some(Outcome::Skipped { at: 3 });
    let mut widget = widget(vec![question("open", 1), closed]);
    assert_eq!(
        widget
            .pending_attention_items()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["open"]
    );
    widget.slot = FrontendSlot::Navigation;
    assert_eq!(widget.pending_attention_items().count(), 0);
}

#[test]
fn poisoned_sessions_are_an_internal_error() {
    let questions = Questions::default();
    let _panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = questions.sessions.lock().expect("lock");
        panic!("poison registry");
    }));
    assert!(matches!(
        questions.session("missing"),
        Err(Error::Poisoned("questions sessions"))
    ));
}
