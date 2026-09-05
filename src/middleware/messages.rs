//! Durable conversation-message delivery.

pub mod voice;

use super::manifest::{
    MiddlewareManifest, MiddlewareSettingChoice, MiddlewareSettingChoices,
    MiddlewareSettingManifest,
};
use super::{
    ActiveCommandContext, MessageRouteContext, Middleware, MiddlewareCommandContext,
    MiddlewareCommandOutput, SessionStartContext, SessionStartSource, SubmissionResult,
};
use crate::backend::checkpoint::QueuedMessageBoundary;
use crate::protocol::{
    ActiveMessageDelivery, EventMsg, FrontendCommand, FrontendContribution, FrontendEvent,
    FrontendSlot, FrontendSymbol, FrontendTone, FrontendWidget, MAX_CAPABILITY_INPUT_BYTES,
    MessageAuthor, MessageDelivery, MessageEvent, Op,
};
use crate::{BoxFuture, Error, Result};

mod text {
    include!(concat!(env!("OUT_DIR"), "/src_middleware_messages_text.rs"));
}

const MAX_PENDING_MESSAGES: usize = 1_024;
const _: () = {
    assert!(text::DEFAULTS_MAX_PENDING >= 1);
    assert!(text::DEFAULTS_MAX_PENDING <= MAX_PENDING_MESSAGES as i64);
    assert!(text::SETTING_MAX_PENDING_STEP > 0);
};

/// Default number of pending messages retained by the delivery queue.
pub const DEFAULT_MAX_PENDING: usize = text::DEFAULTS_MAX_PENDING as usize;
/// Default delivery for user messages submitted during an active turn.
pub const DEFAULT_DELIVERY: ActiveMessageDelivery = ActiveMessageDelivery::Steer;

const DELIVERIES: &[MiddlewareSettingChoice] = &[
    MiddlewareSettingChoice {
        value: "steer",
        label: text::SETTING_DELIVERY_STEER_LABEL,
        description: text::SETTING_DELIVERY_STEER_DESCRIPTION,
        symbol: Some("steer"),
        tone: FrontendTone::Neutral,
    },
    MiddlewareSettingChoice {
        value: "queue",
        label: text::SETTING_DELIVERY_QUEUE_LABEL,
        description: text::SETTING_DELIVERY_QUEUE_DESCRIPTION,
        symbol: Some("queue"),
        tone: FrontendTone::Neutral,
    },
];

const SETTINGS: &[MiddlewareSettingManifest] = &[
    MiddlewareSettingManifest::Select {
        id: "delivery",
        label: text::SETTING_DELIVERY_LABEL,
        description: text::SETTING_DELIVERY_DESCRIPTION,
        choices: MiddlewareSettingChoices::Static(DELIVERIES),
        unset_label: None,
        default: Some("steer"),
        max_bytes: 8,
        composer: false,
    },
    MiddlewareSettingManifest::Integer {
        id: "max_pending",
        label: text::SETTING_MAX_PENDING_LABEL,
        description: text::SETTING_MAX_PENDING_DESCRIPTION,
        min: 1,
        max: Some(MAX_PENDING_MESSAGES as i64),
        step: text::SETTING_MAX_PENDING_STEP,
        default: DEFAULT_MAX_PENDING as i64,
    },
];

/// Configuration and presentation metadata for message delivery.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "messages",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: true,
    default_enabled: true,
    settings: SETTINGS,
};

const EDIT_COMMAND: &str = "edit";
const STALE_EDIT: &str = "message is no longer queued";
const INVALID_EDIT: &str = "message edit requires non-empty text";

/// Prepares every conversation message and owns its durable delivery lifecycle.
pub struct Messages {
    max_pending: usize,
    delivery: ActiveMessageDelivery,
}

impl Default for Messages {
    fn default() -> Self {
        Self {
            max_pending: DEFAULT_MAX_PENDING,
            delivery: DEFAULT_DELIVERY,
        }
    }
}

impl Messages {
    /// Creates message delivery with a bounded queue and active-turn default.
    pub fn new(max_pending: usize, delivery: ActiveMessageDelivery) -> Result<Self> {
        if max_pending == 0 || max_pending > MAX_PENDING_MESSAGES {
            return Err(Error::Config(format!(
                "message queue limit must be between 1 and {MAX_PENDING_MESSAGES}"
            )));
        }
        Ok(Self {
            max_pending,
            delivery,
        })
    }

    fn remove_widget(&self, id: &str) -> FrontendEvent {
        FrontendEvent::RemoveWidget {
            capability: self.name().into(),
            id: id.into(),
        }
    }

    fn queued_widget(&self, id: &str, message: &MessageEvent) -> FrontendEvent {
        let action = matches!(message.author, MessageAuthor::User).then(|| Op::CapabilityCommand {
            capability: self.name().into(),
            command: EDIT_COMMAND.into(),
            arguments: id.into(),
            input: Some(message.text.clone()),
            target: None,
        });
        FrontendEvent::Widget {
            capability: self.name().into(),
            item: FrontendWidget {
                id: id.into(),
                slot: FrontendSlot::TranscriptTail,
                text: message.text.clone(),
                tone: FrontendTone::Neutral,
                symbol: Some(match message.delivery {
                    MessageDelivery::Turn => FrontendSymbol::Chat,
                    MessageDelivery::Steer => FrontendSymbol::Custom("steer".into()),
                    MessageDelivery::Queue => FrontendSymbol::Custom("queue".into()),
                }),
                icon_only: false,
                progress: None,
                content: None,
                action,
            },
        }
    }

    fn prepare(
        &self,
        context: &MessageRouteContext<'_>,
    ) -> std::result::Result<QueuedMessageBoundary, String> {
        let Some(turn_id) = context.active_turn_id else {
            return if context.message.target_turn_id.is_some() {
                Err("message targeted a stale turn".into())
            } else {
                Ok(QueuedMessageBoundary::Turn)
            };
        };
        if matches!(context.message.author, MessageAuthor::Peer { .. }) {
            return Ok(QueuedMessageBoundary::Steer {
                turn_id: turn_id.into(),
            });
        }
        if let Some(target) = &context.message.target_turn_id
            && target != turn_id
        {
            return Err("message targeted a stale turn".into());
        }
        match context.message.requested_delivery.unwrap_or(self.delivery) {
            ActiveMessageDelivery::Steer => Ok(QueuedMessageBoundary::Steer {
                turn_id: turn_id.into(),
            }),
            ActiveMessageDelivery::Queue => Ok(QueuedMessageBoundary::Queue),
        }
    }
}

impl Middleware for Messages {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn frontend(&self) -> FrontendContribution {
        FrontendContribution {
            capability: self.name().into(),
            commands: vec![FrontendCommand {
                name: voice::transcript::COMMAND.into(),
                arguments: String::new(),
                description: "Open the voice transcript".into(),
                requires_idle: false,
            }],
            ..FrontendContribution::default()
        }
    }

    fn command<'a>(
        &'a self,
        context: MiddlewareCommandContext<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareCommandOutput>> {
        Box::pin(async move {
            if context.command != voice::transcript::COMMAND {
                return Err(Error::Unknown(format!(
                    "messages command `{}`",
                    context.command
                )));
            }
            Ok(MiddlewareCommandOutput::events(vec![
                voice::transcript::read_preview(
                    context.checkpoints.as_ref(),
                    context.session_id,
                    context.arguments,
                )
                .await?,
            ]))
        })
    }

    fn handles_messages(&self) -> bool {
        true
    }

    fn route_message(&self, context: &mut MessageRouteContext<'_>) -> Result<SubmissionResult> {
        let boundary = match self.prepare(context) {
            Ok(boundary) => boundary,
            Err(message) => return Ok(SubmissionResult::Rejected(message)),
        };
        if context.queued_messages.count() >= self.max_pending {
            return Ok(SubmissionResult::Rejected("message queue is full".into()));
        }
        let event = MessageEvent {
            author: context.message.author.clone(),
            delivery: boundary.delivery(),
            text: context.message.text.clone(),
            attachments: context.message.attachments.clone(),
            reply: context.message.reply.clone(),
            message_target: None,
        };
        if !context.queued_messages.enqueue(
            context.submission_id,
            boundary.clone(),
            event.clone(),
        )? {
            return Ok(SubmissionResult::Rejected(
                "message could not be queued".into(),
            ));
        }
        context.events.push(EventMsg::Frontend(
            self.queued_widget(context.submission_id, &event),
        ));
        Ok(SubmissionResult::Accepted {
            input_changed: matches!(boundary, QueuedMessageBoundary::Steer { .. }),
        })
    }

    fn message_boundary_events(&self, submission_id: &str) -> Vec<EventMsg> {
        vec![EventMsg::Frontend(self.remove_widget(submission_id))]
    }

    fn active_command<'a>(
        &'a self,
        context: &'a mut ActiveCommandContext<'_>,
    ) -> BoxFuture<'a, Result<Option<SubmissionResult>>> {
        Box::pin(async move {
            if context.command == voice::transcript::COMMAND {
                let result = voice::transcript::read_preview(
                    context.checkpoints,
                    context.session_id,
                    context.arguments,
                )
                .await;
                return Ok(Some(match result {
                    Ok(event) => {
                        context.events.push(EventMsg::Frontend(event));
                        SubmissionResult::Handled
                    }
                    Err(error) => SubmissionResult::Rejected(error.to_string()),
                }));
            }
            if context.command != EDIT_COMMAND {
                return Ok(None);
            }
            let Some(input) = context.input.filter(|input| !input.trim().is_empty()) else {
                return Ok(Some(SubmissionResult::Rejected(INVALID_EDIT.into())));
            };
            if input.len() > MAX_CAPABILITY_INPUT_BYTES {
                return Ok(Some(SubmissionResult::Rejected(
                    "message exceeds editable size limit".into(),
                )));
            }
            let Some(queued) = context.queued_messages.find(context.arguments) else {
                return Ok(Some(SubmissionResult::Rejected(STALE_EDIT.into())));
            };
            let mut event = queued.event();
            if !matches!(event.author, MessageAuthor::User) {
                return Ok(Some(SubmissionResult::Rejected(
                    "peer messages cannot be edited".into(),
                )));
            }
            event.text = input.into();
            let input_changed = event.delivery == MessageDelivery::Steer;
            if !context.queued_messages.replace(
                context.arguments,
                context.submission_id,
                event.clone(),
            )? {
                return Ok(Some(SubmissionResult::Rejected(STALE_EDIT.into())));
            }
            context
                .events
                .push(EventMsg::Frontend(self.remove_widget(context.arguments)));
            context.events.push(EventMsg::Frontend(
                self.queued_widget(context.submission_id, &event),
            ));
            Ok(Some(SubmissionResult::Accepted { input_changed }))
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
            for queued in context.queued_messages().views() {
                (context.runtime.frontend)(self.queued_widget(queued.id(), &queued.event()))?;
            }
            if let Some(widget) = voice::transcript::restore_widget(
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::backend::checkpoint::QueuedMessage;
    use crate::middleware::{
        ActiveCommandContext, MessageQueue, MessageRouteContext, MiddlewareStack,
    };
    use crate::protocol::{MessageReply, MessageSubmission, MessageTarget, SessionFileReference};

    #[test]
    fn manifest_advertises_delivery_symbols() {
        assert!(
            MANIFEST
                .feature(&[])
                .settings
                .iter()
                .all(|setting| !setting.composer)
        );
        assert_eq!(
            DELIVERIES
                .iter()
                .map(|choice| (choice.value, choice.symbol))
                .collect::<Vec<_>>(),
            [("steer", Some("steer")), ("queue", Some("queue"))]
        );
    }

    #[test]
    fn queued_widgets_name_their_delivery() {
        let messages = Messages::default();
        let symbol = |delivery| {
            let FrontendEvent::Widget { item, .. } = messages.queued_widget(
                "message-1",
                &MessageEvent {
                    author: MessageAuthor::User,
                    delivery,
                    text: "hello".into(),
                    attachments: Vec::new(),
                    reply: None,
                    message_target: None,
                },
            ) else {
                panic!("queued message widget");
            };
            item.symbol
        };

        assert_eq!(
            (
                symbol(MessageDelivery::Steer),
                symbol(MessageDelivery::Queue),
            ),
            (
                Some(FrontendSymbol::Custom("steer".into())),
                Some(FrontendSymbol::Custom("queue".into())),
            )
        );
    }

    fn user(delivery: Option<ActiveMessageDelivery>) -> MessageSubmission {
        MessageSubmission {
            author: MessageAuthor::User,
            text: "hello".into(),
            attachments: Vec::<SessionFileReference>::new(),
            reply: None,
            requested_delivery: delivery,
            target_turn_id: None,
        }
    }

    fn route(
        stack: &MiddlewareStack,
        queued: &mut Vec<QueuedMessage>,
        message: &MessageSubmission,
        active_turn_id: Option<&str>,
    ) -> SubmissionResult {
        stack
            .route_message(&mut MessageRouteContext {
                submission_id: "message-1",
                message,
                active_turn_id,
                queued_messages: MessageQueue::new(queued),
                events: &mut Vec::new(),
            })
            .expect("route message")
    }

    #[test]
    fn active_user_uses_the_configured_queue_boundary() {
        let stack = MiddlewareStack::new(vec![Arc::new(
            Messages::new(4, ActiveMessageDelivery::Queue).expect("messages"),
        )])
        .expect("stack");
        let mut queued = Vec::new();

        let result = route(&stack, &mut queued, &user(None), Some("turn-1"));

        assert_eq!(
            result,
            SubmissionResult::Accepted {
                input_changed: false
            }
        );
        assert!(
            !stack
                .messages_ready(&queued, "turn-1")
                .expect("message input readiness")
        );
        assert_eq!(
            stack
                .next_turn(&mut queued)
                .expect("next turn")
                .expect("queued message")
                .event
                .message()
                .map(|message| message.delivery),
            Some(MessageDelivery::Queue)
        );
    }

    #[tokio::test]
    async fn queued_message_edit_preserves_its_reply_snapshot() {
        let stack = MiddlewareStack::new(vec![Arc::new(Messages::default())]).expect("stack");
        let mut queued = Vec::new();
        let mut message = user(Some(ActiveMessageDelivery::Queue));
        message.reply = Some(MessageReply {
            target: MessageTarget {
                checkpoint_sequence: 5,
                batch_item_count: 2,
            },
            text: "Earlier".into(),
        });
        route(&stack, &mut queued, &message, Some("turn-1"));
        let mut events = Vec::new();
        let metadata = BTreeMap::new();
        let directory = tempfile::tempdir().expect("checkpoint directory");
        let checkpoints = crate::backend::checkpoint::sqlite::SqliteCheckpoint::new(
            directory.path().join("checkpoints.sqlite3"),
        )
        .expect("checkpoint store");

        stack
            .active_command(
                MANIFEST.id,
                &mut ActiveCommandContext {
                    checkpoints: &checkpoints,
                    submission_id: "message-2",
                    session_id: "session-1",
                    metadata: &metadata,
                    active_turn_id: "turn-1",
                    command: EDIT_COMMAND,
                    arguments: "message-1",
                    input: Some("Updated"),
                    target: None,
                    queued_messages: MessageQueue::new(&mut queued),
                    events: &mut events,
                },
            )
            .await
            .expect("edit queued message")
            .expect("message command");

        let edited = queued[0].event();
        assert_eq!(
            (edited.text.as_str(), edited.reply),
            ("Updated", message.reply)
        );
    }

    #[test]
    fn active_peer_always_steers_as_non_authoritative_input() {
        let stack = MiddlewareStack::new(vec![Arc::new(
            Messages::new(4, ActiveMessageDelivery::Queue).expect("messages"),
        )])
        .expect("stack");
        let peer = MessageSubmission {
            author: MessageAuthor::Peer {
                message_id: "board-1".into(),
                session_id: "peer-1".into(),
                handle: "worker".into(),
                symbol: None,
            },
            text: "review this".into(),
            attachments: Vec::new(),
            reply: None,
            requested_delivery: Some(ActiveMessageDelivery::Queue),
            target_turn_id: None,
        };
        let mut queued = Vec::new();

        let result = route(&stack, &mut queued, &peer, Some("turn-1"));
        let staged = stack
            .stage_model_messages(&mut queued, "turn-1")
            .expect("stage message");

        assert_eq!(
            result,
            SubmissionResult::Accepted {
                input_changed: true
            }
        );
        assert!(staged[0].input.get("_mobius_internal").is_some());
        assert_eq!(
            staged[0].event.message().map(|message| message.delivery),
            Some(MessageDelivery::Steer)
        );
    }

    #[test]
    fn failed_turn_promotes_unstaged_steering_to_a_queued_turn() {
        let stack = MiddlewareStack::new(vec![Arc::new(Messages::default())]).expect("stack");
        let mut queued = Vec::new();
        route(&stack, &mut queued, &user(None), Some("turn-1"));

        stack
            .finish_message_turn(
                &mut queued,
                "turn-1",
                crate::backend::checkpoint::ExecutionOutcome::Failed,
            )
            .expect("promote failed turn");
        let next = stack
            .next_turn(&mut queued)
            .expect("next turn")
            .expect("promoted message");

        assert_eq!(
            next.event.message().map(|message| message.delivery),
            Some(MessageDelivery::Queue)
        );
    }
}
