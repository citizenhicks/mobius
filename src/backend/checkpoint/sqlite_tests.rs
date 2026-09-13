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
    checkpoint.session_context.bot_id = "test-bot".into();
    checkpoint
}

fn execution(session_id: &str, turn: u64) -> ExecutionRecord {
    let started_at_ms = i64::try_from(turn * 100).expect("execution start");
    ExecutionRecord {
        session_id: session_id.into(),
        submission_id: format!("submission-{turn}"),
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
