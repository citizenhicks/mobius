//! SQLite checkpoint storage tests.

use std::sync::mpsc;

use serde_json::json;
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::*;
use crate::backend::checkpoint::ExecutionOutcome;
use crate::backend::checkpoint::StreamMetrics;
use crate::backend::checkpoint::event_turn_page;
use crate::protocol::EventMsg;
use crate::protocol::ModelStepContentPhase;
use crate::protocol::ModelStepOutcome;
use crate::protocol::TurnAbortedEvent;
use crate::protocol::TurnCompleteEvent;
use crate::protocol::TurnStartedEvent;

fn checkpoint(session_id: impl Into<String>) -> Checkpoint {
    let mut checkpoint = Checkpoint::empty(session_id);
    checkpoint.session_context.owner_id = "test-bot".into();
    checkpoint
}

#[tokio::test]
async fn admission_receipts_commit_with_queue_and_survive_consumption() {
    use crate::backend::checkpoint::{QueuedMessage, QueuedMessageBoundary};
    use crate::protocol::{MessageAuthor, MessageDelivery, MessageEvent};

    let directory = tempfile::tempdir().expect("directory");
    let store = SqliteCheckpoint::new(directory.path().join("receipts.sqlite3")).expect("store");
    let mut state = checkpoint("session");
    store.save(&state, &[], None).await.expect("seed");
    state.pending_messages.push(
        QueuedMessage::new(
            "messages",
            "input",
            QueuedMessageBoundary::Turn,
            MessageEvent {
                author: MessageAuthor::User,
                delivery: MessageDelivery::Turn,
                text: "hello".into(),
                attachments: Vec::new(),
                reply: None,
                message_target: None,
            },
        )
        .expect("message"),
    );
    assert!(store.save(&state, &[], None).await.is_err());
    assert!(
        !store
            .message_accepted("session", "input")
            .await
            .expect("absent receipt")
    );
    state.sequence += 1;
    store.save(&state, &[], None).await.expect("admit");
    state.sequence += 1;
    state.pending_messages.clear();
    store.save(&state, &[], None).await.expect("consume");
    assert!(
        store
            .message_accepted("session", "input")
            .await
            .expect("durable receipt")
    );
    assert!(
        !store
            .message_accepted("other", "input")
            .await
            .expect("session scope")
    );
    store
        .delete_sessions(&["session".into()])
        .await
        .expect("delete");
    assert!(
        !store
            .message_accepted("session", "input")
            .await
            .expect("deleted receipt")
    );
}

fn execution(session_id: &str, turn: u64) -> ExecutionRecord {
    let started_at_ms = i64::try_from(turn * 100).expect("execution start");
    ExecutionRecord {
        session_id: session_id.into(),
        submission_id: format!("submission-{turn}"),
        author: crate::protocol::MessageAuthor::User,
        turn_id: format!("turn-{turn}"),
        started_at_ms,
        finished_at_ms: started_at_ms + 25,
        elapsed_ms: 25,
        outcome: ExecutionOutcome::Completed,
        model_calls: 1,
        tool_calls: turn,
        failed_tool_calls: 0,
        usage: crate::protocol::TokenUsage {
            total_tokens: 1,
            ..crate::protocol::TokenUsage::default()
        },
    }
}

#[path = "sqlite_tests/event_journal.rs"]
mod event_journal;
#[path = "sqlite_tests/journals.rs"]
mod journals;
#[path = "sqlite_tests/model_steps.rs"]
mod model_steps;
#[path = "sqlite_tests/sessions.rs"]
mod sessions;
#[path = "sqlite_tests/strict.rs"]
mod strict;
