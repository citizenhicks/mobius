//! Voice handoffs use the ordinary message queue and committed conversation events.

use std::collections::{BTreeMap, BTreeSet};

use crate::backend::model::{RealtimeVoiceCommand, ToolDefinition};
use crate::protocol::{
    Event, EventMsg, MessageAuthor, MessageDelivery, MessageSubmission, ModelStepContentPhase, Op,
    Submission,
};
use crate::{Error, Result};

/// Voice renders the selected agent's work; it never acquires its own execution tools.
pub const INSTRUCTIONS: &str = "You are the voice interface for the user's möbius Bot. \
For every substantive user request, hand it to the agent using ask_agent (or your native handoff). \
Do not answer it yourself or claim to have performed work. You may briefly acknowledge requests. \
The agent owns the conversation, tools, and approval decisions. Wait for its result, then deliver \
that result aloud in the user's language without adding facts or instructions. The complete \
answer remains visible in the chat. Never approve an action on the user's behalf. Do not initiate \
speech before the user speaks. If interrupted, stop speaking and listen; running work continues.";

/// The voice provider delegates the current audio turn without rewriting its transcript.
#[must_use]
pub fn handoff_tool() -> ToolDefinition {
    ToolDefinition {
        name: "ask_agent".into(),
        description: "Delegate the current spoken request to the user's Bot. The final audio transcript is submitted automatically. Wait for its result before answering.".into(),
        parameters: serde_json::json!({
            "type": "object", "properties": {}, "required": [], "additionalProperties": false,
        }),
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
}

impl VoiceConversation {
    /// Attaches to the currently committed turn when voice starts during existing work.
    #[must_use]
    pub fn new(active_turn_id: Option<String>) -> Self {
        Self {
            active_turn_id,
            ..Self::default()
        }
    }

    /// Submits each finalized utterance once using the configured message delivery policy.
    pub fn handoff(&mut self, id: String, text: String) -> Result<Option<Submission>> {
        if self.seen.contains(&id) {
            return Ok(None);
        }
        if id.is_empty() || id.len() > 4_096 {
            return Err(Error::Provider("invalid voice handoff identity".into()));
        }
        if self.pending.len() >= MAX_PENDING || self.seen.len() >= MAX_HANDOFFS {
            return Err(Error::Stopped(
                "voice conversation reached its message limit".into(),
            ));
        }
        let submission = Submission {
            id: uuid::Uuid::new_v4().to_string(),
            op: Op::Message {
                message: MessageSubmission {
                    author: MessageAuthor::User,
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
            submission.id.clone(),
            PendingHandoff {
                handoff_id: id,
                ..PendingHandoff::default()
            },
        );
        Ok(Some(submission))
    }

    /// Handles an ingress rejection that did not reach the agent's event journal.
    pub fn reject(&mut self, submission_id: &str, message: &str) -> Option<RealtimeVoiceCommand> {
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
                    .and_then(|id| self.finish(id, Some(rejection.message.clone())))
                    .into_iter()
                    .collect();
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
            .filter_map(|id| self.finish(&id, error.clone()))
            .collect()
    }

    fn finish(
        &mut self,
        submission_id: &str,
        error: Option<String>,
    ) -> Option<RealtimeVoiceCommand> {
        let pending = self.pending.remove(submission_id)?;
        let text = match error.or(pending.error) {
            Some(message) => format!("The request did not complete: {message}"),
            None => pending
                .answer
                .unwrap_or_else(|| "The request completed without a text reply.".into()),
        };
        Some(RealtimeVoiceCommand::Reply {
            handoff_id: pending.handoff_id,
            text,
        })
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
        }
    }

    #[test]
    fn voice_submits_once_and_only_speaks_its_committed_complete_answer() {
        let mut voice = VoiceConversation::default();
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
        let mut voice = VoiceConversation::new(Some("existing-turn".into()));
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
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), MAX_PENDING);
        assert!(voice.observe(&complete).is_empty());
        assert!(voice.pending.is_empty());
    }

    #[test]
    fn rejected_and_aborted_voice_work_settles_without_claiming_success() {
        for aborted in [false, true] {
            let mut voice = VoiceConversation::default();
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
}
