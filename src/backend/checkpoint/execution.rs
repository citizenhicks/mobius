//! Durable execution records and approval state.

use crate::SessionId;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;

use crate::backend::sandbox::NetworkAccess;
use crate::backend::sandbox::SandboxMode;
use crate::protocol::MessageAuthor;
use crate::protocol::TokenUsage;
use crate::protocol::ToolCall;

/// Durable phase of the user turn currently running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionPhase {
    /// Selects the model case.
    Model,
    /// Selects the completion case.
    Completion {
        /// The last assistant message.
        last_assistant_message: Option<String>,
    },
}

/// Mutable state for the user turn currently running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveExecution {
    /// The submission identifier.
    pub submission_id: Arc<str>,
    /// Trusted provenance of the message that initiated this execution.
    pub author: Arc<MessageAuthor>,
    /// The turn identifier.
    pub turn_id: Arc<str>,
    /// The started at milliseconds.
    pub started_at_ms: i64,
    /// The model calls.
    pub model_calls: u64,
    /// Tool results accepted by the execution; in-flight calls are excluded.
    pub tool_calls: u64,
    /// Accepted tool results marked as failures.
    pub failed_tool_calls: u64,
    /// The usage.
    pub usage: TokenUsage,
    /// The next model step.
    pub next_model_step: usize,
    /// The stop hook active.
    pub stop_hook_active: bool,
    /// The phase.
    pub phase: ExecutionPhase,
    /// A hidden turn started by the idle timer; it runs idle hooks instead of a model step.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub idle: bool,
}

/// The model step currently in flight for an active execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveModelStep {
    /// The model step identifier.
    pub model_step_id: Arc<str>,
    /// The step index.
    pub step_index: usize,
    /// The started at milliseconds.
    pub started_at_ms: i64,
}

/// Terminal outcome of one user turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    /// Selects the completed case.
    Completed,
    /// Selects the aborted case.
    Aborted,
    /// Selects the failed case.
    Failed,
}

/// Durable observability record for one completed user turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionRecord {
    /// The session identifier.
    pub session_id: SessionId,
    /// The submission identifier.
    pub submission_id: Arc<str>,
    /// Trusted initiating message provenance retained after the turn ends.
    pub author: Arc<MessageAuthor>,
    /// The turn identifier.
    pub turn_id: Arc<str>,
    /// The started at milliseconds.
    pub started_at_ms: i64,
    /// The finished at milliseconds.
    pub finished_at_ms: i64,
    /// The elapsed milliseconds.
    pub elapsed_ms: u64,
    /// The outcome.
    pub outcome: ExecutionOutcome,
    /// The model calls.
    pub model_calls: u64,
    /// The tool calls.
    pub tool_calls: u64,
    /// The failed tool calls.
    pub failed_tool_calls: u64,
    /// The usage.
    pub usage: TokenUsage,
}

/// Aggregate execution metrics for one durable session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionStats {
    /// The run count.
    pub run_count: u64,
    /// The failed run count.
    pub failed_run_count: u64,
    /// The aborted run count.
    pub aborted_run_count: u64,
    /// The model calls.
    pub model_calls: u64,
    /// The tool calls.
    pub tool_calls: u64,
    /// The failed tool calls.
    pub failed_tool_calls: u64,
    /// The elapsed milliseconds.
    pub elapsed_ms: u64,
    /// The usage.
    pub usage: TokenUsage,
}

/// One intentional replacement of active model history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextRewriteReason {
    /// Bounds materialized attachment images retained for replay.
    Attachments,
    /// Selects the compaction case.
    Compaction,
    /// Discards provider-owned reasoning before a different model route replays context.
    ModelChange,
    /// Selects the memory case.
    Memory,
}

impl ContextRewriteReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Attachments => "attachments",
            Self::Compaction => "compaction",
            Self::ModelChange => "model_change",
            Self::Memory => "memory",
        }
    }
}

/// The latest deliberate active-context rewrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRewrite {
    /// The epoch.
    pub epoch: u64,
    /// The reasons.
    pub reasons: Vec<ContextRewriteReason>,
}

impl ExecutionStats {
    pub(crate) fn checked_record(&mut self, record: &ExecutionRecord) -> Option<()> {
        // Stage the sum so overflow leaves the existing aggregate unchanged.
        let usage = self.usage.checked_sum(&record.usage)?;
        let mut next = self.checked_sum(&Self {
            run_count: 1,
            failed_run_count: u64::from(record.outcome == ExecutionOutcome::Failed),
            aborted_run_count: u64::from(record.outcome == ExecutionOutcome::Aborted),
            model_calls: record.model_calls,
            tool_calls: record.tool_calls,
            failed_tool_calls: record.failed_tool_calls,
            elapsed_ms: record.elapsed_ms,
            usage: TokenUsage::default(),
        })?;
        next.usage = usage;
        *self = next;
        Some(())
    }

    /// Adds completed execution totals, leaving this value unchanged on overflow.
    pub fn checked_add(&mut self, other: &Self) -> Option<()> {
        *self = self.checked_sum(other)?;
        Some(())
    }

    fn checked_sum(&self, other: &Self) -> Option<Self> {
        Some(Self {
            run_count: self.run_count.checked_add(other.run_count)?,
            failed_run_count: self.failed_run_count.checked_add(other.failed_run_count)?,
            aborted_run_count: self
                .aborted_run_count
                .checked_add(other.aborted_run_count)?,
            model_calls: self.model_calls.checked_add(other.model_calls)?,
            tool_calls: self.tool_calls.checked_add(other.tool_calls)?,
            failed_tool_calls: self
                .failed_tool_calls
                .checked_add(other.failed_tool_calls)?,
            elapsed_ms: self.elapsed_ms.checked_add(other.elapsed_ms)?,
            usage: self.usage.checked_sum(&other.usage)?,
        })
    }
}

/// A tool batch waiting for a frontend decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApproval {
    /// The submission identifier.
    pub submission_id: Arc<str>,
    /// The turn identifier.
    pub turn_id: Arc<str>,
    /// The request identifier.
    pub request_id: Arc<str>,
    /// The approval call identifiers.
    pub approval_call_ids: Vec<Arc<str>>,
    /// The authorized call identifiers.
    pub authorized_call_ids: Vec<Arc<str>>,
    /// The calls.
    pub calls: Vec<ToolCall>,
    /// The reason.
    pub reason: String,
    /// The sandbox mode.
    pub sandbox_mode: SandboxMode,
    /// The network access.
    pub network_access: NetworkAccess,
    /// The decision received.
    pub decision_received: bool,
}

impl PendingApproval {
    /// Presents only the calls awaiting approval, excluding already authorized calls.
    #[must_use]
    pub fn request_event(&self) -> crate::protocol::ExecApprovalRequestEvent {
        crate::protocol::ExecApprovalRequestEvent {
            id: Arc::clone(&self.request_id),
            turn_id: Arc::clone(&self.turn_id),
            calls: self
                .calls
                .iter()
                .filter(|call| self.approval_call_ids.contains(&call.call_id))
                .map(|call| crate::protocol::ApprovalCall {
                    call_id: Arc::clone(&call.call_id),
                    name: Arc::clone(&call.name),
                    arguments: call.arguments.clone(), // rules-allow: the pending approval keeps its calls
                    preview: None,
                })
                .collect(),
            reason: self.reason.clone(), // rules-allow: the pending approval stays durable while the event is emitted
        }
    }
}
