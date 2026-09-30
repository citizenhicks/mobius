//! Durable message admission and provenance tests.

use super::*;
use crate::agent::MessageAcceptance;
use crate::backend::checkpoint::{ActiveExecution, ExecutionPhase};
use crate::protocol::{MessageSource, Submission};

#[tokio::test]
async fn admission_survives_turn_completion_and_restart() {
    let workspace = tempfile::tempdir().expect("workspace");
    let path = workspace.path().join("checkpoints.sqlite3");
    let checkpoints = Arc::new(SqliteCheckpoint::new(&path).expect("checkpoint store"));
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([scripted_message("done")])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        checkpoints.clone(),
        "admission",
        "test",
        model,
    ))
    .await
    .expect("agent");
    let submission = Submission {
        id: "stable-input".into(),
        op: user_op("hello"),
    };
    let admission = agent
        .sender()
        .send_with_admission(submission.clone())
        .expect("send");
    assert_eq!(
        admission.wait().await.expect("durable admission"),
        MessageAcceptance::Accepted
    );
    while let Some(event) = agent.next_event().await {
        if let EventMsg::TurnAborted(event) = &event.msg {
            panic!("turn aborted: {event:?}");
        }
        if matches!(event.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }
    assert!(
        checkpoints
            .message_accepted("admission", "stable-input")
            .await
            .expect("receipt")
    );
    let before = checkpoints
        .load("admission")
        .await
        .expect("load")
        .expect("checkpoint");
    assert!(before.pending_messages.is_empty());
    let (sender, mut events) = agent.into_parts();
    drop(sender);
    while events.recv().await.is_some() {}
    drop(checkpoints);

    let checkpoints = Arc::new(SqliteCheckpoint::new(path).expect("reopen checkpoint store"));
    let agent = create_agent(config(workspace.path(), checkpoints.clone(), "admission"))
        .await
        .expect("resume agent");
    let admission = agent
        .sender()
        .send_with_admission(submission)
        .expect("resend");
    assert_eq!(
        admission.wait().await.expect("duplicate admission"),
        MessageAcceptance::AlreadyAccepted
    );
    let after = checkpoints
        .load("admission")
        .await
        .expect("load")
        .expect("checkpoint");
    assert!(after.pending_messages.is_empty());
    assert!(after.active_execution.is_none());
    assert_eq!(
        after.execution_stats.run_count,
        before.execution_stats.run_count
    );
}

#[tokio::test]
async fn rejected_message_has_no_admission_receipt() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let agent = create_agent(config(workspace.path(), checkpoints.clone(), "rejected"))
        .await
        .expect("agent");
    let Op::Message { mut message } = user_op("stale") else {
        unreachable!()
    };
    message.target_turn_id = Some("missing-turn".into());
    let admission = agent
        .sender()
        .send_with_admission(Submission {
            id: "stale-input".into(),
            op: Op::Message { message },
        })
        .expect("send");
    let rejection = admission
        .wait()
        .await
        .expect_err("stale target is rejected");
    assert!(rejection.to_string().contains("stale turn"), "{rejection}");
    assert!(
        !checkpoints
            .message_accepted("rejected", "stale-input")
            .await
            .expect("receipt")
    );
    let admission = agent
        .sender()
        .send_with_admission(Submission {
            id: "stale-input".into(),
            op: user_op("corrected target"),
        })
        .expect("runner remains available after rejection");
    assert_eq!(
        admission.wait().await.expect("corrected message admission"),
        MessageAcceptance::Accepted
    );
}

struct ObserveOrigin(Arc<Mutex<Vec<MessageAuthor>>>);

impl Middleware for ObserveOrigin {
    fn name(&self) -> &'static str {
        "observe_origin"
    }

    fn pre_model<'a>(&'a self, context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        self.0.lock().expect("origins").push(context.author.clone());
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn resumed_source_turn_retains_origin_for_hooks() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let author = MessageAuthor::Source {
        message_id: "event-message".into(),
        source: MessageSource::External {
            source_id: "routine-run".into(),
            event_id: "finished-event".into(),
        },
        cause_id: None,
        ancestry: Vec::new(),
        handle: "routine".into(),
        symbol: None,
    };
    let mut checkpoint = Checkpoint::empty("source-resume");
    checkpoint.session_context = test_session_context();
    checkpoint.model_route = Some("test".into());
    checkpoint.active_execution = Some(ActiveExecution {
        submission_id: "event-submission".into(),
        author: author.clone(),
        turn_id: "report-turn".into(),
        started_at_ms: 1,
        model_calls: 0,
        tool_calls: 0,
        failed_tool_calls: 0,
        usage: TokenUsage::default(),
        next_model_step: 0,
        stop_hook_active: false,
        phase: ExecutionPhase::Model,
    });
    checkpoints
        .save(&checkpoint, &[], None)
        .await
        .expect("checkpoint source turn");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([scripted_message("done")])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let configuration = config_with_model(
        workspace.path(),
        checkpoints,
        "source-resume",
        "test",
        model,
    )
    .middleware(test_middleware(vec![Arc::new(ObserveOrigin(
        observed.clone(),
    ))]));
    let mut agent = create_agent(configuration)
        .await
        .expect("resume source turn");
    while let Some(event) = agent.next_event().await {
        if let EventMsg::TurnAborted(event) = &event.msg {
            panic!("turn aborted: {event:?}");
        }
        if matches!(event.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }
    assert_eq!(*observed.lock().expect("origins"), vec![author]);
}
