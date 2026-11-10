//! Paged reads of the session catalog, transcript, executions, and event journal.

use crate::SessionId;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use super::CheckpointStore;
use super::ExecutionRecord;
use super::ExecutionStats;
use crate::Result;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::MessageTarget;
use crate::protocol::ModelStepContentPhase;
use crate::protocol::SessionContext;

pub(super) const TURN_PAGE_BATCH_SIZE: usize = 100;

/// Catalog metadata for one durable session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// The session identifier.
    pub session_id: SessionId,
    /// The session context.
    pub session_context: SessionContext,
    /// The parent session identifier.
    pub parent_session_id: Option<SessionId>,
    /// The parent sequence.
    pub parent_sequence: Option<u64>,
    /// The sequence.
    pub sequence: u64,
    /// The catalog visible.
    pub catalog_visible: bool,
    /// The first user message.
    pub first_user_message: Option<String>,
    /// The execution stats.
    pub execution_stats: ExecutionStats,
    /// The created at.
    pub created_at: i64,
    /// The updated at.
    pub updated_at: i64,
}

/// Stable key for continuing a newest-first session catalog query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCursor {
    /// The updated at.
    pub updated_at: i64,
    /// The sequence.
    pub sequence: u64,
    /// The session identifier.
    pub session_id: SessionId,
}

/// Bounds one newest-first session catalog query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPageRequest {
    /// Restricts sessions and cursor keys to this owner; `None` lists every owner.
    pub owner_id: Option<Arc<str>>,
    /// The cursor.
    pub cursor: Option<SessionCursor>,
    /// The limit.
    pub limit: usize,
}

/// One page of durable sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPage {
    /// The sessions.
    pub sessions: Vec<SessionSummary>,
    /// The next cursor.
    pub next_cursor: Option<SessionCursor>,
}

/// One append-only transcript delta at its durable checkpoint sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptBatch {
    /// The sequence.
    pub sequence: u64,
    /// The created at.
    pub created_at: i64,
    /// The items.
    pub items: Vec<Value>,
}

/// Bounds one newest-first execution-journal query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPageRequest {
    /// The before sequence.
    pub before_sequence: Option<u64>,
    /// The limit.
    pub limit: usize,
}

/// One newest-first page of terminal user-turn records.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPage {
    /// The executions.
    pub executions: Vec<ExecutionRecord>,
    /// The next before sequence.
    pub next_before_sequence: Option<u64>,
}

/// Bounds one newest-first transcript query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptPageRequest {
    /// The before sequence.
    pub before_sequence: Option<u64>,
    /// The max batches.
    pub max_batches: usize,
}

/// One newest-first page of transcript deltas.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptPage {
    /// The batches.
    pub batches: Vec<TranscriptBatch>,
    /// The next before sequence.
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
    /// The recorded at milliseconds.
    pub recorded_at_ms: i64,
    /// The event.
    pub event: Event,
}

/// Delivery metrics for one typed text stream within a completed model step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamMetrics {
    /// The phase.
    pub phase: ModelStepContentPhase,
    /// Receipt time of the first delivered batch in Unix milliseconds.
    pub first_delta_at_ms: i64,
    /// Receipt time of the first source delta in the last delivered batch.
    pub last_delta_at_ms: i64,
    /// Durable batch count; adjacent provider deltas may be coalesced.
    pub chunk_count: u64,
    /// The utf8 bytes.
    pub utf8_bytes: u64,
    /// Longest gap between batch receipt times, in milliseconds.
    pub longest_gap_ms: u64,
}

/// Bounds one newest-first event-journal query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventPageRequest {
    /// The before sequence.
    pub before_sequence: Option<u64>,
    /// The limit.
    pub limit: usize,
}

/// One newest-first page of normalized session events.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventPage {
    /// Durable sequence high-water, including intentionally discarded transient events.
    pub latest_sequence: u64,
    /// The events.
    pub events: Vec<JournalEvent>,
    /// The next before sequence.
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
/// # Errors
///
/// Returns an error if validation or an operation required by this function fails.
pub async fn event_turn_page(
    checkpoints: &dyn CheckpointStore,
    session_id: &SessionId,
    before_sequence: Option<u64>,
) -> Result<EventPage> {
    let mut cursor = before_sequence;
    let mut latest_sequence = 0;
    let mut events = Vec::new();
    let mut found_start: Option<Arc<str>> = None;
    let mut has_earlier_turn = false;
    // Join records between promotion starts; only agent starts survive an older turn gap.
    let mut starts_before = Vec::new();
    let mut in_gap = true;

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
            if let Some(turn_id) = &found_start {
                match &event.event.msg {
                    EventMsg::TurnStarted(started) if &started.turn_id == turn_id => {
                        events.append(&mut starts_before);
                        events.push(event);
                        in_gap = true;
                    }
                    EventMsg::TurnStarted(_) => {
                        has_earlier_turn = true;
                        break;
                    }
                    EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) => in_gap = false,
                    _ if in_gap => starts_before.push(event),
                    _ => {}
                }
            } else {
                if let EventMsg::TurnStarted(started) = &event.event.msg {
                    found_start = Some(Arc::clone(&started.turn_id));
                }
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

    let Some((start_index, start_sequence, turn_id)) =
        events.iter().enumerate().rev().find_map(|(index, event)| {
            let EventMsg::TurnStarted(started) = &event.event.msg else {
                return None;
            };
            Some((index, event.sequence, &*started.turn_id))
        })
    else {
        return Ok(EventPage {
            latest_sequence,
            events: Vec::new(),
            next_before_sequence: None,
        });
    };
    let page_start = events
        .iter()
        .take(start_index)
        .position(|event| match &event.event.msg {
            EventMsg::TurnComplete(completed) => *completed.turn_id == *turn_id,
            EventMsg::TurnAborted(aborted) => *aborted.turn_id == *turn_id,
            _ => false,
        })
        .unwrap_or(0);
    let next_before_sequence = has_earlier_turn.then_some(start_sequence);
    let mut events: Vec<_> = events.drain(page_start..=start_index).collect();
    if has_earlier_turn {
        events.extend(
            starts_before
                .into_iter()
                .filter(|event| matches!(event.event.msg, EventMsg::SessionConfigured(_))),
        );
    }

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
                                batch_item_count: std::num::NonZeroUsize::MIN.saturating_add(index),
                            },
                            item,
                        )
                    })
            })
            .collect()
    }
}
