//! Event-journal indexing, compaction, and stream metrics.

use rusqlite::OptionalExtension;
use rusqlite::Transaction;
use rusqlite::params;

use super::super::JournalEvent;
use super::super::StreamMetrics;
use super::super::TimestampedEvent;
use crate::Error;
use crate::Result;
use crate::protocol::EventMsg;
use crate::protocol::FrontendEvent;
use crate::protocol::ModelStepContentPhase;

pub(super) fn store_event(
    transaction: &Transaction<'_>,
    session_id: &str,
    timestamped: TimestampedEvent,
) -> Result<JournalEvent> {
    let TimestampedEvent {
        recorded_at_ms,
        event,
    } = timestamped;
    if recorded_at_ms < 0 {
        return Err(Error::Checkpoint(
            "event journal timestamp cannot be negative".into(),
        ));
    }
    let has_authoritative_snapshot = matches!(
        &event.msg,
        EventMsg::AssistantMessage(message) if !message.content.is_empty()
    );
    let discard_after_delivery = is_transient_event(&event.msg);
    let index = event_index(&event.msg)?;
    let event_json = serde_json::to_string(&event)?;
    let latest = transaction
        .query_row(
            "SELECT latest_event_sequence FROM sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .ok_or_else(|| Error::Checkpoint("event journal session does not exist".into()))?;
    let sequence = latest
        .checked_add(1)
        .ok_or_else(|| Error::Checkpoint("event journal sequence overflow".into()))?;
    let stream_metrics = if index.kind == "model_step_completed" {
        index
            .model_step_id
            .map(|model_step_id| load_stream_metrics(transaction, session_id, model_step_id))
            .transpose()?
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let stream_metrics_json = serde_json::to_string(&stream_metrics)?;
    transaction.execute(
        "INSERT INTO event_journal (
             session_id, sequence, recorded_at_ms, event_kind, model_step_id,
             stream_phase, delta_bytes, event_json, stream_metrics_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            session_id,
            sequence,
            recorded_at_ms,
            index.kind,
            index.model_step_id,
            index.stream_phase.map(stream_phase_name),
            index.delta_bytes,
            event_json,
            stream_metrics_json,
        ],
    )?;
    transaction.execute(
        "UPDATE sessions SET latest_event_sequence = ?2 WHERE session_id = ?1",
        params![session_id, sequence],
    )?;
    if matches!(&event.msg, EventMsg::Message(_)) {
        transaction.execute(
            "DELETE FROM event_journal WHERE session_id = ?1 AND event_kind = 'message_delta'
             AND json_extract(event_json, '$.submission_id') = ?2",
            params![session_id, event.submission_id],
        )?;
    } else if has_authoritative_snapshot && let Some(model_step_id) = index.model_step_id {
        transaction.execute(
            "DELETE FROM event_journal
             WHERE session_id = ?1 AND model_step_id = ?2
               AND event_kind IN (
                   'assistant_content_delta'
               )",
            params![session_id, model_step_id],
        )?;
    } else if index.kind == "token_count" {
        transaction.execute(
            "DELETE FROM event_journal
             WHERE session_id = ?1 AND event_kind = 'token_count' AND sequence < ?2",
            params![session_id, sequence],
        )?;
    }
    if discard_after_delivery {
        transaction.execute(
            "DELETE FROM event_journal WHERE session_id = ?1 AND sequence = ?2",
            params![session_id, sequence],
        )?;
    }
    Ok(JournalEvent {
        sequence: u64::try_from(sequence)
            .map_err(|_| Error::Checkpoint("event journal sequence became negative".into()))?,
        recorded_at_ms,
        event,
        stream_metrics,
    })
}

struct EventIndex<'a> {
    kind: &'static str,
    model_step_id: Option<&'a str>,
    stream_phase: Option<ModelStepContentPhase>,
    delta_bytes: Option<i64>,
}

fn event_index(event: &EventMsg) -> Result<EventIndex<'_>> {
    let plain = |kind| EventIndex {
        kind,
        model_step_id: None,
        stream_phase: None,
        delta_bytes: None,
    };
    let step = |kind, model_step_id| EventIndex {
        kind,
        model_step_id: Some(model_step_id),
        stream_phase: None,
        delta_bytes: None,
    };
    Ok(match event {
        EventMsg::Error(_) => plain("error"),
        EventMsg::Warning(_) => plain("warning"),
        EventMsg::SubmissionRejected(_) => plain("submission_rejected"),
        EventMsg::SessionConfigured(_) => plain("session_configured"),
        EventMsg::TurnStarted(_) => plain("turn_started"),
        EventMsg::TurnComplete(_) => plain("turn_complete"),
        EventMsg::TurnAborted(_) => plain("turn_aborted"),
        EventMsg::Message(_) => plain("message"),
        EventMsg::MessageDelta(_) => plain("message_delta"),
        EventMsg::AssistantMessage(message) => step("assistant_message", &message.model_step_id),
        EventMsg::AssistantContentDelta(delta) => EventIndex {
            kind: "assistant_content_delta",
            model_step_id: Some(&delta.model_step_id),
            stream_phase: Some(delta.phase),
            delta_bytes: Some(i64::try_from(delta.delta.len()).map_err(|_| {
                Error::Checkpoint("stream delta length exceeds SQLite INTEGER".into())
            })?),
        },
        EventMsg::ModelStepStarted(model_step) => {
            step("model_step_started", &model_step.model_step_id)
        }
        EventMsg::ModelStepCompleted(model_step) => {
            step("model_step_completed", &model_step.model_step_id)
        }
        EventMsg::SessionHistory(_) => plain("session_history"),
        EventMsg::ModelChanged(_) => plain("model_changed"),
        EventMsg::SessionResumeRequested(_) => plain("session_resume_requested"),
        EventMsg::ToolCallBegin(_) => plain("tool_call_begin"),
        EventMsg::ToolCallEnd(_) => plain("tool_call_end"),
        EventMsg::ToolLoad(_) => plain("tool_load"),
        EventMsg::ExecApprovalRequest(_) => plain("exec_approval_request"),
        EventMsg::TokenCount(_) => plain("token_count"),
        EventMsg::ContextCompacted => plain("context_compacted"),
        EventMsg::WebSearchBegin(search) => step("web_search_begin", &search.model_step_id),
        EventMsg::WebSearchEnd(search) => step("web_search_end", &search.model_step_id),
        EventMsg::Frontend(_) => plain("frontend"),
    })
}

fn is_transient_event(event: &EventMsg) -> bool {
    matches!(
        event,
        EventMsg::SessionHistory(_)
            | EventMsg::SessionResumeRequested(_)
            | EventMsg::Frontend(
                FrontendEvent::Picker { .. }
                    | FrontendEvent::Preview { .. }
                    | FrontendEvent::Widget { .. }
                    | FrontendEvent::RemoveWidget { .. }
            )
    )
}

fn stream_phase_name(phase: ModelStepContentPhase) -> &'static str {
    match phase {
        ModelStepContentPhase::Reasoning => "reasoning",
        ModelStepContentPhase::Commentary => "commentary",
        ModelStepContentPhase::FinalAnswer => "final_answer",
    }
}

#[derive(Default)]
pub(super) struct StreamMetricAccumulator {
    first_delta_at_ms: Option<i64>,
    last_delta_at_ms: Option<i64>,
    chunk_count: u64,
    utf8_bytes: u64,
    longest_gap_ms: u64,
}

impl StreamMetricAccumulator {
    pub(super) fn observe(&mut self, recorded_at_ms: i64, bytes: i64) -> Result<()> {
        let bytes = u64::try_from(bytes)
            .map_err(|_| Error::Checkpoint("stream delta has a negative length".into()))?;
        if let Some(previous) = self.last_delta_at_ms {
            let recorded_at_ms = recorded_at_ms.max(previous);
            let gap = recorded_at_ms - previous;
            self.longest_gap_ms =
                self.longest_gap_ms
                    .max(u64::try_from(gap).map_err(|_| {
                        Error::Checkpoint("stream delta gap is unsupported".into())
                    })?);
            self.last_delta_at_ms = Some(recorded_at_ms);
        } else {
            self.first_delta_at_ms = Some(recorded_at_ms);
            self.last_delta_at_ms = Some(recorded_at_ms);
        }
        self.chunk_count = self
            .chunk_count
            .checked_add(1)
            .ok_or_else(|| Error::Checkpoint("stream chunk count overflow".into()))?;
        self.utf8_bytes = self
            .utf8_bytes
            .checked_add(bytes)
            .ok_or_else(|| Error::Checkpoint("stream byte count overflow".into()))?;
        Ok(())
    }

    pub(super) fn finish(self, phase: ModelStepContentPhase) -> Option<StreamMetrics> {
        Some(StreamMetrics {
            phase,
            first_delta_at_ms: self.first_delta_at_ms?,
            last_delta_at_ms: self.last_delta_at_ms?,
            chunk_count: self.chunk_count,
            utf8_bytes: self.utf8_bytes,
            longest_gap_ms: self.longest_gap_ms,
        })
    }
}

fn load_stream_metrics(
    transaction: &Transaction<'_>,
    session_id: &str,
    model_step_id: &str,
) -> Result<Vec<StreamMetrics>> {
    let mut statement = transaction.prepare(
        "SELECT stream_phase, recorded_at_ms, delta_bytes
         FROM event_journal
         WHERE session_id = ?1 AND model_step_id = ?2 AND stream_phase IS NOT NULL
         ORDER BY sequence",
    )?;
    let rows = statement
        .query_map(params![session_id, model_step_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut reasoning = StreamMetricAccumulator::default();
    let mut commentary = StreamMetricAccumulator::default();
    let mut final_answer = StreamMetricAccumulator::default();
    for (phase, recorded_at_ms, bytes) in rows {
        match phase.as_str() {
            "reasoning" => reasoning.observe(recorded_at_ms, bytes)?,
            "commentary" => commentary.observe(recorded_at_ms, bytes)?,
            "final_answer" => final_answer.observe(recorded_at_ms, bytes)?,
            _ => {
                return Err(Error::Checkpoint(
                    "event journal contains an unknown stream phase".into(),
                ));
            }
        }
    }
    Ok([
        reasoning.finish(ModelStepContentPhase::Reasoning),
        commentary.finish(ModelStepContentPhase::Commentary),
        final_answer.finish(ModelStepContentPhase::FinalAnswer),
    ]
    .into_iter()
    .flatten()
    .collect())
}
