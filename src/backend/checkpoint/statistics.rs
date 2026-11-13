//! Execution accounting values shared by library observers and gateway clients.

use super::{ActiveExecution, Checkpoint, ExecutionOutcome, ExecutionRecord, ExecutionStats};
use crate::SessionId;
use crate::protocol::TokenUsage;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Completed execution totals plus the active run, when one exists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunStats {
    #[serde(flatten)]
    /// The completed.
    pub completed: ExecutionStats,
    /// The active.
    pub active: Option<RunSummary>,
}

/// Frontend-safe summary of one completed or active user turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunSummary {
    /// The session identifier.
    pub session_id: SessionId,
    /// The submission identifier.
    pub submission_id: Arc<str>,
    /// The turn identifier.
    pub turn_id: Arc<str>,
    /// The started at milliseconds.
    pub started_at_ms: i64,
    /// The finished at milliseconds.
    pub finished_at_ms: Option<i64>,
    /// The elapsed milliseconds.
    pub elapsed_ms: u64,
    /// The outcome.
    pub outcome: Option<ExecutionOutcome>,
    /// The model calls.
    pub model_calls: u64,
    /// The tool calls.
    pub tool_calls: u64,
    /// The failed tool calls.
    pub failed_tool_calls: u64,
    /// The usage.
    pub usage: TokenUsage,
}

/// Statistics from one committed checkpoint and the journal receipt that orders them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommittedRunStats {
    /// Last journal sequence in the checkpoint's atomic event batch.
    pub after_sequence: u64,
    /// The committed counts and active execution, without model context or transcript data.
    pub stats: Arc<RunStats>,
}

impl RunStats {
    /// Projects checkpoint counters without loading or retaining model context.
    #[must_use]
    pub fn from_checkpoint(checkpoint: &Checkpoint, now_ms: i64) -> Self {
        Self {
            completed: checkpoint.execution_stats,
            active: checkpoint
                .active_execution
                .as_ref()
                .map(|active| RunSummary::from_active(&checkpoint.session_id, active, now_ms)),
        }
    }
}

impl RunSummary {
    /// Projects an active execution at the supplied Unix time in milliseconds.
    #[must_use]
    pub fn from_active(session_id: &SessionId, active: &ActiveExecution, now_ms: i64) -> Self {
        Self {
            session_id: session_id.share(),
            submission_id: Arc::clone(&active.submission_id),
            turn_id: Arc::clone(&active.turn_id),
            started_at_ms: active.started_at_ms,
            finished_at_ms: None,
            elapsed_ms: now_ms
                .checked_sub(active.started_at_ms)
                .and_then(|elapsed| u64::try_from(elapsed).ok())
                .unwrap_or_default(),
            outcome: None,
            model_calls: active.model_calls,
            tool_calls: active.tool_calls,
            failed_tool_calls: active.failed_tool_calls,
            usage: active.usage,
        }
    }
}

impl From<ExecutionRecord> for RunSummary {
    fn from(record: ExecutionRecord) -> Self {
        Self {
            session_id: record.session_id,
            submission_id: record.submission_id,
            turn_id: record.turn_id,
            started_at_ms: record.started_at_ms,
            finished_at_ms: Some(record.finished_at_ms),
            elapsed_ms: record.elapsed_ms,
            outcome: Some(record.outcome),
            model_calls: record.model_calls,
            tool_calls: record.tool_calls,
            failed_tool_calls: record.failed_tool_calls,
            usage: record.usage,
        }
    }
}
