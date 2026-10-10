//! Live voice calls: a read-only transcript beside the chat, with handoffs that use the
//! ordinary message queue and committed conversation events.

mod call;
pub mod transcript;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use super::{
    ActiveCommandContext, Middleware, MiddlewareCommandContext, MiddlewareCommandOutput,
    SessionStartContext, SessionStartSource, SubmissionResult,
};
use crate::agent::ValidatedSubmission;
use crate::backend::model::RealtimeVoiceCommand;
use crate::protocol::{
    Event, EventMsg, FrontendCommand, FrontendContribution, FrontendSymbol, MessageAuthor,
    MessageDelivery, MessageSubmission, ModelStepContentPhase, Op, Submission,
};
use crate::{BoxFuture, Error, Result};

pub use call::{VoiceCall, VoiceCallContext, VoiceWake};
use transcript::{COMMAND, read_preview};

const SYMBOL: &str = "voice";

mod text {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Definition {
        pub(super) default_enabled: bool,
        pub(super) manifest_description: String,
        pub(super) manifest_label: String,
        pub(super) voice_user_label: String,
        pub(super) voice_speaker_label: String,
        pub(super) voice_workspace_label: String,
        pub(super) voice_retained_context: String,
        pub(super) voice_clarify_request: String,
        pub(super) voice_delegation_policy: String,
        pub(super) voice_current_request_heading: String,
        pub(super) voice_recent_discussion_heading: String,
        pub(super) voice_latest_request: String,
        pub(super) voice_tool_started: String,
        pub(super) voice_tool_result: String,
        pub(super) voice_tool_failed: String,
        pub(super) voice_tool_finished: String,
        pub(super) voice_work_stopped: String,
        pub(super) voice_request_failed: String,
        pub(super) voice_request_empty: String,
        pub(super) identity: String,
        pub(super) voice_instructions: String,
        pub(super) voice_workspace_heading: String,
        pub(super) voice_previous_heading: String,
        pub(super) command_description: String,
        pub(super) widget_text: String,
        pub(super) transcript_title: String,
        pub(super) handoff_handle: String,
        pub(super) handoff_continued: String,
        pub(super) handoff_correction: String,
        pub(super) handoff_discarded: String,
        #[serde(deserialize_with = "crate::middleware::manifest::deserialize_settings")]
        pub(super) settings: Vec<crate::middleware::manifest::MiddlewareSettingManifest>,
    }
    crate::embedded_config! { pub(super) static DEFINITION: Definition = include_str!("voice.toml"); }
}

super::manifest::middleware_manifest! {
    /// Configuration and presentation metadata for live voice calls.
    "voice", text::DEFINITION, required: false, settings: &text::DEFINITION.settings
}

/// Restores the voice transcript entry point beside a chat that has one.
pub struct Voice;

impl Middleware for Voice {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn frontend(&self, _session_id: &str) -> FrontendContribution {
        FrontendContribution {
            capability: MANIFEST.id.into(),
            commands: vec![transcript_command()],
            ..FrontendContribution::default()
        }
    }

    fn command<'a>(
        &'a self,
        context: MiddlewareCommandContext<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareCommandOutput>> {
        Box::pin(async move {
            Ok(MiddlewareCommandOutput::events(vec![
                read_preview(
                    context.checkpoints.as_ref(),
                    context.session_id,
                    context.arguments,
                )
                .await?,
            ]))
        })
    }

    fn active_command<'a>(
        &'a self,
        context: &'a mut ActiveCommandContext<'_>,
    ) -> BoxFuture<'a, Result<Option<SubmissionResult>>> {
        Box::pin(async move {
            if context.command != COMMAND {
                return Ok(None);
            }
            let result =
                read_preview(context.checkpoints, context.session_id, context.arguments).await;
            Ok(Some(match result {
                Ok(event) => {
                    context.events.push(EventMsg::Frontend(event));
                    SubmissionResult::Handled
                }
                Err(error) => SubmissionResult::Rejected(error.to_string()),
            }))
        })
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if context.source() == SessionStartSource::Compact {
                return Ok(());
            }
            if let Some(widget) = transcript::restore_widget(
                context.runtime.checkpoints.as_ref(),
                &context.runtime.session_id,
            )
            .await?
            {
                (context.runtime.frontend)(widget)?;
            }
            Ok(())
        })
    }
}

fn transcript_command() -> FrontendCommand {
    FrontendCommand {
        name: COMMAND.into(),
        arguments: String::new(),
        description: text::DEFINITION.command_description.as_str().into(),
        requires_idle: false,
    }
}

/// Seeds a new voice call with the Bot's current durable conversation, never the reverse.
fn instructions(
    bot_instructions: &str,
    checkpoint: &crate::backend::checkpoint::Checkpoint,
    voice_session_id: &str,
    voice_context: &str,
) -> Result<String> {
    let identity = format!(
        "{bot_instructions}\n\n{}",
        text::DEFINITION.voice_instructions
    );
    // Preserve the complete persona and call policy; only historical context may be trimmed.
    let available = (64 * 1024_usize)
        .checked_sub(identity.len() + 256)
        .ok_or_else(|| Error::Config("Bot instructions exceed the voice prompt limit".into()))?;
    let voice_context = tail(voice_context, (16 * 1024).min(available / 3));
    let context = parent_context(checkpoint, voice_session_id);
    Ok(format!(
        "{identity}\n\n{}\n{}\n\n{}\n{}",
        text::DEFINITION.voice_workspace_heading,
        tail(&context, (32 * 1024).min(available - voice_context.len())),
        text::DEFINITION.voice_previous_heading,
        voice_context
    ))
}

/// Captures speech already received, replacing partial text with its canonical final message.
/// The bounded snapshot stays unchanged if later journal compaction replaces those deltas.
fn task_context(voice_events: &[crate::backend::checkpoint::JournalEvent]) -> String {
    let text = task_messages(voice_events)
        .into_iter()
        .filter(|message| !message.text.is_empty())
        .map(|message| format!("{}: {}", message.speaker, message.text))
        .collect::<Vec<_>>()
        .join("\n\n");
    tail(&text, 24 * 1024).into()
}

struct VoiceMessage<'a> {
    id: Option<&'a str>,
    speaker: &'static str,
    text: Cow<'a, str>,
    sequence: u64,
    complete: bool,
}

fn task_messages(
    voice_events: &[crate::backend::checkpoint::JournalEvent],
) -> Vec<VoiceMessage<'_>> {
    let mut messages: Vec<VoiceMessage<'_>> = Vec::new();
    for record in voice_events {
        let event = &record.event;
        let (text, speaker, complete) = match &event.msg {
            EventMsg::MessageDelta(message) => (
                Cow::Borrowed(message.text.as_str()),
                text::DEFINITION.voice_user_label.as_str(),
                false,
            ),
            EventMsg::AssistantContentDelta(message)
                if message.phase != ModelStepContentPhase::Reasoning =>
            {
                (
                    Cow::Borrowed(message.delta.as_str()),
                    text::DEFINITION.voice_speaker_label.as_str(),
                    false,
                )
            }
            EventMsg::Message(message) => (
                Cow::Borrowed(message.text.as_str()),
                text::DEFINITION.voice_user_label.as_str(),
                true,
            ),
            EventMsg::AssistantMessage(message) => (
                joined_text(
                    message
                        .content
                        .iter()
                        .filter(|part| part.phase != ModelStepContentPhase::Reasoning)
                        .map(|part| part.text.as_str()),
                ),
                text::DEFINITION.voice_speaker_label.as_str(),
                true,
            ),
            _ => continue,
        };
        let id = event.submission_id.as_deref();
        if let Some(previous) = messages
            .iter_mut()
            .find(|previous| id.is_some() && previous.id == id)
        {
            if complete {
                previous.text = text;
            } else {
                previous.text.to_mut().push_str(text.as_ref());
            }
            previous.sequence = record.sequence;
            previous.complete = complete;
        } else {
            messages.push(VoiceMessage {
                id,
                speaker,
                text,
                sequence: record.sequence,
                complete,
            });
        }
    }
    messages
}

fn joined_text<'a>(parts: impl Iterator<Item = &'a str>) -> Cow<'a, str> {
    Cow::Owned(parts.collect::<Vec<_>>().join("\n"))
}

fn parent_context(
    checkpoint: &crate::backend::checkpoint::Checkpoint,
    voice_session_id: &str,
) -> String {
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
                .filter_map(|event| progress_text(event, voice_session_id)),
        );
        // Compaction can retain neutral user/developer context without frontend metadata.
        if crate::protocol::message_metadata(item).is_some() || item["role"] == "assistant" {
            continue;
        }
        let text = match item.get("content") {
            Some(serde_json::Value::String(text)) => Cow::Borrowed(text.as_str()),
            Some(serde_json::Value::Array(parts)) => joined_text(
                parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str)),
            ),
            _ => continue,
        };
        if !text.is_empty() {
            context.push(format!(
                "{} {text}",
                text::DEFINITION.voice_retained_context
            ));
        }
    }
    context.join("\n\n")
}

/// Gives the Bot the delegated request and recent voice context without another model call.
fn delegated_task(utterance: Option<&str>, voice_context: &str) -> Result<String> {
    if utterance
        .is_some_and(|text| text.trim().is_empty() || text.len() > 16 * 1024 || text.contains('\0'))
        || voice_context.contains('\0')
        || utterance.is_none() && voice_context.trim().is_empty()
    {
        return Err(Error::Provider(
            text::DEFINITION.voice_clarify_request.as_str().into(),
        ));
    }
    Ok(format!(
        "{}\n\n{} {}\n\n{}\n{}",
        text::DEFINITION.voice_delegation_policy,
        text::DEFINITION.voice_current_request_heading,
        utterance.unwrap_or(text::DEFINITION.voice_latest_request.as_str()),
        text::DEFINITION.voice_recent_discussion_heading,
        tail(voice_context, 24 * 1024),
    ))
}

fn progress_text(event: &EventMsg, voice_session_id: &str) -> Option<String> {
    match event {
        EventMsg::Message(message) => {
            let author = match &message.author {
                MessageAuthor::User => text::DEFINITION.voice_user_label.as_str(),
                MessageAuthor::Source {
                    source: crate::protocol::MessageSource::Session { session_id },
                    ..
                } if session_id == voice_session_id => {
                    return None;
                }
                MessageAuthor::Source { handle, .. } => handle,
            };
            Some(format!("{author}: {}", message.text))
        }
        EventMsg::AssistantMessage(message) => {
            let text = joined_text(
                message
                    .content
                    .iter()
                    .filter(|part| part.phase != ModelStepContentPhase::Reasoning)
                    .map(|part| part.text.as_str()),
            );
            (!text.is_empty())
                .then(|| format!("{}: {text}", text::DEFINITION.voice_workspace_label))
        }
        EventMsg::ToolCallBegin(tool) => Some(format!(
            "{} {}",
            text::DEFINITION.voice_tool_started,
            tool.name
        )),
        EventMsg::ToolCallEnd(tool) => Some(format!(
            "{} {} {}",
            text::DEFINITION.voice_tool_result,
            tool.name,
            if tool.is_error {
                text::DEFINITION.voice_tool_failed.as_str()
            } else {
                text::DEFINITION.voice_tool_finished.as_str()
            },
        )),
        EventMsg::TurnAborted(turn) => Some(format!(
            "{} {}",
            text::DEFINITION.voice_work_stopped,
            turn.reason
        )),
        _ => None,
    }
}

fn tail(text: &str, max_bytes: usize) -> &str {
    let start = text.ceil_char_boundary(text.len().saturating_sub(max_bytes));
    &text[start..]
}

/// Reports an unresolved handoff without submitting an ambiguous request to the Bot.
fn reject_handoff(id: String, message: &str) -> RealtimeVoiceCommand {
    RealtimeVoiceCommand::Reply {
        handoff_id: id,
        text: format!("{} {message}", text::DEFINITION.voice_request_failed),
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
struct VoiceConversation {
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

    /// Sends workspace progress without echoing this linked voice session's messages.
    #[must_use]
    pub fn progress(&self, event: &Event) -> Option<RealtimeVoiceCommand> {
        progress_text(&event.msg, &self.session_id).map(|text| RealtimeVoiceCommand::Context {
            text: tail(&text, 16 * 1024).into(),
        })
    }

    /// Sends only an explicit voice-agent task through normal peer-message delivery.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn handoff(&mut self, id: String, text: String) -> Result<Option<ValidatedSubmission>> {
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
                    author: MessageAuthor::Source {
                        message_id: submission_id.clone(),
                        source: crate::protocol::MessageSource::Session {
                            session_id: self.session_id.clone(),
                        },
                        cause_id: None,
                        ancestry: Vec::new(),
                        handle: text::DEFINITION
                            .handoff_handle
                            .replace("{bot}", &self.bot_name),
                        symbol: Some(FrontendSymbol::Custom(SYMBOL.into())),
                    },
                    text,
                    attachments: Vec::new(),
                    reply: None,
                    requested_delivery: Some(crate::protocol::ActiveMessageDelivery::Steer),
                    target_turn_id: None,
                },
            },
        };
        let submission = ValidatedSubmission::new(submission)?;
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

    /// Reports whether this call already handled a provider handoff identity.
    #[must_use]
    pub fn has_handoff(&self, id: &str) -> bool {
        self.seen.contains(id)
    }

    /// Handles an ingress rejection that did not reach the agent's event journal.
    pub fn reject(&mut self, submission_id: &str, message: &str) -> Vec<RealtimeVoiceCommand> {
        self.finish(submission_id, Some(message))
    }

    /// Returns speech only after a matching committed terminal event.
    pub fn observe(&mut self, event: &Event) -> Vec<RealtimeVoiceCommand> {
        match &event.msg {
            EventMsg::TurnStarted(turn) => {
                self.active_turn_id = Some(turn.turn_id.clone());
                if let Some(pending) = event
                    .submission_id
                    .as_ref()
                    .and_then(|id| self.pending.get_mut(id.as_ref()))
                {
                    pending.turn_id = Some(turn.turn_id.clone());
                }
            }
            EventMsg::Message(message) if message.delivery == MessageDelivery::Steer => {
                if let Some(pending) = event
                    .submission_id
                    .as_ref()
                    .and_then(|id| self.pending.get_mut(id.as_ref()))
                {
                    // A pending handoff retains its assigned turn even when the active turn later changes.
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
                    let mut recipients = self
                        .pending
                        .values_mut()
                        .filter(|pending| pending.turn_id.as_deref() == Some(&message.turn_id));
                    if let Some(first) = recipients.next() {
                        for pending in recipients {
                            pending.answer = Some(text.clone());
                        }
                        first.answer = Some(text);
                    }
                }
            }
            EventMsg::Error(error) => {
                for (id, pending) in &mut self.pending {
                    if event.submission_id.as_deref() == Some(id.as_str())
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
                    .map(|id| self.finish(id, Some(&rejection.message)))
                    .unwrap_or_default();
            }
            EventMsg::TurnAborted(turn) => {
                return self.finish_turn(&turn.turn_id, Some(&turn.reason));
            }
            EventMsg::TurnComplete(turn) => {
                return self.finish_turn(&turn.turn_id, None);
            }
            _ => {}
        }
        Vec::new()
    }

    fn finish_turn(&mut self, turn_id: &str, error: Option<&str>) -> Vec<RealtimeVoiceCommand> {
        if self.active_turn_id.as_deref() == Some(turn_id) {
            self.active_turn_id = None;
        }
        self.pending
            .extract_if(.., |_, pending| pending.turn_id.as_deref() == Some(turn_id))
            .map(|(_, pending)| pending.finish(error))
            .collect()
    }

    fn finish(&mut self, submission_id: &str, error: Option<&str>) -> Vec<RealtimeVoiceCommand> {
        self.pending
            .remove(submission_id)
            .map(|pending| pending.finish(error))
            .into_iter()
            .collect()
    }
}

impl PendingHandoff {
    fn finish(self, error: Option<&str>) -> RealtimeVoiceCommand {
        if let Some(message) = error.or(self.error.as_deref()) {
            return reject_handoff(self.handoff_id, message);
        }
        RealtimeVoiceCommand::Reply {
            handoff_id: self.handoff_id,
            text: self
                .answer
                .unwrap_or_else(|| text::DEFINITION.voice_request_empty.clone()),
        }
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
    fn voice_is_optional_and_on_by_default() {
        assert!(!MANIFEST.required && MANIFEST.default_enabled);
    }

    #[test]
    fn progress_filters_linked_voice_messages_independently_of_pending_handoffs() {
        let mut voice = VoiceConversation::new("voice-session".into(), None, "Builder".into());
        let submission = voice
            .handoff("audio".into(), "Do this".into())
            .unwrap()
            .unwrap()
            .into_submission();
        let Op::Message { message } = submission.op else {
            panic!("normal message")
        };
        let mut message = crate::protocol::MessageEvent {
            author: message.author,
            delivery: MessageDelivery::Turn,
            text: message.text,
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        };
        let echo = event(&submission.id, EventMsg::Message(message.clone()));
        assert!(voice.progress(&echo).is_none());
        voice.reject(&submission.id, "rejected");
        assert!(voice.progress(&echo).is_none());
        let resumed = VoiceConversation::new("voice-session".into(), None, "Renamed".into());
        assert!(resumed.progress(&echo).is_none());
        let mut parent = crate::backend::checkpoint::Checkpoint::empty("parent");
        parent.context = std::sync::Arc::new(
            vec![crate::backend::model::message_input(&message).unwrap()]
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
        );
        assert!(
            !instructions("Bot", &parent, "voice-session", "")
                .unwrap()
                .contains("Do this")
        );

        if let MessageAuthor::Source {
            source: crate::protocol::MessageSource::Session { session_id },
            ..
        } = &mut message.author
        {
            *session_id = "other-session".into();
        }
        for author in [message.author.clone(), MessageAuthor::User] {
            message.author = author;
            let update = event(&submission.id, EventMsg::Message(message.clone()));
            assert!(
                matches!(voice.progress(&update), Some(RealtimeVoiceCommand::Context { text }) if text.ends_with("Do this"))
            );
            parent.context = std::sync::Arc::new(
                vec![crate::backend::model::message_input(&message).unwrap()]
                    .into_iter()
                    .map(std::sync::Arc::new)
                    .collect(),
            );
            assert!(
                instructions("Bot", &parent, "voice-session", "")
                    .unwrap()
                    .contains("Do this")
            );
        }
    }

    #[test]
    fn voice_submits_once_and_only_speaks_its_committed_complete_answer() {
        let mut voice = VoiceConversation::new("voice-session".into(), None, "Builder".into());
        let submission = voice
            .handoff("audio-1".into(), "Help me".into())
            .unwrap()
            .unwrap()
            .into_submission();
        assert!(
            voice
                .handoff("audio-1".into(), "duplicate".into())
                .unwrap()
                .is_none()
        );
        let Op::Message { message } = &submission.op else {
            panic!("normal message")
        };
        assert_eq!(
            message.requested_delivery,
            Some(crate::protocol::ActiveMessageDelivery::Steer)
        );
        assert!(
            matches!(&message.author, MessageAuthor::Source { source: crate::protocol::MessageSource::Session { session_id }, handle, symbol: Some(FrontendSymbol::Custom(symbol)), .. }
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
                .unwrap()
                .into_submission();
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
                .unwrap()
                .into_submission();
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
        parent.context = std::sync::Arc::new(vec![
            serde_json::json!({"role":"user","content":"Earlier agreed requirements"}),
            serde_json::json!({"role":"user","content":"Later updated requirements"}),
            serde_json::json!({"role":"assistant","content":[{"type":"output_text","text":"The Bot result"}]}),
        ].into_iter().map(std::sync::Arc::new).collect());
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
        let prompt = instructions(identity, &parent, "voice-session", &voice_context).unwrap();
        assert!(prompt.starts_with(identity));
        assert!(prompt.contains("You are this same Bot"));
        assert!(prompt.contains("never claim work is complete before its result arrives"));
        assert!(prompt.contains("You (workspace): The Bot result"));
        assert!(prompt.contains("You (voice): Our private voice decision"));
        assert!(!prompt.contains("You are the voice agent"));
        assert!(prompt.find("Earlier agreed").unwrap() < prompt.find("Later updated").unwrap());
        assert_eq!(parent.context, before);
        let retained = "🗣".repeat(20_000);
        std::sync::Arc::make_mut(&mut parent.context).push(std::sync::Arc::new(
            serde_json::json!({"role":"user","content":retained}),
        ));
        assert!(
            instructions(identity, &parent, "voice-session", &voice_context)
                .unwrap()
                .len()
                < 64 * 1024
        );
        let long_identity = "🗣".repeat(15_000);
        let prompt =
            instructions(&long_identity, &parent, "voice-session", &voice_context).unwrap();
        assert!(prompt.starts_with(&long_identity));
        assert!(prompt.len() <= 64 * 1024);
        assert!(
            instructions(
                &"x".repeat(64 * 1024),
                &parent,
                "voice-session",
                &voice_context
            )
            .is_err()
        );
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
