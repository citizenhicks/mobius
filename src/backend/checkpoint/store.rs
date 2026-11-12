//! The checkpoint snapshot and the store that persists it.

use crate::SessionId;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use super::ActiveExecution;
use super::ActiveModelStep;
use super::ContextRewrite;
use super::EventPage;
use super::EventPageRequest;
use super::ExecutionOutcome;
use super::ExecutionPage;
use super::ExecutionPageRequest;
use super::ExecutionPhase;
use super::ExecutionRecord;
use super::ExecutionStats;
use super::JournalEvent;
use super::PendingApproval;
use super::QueuedMessage;
use super::SessionPage;
use super::SessionPageRequest;
use super::SessionSummary;
use super::TimestampedEvent;
use super::TranscriptPage;
use super::TranscriptPageRequest;
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::protocol::Event;
use crate::protocol::SessionContext;
use crate::protocol::TokenUsage;
use crate::protocol::ToolCall;

pub(crate) const CHECKPOINT_VERSION: u32 = 21;
pub(crate) const MAX_QUEUED_MESSAGES: usize = 1_024;

pub(super) fn validate_session_context(context: &SessionContext) -> Result<()> {
    context.validate().map_err(|error| match error {
        Error::Config(message) => Error::Checkpoint(message),
        error => error,
    })
}

/// Versioned state persisted at each durable loop boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// The version.
    pub version: u32,
    /// The session identifier.
    pub session_id: SessionId,
    /// The session context.
    pub session_context: Arc<SessionContext>,
    /// The metadata.
    pub metadata: BTreeMap<String, Value>,
    /// The catalog visible.
    pub catalog_visible: bool,
    /// The first user message.
    pub first_user_message: Option<String>,
    /// The model route.
    pub model_route: Option<Arc<str>>,
    /// The model route whose accepted output last contributed to active context.
    pub context_model_route: Option<Arc<str>>,
    /// Monotonically increasing durable checkpoint revision for this session.
    pub sequence: u64,
    /// The complete active model context.
    pub context: Arc<Vec<Arc<Value>>>,
    /// Session-scoped receipts for middleware guidance durably added to context.
    pub delivered_once: std::sync::Arc<BTreeMap<String, BTreeSet<String>>>,
    /// Monotonically increasing revision of the active context prefix.
    /// Advance it before changing or truncating already persisted context items;
    /// an unchanged epoch permits only an identical prefix with optional appends.
    pub context_epoch: u64,
    /// The last context rewrite.
    pub last_context_rewrite: Option<ContextRewrite>,
    /// The total usage.
    pub total_usage: TokenUsage,
    /// Usage of the last foreground model request; cleared when its context is rewritten.
    pub last_usage: Option<TokenUsage>,
    /// The pending messages.
    pub pending_messages: Vec<QueuedMessage>,
    /// The active execution.
    pub active_execution: Option<ActiveExecution>,
    /// The active model step.
    pub active_model_step: Option<ActiveModelStep>,
    /// The execution stats.
    pub execution_stats: ExecutionStats,
    /// The pending tools.
    pub pending_tools: Vec<ToolCall>,
    /// The pending approval.
    pub pending_approval: Option<PendingApproval>,
}

impl Checkpoint {
    /// Creates an empty session checkpoint.
    #[must_use]
    pub fn empty(session_id: SessionId) -> Self {
        Self {
            version: CHECKPOINT_VERSION,
            session_id,
            session_context: Arc::default(),
            metadata: BTreeMap::new(),
            catalog_visible: true,
            first_user_message: None,
            model_route: None,
            context_model_route: None,
            sequence: 0,
            context: Arc::default(),
            delivered_once: Default::default(),
            context_epoch: 0,
            last_context_rewrite: None,
            total_usage: TokenUsage::default(),
            last_usage: None,
            pending_messages: Vec::new(),
            active_execution: None,
            active_model_step: None,
            execution_stats: ExecutionStats::default(),
            pending_tools: Vec::new(),
            pending_approval: None,
        }
    }

    /// The sequence the next durable save of this checkpoint takes.
    pub(crate) fn next_sequence(&self) -> Result<u64> {
        self.sequence
            .checked_add(1)
            .ok_or_else(|| Error::Checkpoint("checkpoint sequence overflow".into()))
    }

    /// Checks the rules every durable checkpoint obeys, whichever store holds it.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.version != CHECKPOINT_VERSION {
            return Err(Error::Checkpoint(format!(
                "unsupported checkpoint version {}",
                self.version
            )));
        }
        if let Some(active) = &self.active_execution {
            if active.submission_id.trim().is_empty() || active.turn_id.trim().is_empty() {
                return Err(Error::Checkpoint(
                    "active execution identifiers cannot be empty".into(),
                ));
            }
            if active.started_at_ms < 0 {
                return Err(Error::Checkpoint(
                    "active execution start time cannot be negative".into(),
                ));
            }
            if active.failed_tool_calls > active.tool_calls {
                return Err(Error::Checkpoint(
                    "active execution failed-tool count exceeds tool count".into(),
                ));
            }
            if matches!(&active.phase, ExecutionPhase::Completion { .. })
                && (self.active_model_step.is_some()
                    || !self.pending_tools.is_empty()
                    || self.pending_approval.is_some())
            {
                return Err(Error::Checkpoint(
                    "turn completion conflicts with pending model or tool work".into(),
                ));
            }
        } else if self.active_model_step.is_some()
            || !self.pending_tools.is_empty()
            || self.pending_approval.is_some()
        {
            return Err(Error::Checkpoint(
                "pending model or tool work has no active execution".into(),
            ));
        }
        if let Some(approval) = &self.pending_approval {
            let known = approval
                .calls
                .iter()
                .map(|call| call.call_id.as_ref())
                .collect::<BTreeSet<_>>();
            let selected = approval
                .approval_call_ids
                .iter()
                .map(AsRef::as_ref)
                .collect::<BTreeSet<&str>>();
            if selected.is_empty()
                || selected.len() != approval.approval_call_ids.len()
                || !selected.is_subset(&known)
            {
                return Err(Error::Checkpoint(
                    "pending approval selects unknown or repeated tool calls".into(),
                ));
            }
        }
        validate_session_context(&self.session_context)?;
        if let Some(step) = &self.active_model_step {
            let execution = self.active_execution.as_ref().ok_or_else(|| {
                Error::Checkpoint("active model step has no active execution".into())
            })?;
            if step.model_step_id.trim().is_empty() {
                return Err(Error::Checkpoint(
                    "active model-step identifier cannot be empty".into(),
                ));
            }
            if step.started_at_ms < execution.started_at_ms {
                return Err(Error::Checkpoint(
                    "active model step predates its execution".into(),
                ));
            }
        }
        if self.pending_messages.len() > MAX_QUEUED_MESSAGES {
            return Err(Error::Checkpoint(
                "queued messages exceed the durable item limit".into(),
            ));
        }
        let mut pending_ids = BTreeSet::new();
        for message in &self.pending_messages {
            if !pending_ids.insert((message.owner(), message.id())) {
                return Err(Error::Checkpoint(format!(
                    "duplicate queued message `{}/{}`",
                    message.owner(),
                    message.id(),
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn finish_execution(
        &mut self,
        outcome: ExecutionOutcome,
        finished_at_ms: i64,
    ) -> Result<ExecutionRecord> {
        if self.active_model_step.is_some() {
            return Err(Error::Checkpoint(
                "turn ended with an active model step".into(),
            ));
        }
        let active = self
            .active_execution
            .take()
            .ok_or_else(|| Error::Checkpoint("turn ended without an active execution".into()))?;
        let finished_at_ms = finished_at_ms.max(active.started_at_ms);
        let elapsed_ms = finished_at_ms.abs_diff(active.started_at_ms);
        let record = ExecutionRecord {
            session_id: self.session_id.share(),
            submission_id: active.submission_id,
            author: active.author,
            turn_id: active.turn_id,
            started_at_ms: active.started_at_ms,
            finished_at_ms,
            elapsed_ms,
            outcome,
            model_calls: active.model_calls,
            tool_calls: active.tool_calls,
            failed_tool_calls: active.failed_tool_calls,
            usage: active.usage,
        };
        if self.execution_stats.checked_record(&record).is_none() {
            self.active_execution = Some(ActiveExecution {
                submission_id: record.submission_id,
                author: record.author,
                turn_id: record.turn_id,
                usage: record.usage,
                ..active
            });
            return Err(Error::Checkpoint(
                "execution statistics exceed the supported range".into(),
            ));
        }
        Ok(record)
    }
}

/// One immutable checkpoint that passed validation, the only form a store's batched save takes.
#[derive(Debug)]
pub struct ValidatedCheckpoint(Arc<Checkpoint>);

impl ValidatedCheckpoint {
    /// Validates a snapshot for saving.
    /// # Errors
    ///
    /// Returns an error if the checkpoint could not be read back after saving.
    pub fn new(checkpoint: Arc<Checkpoint>) -> Result<Self> {
        checkpoint.validate()?;
        Ok(Self(checkpoint))
    }
}

impl std::ops::Deref for ValidatedCheckpoint {
    type Target = Checkpoint;

    fn deref(&self) -> &Checkpoint {
        &self.0
    }
}

/// Stores durable session checkpoints and middleware state.
///
/// Atomic operations must not expose a partially committed logical update.
/// Power-loss durability depends on the backend's storage guarantees. Optional
/// catalog, transcript, execution-history, and fork methods return explicit
/// unsupported errors unless implemented; compacted context is not a transcript
/// journal.
pub trait CheckpointStore: Send + Sync {
    /// Loads the latest checkpoint for a session.
    fn load<'a>(&'a self, session_id: &'a SessionId) -> BoxFuture<'a, Result<Option<Checkpoint>>>;

    /// Returns whether this session durably admitted a message with the submission ID.
    ///
    /// Receipts survive queue consumption and must be written in the same transaction
    /// as the checkpoint containing the admitted message.
    fn message_accepted<'a>(
        &'a self,
        session_id: &'a SessionId,
        submission_id: &'a str,
    ) -> BoxFuture<'a, Result<bool>>;

    /// Atomically deletes the selected sessions, their descendants, and session-scoped state.
    ///
    /// Returns `false` without deleting anything when any selected session is absent.
    fn delete_sessions<'a>(&'a self, session_ids: &'a [SessionId]) -> BoxFuture<'a, Result<bool>>;

    /// Atomically replaces the checkpoint, appends transcript items, and records a finished turn.
    ///
    /// Existing sessions must advance `sequence`. Within one `context_epoch`,
    /// previously stored context items remain an immutable prefix; advance the
    /// epoch when rewriting or truncating that prefix. SQLite rejects violations
    /// rather than silently discarding the changed context.
    fn save(
        &self,
        checkpoint: Checkpoint,
        transcript_delta: Vec<Arc<Value>>,
        execution: Option<ExecutionRecord>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.save_with_events(
                ValidatedCheckpoint::new(Arc::new(checkpoint))?,
                transcript_delta,
                execution,
                Vec::new(),
            )
            .await
            .map(drop)
        })
    }

    /// Atomically saves one immutable checkpoint snapshot and its normalized event batch.
    ///
    /// Shared ownership lets storage transfer the snapshot without copying its payload.
    /// The snapshot's type proves it passed validation, so a store does not check it again.
    ///
    /// The checkpoint, transcript delta, optional execution record, and journal
    /// events form one commit boundary. Return the durably assigned event
    /// sequences only after that commit succeeds. The sequence and context-prefix
    /// requirements of [`Self::save`] also apply.
    fn save_with_events(
        &self,
        checkpoint: ValidatedCheckpoint,
        transcript_delta: Vec<std::sync::Arc<Value>>,
        execution: Option<ExecutionRecord>,
        events: Vec<TimestampedEvent>,
    ) -> BoxFuture<'_, Result<Vec<JournalEvent>>>;

    /// Assigns a session-local sequence and appends one normalized event atomically.
    fn append_event<'a>(
        &'a self,
        session_id: &'a SessionId,
        recorded_at_ms: i64,
        event: Event,
    ) -> BoxFuture<'a, Result<JournalEvent>>;

    /// Loads one newest-first page of normalized session events.
    fn event_page<'a>(
        &'a self,
        session_id: &'a SessionId,
        request: EventPageRequest,
    ) -> BoxFuture<'a, Result<EventPage>>;

    /// Loads retained events after a durable cursor in ascending sequence order.
    fn events_after<'a>(
        &'a self,
        _session_id: &'a SessionId,
        _after_sequence: u64,
        _limit: usize,
    ) -> BoxFuture<'a, Result<Vec<JournalEvent>>> {
        Box::pin(async {
            Err(Error::Checkpoint(
                "this backend has no ordered event catch-up".into(),
            ))
        })
    }

    /// Loads catalog metadata for one session without reading its checkpoint context.
    fn session_summary<'a>(
        &'a self,
        _session_id: &'a SessionId,
    ) -> BoxFuture<'a, Result<Option<SessionSummary>>> {
        Box::pin(async {
            Err(Error::Checkpoint(
                "this checkpoint backend has no session catalog".into(),
            ))
        })
    }

    /// Reads session metadata without materializing the transcript or model context.
    /// Backends may override the checkpoint-based default with a projection query.
    fn session_metadata<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> BoxFuture<'a, Result<Option<BTreeMap<String, Value>>>> {
        Box::pin(async move {
            Ok(self
                .load(session_id)
                .await?
                .map(|checkpoint| checkpoint.metadata))
        })
    }

    /// Lists one page of the most recently updated sessions, newest first.
    fn list_sessions_page(
        &self,
        _request: SessionPageRequest,
    ) -> BoxFuture<'_, Result<SessionPage>> {
        Box::pin(async {
            Err(Error::Checkpoint(
                "this checkpoint backend has no session catalog".into(),
            ))
        })
    }

    /// Loads one newest-first page of append-only transcript deltas.
    fn transcript_page<'a>(
        &'a self,
        _session_id: &'a SessionId,
        _request: TranscriptPageRequest,
    ) -> BoxFuture<'a, Result<TranscriptPage>> {
        Box::pin(async {
            Err(Error::Checkpoint(
                "this checkpoint backend has no transcript journal".into(),
            ))
        })
    }

    /// Loads one newest-first page of terminal user-turn records.
    fn execution_page<'a>(
        &'a self,
        _session_id: &'a SessionId,
        _request: ExecutionPageRequest,
    ) -> BoxFuture<'a, Result<ExecutionPage>> {
        Box::pin(async {
            Err(Error::Checkpoint(
                "this checkpoint backend has no execution journal".into(),
            ))
        })
    }

    /// Loads the most recently started terminal user turns across all sessions.
    fn recent_executions(&self, _limit: usize) -> BoxFuture<'_, Result<Vec<ExecutionRecord>>> {
        Box::pin(async {
            Err(Error::Checkpoint(
                "this checkpoint backend has no execution journal".into(),
            ))
        })
    }

    /// Creates a child session at an exact durable parent sequence.
    fn fork<'a>(
        &'a self,
        _parent_session_id: &'a SessionId,
        _parent_sequence: u64,
        _checkpoint: Checkpoint,
    ) -> BoxFuture<'a, Result<SessionSummary>> {
        Box::pin(async {
            Err(Error::Checkpoint(
                "this checkpoint backend cannot fork sessions".into(),
            ))
        })
    }

    /// Loads the latest opaque state owned by one middleware namespace.
    fn load_state<'a>(
        &'a self,
        scope: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>>>;

    /// Durably replaces opaque middleware state.
    fn save_state<'a>(
        &'a self,
        scope: &'a str,
        key: &'a str,
        value: &'a Value,
    ) -> BoxFuture<'a, Result<()>>;
}
