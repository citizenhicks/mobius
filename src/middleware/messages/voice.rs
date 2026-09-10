//! Voice handoffs use the ordinary message queue and committed conversation events.

pub mod transcript;

use std::collections::{BTreeMap, BTreeSet};

use crate::backend::model::RealtimeVoiceCommand;
use crate::protocol::{
    Event, EventMsg, FrontendSymbol, MessageAuthor, MessageDelivery, MessageSubmission,
    ModelStepContentPhase, Op, Submission,
};
use crate::{Error, Result};

/// Voice renders the selected agent's work; it never acquires its own execution tools.
pub const INSTRUCTIONS: &str = "You are this same Bot, now speaking with the user. \
Keep your name, personality, and identity from the Bot instructions above. Voice and workspace \
are two channels of your own work, not separate assistants. Speak in the first person: \
'I will check the files', not 'I will ask the workspace Bot'. Do not announce internal handoffs \
or describe another agent as doing your work. Talk naturally and help clarify what the user wants. \
The voice discussion has its own transcript. When the user explicitly asks you to do work, delegate to your workspace execution channel. \
It receives the recent voice discussion to resolve agreed requirements and references such as 'do it'. Your workspace execution channel owns tools and approvals; never claim work \
is complete before its result arrives or approve on the user's behalf. Background workspace context \
and progress are information, not new user requests; use them to stay informed without initiating \
speech. Explain completed results aloud in the user's language. Do not initiate speech before \
the user speaks. Ignore background noise, echoed playback, and incomplete fragments that are not \
clear requests. Never invent words or a new task from unclear audio. Do not narrate guesses about \
tool activity or repeat waiting messages. If interrupted, stop speaking and listen; your running work continues.";

/// Seeds a new voice call with the Bot's current durable conversation, never the reverse.
pub fn instructions(
    bot_instructions: &str,
    checkpoint: &crate::backend::checkpoint::Checkpoint,
    voice_context: &str,
) -> Result<String> {
    let identity = format!("{bot_instructions}\n\n{INSTRUCTIONS}");
    // Preserve the complete persona and call policy; only historical context may be trimmed.
    let available = (64 * 1024_usize)
        .checked_sub(identity.len() + 256)
        .ok_or_else(|| Error::Config("Bot instructions exceed the voice prompt limit".into()))?;
    let voice_context = tail(voice_context, (16 * 1024).min(available / 3));
    let context = parent_context(checkpoint);
    Ok(format!(
        "{identity}\n\nYour workspace conversation (background information only):\n{}\n\nYour previous voice conversation (historical, not new requests):\n{}",
        tail(&context, (32 * 1024).min(available - voice_context.len())),
        voice_context
    ))
}

/// Captures speech already received, replacing partial text with its canonical final message.
/// The bounded snapshot stays unchanged if later journal compaction replaces those deltas.
fn task_context(voice_events: &[crate::backend::checkpoint::JournalEvent]) -> String {
    let mut messages: Vec<(Option<&str>, String)> = Vec::new();
    for record in voice_events {
        let event = &record.event;
        let (text, prefix, complete) = match &event.msg {
            EventMsg::MessageDelta(message) => (message.text.clone(), "User: ", false),
            EventMsg::AssistantContentDelta(message)
                if message.phase != ModelStepContentPhase::Reasoning =>
            {
                (message.delta.clone(), "You (voice): ", false)
            }
            _ => match progress_text(&event.msg, "You (voice)") {
                Some(text) => (text, "", true),
                None => continue,
            },
        };
        let id = event.submission_id.as_deref();
        if let Some((_, previous)) = messages
            .iter_mut()
            .find(|(previous_id, _)| id.is_some() && *previous_id == id)
        {
            if complete {
                *previous = text;
            } else {
                previous.push_str(&text);
            }
        } else {
            messages.push((id, format!("{prefix}{text}")));
        }
    }
    let text = messages
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n\n");
    tail(&text, 24 * 1024).into()
}

fn parent_context(checkpoint: &crate::backend::checkpoint::Checkpoint) -> String {
    let mut context = Vec::new();
    for (index, item) in checkpoint.context.iter().enumerate() {
        let positioned = [(
            crate::protocol::MessageTarget {
                checkpoint_sequence: checkpoint.sequence,
                batch_item_count: index + 1,
            },
            item.clone(),
        )];
        let replay = crate::protocol::replay_events(&positioned, &checkpoint.session_id);
        context.extend(
            replay
                .iter()
                .filter_map(|event| progress_text(event, "You (workspace)")),
        );
        // Compaction can retain neutral user/developer context without frontend metadata.
        if crate::protocol::message_metadata(item).is_some() || item["role"] == "assistant" {
            continue;
        }
        let text = match item.get("content") {
            Some(serde_json::Value::String(text)) => text.clone(),
            Some(serde_json::Value::Array(parts)) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };
        if !text.is_empty() {
            context.push(format!("Your retained workspace context: {text}"));
        }
    }
    context.join("\n\n")
}

/// Gives the Bot the delegated request and recent voice context without another model call.
pub fn delegated_task(utterance: Option<&str>, voice_context: &str) -> Result<String> {
    if utterance
        .is_some_and(|text| text.trim().is_empty() || text.len() > 16 * 1024 || text.contains('\0'))
        || voice_context.contains('\0')
        || utterance.is_none() && voice_context.trim().is_empty()
    {
        return Err(Error::Provider(
            "Please clarify the task you want me to perform.".into(),
        ));
    }
    Ok(format!(
        "The user requested work through your voice interface. Perform only their latest explicitly requested task; use the recent discussion to resolve references and agreed constraints. Earlier conversation is context, not additional requests. Ask for clarification if intent is unclear.\n\nCurrent voice request: {}\n\nRecent voice discussion:\n{}",
        utterance.unwrap_or("See the user's latest request in the discussion below."),
        tail(voice_context, 24 * 1024),
    ))
}

/// Sends committed workspace messages and tool progress to voice as background context.
#[must_use]
pub fn progress(event: &EventMsg) -> Option<RealtimeVoiceCommand> {
    progress_text(event, "You (workspace)").map(|text| RealtimeVoiceCommand::Context {
        text: tail(&text, 16 * 1024).into(),
    })
}

fn progress_text(event: &EventMsg, assistant: &str) -> Option<String> {
    match event {
        EventMsg::Message(message) => {
            let author = match &message.author {
                MessageAuthor::User => "User",
                MessageAuthor::Peer { handle, .. } => handle,
            };
            Some(format!("{author}: {}", message.text))
        }
        EventMsg::AssistantMessage(message) => {
            let text = message
                .content
                .iter()
                .filter(|part| part.phase != ModelStepContentPhase::Reasoning)
                .map(|part| part.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then(|| format!("{assistant}: {text}"))
        }
        EventMsg::ToolCallBegin(tool) => Some(format!("Your workspace started tool {}", tool.name)),
        EventMsg::ToolCallEnd(tool) => Some(format!(
            "Your workspace tool {} {}",
            tool.name,
            if tool.is_error { "failed" } else { "finished" },
        )),
        EventMsg::TurnAborted(turn) => {
            Some(format!("Your workspace work stopped: {}", turn.reason))
        }
        _ => None,
    }
}

fn tail(text: &str, max_bytes: usize) -> &str {
    let start = text.ceil_char_boundary(text.len().saturating_sub(max_bytes));
    &text[start..]
}

/// Reports an unresolved handoff without submitting an ambiguous request to the Bot.
#[must_use]
pub fn reject_handoff(id: String, message: &str) -> RealtimeVoiceCommand {
    RealtimeVoiceCommand::Reply {
        handoff_id: id,
        text: format!("The request did not complete: {message}"),
    }
}

const MAX_PENDING: usize = 32;
const MAX_HANDOFFS: usize = 4_096;

#[derive(Default)]
struct PendingHandoff {
    handoff_id: String,
    turn_id: Option<String>,
    answer: Option<String>,
    error: Option<String>,
}

/// Correlates one live voice call with the agent's existing durable message lifecycle.
#[derive(Default)]
pub struct VoiceConversation {
    pending: BTreeMap<String, PendingHandoff>,
    seen: BTreeSet<String>,
    active_turn_id: Option<String>,
    session_id: String,
    bot_name: String,
}

impl VoiceConversation {
    /// Attaches a linked voice session to the currently committed Bot turn.
    #[must_use]
    pub fn new(session_id: String, active_turn_id: Option<String>, bot_name: String) -> Self {
        Self {
            session_id,
            active_turn_id,
            bot_name,
            ..Self::default()
        }
    }

    /// Sends only an explicit voice-agent task through normal peer-message delivery.
    pub fn handoff(&mut self, id: String, text: String) -> Result<Option<Submission>> {
        if self.seen.contains(&id) {
            return Ok(None);
        }
        if id.is_empty() || id.len() > 4096 {
            return Err(Error::Provider("invalid voice handoff identity".into()));
        }
        if self.pending.len() >= MAX_PENDING || self.seen.len() >= MAX_HANDOFFS {
            return Err(Error::Stopped(
                "voice conversation reached its message limit".into(),
            ));
        }
        let submission_id = uuid::Uuid::new_v4().to_string();
        let submission = Submission {
            id: submission_id.clone(),
            op: Op::Message {
                message: MessageSubmission {
                    author: MessageAuthor::Peer {
                        message_id: submission_id.clone(),
                        session_id: self.session_id.clone(),
                        handle: format!("{} (voice)", self.bot_name),
                        symbol: Some(FrontendSymbol::Custom("voice".into())),
                    },
                    text,
                    attachments: Vec::new(),
                    reply: None,
                    requested_delivery: None,
                    target_turn_id: None,
                },
            },
        };
        crate::agent::validate_submission(&submission)?;
        self.seen.insert(id.clone());
        self.pending.insert(
            submission_id,
            PendingHandoff {
                handoff_id: id,
                ..PendingHandoff::default()
            },
        );
        Ok(Some(submission))
    }

    /// Handles an ingress rejection that did not reach the agent's event journal.
    pub fn reject(&mut self, submission_id: &str, message: &str) -> Vec<RealtimeVoiceCommand> {
        self.finish(submission_id, Some(message.to_owned()))
    }

    /// Returns speech only after a matching committed terminal event.
    pub fn observe(&mut self, event: &Event) -> Vec<RealtimeVoiceCommand> {
        match &event.msg {
            EventMsg::TurnStarted(turn) => {
                self.active_turn_id = Some(turn.turn_id.clone());
                if let Some(pending) = event
                    .submission_id
                    .as_ref()
                    .and_then(|id| self.pending.get_mut(id))
                {
                    pending.turn_id = Some(turn.turn_id.clone());
                }
            }
            EventMsg::Message(message) if message.delivery == MessageDelivery::Steer => {
                if let Some(pending) = event
                    .submission_id
                    .as_ref()
                    .and_then(|id| self.pending.get_mut(id))
                {
                    pending.turn_id.clone_from(&self.active_turn_id);
                }
            }
            EventMsg::AssistantMessage(message) => {
                let text = message
                    .content
                    .iter()
                    .filter(|part| part.phase == ModelStepContentPhase::FinalAnswer)
                    .map(|part| part.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    for pending in self
                        .pending
                        .values_mut()
                        .filter(|pending| pending.turn_id.as_deref() == Some(&message.turn_id))
                    {
                        pending.answer = Some(text.clone());
                    }
                }
            }
            EventMsg::Error(error) => {
                for (id, pending) in &mut self.pending {
                    if event.submission_id.as_ref() == Some(id)
                        || pending.turn_id.is_some() && pending.turn_id == self.active_turn_id
                    {
                        pending.error = Some(error.message.clone());
                    }
                }
            }
            EventMsg::SubmissionRejected(rejection) => {
                return event
                    .submission_id
                    .as_deref()
                    .map(|id| self.finish(id, Some(rejection.message.clone())))
                    .unwrap_or_default();
            }
            EventMsg::TurnAborted(turn) => {
                return self.finish_turn(&turn.turn_id, Some(turn.reason.clone()));
            }
            EventMsg::TurnComplete(turn) => {
                return self.finish_turn(&turn.turn_id, None);
            }
            _ => {}
        }
        Vec::new()
    }

    fn finish_turn(&mut self, turn_id: &str, error: Option<String>) -> Vec<RealtimeVoiceCommand> {
        if self.active_turn_id.as_deref() == Some(turn_id) {
            self.active_turn_id = None;
        }
        let ids = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.turn_id.as_deref() == Some(turn_id))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        ids.into_iter()
            .flat_map(|id| self.finish(&id, error.clone()))
            .collect()
    }

    fn finish(&mut self, submission_id: &str, error: Option<String>) -> Vec<RealtimeVoiceCommand> {
        let Some(pending) = self.pending.remove(submission_id) else {
            return Vec::new();
        };
        let text = match error.or(pending.error) {
            Some(message) => return vec![reject_handoff(pending.handoff_id, &message)],
            None => pending
                .answer
                .unwrap_or_else(|| "The request completed without a text reply.".into()),
        };
        vec![RealtimeVoiceCommand::Reply {
            handoff_id: pending.handoff_id,
            text,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        AssistantMessageEvent, ModelStepContent, SubmissionRejectedEvent, TurnAbortedEvent,
        TurnCompleteEvent, TurnStartedEvent,
    };

    fn event(id: &str, msg: EventMsg) -> Event {
        Event {
            submission_id: Some(id.into()),
            msg,
        }
    }

    fn reply(mut commands: Vec<RealtimeVoiceCommand>) -> (String, String) {
        assert_eq!(commands.len(), 1);
        match commands.pop().expect("voice result") {
            RealtimeVoiceCommand::Reply { handoff_id, text } => (handoff_id, text),
            RealtimeVoiceCommand::Context { .. } | RealtimeVoiceCommand::Close => {
                panic!("expected handoff reply")
            }
        }
    }

    #[test]
    fn voice_submits_once_and_only_speaks_its_committed_complete_answer() {
        let mut voice = VoiceConversation::new("voice-session".into(), None, "Builder".into());
        let submission = voice
            .handoff("audio-1".into(), "Help me".into())
            .unwrap()
            .unwrap();
        assert!(
            voice
                .handoff("audio-1".into(), "duplicate".into())
                .unwrap()
                .is_none()
        );
        let Op::Message { message } = &submission.op else {
            panic!("normal message")
        };
        assert_eq!(message.requested_delivery, None);
        assert!(
            matches!(&message.author, MessageAuthor::Peer { session_id, handle, symbol: Some(FrontendSymbol::Custom(symbol)), .. }
            if session_id == "voice-session" && handle == "Builder (voice)" && symbol == "voice")
        );
        let started = EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn".into(),
            model_context_window: None,
        });
        assert!(voice.observe(&event("another", started.clone())).is_empty());
        assert!(voice.observe(&event(&submission.id, started)).is_empty());
        let answer = EventMsg::AssistantMessage(AssistantMessageEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "step".into(),
            message_target: None,
            content: ["First part", "Second part"]
                .into_iter()
                .enumerate()
                .map(|(index, text)| ModelStepContent {
                    output_index: index,
                    part_index: 0,
                    phase: ModelStepContentPhase::FinalAnswer,
                    text: text.into(),
                    annotations: Vec::new(),
                })
                .collect(),
        });
        assert!(voice.observe(&event(&submission.id, answer)).is_empty());
        let complete = event(
            &submission.id,
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn".into(),
            }),
        );
        assert_eq!(
            reply(voice.observe(&complete)),
            ("audio-1".into(), "First part\nSecond part".into())
        );
        assert!(voice.observe(&complete).is_empty());
        assert!(
            voice
                .handoff("audio-1".into(), "late duplicate".into())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn steered_voice_handoffs_finish_with_the_existing_parent_turn() {
        let mut voice = VoiceConversation::new(
            "voice-session".into(),
            Some("existing-turn".into()),
            "Builder".into(),
        );
        for index in 0..MAX_PENDING {
            let submission = voice
                .handoff(format!("audio-{index}"), "One more thing".into())
                .unwrap()
                .unwrap();
            assert!(
                voice
                    .observe(&event(
                        &submission.id,
                        EventMsg::Message(crate::protocol::MessageEvent {
                            author: MessageAuthor::User,
                            delivery: MessageDelivery::Steer,
                            text: "One more thing".into(),
                            attachments: Vec::new(),
                            reply: None,
                            message_target: None,
                        })
                    ))
                    .is_empty()
            );
        }
        let complete = event(
            "original-text-submission",
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "existing-turn".into(),
            }),
        );
        let replies = voice.observe(&complete);
        assert_eq!(replies.len(), MAX_PENDING);
        let ids = replies
            .into_iter()
            .map(|reply| match reply {
                RealtimeVoiceCommand::Reply { handoff_id, .. } => handoff_id,
                RealtimeVoiceCommand::Context { .. } | RealtimeVoiceCommand::Close => {
                    panic!("expected reply")
                }
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), MAX_PENDING);
        assert!(voice.observe(&complete).is_empty());
        assert!(voice.pending.is_empty());
    }

    #[test]
    fn rejected_and_aborted_voice_work_settles_without_claiming_success() {
        for aborted in [false, true] {
            let mut voice = VoiceConversation::new("voice-session".into(), None, "Builder".into());
            let submission = voice
                .handoff("audio".into(), "Do something".into())
                .unwrap()
                .unwrap();
            let msg = if aborted {
                voice.observe(&event(
                    &submission.id,
                    EventMsg::TurnStarted(TurnStartedEvent {
                        turn_id: "turn".into(),
                        model_context_window: None,
                    }),
                ));
                EventMsg::TurnAborted(TurnAbortedEvent {
                    turn_id: "turn".into(),
                    reason: "interrupted".into(),
                })
            } else {
                EventMsg::SubmissionRejected(SubmissionRejectedEvent {
                    message: "queue full".into(),
                })
            };
            let (_, text) = reply(voice.observe(&event(&submission.id, msg)));
            assert!(text.starts_with("The request did not complete:"));
            assert!(voice.pending.is_empty());
        }
    }

    #[test]
    fn task_context_replaces_drafts_and_bounds_unicode_text() {
        use crate::backend::checkpoint::JournalEvent;
        use crate::protocol::MessageDeltaEvent;
        let mut history = Vec::new();
        for (sequence, msg) in [
            EventMsg::MessageDelta(MessageDeltaEvent {
                text: "Use blue".into(),
            }),
            EventMsg::MessageDelta(MessageDeltaEvent {
                text: " accent".into(),
            }),
            EventMsg::Message(crate::protocol::MessageEvent {
                author: MessageAuthor::User,
                delivery: MessageDelivery::Turn,
                text: "Use a blue accent.".into(),
                attachments: Vec::new(),
                reply: None,
                message_target: None,
            }),
        ]
        .into_iter()
        .enumerate()
        {
            history.push(JournalEvent {
                sequence: sequence as u64 + 1,
                recorded_at_ms: 1,
                stream_metrics: Vec::new(),
                event: event("spoken", msg),
            });
        }
        assert_eq!(task_context(&history[..2]), "User: Use blue accent");
        assert_eq!(task_context(&history), "User: Use a blue accent.");
        history.truncate(1);
        history[0].event.msg = EventMsg::MessageDelta(MessageDeltaEvent {
            text: "🗣".repeat(20_000),
        });
        let snapshot = task_context(&history);
        assert!(snapshot.len() <= 24 * 1024);
        assert!(snapshot.ends_with('🗣'));
    }

    #[test]
    fn tail_keeps_complete_characters_within_the_byte_budget() {
        assert_eq!(tail("a🗣é", 0), "");
        assert_eq!(tail("a🗣é", 1), "");
        assert_eq!(tail("a🗣é", 3), "é");
        assert_eq!(tail("a🗣é", 6), "🗣é");
        assert_eq!(tail("a🗣é", usize::MAX), "a🗣é");
    }

    #[test]
    fn startup_context_preserves_sources_order_and_voice_history_without_mutating_parent() {
        let mut parent = crate::backend::checkpoint::Checkpoint::empty("parent");
        parent.context = vec![
            serde_json::json!({"role":"user","content":"Earlier agreed requirements"}),
            serde_json::json!({"role":"user","content":"Later updated requirements"}),
            serde_json::json!({"role":"assistant","content":[{"type":"output_text","text":"The Bot result"}]}),
        ];
        let before = parent.context.clone();
        let history = vec![crate::backend::checkpoint::JournalEvent {
            sequence: 1,
            recorded_at_ms: 1,
            stream_metrics: Vec::new(),
            event: event(
                "spoken",
                EventMsg::AssistantMessage(AssistantMessageEvent {
                    session_id: "voice-session".into(),
                    turn_id: "spoken".into(),
                    model_step_id: "spoken".into(),
                    message_target: None,
                    content: vec![ModelStepContent {
                        output_index: 0,
                        part_index: 0,
                        phase: ModelStepContentPhase::FinalAnswer,
                        text: "Our private voice decision".into(),
                        annotations: Vec::new(),
                    }],
                }),
            ),
        }];
        let voice_context = task_context(&history);
        let identity =
            "Your name is Builder (@builder).\nUse concise French. Preserve unrelated work.";
        let prompt = instructions(identity, &parent, &voice_context).unwrap();
        assert!(prompt.starts_with(identity));
        assert!(prompt.contains("You are this same Bot"));
        assert!(prompt.contains("never claim work is complete before its result arrives"));
        assert!(prompt.contains("You (workspace): The Bot result"));
        assert!(prompt.contains("You (voice): Our private voice decision"));
        assert!(!prompt.contains("You are the voice agent"));
        assert!(prompt.find("Earlier agreed").unwrap() < prompt.find("Later updated").unwrap());
        assert_eq!(parent.context, before);
        let retained = "🗣".repeat(20_000);
        parent
            .context
            .push(serde_json::json!({"role":"user","content":retained}));
        assert!(
            instructions(identity, &parent, &voice_context)
                .unwrap()
                .len()
                < 64 * 1024
        );
        let long_identity = "🗣".repeat(15_000);
        let prompt = instructions(&long_identity, &parent, &voice_context).unwrap();
        assert!(prompt.starts_with(&long_identity));
        assert!(prompt.len() <= 64 * 1024);
        assert!(instructions(&"x".repeat(64 * 1024), &parent, &voice_context).is_err());
    }

    #[test]
    fn delegation_preserves_context_without_rewriting_the_request() {
        let context =
            "You (voice): Use a blue accent; preserve toolbar actions.\n\nUser: Do that now.";
        for utterance in [Some("Do that now."), None] {
            let task = delegated_task(utterance, context).unwrap();
            assert!(task.contains(context));
            assert!(task.contains("Perform only their latest explicitly requested task"));
        }
        assert!(delegated_task(None, "").is_err());
        assert!(delegated_task(Some(""), context).is_err());
        assert!(delegated_task(Some("bad\0request"), context).is_err());
        assert!(delegated_task(Some(&"x".repeat(16 * 1024 + 1)), context).is_err());
        assert!(delegated_task(None, &"🗣".repeat(20_000)).unwrap().len() < 25 * 1024);
    }
}
