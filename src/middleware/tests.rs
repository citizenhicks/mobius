use std::collections::BTreeMap;

use super::*;
use crate::backend::checkpoint::QueuedMessageBoundary;
use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
use crate::protocol::MessageAuthor;
use crate::protocol::MessageEvent;
use crate::protocol::SessionContext;

struct LifecycleProbe {
    id: &'static str,
    fail: bool,
    calls: Arc<std::sync::Mutex<Vec<String>>>,
}

struct CompactInputRewrite;

impl Middleware for CompactInputRewrite {
    fn name(&self) -> &'static str {
        "compact_input_rewrite"
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            context.input.retain(|item| item.as_ref() != "remove");
            context.input_changed = true;
            context.input.reverse();
            Ok(())
        })
    }
}

impl Middleware for LifecycleProbe {
    fn name(&self) -> &'static str {
        self.id
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("lifecycle calls")
                .push(format!("start:{}", self.id));
            context.push_input(serde_json::json!(self.id));
            if self.fail {
                return Err(Error::Config(format!("{} failed", self.id)));
            }
            Ok(())
        })
    }

    fn session_end<'a>(&'a self, _runtime: &'a RuntimeContext) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("lifecycle calls")
                .push(format!("end:{}", self.id));
            Ok(())
        })
    }
}

fn lifecycle_probe(
    id: &'static str,
    fail: bool,
    calls: &Arc<std::sync::Mutex<Vec<String>>>,
) -> Arc<dyn Middleware> {
    Arc::new(LifecycleProbe {
        id,
        fail,
        calls: Arc::clone(calls),
    })
}

fn lifecycle_runtime(path: &std::path::Path) -> RuntimeContext {
    RuntimeContext {
        children: crate::agent::ChildAgents::default(),
        sender: crate::agent::test_sender(),
        checkpoints: Arc::new(
            SqliteCheckpoint::new(path.join("checkpoints.sqlite3")).expect("checkpoint store"),
        ),
        session_id: "session".into(),
        model_route: "model".into(),
        model: "model".into(),
        approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
        session_context: SessionContext::default(),
        metadata: BTreeMap::new(),
        role: crate::agent::AgentRole::Main,
        frontend: Arc::new(|_| Ok(())),
    }
}

#[tokio::test]
async fn session_lifecycle_starts_forward_and_ends_or_rolls_back_in_reverse() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let stack = MiddlewareStack::new(vec![
        lifecycle_probe("a", false, &calls),
        lifecycle_probe("b", false, &calls),
    ])
    .expect("middleware stack");

    let runtime = lifecycle_runtime(temporary.path());
    let mut input = Vec::new();
    let started = stack
        .session_start(
            &runtime,
            &[],
            SessionStartSource::Startup,
            &mut input,
            &mut Default::default(),
        )
        .await
        .expect("session start");
    stack.session_end(&runtime).await.expect("session end");
    assert_eq!(
        input,
        [
            Arc::new(serde_json::json!("a")),
            Arc::new(serde_json::json!("b"))
        ]
    );
    assert!(started.input_changed);
    assert_eq!(
        *calls.lock().expect("lifecycle calls"),
        ["start:a", "start:b", "end:b", "end:a"]
    );

    calls.lock().expect("lifecycle calls").clear();
    let failing = MiddlewareStack::new(vec![
        lifecycle_probe("a", false, &calls),
        lifecycle_probe("b", true, &calls),
    ])
    .expect("middleware stack");
    let mut failing_input = Vec::new();
    assert!(
        failing
            .session_start(
                &runtime,
                &[],
                SessionStartSource::Resume,
                &mut failing_input,
                &mut Default::default(),
            )
            .await
            .is_err()
    );
    assert_eq!(
        *calls.lock().expect("lifecycle calls"),
        ["start:a", "start:b", "end:a"]
    );

    calls.lock().expect("lifecycle calls").clear();
    let mut compact_input = vec![Arc::new(serde_json::json!("compacted"))];
    assert!(
        failing
            .session_start(
                &runtime,
                &[],
                SessionStartSource::Compact,
                &mut compact_input,
                &mut Default::default(),
            )
            .await
            .is_err()
    );
    assert_eq!(compact_input, [Arc::new(serde_json::json!("compacted"))]);
    assert_eq!(
        *calls.lock().expect("lifecycle calls"),
        ["start:a", "start:b"]
    );
}

#[tokio::test]
async fn compact_session_start_restores_removed_and_reordered_input_on_failure() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let stack = MiddlewareStack::new(vec![
        Arc::new(CompactInputRewrite),
        lifecycle_probe("failure", true, &calls),
    ])
    .expect("middleware stack");
    let runtime = lifecycle_runtime(temporary.path());
    let original = vec![
        serde_json::json!("first"),
        serde_json::json!("remove"),
        serde_json::json!("last"),
    ]
    .into_iter()
    .map(Arc::new)
    .collect::<Vec<_>>();
    let mut input = original.clone();

    stack
        .session_start(
            &runtime,
            &[],
            SessionStartSource::Compact,
            &mut input,
            &mut Default::default(),
        )
        .await
        .expect_err("later middleware must fail");

    assert_eq!(input, original);
}

#[test]
fn hook_policy_decisions_are_monotonic_and_stop_continuation_is_bounded() {
    let tools = Catalog::default();
    let mut permission_events = Vec::new();
    let mut permission = PermissionRequestContext {
        turn: TurnIdentity {
            author: &crate::protocol::MessageAuthor::User,
            session_id: "session",
            turn_id: "turn",
            model: "model",
            approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
        },
        calls: &[],
        requested_call_ids: &[],
        reason: "test",
        events: &mut permission_events,
        tools: &tools,
        decision: None,
    };
    permission.allow();
    permission.deny("blocked").expect("deny permission");
    permission.allow();
    assert_eq!(
        permission.decision(),
        Some(&crate::protocol::ReviewDecision::Denied {
            rejection: "blocked".into()
        })
    );

    let mut events = Vec::new();
    let mut stop = StopContext {
        turn: permission.turn,
        role: &crate::agent::AgentRole::Main,
        stop_hook_active: false,
        last_assistant_message: Some("done"),
        events: &mut events,
        continuation: None,
    };
    stop.continue_with("first").expect("first continuation");
    stop.continue_with("second").expect("first decision wins");
    assert_eq!(stop.continuation(), Some("first"));
    let mut active = StopContext {
        stop_hook_active: true,
        continuation: None,
        ..stop
    };
    assert!(active.continue_with("again").is_err());
}

#[test]
fn pre_tool_rewrite_rejects_invalid_calls_without_mutation() {
    let tools = Catalog::default();
    let original = crate::protocol::ToolCall {
        call_id: "call".into(),
        name: "read".into(),
        arguments: serde_json::json!({"path": "README.md"}),
    };
    for (name, arguments) in [
        ("", serde_json::json!({})),
        ("search", serde_json::json!([])),
    ] {
        let mut call = original.clone();
        let mut events = Vec::new();
        let mut context = PreToolUseContext {
            delivery_once: crate::middleware::delivery_once::DeliveryOnce::testing(),
            turn: TurnIdentity {
                author: &crate::protocol::MessageAuthor::User,
                session_id: "session",
                turn_id: "turn",
                model: "model",
                approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
            },
            events: &mut events,
            tools: &tools,
            call: &mut call,
            changed: false,
            input: Vec::new(),
            denial: None,
        };
        let error = context
            .replace(name, arguments)
            .expect_err("invalid rewrite must fail");
        assert!(matches!(error, Error::Tool(_)));
        assert!(!context.changed);
        assert_eq!(context.call(), &original);
        context
            .replace(original.name.clone(), original.arguments.clone())
            .unwrap();
        assert!(
            !context.changed,
            "identical replacements do not rewrite output"
        );
        context
            .replace("read", serde_json::json!({"path": "Cargo.toml"}))
            .unwrap();
        assert!(context.changed);
        assert_eq!(context.call().call_id, original.call_id);
    }
}

fn queued(owner: &str, id: &str, text: &str) -> DurableQueuedMessage {
    let event = MessageEvent {
        author: MessageAuthor::User,
        delivery: crate::protocol::MessageDelivery::Turn,
        text: text.into(),
        attachments: Vec::new(),
        reply: None,
        message_target: None,
    };
    DurableQueuedMessage::new(owner, id, QueuedMessageBoundary::Turn, event)
        .expect("valid queued message")
}

fn scoped_queue<'a>(
    items: &'a mut Vec<DurableQueuedMessage>,
    owner: &'static str,
) -> MessageQueue<'a> {
    let mut queue = MessageQueue::new(items);
    queue.scope(owner);
    queue
}

fn enqueue(queue: &mut MessageQueue<'_>, id: &str, text: &str) -> Result<bool> {
    let event = MessageEvent {
        author: MessageAuthor::User,
        delivery: crate::protocol::MessageDelivery::Turn,
        text: text.into(),
        attachments: Vec::new(),
        reply: None,
        message_target: None,
    };
    queue.enqueue(id, QueuedMessageBoundary::Turn, event)
}

#[test]
fn queued_message_queue_cannot_observe_or_consume_another_owner() {
    let mut items = vec![
        queued("alpha", "one", "first"),
        queued("beta", "one", "private"),
    ];
    let prepared = {
        let mut queue = scoped_queue(&mut items, "alpha");
        assert_eq!(queue.count(), 1);
        assert_eq!(queue.latest().map(|item| item.id()), Some("one"));
        let prepared = queue
            .next_turn()
            .expect("valid queued message")
            .expect("owned next turn");
        queue
            .consume_next_turn(&prepared.submission_id)
            .expect("consume prepared turn");
        prepared
    };

    assert_eq!(prepared.submission_id, "one");
    assert_eq!(items, vec![queued("beta", "one", "private")]);
}

#[test]
fn queued_message_invalid_mutations_are_atomic() {
    let mut items = vec![queued("alpha", "one", "first")];
    let original = items.clone();
    {
        let mut queue = scoped_queue(&mut items, "alpha");
        assert!(enqueue(&mut queue, "", "second").is_err());
        assert!(
            enqueue(
                &mut queue,
                "two",
                &"x".repeat(crate::protocol::MAX_MESSAGE_BYTES + 1),
            )
            .is_err()
        );
        let invalid = queued("owner", "edit-one", "replacement");
        let (_, event) = invalid.into_parts();
        assert!(queue.replace("one", "", event.clone()).is_err());
        assert!(
            !queue
                .replace("missing", "edit-one", event)
                .expect("stale replacement")
        );
    }

    assert_eq!(items, original);
}
