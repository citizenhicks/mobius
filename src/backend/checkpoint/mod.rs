//! Durable agent checkpoints.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::backend::model::ToolCall;
use crate::backend::sandbox::NetworkAccess;
use crate::backend::sandbox::SandboxMode;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::MAX_MESSAGE_BYTES;
use crate::protocol::MessageAuthor;
use crate::protocol::MessageDelivery;
use crate::protocol::MessageEvent;
use crate::protocol::MessageTarget;
use crate::protocol::ModelStepContentPhase;
use crate::protocol::SessionContext;
use crate::protocol::SessionFileReference;
use crate::protocol::TokenUsage;

pub mod sqlite;

pub(crate) const CHECKPOINT_VERSION: u32 = 12;
pub(crate) const MAX_QUEUED_MESSAGES: usize = 1_024;
const TURN_PAGE_BATCH_SIZE: usize = 100;
const MAX_QUEUED_OWNER_BYTES: usize = 256;
const MAX_QUEUED_ID_BYTES: usize = 4 * 1024;
const MAX_QUEUED_TURN_ID_BYTES: usize = 4 * 1024;
const MAX_QUEUED_MESSAGE_BYTES: usize = MAX_MESSAGE_BYTES * 2;

/// Durable phase of the user turn currently running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionPhase {
    Model,
    Completion {
        last_assistant_message: Option<String>,
    },
}

/// Mutable state for the user turn currently running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveExecution {
    pub submission_id: String,
    pub turn_id: String,
    pub started_at_ms: i64,
    pub model_calls: u64,
    pub tool_calls: u64,
    pub failed_tool_calls: u64,
    pub usage: TokenUsage,
    pub next_model_step: usize,
    pub stop_hook_active: bool,
    pub phase: ExecutionPhase,
}

/// The model step currently in flight for an active execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveModelStep {
    pub model_step_id: String,
    pub step_index: usize,
    pub started_at_ms: i64,
}

/// Terminal outcome of one user turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Completed,
    Aborted,
    Failed,
}

/// Durable observability record for one completed user turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub session_id: String,
    pub submission_id: String,
    pub turn_id: String,
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub elapsed_ms: u64,
    pub outcome: ExecutionOutcome,
    pub model_calls: u64,
    pub tool_calls: u64,
    pub failed_tool_calls: u64,
    pub usage: TokenUsage,
}

/// Aggregate execution metrics for one durable session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionStats {
    pub run_count: u64,
    pub failed_run_count: u64,
    pub aborted_run_count: u64,
    pub model_calls: u64,
    pub tool_calls: u64,
    pub failed_tool_calls: u64,
    pub elapsed_ms: u64,
    pub usage: TokenUsage,
}

/// One intentional replacement of active model history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextRewriteReason {
    ContextOffloading,
    Compaction,
    Scratchpad,
}

impl ContextRewriteReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ContextOffloading => "context_offloading",
            Self::Compaction => "compaction",
            Self::Scratchpad => "scratchpad",
        }
    }
}

/// The latest deliberate active-context rewrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRewrite {
    pub epoch: u64,
    pub reasons: Vec<ContextRewriteReason>,
}

impl ExecutionStats {
    pub(crate) fn checked_record(&mut self, record: &ExecutionRecord) -> Option<()> {
        let run_count = self.run_count.checked_add(1)?;
        let failed_run_count = self
            .failed_run_count
            .checked_add(u64::from(record.outcome == ExecutionOutcome::Failed))?;
        let aborted_run_count = self
            .aborted_run_count
            .checked_add(u64::from(record.outcome == ExecutionOutcome::Aborted))?;
        let model_calls = self.model_calls.checked_add(record.model_calls)?;
        let tool_calls = self.tool_calls.checked_add(record.tool_calls)?;
        let failed_tool_calls = self
            .failed_tool_calls
            .checked_add(record.failed_tool_calls)?;
        let elapsed_ms = self.elapsed_ms.checked_add(record.elapsed_ms)?;
        let mut usage = self.usage.clone();
        usage.checked_add(&record.usage)?;
        *self = Self {
            run_count,
            failed_run_count,
            aborted_run_count,
            model_calls,
            tool_calls,
            failed_tool_calls,
            elapsed_ms,
            usage,
        };
        Some(())
    }
}

/// A tool batch waiting for a frontend decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingApproval {
    pub submission_id: String,
    pub turn_id: String,
    pub request_id: String,
    pub approval_call_ids: Vec<String>,
    pub authorized_call_ids: Vec<String>,
    pub calls: Vec<ToolCall>,
    pub reason: String,
    pub sandbox_mode: SandboxMode,
    pub network_access: NetworkAccess,
    pub decision_received: bool,
}

/// The single delivery boundary for one queued message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueuedMessageBoundary {
    Turn,
    Steer { turn_id: String },
    Queue,
}

impl QueuedMessageBoundary {
    pub(crate) const fn delivery(&self) -> MessageDelivery {
        match self {
            Self::Turn => MessageDelivery::Turn,
            Self::Steer { .. } => MessageDelivery::Steer,
            Self::Queue => MessageDelivery::Queue,
        }
    }

    pub(crate) const fn starts_turn(&self) -> bool {
        matches!(self, Self::Turn | Self::Queue)
    }
}

/// One typed conversation message waiting for its delivery boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedMessage {
    owner: String,
    id: String,
    boundary: QueuedMessageBoundary,
    author: MessageAuthor,
    message: String,
    attachments: Vec<SessionFileReference>,
}

impl QueuedMessage {
    pub(crate) fn new(
        owner: &str,
        id: &str,
        boundary: QueuedMessageBoundary,
        event: MessageEvent,
    ) -> Result<Self> {
        if event.delivery != boundary.delivery() || event.message_target.is_some() {
            return Err(Error::Config("queued message event is inconsistent".into()));
        }
        let queued = Self {
            owner: owner.into(),
            id: id.into(),
            boundary,
            author: event.author,
            message: event.text,
            attachments: event.attachments,
        };
        queued.validate()?;
        Ok(queued)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        validate_queued_message(self)
    }

    pub(crate) fn owner(&self) -> &str {
        &self.owner
    }

    /// Returns the submission that owns this queued message.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn boundary(&self) -> &QueuedMessageBoundary {
        &self.boundary
    }

    pub(crate) fn event(&self) -> MessageEvent {
        MessageEvent {
            author: self.author.clone(),
            delivery: self.boundary.delivery(),
            text: self.message.clone(),
            attachments: self.attachments.clone(),
            message_target: None,
        }
    }

    pub(crate) fn replace(&mut self, id: &str, event: MessageEvent) -> Result<()> {
        let replacement = Self::new(&self.owner, id, self.boundary.clone(), event)?;
        *self = replacement;
        Ok(())
    }

    pub(crate) fn promote_to_next_turn(&mut self) -> Result<()> {
        self.boundary = QueuedMessageBoundary::Queue;
        Ok(())
    }

    pub(crate) fn into_parts(self) -> (String, MessageEvent) {
        let event = self.event();
        (self.id, event)
    }
}

fn validate_queued_message(message: &QueuedMessage) -> Result<()> {
    if message.owner.trim().is_empty() || message.owner.len() > MAX_QUEUED_OWNER_BYTES {
        return Err(Error::Config("queued message owner is invalid".into()));
    }
    if message.id.trim().is_empty() || message.id.len() > MAX_QUEUED_ID_BYTES {
        return Err(Error::Config("queued message ID is invalid".into()));
    }
    if matches!(
        &message.boundary,
        QueuedMessageBoundary::Steer { turn_id }
            if turn_id.trim().is_empty() || turn_id.len() > MAX_QUEUED_TURN_ID_BYTES
    ) {
        return Err(Error::Config("queued message turn ID is invalid".into()));
    }
    crate::protocol::validate_message_content(
        &message.author,
        &message.message,
        &message.attachments,
    )?;
    if serde_json::to_vec(&message.event())
        .map_or(true, |value| value.len() > MAX_QUEUED_MESSAGE_BYTES)
    {
        return Err(Error::Config("queued message is invalid".into()));
    }
    Ok(())
}

/// Versioned state persisted at each durable loop boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub version: u32,
    pub session_id: String,
    pub session_context: SessionContext,
    pub metadata: BTreeMap<String, Value>,
    pub catalog_visible: bool,
    pub first_user_message: Option<String>,
    pub model_route: Option<String>,
    pub sequence: u64,
    pub context: Vec<Value>,
    pub context_epoch: u64,
    pub compaction_count: u64,
    pub last_context_rewrite: Option<ContextRewrite>,
    pub total_usage: TokenUsage,
    pub last_usage: Option<TokenUsage>,
    pub pending_messages: Vec<QueuedMessage>,
    pub active_execution: Option<ActiveExecution>,
    pub active_model_step: Option<ActiveModelStep>,
    pub execution_stats: ExecutionStats,
    pub pending_tools: Vec<ToolCall>,
    pub pending_approval: Option<PendingApproval>,
}

impl Checkpoint {
    /// Creates an empty session checkpoint.
    #[must_use]
    pub fn empty(session_id: impl Into<String>) -> Self {
        Self {
            version: CHECKPOINT_VERSION,
            session_id: session_id.into(),
            session_context: SessionContext::default(),
            metadata: BTreeMap::new(),
            catalog_visible: true,
            first_user_message: None,
            model_route: None,
            sequence: 0,
            context: Vec::new(),
            context_epoch: 0,
            compaction_count: 0,
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
            .as_ref()
            .ok_or_else(|| Error::Checkpoint("turn ended without an active execution".into()))?;
        let finished_at_ms = finished_at_ms.max(active.started_at_ms);
        let elapsed_ms = u64::try_from(finished_at_ms - active.started_at_ms)
            .map_err(|_| Error::Checkpoint("execution elapsed time is unsupported".into()))?;
        let record = ExecutionRecord {
            session_id: self.session_id.clone(),
            submission_id: active.submission_id.clone(),
            turn_id: active.turn_id.clone(),
            started_at_ms: active.started_at_ms,
            finished_at_ms,
            elapsed_ms,
            outcome,
            model_calls: active.model_calls,
            tool_calls: active.tool_calls,
            failed_tool_calls: active.failed_tool_calls,
            usage: active.usage.clone(),
        };
        let mut stats = self.execution_stats.clone();
        stats.checked_record(&record).ok_or_else(|| {
            Error::Checkpoint("execution statistics exceed the supported range".into())
        })?;
        self.active_execution = None;
        self.execution_stats = stats;
        Ok(record)
    }
}

/// Catalog metadata for one durable session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub session_context: SessionContext,
    pub parent_session_id: Option<String>,
    pub parent_sequence: Option<u64>,
    pub sequence: u64,
    pub catalog_visible: bool,
    pub first_user_message: Option<String>,
    pub execution_stats: ExecutionStats,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Stable key for continuing a newest-first session catalog query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCursor {
    pub updated_at: i64,
    pub sequence: u64,
    pub session_id: String,
}

/// Bounds one newest-first session catalog query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPageRequest {
    pub cursor: Option<SessionCursor>,
    pub limit: usize,
}

/// One page of durable sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPage {
    pub sessions: Vec<SessionSummary>,
    pub next_cursor: Option<SessionCursor>,
}

/// One append-only transcript delta at its durable checkpoint sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptBatch {
    pub sequence: u64,
    pub created_at: i64,
    pub items: Vec<Value>,
}

/// Bounds one newest-first execution-journal query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPageRequest {
    pub before_sequence: Option<u64>,
    pub limit: usize,
}

/// One newest-first page of terminal user-turn records.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPage {
    pub executions: Vec<ExecutionRecord>,
    pub next_before_sequence: Option<u64>,
}

/// Bounds one newest-first transcript query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptPageRequest {
    pub before_sequence: Option<u64>,
    pub max_batches: usize,
}

/// One newest-first page of transcript deltas.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptPage {
    pub batches: Vec<TranscriptBatch>,
    pub next_before_sequence: Option<u64>,
}

/// One normalized frontend event in the durable session journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEvent {
    /// Monotonic sequence within one session.
    pub sequence: u64,
    /// Framework record time in Unix milliseconds.
    pub recorded_at_ms: i64,
    /// Provider-neutral framework event.
    pub event: Event,
    /// Compact delivery characteristics retained after progressive deltas are removed.
    pub stream_metrics: Vec<StreamMetrics>,
}

/// One normalized event paired with its framework receipt time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimestampedEvent {
    pub recorded_at_ms: i64,
    pub event: Event,
}

/// Delivery metrics for one typed text stream within a completed model step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamMetrics {
    pub phase: ModelStepContentPhase,
    pub first_delta_at_ms: i64,
    pub last_delta_at_ms: i64,
    pub chunk_count: u64,
    pub utf8_bytes: u64,
    pub longest_gap_ms: u64,
}

/// Bounds one newest-first event-journal query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventPageRequest {
    pub before_sequence: Option<u64>,
    pub limit: usize,
}

/// One newest-first page of normalized session events.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventPage {
    /// Durable sequence high-water, including intentionally discarded transient events.
    pub latest_sequence: u64,
    pub events: Vec<JournalEvent>,
    pub next_before_sequence: Option<u64>,
}

impl EventPage {
    /// Returns this newest-first page in replay order.
    #[must_use]
    pub fn into_chronological(mut self) -> Vec<JournalEvent> {
        self.events.reverse();
        self.events
    }
}

/// Loads the newest logical turn before a durable event cursor.
pub async fn event_turn_page(
    checkpoints: &dyn CheckpointStore,
    session_id: &str,
    before_sequence: Option<u64>,
) -> Result<EventPage> {
    let mut cursor = before_sequence;
    let mut latest_sequence = 0;
    let mut events = Vec::new();
    let mut found_start = false;
    let mut has_earlier_turn = false;

    loop {
        let page = checkpoints
            .event_page(
                session_id,
                EventPageRequest {
                    before_sequence: cursor,
                    limit: TURN_PAGE_BATCH_SIZE,
                },
            )
            .await?;
        if events.is_empty() {
            latest_sequence = page.latest_sequence;
        }
        for event in page.events {
            if found_start {
                if matches!(&event.event.msg, EventMsg::TurnStarted(_)) {
                    has_earlier_turn = true;
                    break;
                }
            } else {
                found_start = matches!(&event.event.msg, EventMsg::TurnStarted(_));
                events.push(event);
            }
        }
        if has_earlier_turn {
            break;
        }
        let Some(next) = page.next_before_sequence else {
            break;
        };
        cursor = Some(next);
    }

    let Some((start_index, turn_id)) = events.iter().enumerate().find_map(|(index, event)| {
        let EventMsg::TurnStarted(started) = &event.event.msg else {
            return None;
        };
        Some((index, started.turn_id.as_str()))
    }) else {
        return Ok(EventPage {
            latest_sequence,
            events: Vec::new(),
            next_before_sequence: None,
        });
    };
    let page_start = events[..start_index]
        .iter()
        .position(|event| match &event.event.msg {
            EventMsg::TurnComplete(completed) => completed.turn_id == turn_id,
            EventMsg::TurnAborted(aborted) => aborted.turn_id == turn_id,
            _ => false,
        })
        .unwrap_or(0);
    let next_before_sequence = has_earlier_turn.then_some(events[start_index].sequence);
    let events = events.drain(page_start..=start_index).collect();

    Ok(EventPage {
        latest_sequence,
        events,
        next_before_sequence,
    })
}

impl TranscriptPage {
    /// Flattens this newest-first page into chronological items with durable positions.
    #[must_use]
    pub fn into_positioned_items_chronological(self) -> Vec<(MessageTarget, Value)> {
        self.batches
            .into_iter()
            .rev()
            .flat_map(|batch| {
                batch
                    .items
                    .into_iter()
                    .enumerate()
                    .map(move |(index, item)| {
                        (
                            MessageTarget {
                                checkpoint_sequence: batch.sequence,
                                batch_item_count: index + 1,
                            },
                            item,
                        )
                    })
            })
            .collect()
    }
}

/// Stores durable session checkpoints and middleware state.
pub trait CheckpointStore: Send + Sync {
    /// Loads the latest checkpoint for a session.
    fn load<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<Option<Checkpoint>>>;

    /// Permanently deletes a session, its descendants, and their session-scoped state.
    ///
    /// Returns whether the requested session existed.
    fn delete_session<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<bool>>;

    /// Atomically replaces the checkpoint, appends transcript items, and records a finished turn.
    fn save<'a>(
        &'a self,
        checkpoint: &'a Checkpoint,
        transcript_delta: &'a [Value],
        execution: Option<&'a ExecutionRecord>,
    ) -> BoxFuture<'a, Result<()>>;

    /// Atomically saves one checkpoint and appends its normalized event batch.
    fn save_with_events<'a>(
        &'a self,
        checkpoint: &'a Checkpoint,
        transcript_delta: &'a [Value],
        execution: Option<&'a ExecutionRecord>,
        events: &'a [TimestampedEvent],
    ) -> BoxFuture<'a, Result<Vec<JournalEvent>>>;

    /// Assigns a session-local sequence and appends one normalized event atomically.
    fn append_event<'a>(
        &'a self,
        session_id: &'a str,
        recorded_at_ms: i64,
        event: &'a Event,
    ) -> BoxFuture<'a, Result<JournalEvent>>;

    /// Loads one newest-first page of normalized session events.
    fn event_page<'a>(
        &'a self,
        session_id: &'a str,
        request: EventPageRequest,
    ) -> BoxFuture<'a, Result<EventPage>>;

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
        session_id: &'a str,
        request: TranscriptPageRequest,
    ) -> BoxFuture<'a, Result<TranscriptPage>> {
        Box::pin(async move {
            if request.max_batches == 0 {
                return Err(Error::Checkpoint(
                    "transcript page limit must be positive".into(),
                ));
            }
            let Some(checkpoint) = self.load(session_id).await? else {
                return Ok(TranscriptPage::default());
            };
            if checkpoint.context.is_empty()
                || request
                    .before_sequence
                    .is_some_and(|before| checkpoint.sequence >= before)
            {
                return Ok(TranscriptPage::default());
            }
            Ok(TranscriptPage {
                batches: vec![TranscriptBatch {
                    sequence: checkpoint.sequence,
                    created_at: 0,
                    items: checkpoint.context,
                }],
                next_before_sequence: None,
            })
        })
    }

    /// Loads one newest-first page of terminal user-turn records.
    fn execution_page<'a>(
        &'a self,
        _session_id: &'a str,
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
        _parent_session_id: &'a str,
        _parent_sequence: u64,
        _checkpoint: &'a Checkpoint,
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
