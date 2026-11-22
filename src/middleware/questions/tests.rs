use super::*;
use crate::agent::{Agent, AgentConfig, create_agent};
use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
use crate::backend::model::{Model, ModelEventSink, ModelOutput, ModelRequest, ModelRouter};
use crate::backend::sandbox::local::LocalSandbox;
use crate::backend::sandbox::{
    ApprovalPolicy, NetworkAccess, Sandbox, SandboxMode, SandboxPermissions,
};
use crate::middleware::MiddlewareStack;
use crate::middleware::messages::Messages;
use crate::protocol::{MessageAuthor, MessageSubmission, SessionContext, Submission, TokenUsage};

const ANSWERED_A: &str = r#"{"answer":"A","status":"answered"}"#;

/// Whether the saved context holds the tool's later result carrying `output`.
fn has_tool_result(context: &[Arc<Value>], output: &str) -> bool {
    context.iter().any(|message| {
        message["role"] == "user"
            && message["content"][0]["text"].as_str().is_some_and(|text| {
                text.starts_with("Result of an earlier asynchronous `ask_user` call \"q\".")
                    && text.ends_with(output)
            })
    })
}
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

fn question(id: &str, at: i64) -> Question {
    Question {
        call: crate::protocol::DeferredToolCall(id.into()),
        title: "Choose {answer}?".into(),
        options: vec!["A".into(), "B".into()],
        asked_at: at,
        asked_turn: None,
        outcome: None,
        pending_outcome: None,
    }
}

#[test]
fn question_outcome_preserves_the_whole_prompt() {
    let mut question = question("multiline", 1);
    question.title = "Which suite?\nThe full suite takes an hour.".into();
    question.outcome = Some(Outcome::Answered {
        text: "Run just the wire tests".into(),
        at: 2,
    });

    let item = question_item(question);
    assert_eq!(item.text, "Which suite?\nThe full suite takes an hour.");
    assert_eq!(
        item.outcome.as_deref(),
        Some("Answered: Run just the wire tests")
    );
    assert_eq!(item.state, FrontendListItemState::Completed);
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

async fn next(agent: &mut Agent, predicate: impl Fn(&EventMsg) -> bool + Sync) -> EventMsg {
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

fn closed_question(event: &EventMsg) -> bool {
    matches!(event, EventMsg::Frontend(FrontendEvent::Widget { capability, item })
        if &**capability == "questions" && item.text == "0")
}

async fn agent<M: Model + 'static>(
    path: &std::path::Path,
    checkpoints: Arc<dyn CheckpointStore>,
    questions: Arc<Questions>,
    model: Arc<M>,
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
        .session_id("session".parse().expect("session ID"))
        .session_context(SessionContext {
            owner_id: "test-owner".into(),
            ..SessionContext::default()
        }),
    )
    .await
    .expect("agent")
}

#[tokio::test]
async fn answers_reach_model_history_without_duplicate_frontend_messages() {
    for active in [false, true] {
        let (temp, store) = SqliteCheckpoint::temporary();
        let store: Arc<dyn CheckpointStore> = Arc::new(store);
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
            next(&mut agent, closed_question).await;
            model.release.notify_one();
        }
        let mut complete = false;
        let mut closed = active;
        while !complete || !closed {
            let event = next(&mut agent, |_| true).await;
            assert!(
                !matches!(&event, EventMsg::Message(message) if message.text == ANSWERED_A),
                "the question card already shows its answer"
            );
            complete |= matches!(event, EventMsg::TurnComplete(_));
            closed |= closed_question(&event);
        }
        let checkpoint = store
            .load(&"session".parse().expect("session ID"))
            .await
            .expect("checkpoint")
            .expect("saved");
        assert!(
            has_tool_result(&checkpoint.context, ANSWERED_A),
            "the model reads the answer as the call's result"
        );
        let session = questions
            .session(&"session".parse().expect("session ID"))
            .expect("session");
        let saved = session.load().await.expect("load");
        assert!(matches!(&saved[0].outcome, Some(Outcome::Answered { .. })));
        assert!(
            session
                .command("answer_question", "q", Some("B"))
                .await
                .is_err()
        );
        let Some(FrontendWidgetContent::ActionList { items, .. }) = widget(saved).content else {
            panic!("list")
        };
        assert_eq!(items[0].state, FrontendListItemState::Completed);
        assert!(items[0].actions.is_empty());
    }
}

/// A provider may start its call counter over on each response.
struct ReusedCall(std::sync::atomic::AtomicUsize);

impl Model for ReusedCall {
    fn respond<'a>(
        &'a self,
        _: ModelRequest,
        _: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        let index = self.0.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let output = if index < 2 {
                serde_json::json!({"type":"function_call", "call_id":"same/call", "name":"ask_user",
                    "arguments": serde_json::json!({"title":format!("Question {index}"),"options":["A","B"]}).to_string()})
            } else {
                serde_json::json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]})
            };
            ModelOutput::from_output(vec![output], index >= 2, TokenUsage::default())
        })
    }
}

#[tokio::test]
async fn repeated_provider_call_ids_have_independent_cards_and_receipts() {
    let (temp, store) = SqliteCheckpoint::temporary();
    let store: Arc<dyn CheckpointStore> = Arc::new(store);
    let questions = Arc::new(Questions::default());
    let mut agent = agent(
        temp.path(),
        Arc::clone(&store),
        Arc::clone(&questions),
        Arc::new(ReusedCall(0.into())),
    )
    .await;
    agent
        .sender()
        .send(Submission::message(MessageSubmission::user(
            "Ask twice",
            Vec::new(),
        )))
        .unwrap();
    let mut calls = Vec::new();
    while let EventMsg::ToolCallEnd(end) = next(&mut agent, |event| {
        matches!(event, EventMsg::ToolCallEnd(_) | EventMsg::TurnComplete(_))
    })
    .await
    {
        assert_eq!(end.call_id.as_ref(), "same/call");
        let pending: Value = serde_json::from_str(&end.output.text()).unwrap();
        let event = EventMsg::ToolCallEnd(end);
        let block = questions.render(&event, "session").unwrap();
        let call = pending["call_id"].as_str().expect("pending reference");
        assert_eq!(
            block.id.as_deref().unwrap().split_once('/').unwrap().1,
            call
        );
        calls.push(call.to_owned());
    }
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0], calls[1]);
    let session_id = "session".parse().unwrap();
    let saved = questions
        .session(&session_id)
        .unwrap()
        .load()
        .await
        .unwrap();
    assert_eq!(saved.len(), calls.len());
    for (question, call) in saved.iter().zip(&calls) {
        assert_eq!(question.call.id(), call);
    }
    for (index, command) in ["answer_question", "skip_question"].into_iter().enumerate() {
        agent
            .sender()
            .submit(Op::CapabilityCommand {
                capability: "questions".into(),
                command: command.into(),
                arguments: calls[index].as_str().into(),
                input: (index == 0).then(|| "A".into()),
                target: None,
            })
            .unwrap();
        next(&mut agent, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
    }
    let checkpoint = store.load(&session_id).await.unwrap().unwrap();
    let results: Vec<_> = checkpoint
        .context
        .iter()
        .filter_map(|item| item["content"][0]["text"].as_str())
        .filter(|text| text.starts_with("Result of an earlier asynchronous"))
        .collect();
    assert_eq!(results.len(), 2);
    for ((result, call), status) in results.iter().zip(calls).zip(["answered", "no_answer"]) {
        assert!(
            result.contains(&call) && result.contains(status),
            "{result}"
        );
    }
}

#[tokio::test]
async fn concurrent_answers_close_a_question_once() {
    let (temp, store) = SqliteCheckpoint::temporary();
    let store: Arc<dyn CheckpointStore> = Arc::new(store);
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
    let checkpoint =
        crate::backend::checkpoint::Checkpoint::empty("session".parse().expect("session ID"));
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
            session_id: &checkpoint.session_id,
            session_context: &context,
            checkpoint: &checkpoint,
            checkpoints: Arc::clone(&store),
        })
    };
    let (first, second) = tokio::join!(answer("A"), answer("B"));
    assert_ne!(first.is_ok(), second.is_ok());
    let mut complete = false;
    let mut closed = false;
    while !complete || !closed {
        let event = next(&mut agent, |_| true).await;
        assert!(
            !matches!(event, EventMsg::Message(_)),
            "the card owns the displayed answer"
        );
        complete |= matches!(event, EventMsg::TurnComplete(_));
        closed |= closed_question(&event);
    }
    let saved = questions
        .session(&"session".parse().expect("session ID"))
        .expect("session")
        .load()
        .await
        .expect("load");
    let Some(Outcome::Answered { text, .. }) = &saved[0].outcome else {
        panic!("answered once")
    };
    assert_eq!(text, if first.is_ok() { "A" } else { "B" });
    let checkpoint = store
        .load(&"session".parse().expect("session ID"))
        .await
        .expect("checkpoint")
        .expect("saved");
    let expected = serde_json::json!({"answer": text, "status": "answered"}).to_string();
    let user_messages = checkpoint
        .context
        .iter()
        .filter(|message| message["role"] == "user")
        .count();
    assert_eq!(user_messages, 1, "the call gets exactly one result");
    assert!(has_tool_result(&checkpoint.context, &expected));
}

#[tokio::test]
async fn a_skipped_question_tells_the_model_no_answer() {
    check_skip(false).await;
    check_skip(true).await;
}

async fn check_skip(recovering: bool) {
    let (temp, store) = SqliteCheckpoint::temporary();
    let store: Arc<dyn CheckpointStore> = Arc::new(store);
    let mut question = question("q", 1);
    if recovering {
        question.asked_turn = Some("completed-turn".into());
        question.pending_outcome = Some(Outcome::Skipped { at: 2 });
    }
    store
        .save_state("session", STATE_KEY, &serde_json::json!([question]))
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
    if !recovering {
        agent
            .sender()
            .submit(Op::CapabilityCommand {
                capability: "questions".into(),
                command: "skip_question".into(),
                arguments: "q".into(),
                input: None,
                target: None,
            })
            .expect("skip");
    }
    next(&mut agent, closed_question).await;
    let checkpoint = store
        .load(&"session".parse().expect("session ID"))
        .await
        .expect("checkpoint")
        .expect("saved");
    assert!(has_tool_result(
        &checkpoint.context,
        r#"{"status":"no_answer"}"#
    ));
}

#[tokio::test]
async fn ask_skip_restore_and_bounds_are_durable() {
    let (temp, store) = SqliteCheckpoint::temporary();
    let store: Arc<dyn CheckpointStore> = Arc::new(store);
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let session = Arc::new(QuestionSession {
        id: "session".parse().expect("session ID"),
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
        &"session".parse().expect("session ID"),
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
        .with_call_id(i.to_string().into());
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
                Some(&"a".repeat(MAX_ANSWER_BYTES + 1))
            )
            .await
            .is_err()
    );
    // A dead sender must leave the question open for another client to retry.
    assert!(
        session
            .command("answer_question", "0", Some("A"))
            .await
            .is_err()
    );
    assert!(session.load().await.expect("load")[0].outcome.is_none());
    // A skip is a result too: undelivered, it leaves the question open as well.
    assert!(session.command("skip_question", "0", None).await.is_err());
    assert!(session.load().await.expect("restore")[0].outcome.is_none());
}
