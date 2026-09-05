//! A voice conversation is a read-only child transcript, never a second tool-running Agent.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
const PAGE_SIZE: usize = 128;
const MAX_RECORDINGS: usize = 4_096;
const MAX_PREVIEW_BYTES: usize = 8 * 1024 * 1024;

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
}

impl VoiceTranscript {
    /// Opens the parent's durable voice transcript without starting an Agent or copying context.
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
        Ok(Self {
            checkpoints,
            session_id,
            frontend,
            recordings: BTreeMap::new(),
            visible,
        })
    }

    /// The linked session used for read-only previews and voice context on a later call.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Freezes the private discussion at request receipt, including unfinished speech.
    pub async fn task_context(&self) -> Result<String> {
        let history = history_page(self.checkpoints.as_ref(), &self.session_id, None)
            .await?
            .into_chronological();
        Ok(super::task_context(&history))
    }

    /// Journals normalized speech with a fresh canonical identity for this provider call.
    pub async fn record(
        &mut self,
        input_id: &str,
        role: ConversationRole,
        text: &str,
        complete: bool,
    ) -> Result<()> {
        if text.is_empty() {
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
        let recorded_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .ok_or_else(|| {
                Error::Checkpoint("voice event timestamp is outside the supported range".into())
            })?;
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
        (self.frontend)(preview(self.checkpoints.as_ref(), &self.session_id, None).await?)
    }

    /// Keeps speech already received when the call stops before its provider final event.
    pub async fn finish(&mut self) -> Result<()> {
        let pending = self
            .recordings
            .iter()
            .filter(|(_, recording)| !recording.complete)
            .map(|(input_id, recording)| (input_id.clone(), recording.role, recording.text.clone()))
            .collect::<Vec<_>>();
        for (input_id, role, text) in pending {
            self.record(&input_id, role, &text, true).await?;
        }
        Ok(())
    }
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
    Ok(FrontendEvent::Preview {
        id: session_id.into(),
        title: "Voice".into(),
        subtitle: String::new(),
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
