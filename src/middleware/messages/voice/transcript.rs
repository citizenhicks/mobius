//! A voice conversation is a read-only child transcript, never a second tool-running Agent.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::backend::checkpoint::{
    Checkpoint, CheckpointStore, EventPage, EventPageRequest, JournalEvent,
};
use crate::middleware::FrontendEventSink;
use crate::protocol::{
    AssistantContentDeltaEvent, AssistantMessageEvent, ConversationRole, Event, EventMsg,
    FrontendEvent, FrontendPreviewEvent, FrontendPreviewUpdate, FrontendSlot, FrontendSymbol,
    FrontendTone, FrontendWidget, MAX_MESSAGE_BYTES, MessageAuthor, MessageDelivery,
    MessageDeltaEvent, MessageEvent, ModelStepContent, ModelStepContentPhase, Op,
};
use crate::{Error, Result};

pub(crate) const COMMAND: &str = "voice";
const STATE_KEY: &str = "messages.voice_session";
const CALL_KEY: &str = "messages.voice_calls";
const CURSOR_KEY: &str = "messages.voice_handoff";
const PAGE_SIZE: usize = 128;
const MAX_RECORDINGS: usize = 4_096;
const MAX_PREVIEW_BYTES: usize = 8 * 1024 * 1024;
const PREVIEW_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Default, Deserialize, Serialize)]
struct CallSummary {
    voice: String,
    duration_ms: u64,
    started_at_ms: Option<i64>,
}

#[derive(Default, Deserialize, Serialize)]
struct HandoffCursor {
    sequence: u64,
    drafts: BTreeMap<String, String>,
}

/// A frozen discussion snapshot, acknowledged only after its workspace message is committed.
pub struct VoiceHandoff {
    /// New speech since the last committed handoff, within the recent-discussion budget.
    pub text: String,
    cursor: HandoffCursor,
}

struct Recording {
    id: String,
    role: ConversationRole,
    text: String,
    complete: bool,
}

/// The sole writer for one call, protected by the gateway's parent-session voice lease.
pub struct VoiceTranscript {
    checkpoints: Arc<dyn CheckpointStore>,
    session_id: String,
    frontend: FrontendEventSink,
    recordings: BTreeMap<String, Recording>,
    visible: bool,
    cursor: HandoffCursor,
    preview_deadline: Option<Instant>,
    call_started: Option<Instant>,
}

impl VoiceTranscript {
    /// Opens the parent's durable voice transcript without starting an Agent or copying context.
    /// # Errors
    ///
    /// Returns an error if the resource cannot be read, decoded, or validated.
    pub async fn open(
        checkpoints: Arc<dyn CheckpointStore>,
        parent_session_id: &str,
        frontend: FrontendEventSink,
    ) -> Result<Self> {
        let session_id =
            if let Some(session_id) = child_id(checkpoints.as_ref(), parent_session_id).await? {
                session_id
            } else {
                let parent = checkpoints
                    .load(parent_session_id)
                    .await?
                    .ok_or_else(|| Error::Checkpoint("voice parent session is missing".into()))?;
                let mut child = Checkpoint::empty(uuid::Uuid::new_v4().to_string());
                child.catalog_visible = false;
                child.session_context = parent.session_context;
                checkpoints
                    .fork(parent_session_id, parent.sequence, &child)
                    .await?;
                checkpoints
                    .save_state(
                        parent_session_id,
                        STATE_KEY,
                        &serde_json::json!(child.session_id),
                    )
                    .await?;
                child.session_id
            };
        let visible = has_events(checkpoints.as_ref(), &session_id).await?;
        let cursor = checkpoints
            .load_state(&session_id, CURSOR_KEY)
            .await?
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            checkpoints,
            session_id,
            frontend,
            recordings: BTreeMap::new(),
            visible,
            cursor,
            preview_deadline: None,
            call_started: None,
        })
    }

    /// Records the selected voice and starts timing this call.
    /// # Errors
    ///
    /// Returns an error if call metadata cannot be read or saved.
    pub async fn start_call(&mut self, voice: &str) -> Result<()> {
        let mut summary = call_summary(self.checkpoints.as_ref(), &self.session_id).await?;
        summary.voice = voice.into();
        summary.started_at_ms = Some(timestamp_ms()?);
        self.checkpoints
            .save_state(&self.session_id, CALL_KEY, &serde_json::to_value(summary)?)
            .await?;
        self.call_started = Some(Instant::now());
        self.preview_deadline = Some(Instant::now());
        self.flush_preview().await
    }

    /// The linked session used for read-only previews and voice context on a later call.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Freezes the private discussion at request receipt, including unfinished speech.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn task_context(&self) -> Result<String> {
        let history = history_page(self.checkpoints.as_ref(), &self.session_id, None)
            .await?
            .into_chronological();
        Ok(super::task_context(&history))
    }

    /// Freezes only discussion not yet committed to the workspace, retaining draft corrections.
    /// # Errors
    ///
    /// Returns an error if the transcript cannot be read.
    pub async fn handoff_context(&self) -> Result<VoiceHandoff> {
        let history = history_page(self.checkpoints.as_ref(), &self.session_id, None)
            .await?
            .into_chronological();
        let mut cursor = HandoffCursor {
            sequence: self.cursor.sequence,
            ..HandoffCursor::default()
        };
        let mut discussion = Vec::new();
        for message in super::task_messages(&history) {
            if !message.complete
                && let Some(id) = message.id
            {
                cursor.drafts.insert(id.into(), message.text.clone());
            }
            if message.sequence <= self.cursor.sequence {
                continue;
            }
            cursor.sequence = cursor.sequence.max(message.sequence);
            let previous = message.id.and_then(|id| self.cursor.drafts.get(id));
            let text = match previous {
                Some(previous) => match message.text.strip_prefix(previous) {
                    Some("") => String::new(),
                    Some(rest) => format!("{} (continued): {rest}", message.speaker),
                    None => format!(
                        "Correction to earlier voice speech:\n{}: {}",
                        message.speaker,
                        if message.text.is_empty() {
                            "[speech discarded]"
                        } else {
                            &message.text
                        }
                    ),
                },
                None if message.text.is_empty() => String::new(),
                None => format!("{}: {}", message.speaker, message.text),
            };
            if !text.is_empty() {
                discussion.push(text);
            }
        }
        Ok(VoiceHandoff {
            text: super::tail(&discussion.join("\n\n"), 24 * 1024).into(),
            cursor,
        })
    }

    /// Advances the durable cursor after the workspace commits this snapshot's message.
    /// # Errors
    ///
    /// Returns an error if the cursor cannot be saved.
    pub async fn acknowledge(&mut self, handoff: VoiceHandoff) -> Result<()> {
        if handoff.cursor.sequence <= self.cursor.sequence {
            return Ok(());
        }
        // ponytail: a disconnect between message commit and cursor save may resend context.
        // Store them atomically only if delivery must become exactly-once.
        self.checkpoints
            .save_state(
                &self.session_id,
                CURSOR_KEY,
                &serde_json::to_value(&handoff.cursor)?,
            )
            .await?;
        self.cursor = handoff.cursor;
        Ok(())
    }

    /// Waits for a coalesced live preview; idle calls do not wake periodically.
    pub async fn wait_for_preview(&self) {
        match self.preview_deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    }

    /// Publishes all pending speech in one preview without changing transcript durability.
    /// # Errors
    ///
    /// Returns an error if the preview cannot be read or delivered.
    pub async fn flush_preview(&mut self) -> Result<()> {
        if self.preview_deadline.is_some() {
            (self.frontend)(preview(self.checkpoints.as_ref(), &self.session_id, None).await?)?;
            self.preview_deadline = None;
        }
        Ok(())
    }

    /// Journals normalized speech with a fresh canonical identity for this provider call.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn record(
        &mut self,
        input_id: &str,
        role: ConversationRole,
        text: &str,
        complete: bool,
    ) -> Result<()> {
        if text.is_empty() && (!complete || !self.recordings.contains_key(input_id)) {
            return Ok(());
        }
        if input_id.is_empty()
            || input_id.len() > 4096
            || self.recordings.len() >= MAX_RECORDINGS && !self.recordings.contains_key(input_id)
        {
            return Err(Error::Provider(
                "invalid voice transcript identity or message limit".into(),
            ));
        }
        let recording = self
            .recordings
            .entry(input_id.into())
            .or_insert_with(|| Recording {
                id: uuid::Uuid::new_v4().to_string(),
                role,
                text: String::new(),
                complete: false,
            });
        if recording.role != role {
            return Err(Error::Provider(
                "voice transcript changed its author".into(),
            ));
        }
        if recording.complete {
            return Ok(());
        }
        let next_bytes = if complete {
            text.len()
        } else {
            recording.text.len() + text.len()
        };
        if next_bytes > MAX_MESSAGE_BYTES {
            return Err(Error::Provider(
                "voice transcript exceeded its size limit".into(),
            ));
        }
        let event = speech_event(&self.session_id, &recording.id, role, text, complete);
        let recorded_at_ms = timestamp_ms()?;
        self.checkpoints
            .append_event(&self.session_id, recorded_at_ms, &event)
            .await?;
        if complete {
            recording.text = String::new();
        } else {
            recording.text.push_str(text);
        }
        recording.complete = complete;
        if !self.visible {
            (self.frontend)(widget())?;
            self.visible = true;
        }
        self.preview_deadline
            .get_or_insert_with(|| Instant::now() + PREVIEW_INTERVAL);
        if complete {
            self.flush_preview().await?;
        }
        Ok(())
    }

    /// Keeps speech already received when the call stops before its provider final event.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn finish(&mut self) -> Result<()> {
        if let Some(started) = self.call_started {
            let mut summary = call_summary(self.checkpoints.as_ref(), &self.session_id).await?;
            summary.duration_ms = summary
                .duration_ms
                .saturating_add(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
            summary.started_at_ms = None;
            self.checkpoints
                .save_state(&self.session_id, CALL_KEY, &serde_json::to_value(summary)?)
                .await?;
            self.call_started = None;
            self.preview_deadline = Some(Instant::now());
        }
        let pending = self
            .recordings
            .iter()
            .filter(|(_, recording)| !recording.complete)
            .map(|(input_id, recording)| (input_id.clone(), recording.role, recording.text.clone()))
            .collect::<Vec<_>>();
        for (input_id, role, text) in pending {
            self.record(&input_id, role, &text, true).await?;
        }
        self.flush_preview().await
    }
}

fn timestamp_ms() -> Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or_else(|| Error::Checkpoint("voice timestamp is outside the supported range".into()))
}

async fn call_summary(checkpoints: &dyn CheckpointStore, session_id: &str) -> Result<CallSummary> {
    Ok(checkpoints
        .load_state(session_id, CALL_KEY)
        .await?
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default())
}

fn speech_event(
    session_id: &str,
    id: &str,
    role: ConversationRole,
    text: &str,
    complete: bool,
) -> Event {
    let msg = match (role, complete) {
        (ConversationRole::User, false) => {
            EventMsg::MessageDelta(MessageDeltaEvent { text: text.into() })
        }
        (ConversationRole::User, true) => EventMsg::Message(MessageEvent {
            author: MessageAuthor::User,
            delivery: MessageDelivery::Turn,
            text: text.into(),
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        }),
        (ConversationRole::Assistant, false) => {
            EventMsg::AssistantContentDelta(AssistantContentDeltaEvent {
                session_id: session_id.into(),
                turn_id: id.into(),
                model_step_id: id.into(),
                delta: text.into(),
                phase: ModelStepContentPhase::FinalAnswer,
            })
        }
        (ConversationRole::Assistant, true) => EventMsg::AssistantMessage(AssistantMessageEvent {
            session_id: session_id.into(),
            turn_id: id.into(),
            model_step_id: id.into(),
            message_target: None,
            content: vec![ModelStepContent {
                output_index: 0,
                part_index: 0,
                phase: ModelStepContentPhase::FinalAnswer,
                text: text.into(),
                annotations: Vec::new(),
            }],
        }),
    };
    Event {
        submission_id: Some(id.into()),
        msg,
    }
}

async fn child_id(checkpoints: &dyn CheckpointStore, parent: &str) -> Result<Option<String>> {
    checkpoints
        .load_state(parent, STATE_KEY)
        .await?
        .map(serde_json::from_value)
        .transpose()
        .map_err(Into::into)
}

async fn has_events(checkpoints: &dyn CheckpointStore, session_id: &str) -> Result<bool> {
    Ok(!checkpoints
        .event_page(
            session_id,
            EventPageRequest {
                before_sequence: None,
                limit: 1,
            },
        )
        .await?
        .events
        .is_empty())
}

pub(crate) async fn restore_widget(
    checkpoints: &dyn CheckpointStore,
    parent: &str,
) -> Result<Option<FrontendEvent>> {
    let Some(session_id) = child_id(checkpoints, parent).await? else {
        return Ok(None);
    };
    Ok(has_events(checkpoints, &session_id).await?.then(widget))
}

pub(crate) async fn read_preview(
    checkpoints: &dyn CheckpointStore,
    parent: &str,
    arguments: &str,
) -> Result<FrontendEvent> {
    let before = if arguments.is_empty() {
        None
    } else {
        Some(
            arguments
                .parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| Error::Tool("invalid voice preview cursor".into()))?,
        )
    };
    let session_id = child_id(checkpoints, parent)
        .await?
        .ok_or_else(|| Error::Unknown("voice transcript".into()))?;
    preview(checkpoints, &session_id, before).await
}

fn command(before: Option<u64>) -> Op {
    Op::CapabilityCommand {
        capability: "messages".into(),
        command: COMMAND.into(),
        arguments: before.map_or_else(String::new, |before| before.to_string()),
        input: None,
        target: None,
    }
}

fn widget() -> FrontendEvent {
    FrontendEvent::Widget {
        capability: "messages".into(),
        item: FrontendWidget {
            id: "voice".into(),
            slot: FrontendSlot::ComposerFooter,
            text: "Voice".into(),
            tone: FrontendTone::Neutral,
            symbol: Some(FrontendSymbol::Custom("voice".into())),
            icon_only: true,
            progress: None,
            content: None,
            action: Some(command(None)),
        },
    }
}

async fn history_page(
    checkpoints: &dyn CheckpointStore,
    session_id: &str,
    before: Option<u64>,
) -> Result<EventPage> {
    let mut page = checkpoints
        .event_page(
            session_id,
            EventPageRequest {
                before_sequence: before,
                limit: PAGE_SIZE,
            },
        )
        .await?;
    let mut bytes = serde_json::to_vec(&page.events)?.len();
    if bytes > MAX_PREVIEW_BYTES {
        return Err(Error::Tool("voice preview exceeds its size limit".into()));
    }
    // A page may end amid unfinished speech. Keep its whole delta prefix so replay
    // never displays a truncated live utterance; final snapshots already prune deltas.
    while page.events.last().is_some_and(is_delta) {
        let Some(cursor) = page.next_before_sequence else {
            break;
        };
        let older = checkpoints
            .event_page(
                session_id,
                EventPageRequest {
                    before_sequence: Some(cursor),
                    limit: PAGE_SIZE,
                },
            )
            .await?;
        page.next_before_sequence = older.next_before_sequence;
        for (index, event) in older.events.iter().enumerate() {
            bytes += serde_json::to_vec(event)?.len();
            if bytes > MAX_PREVIEW_BYTES {
                return Err(Error::Tool("voice preview exceeds its size limit".into()));
            }
            page.events.push(event.clone());
            if !is_delta(event) {
                if index + 1 < older.events.len() {
                    page.next_before_sequence = Some(event.sequence);
                }
                break;
            }
        }
    }
    Ok(page)
}

async fn preview(
    checkpoints: &dyn CheckpointStore,
    session_id: &str,
    before: Option<u64>,
) -> Result<FrontendEvent> {
    let page = history_page(checkpoints, session_id, before).await?;
    let next = page
        .next_before_sequence
        .map(|before| command(Some(before)));
    let events = page
        .into_chronological()
        .into_iter()
        .map(|record| FrontendPreviewEvent {
            submission_id: record.event.submission_id,
            recorded_at_ms: record.recorded_at_ms,
            event: record.event.msg,
        })
        .collect::<Vec<_>>();
    if serde_json::to_vec(&events)?.len() > MAX_PREVIEW_BYTES {
        return Err(Error::Tool("voice preview exceeds its size limit".into()));
    }
    let summary = call_summary(checkpoints, session_id).await?;
    Ok(FrontendEvent::Preview {
        symbol: Some(FrontendSymbol::Custom("voice".into())),
        duration_ms: (!summary.voice.is_empty()).then_some(summary.duration_ms),
        started_at_ms: summary.started_at_ms,
        id: session_id.into(),
        title: "Voice transcript".into(),
        subtitle: summary.voice,
        page_id: format!(
            "{session_id}:{}",
            before.map_or_else(|| "latest".into(), |before| before.to_string())
        ),
        update: if before.is_some() {
            FrontendPreviewUpdate::Prepend
        } else {
            FrontendPreviewUpdate::Replace
        },
        events,
        next,
    })
}

fn is_delta(event: &JournalEvent) -> bool {
    matches!(
        event.event.msg,
        EventMsg::MessageDelta(_) | EventMsg::AssistantContentDelta(_)
    )
}

#[cfg(test)]
mod tests;
