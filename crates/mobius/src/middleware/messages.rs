//! Durable conversation-message delivery.

use crate::SessionId;
use std::sync::Arc;

use super::{
    ActiveCommandContext, MessageRouteContext, MessageSubmitContext, Middleware,
    SessionStartContext, SessionStartSource, SubmissionResult,
};
use crate::backend::checkpoint::{CheckpointStore, EventPageRequest, QueuedMessageBoundary};
use crate::backend::model::internal_user_message;
use crate::protocol::{
    ActiveMessageDelivery, EventMsg, FrontendBlock, FrontendCommand, FrontendContribution,
    FrontendEvent, FrontendSlot, FrontendSymbol, FrontendWidget, MessageAuthor, MessageDelivery,
    MessageEvent, MessageReply, Op,
};
use crate::{BoxFuture, Result};

mod text {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Definition {
        #[serde(deserialize_with = "crate::middleware::manifest::deserialize_settings")]
        pub(super) settings: Vec<crate::middleware::manifest::MiddlewareSettingManifest>,
        pub(super) permission_notice: String,
        pub(super) reply_target_invalid: String,
        pub(super) reply_text_changed: String,
        pub(super) stale_turn: String,
        pub(super) queue_full: String,
        pub(super) queue_failed: String,
        pub(super) edit_stale: String,
        pub(super) edit_empty: String,
        pub(super) edit_peer: String,
        pub(super) command_edit_description: String,
        pub(super) command_edit_arguments: String,
        pub(super) received_title: String,
        pub(super) default_enabled: bool,
        pub(super) manifest_description: String,
        pub(super) manifest_label: String,
    }
    crate::embedded_config! { pub(super) static DEFINITION: Definition = include_str!("messages.toml"); }
}
const REPLY_EVENT_PAGE_SIZE: usize = 256;
const MAX_PENDING: &str = "max_pending";
const DELIVERY: &str = "delivery";

super::manifest::middleware_manifest! {
/// Configuration and presentation metadata for message delivery.
    "messages", text::DEFINITION, required: true, settings: &text::DEFINITION.settings
}

const EDIT_COMMAND: &str = "edit";

fn rejected(message: &str) -> SubmissionResult {
    SubmissionResult::Rejected(message.into())
}

/// Prepares every conversation message and owns its durable delivery lifecycle.
#[derive(Debug)]
pub struct Messages {
    max_pending: std::num::NonZeroUsize,
    delivery: ActiveMessageDelivery,
}

impl Default for Messages {
    #[expect(
        clippy::expect_used,
        reason = "messages.toml embeds a non-negative max_pending and a valid delivery default"
    )]
    fn default() -> Self {
        let settings = &text::DEFINITION.settings;
        Self {
            max_pending: usize::try_from(super::manifest::integer_default(settings, MAX_PENDING))
                .ok()
                .and_then(std::num::NonZeroUsize::new)
                .expect("valid embedded max_pending"),
            delivery: super::manifest::string_default(settings, DELIVERY)
                .parse()
                .expect("valid embedded delivery"),
        }
    }
}

impl Messages {
    /// Creates message delivery with a bounded queue and active-turn default.
    #[must_use]
    pub const fn new(max_pending: std::num::NonZeroUsize, delivery: ActiveMessageDelivery) -> Self {
        Self {
            max_pending,
            delivery,
        }
    }

    fn remove_widget(&self, id: &str) -> FrontendEvent {
        FrontendEvent::RemoveWidget {
            capability: self.name().into(),
            id: id.into(),
        }
    }

    fn queued_widget(
        &self,
        id: &str,
        author: &MessageAuthor,
        delivery: MessageDelivery,
        text: &str,
    ) -> FrontendEvent {
        let action = matches!(author, MessageAuthor::User).then(|| Op::CapabilityCommand {
            capability: self.name().into(),
            command: EDIT_COMMAND.into(),
            arguments: id.into(),
            input: Some(text.into()),
            target: None,
        });
        FrontendEvent::Widget {
            capability: self.name().into(),
            item: Arc::new(FrontendWidget {
                symbol: Some(match delivery {
                    MessageDelivery::Turn => FrontendSymbol::Chat,
                    MessageDelivery::Steer => FrontendSymbol::Steer,
                    MessageDelivery::Queue => FrontendSymbol::Queue,
                }),
                action,
                ..FrontendWidget::new(id, FrontendSlot::TranscriptTail, text)
            }),
        }
    }

    fn prepare(
        &self,
        context: &MessageRouteContext<'_>,
    ) -> std::result::Result<QueuedMessageBoundary, String> {
        let tool_result = context.message.author.is_tool_result();
        let Some(turn_id) = context.active_turn_id else {
            return if !tool_result && context.message.target_turn_id.is_some() {
                Err(text::DEFINITION.stale_turn.as_str().into())
            } else {
                Ok(QueuedMessageBoundary::Turn)
            };
        };
        if let Some(target) = &context.message.target_turn_id
            && target != turn_id
        {
            return if tool_result {
                Ok(QueuedMessageBoundary::Queue)
            } else {
                Err(text::DEFINITION.stale_turn.as_str().into())
            };
        }
        match context.message.requested_delivery.unwrap_or(self.delivery) {
            ActiveMessageDelivery::Steer => Ok(QueuedMessageBoundary::Steer {
                turn_id: turn_id.into(),
            }),
            ActiveMessageDelivery::Queue => Ok(QueuedMessageBoundary::Queue),
        }
    }

    fn enqueue(&self, context: &mut MessageRouteContext<'_>) -> Result<SubmissionResult> {
        let boundary = match self.prepare(context) {
            Ok(boundary) => boundary,
            Err(message) => return Ok(SubmissionResult::Rejected(message)),
        };
        if context.queued_messages.count() >= self.max_pending.get() {
            return Ok(rejected(&text::DEFINITION.queue_full));
        }
        let message = &mut *context.message;
        let event = MessageEvent {
            author: std::mem::replace(&mut message.author, MessageAuthor::User),
            delivery: boundary.delivery(),
            text: std::mem::take(&mut message.text),
            attachments: std::mem::take(&mut message.attachments),
            reply: message.reply.take(),
            message_target: None,
        };
        let widget = (!matches!(boundary, QueuedMessageBoundary::Turn)
            && !event.author.is_tool_result())
        .then(|| {
            self.queued_widget(
                context.submission_id,
                &event.author,
                event.delivery,
                &event.text,
            )
        });
        let input_changed = matches!(boundary, QueuedMessageBoundary::Steer { .. });
        if !context
            .queued_messages
            .enqueue_validated(context.submission_id, boundary, event)?
        {
            return Ok(rejected(&text::DEFINITION.queue_failed));
        }
        if let Some(widget) = widget {
            context.events.push(EventMsg::Frontend(widget));
        }
        Ok(SubmissionResult::Accepted { input_changed })
    }

    fn edit(&self, context: &mut ActiveCommandContext<'_>) -> Result<SubmissionResult> {
        let Some(input) = context.input.filter(|input| !input.trim().is_empty()) else {
            return Ok(rejected(&text::DEFINITION.edit_empty));
        };
        let Some(queued) = context.queued_messages.find(context.arguments) else {
            return Ok(rejected(&text::DEFINITION.edit_stale));
        };
        if !matches!(queued.author(), MessageAuthor::User) {
            return Ok(rejected(&text::DEFINITION.edit_peer));
        }
        let event = MessageEvent {
            author: MessageAuthor::User,
            delivery: queued.delivery(),
            text: input.into(),
            attachments: queued.attachments().to_vec(),
            reply: queued.reply().cloned(), // rules-allow: the queued entry is borrowed and replaced by this owned event
            message_target: None,
        };
        let input_changed = event.delivery == MessageDelivery::Steer;
        let widget = self.queued_widget(
            context.submission_id,
            &event.author,
            event.delivery,
            &event.text,
        );
        if !context
            .queued_messages
            .replace(context.arguments, context.submission_id, event)?
        {
            return Ok(rejected(&text::DEFINITION.edit_stale));
        }
        context
            .events
            .push(EventMsg::Frontend(self.remove_widget(context.arguments)));
        context.events.push(EventMsg::Frontend(widget));
        Ok(SubmissionResult::Accepted { input_changed })
    }
}

impl Middleware for Messages {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        let EventMsg::Message(message) = event else {
            return None;
        };
        let MessageAuthor::Source {
            message_id,
            source,
            handle,
            symbol,
            ..
        } = &message.author
        else {
            return None;
        };
        if matches!(
            source,
            crate::protocol::MessageSource::Session { .. }
                | crate::protocol::MessageSource::Call { .. }
                | crate::protocol::MessageSource::Tool { .. }
        ) {
            return None;
        }
        Some(FrontendBlock {
            id: Some(format!(
                "message_received:{}:{session_id}:{message_id}",
                source.id().len(),
                session_id = source.id()
            )),
            title: text::DEFINITION.received_title.replace("{handle}", handle),
            text: message.text.clone(), // rules-allow: the block owns text rendered from a borrowed event
            symbol: Some(symbol.clone().unwrap_or(FrontendSymbol::Chat)), // rules-allow: the block owns a symbol from a borrowed event
            ..Default::default()
        })
    }

    fn handles_messages(&self) -> bool {
        true
    }

    fn route_message<'a>(
        &'a self,
        context: &'a mut MessageRouteContext<'_>,
    ) -> BoxFuture<'a, Result<SubmissionResult>> {
        Box::pin(async move {
            if let Some(reply) = &context.message.reply
                && let Some(rejection) =
                    validate_reply(context.checkpoints, context.session_id, reply).await?
            {
                return Ok(SubmissionResult::Rejected(rejection.into()));
            }
            self.enqueue(context)
        })
    }
    fn message_boundary_events(&self, submission_id: &str) -> Vec<EventMsg> {
        vec![EventMsg::Frontend(self.remove_widget(submission_id))]
    }

    fn message_submit<'a>(
        &'a self,
        context: &'a mut MessageSubmitContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if context.author.is_tool_result() {
                // An asynchronous tool's own card shows its result.
                context.hide_message();
            } else if matches!(context.author, MessageAuthor::Source { .. }) {
                context.deliver_once("permissions", || {
                    internal_user_message(
                        "message_permissions",
                        text::DEFINITION.permission_notice.as_str(),
                    )
                })?;
            }
            Ok(())
        })
    }

    fn frontend(&self, _session_id: &str) -> FrontendContribution {
        FrontendContribution {
            commands: vec![FrontendCommand::new(
                EDIT_COMMAND,
                &text::DEFINITION.command_edit_arguments,
                &text::DEFINITION.command_edit_description,
                false,
            )],
            ..FrontendContribution::default()
        }
    }

    fn active_command<'a>(
        &'a self,
        context: &'a mut ActiveCommandContext<'_>,
    ) -> BoxFuture<'a, Result<Option<SubmissionResult>>> {
        Box::pin(async move {
            if context.command == EDIT_COMMAND {
                self.edit(context).map(Some)
            } else {
                Ok(None)
            }
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
                (context.runtime.frontend)(self.queued_widget(
                    queued.id(),
                    queued.author(),
                    queued.delivery(),
                    queued.text(),
                ))?;
            }
            Ok(())
        })
    }
}

/// Accepts a reply only when it quotes a durable message of this chat exactly.
async fn validate_reply(
    checkpoints: &dyn CheckpointStore,
    session_id: &SessionId,
    reply: &MessageReply,
) -> Result<Option<&'static str>> {
    let mut before_sequence = None;
    loop {
        let page = checkpoints
            .event_page(
                session_id,
                EventPageRequest {
                    before_sequence,
                    limit: REPLY_EVENT_PAGE_SIZE,
                },
            )
            .await?;
        for record in page.events {
            let original = match &record.event.msg {
                EventMsg::Message(message)
                    if message.message_target.as_ref() == Some(&reply.target) =>
                {
                    Some(message.reply_text())
                }
                EventMsg::AssistantMessage(message)
                    if message.message_target.as_ref() == Some(&reply.target) =>
                {
                    Some(message.reply_text())
                }
                _ => None,
            };
            if let Some(original) = original {
                return Ok(match original {
                    Some(original) if original.as_ref() == reply.text => None,
                    Some(_) => Some(text::DEFINITION.reply_text_changed.as_str()),
                    None => Some(text::DEFINITION.reply_target_invalid.as_str()),
                });
            }
        }
        let Some(next) = page.next_before_sequence else {
            return Ok(Some(text::DEFINITION.reply_target_invalid.as_str()));
        };
        before_sequence = Some(next);
    }
}

#[cfg(test)]
mod tests;
