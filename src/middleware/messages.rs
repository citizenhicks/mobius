//! Durable conversation-message delivery.

use super::{
    ActiveCommandContext, MessageRouteContext, MessageSubmitContext, Middleware,
    SessionStartContext, SessionStartSource, SubmissionResult,
};
use crate::backend::checkpoint::{CheckpointStore, EventPageRequest, QueuedMessageBoundary};
use crate::backend::model::internal_user_message;
use crate::protocol::{
    ActiveMessageDelivery, EventMsg, FrontendBlock, FrontendContribution, FrontendEvent,
    FrontendSettingValue, FrontendSlot, FrontendSymbol, FrontendTone, FrontendWidget,
    MAX_CAPABILITY_INPUT_BYTES, MessageAuthor, MessageDelivery, MessageEvent, MessageReply, Op,
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
        pub(super) edit_too_large: String,
        pub(super) edit_peer: String,
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
pub struct Messages {
    max_pending: usize,
    delivery: ActiveMessageDelivery,
}

impl Default for Messages {
    fn default() -> Self {
        let settings = &text::DEFINITION.settings;
        Self {
            max_pending: super::manifest::integer_default(settings, MAX_PENDING) as usize,
            delivery: super::manifest::string_default(settings, DELIVERY)
                .parse()
                .expect("valid embedded delivery"),
        }
    }
}

impl Messages {
    /// Creates message delivery with a bounded queue and active-turn default.
    /// # Errors
    ///
    /// Returns an error if configuration is invalid or a required resource cannot be initialized.
    pub fn new(max_pending: usize, delivery: ActiveMessageDelivery) -> Result<Self> {
        text::DEFINITION
            .settings
            .iter()
            .find(|setting| setting.id() == MAX_PENDING)
            .expect("embedded max_pending setting")
            .validate(
                MANIFEST.id,
                Some(&FrontendSettingValue::Integer(
                    i64::try_from(max_pending).unwrap_or(i64::MAX),
                )),
            )?;
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
            item: FrontendWidget {
                id: id.into(),
                slot: FrontendSlot::TranscriptTail,
                text: text.into(),
                tone: FrontendTone::Neutral,
                symbol: Some(match delivery {
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
                Err(text::DEFINITION.stale_turn.as_str().into())
            } else {
                Ok(QueuedMessageBoundary::Turn)
            };
        };
        if let Some(target) = &context.message.target_turn_id
            && target != turn_id
        {
            return Err(text::DEFINITION.stale_turn.as_str().into());
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
        if context.queued_messages.count() >= self.max_pending {
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
        let widget = (!matches!(boundary, QueuedMessageBoundary::Turn)).then(|| {
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
            .enqueue(context.submission_id, boundary, event)?
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
        if input.len() > MAX_CAPABILITY_INPUT_BYTES {
            return Ok(rejected(&text::DEFINITION.edit_too_large));
        }
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
            reply: queued.reply().cloned(),
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

    fn frontend(&self, _session_id: &str) -> FrontendContribution {
        FrontendContribution {
            capability: self.name().into(),
            ..FrontendContribution::default()
        }
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
        if matches!(source, crate::protocol::MessageSource::Session { .. }) {
            return None;
        }
        Some(FrontendBlock {
            id: Some(format!(
                "message_received:{}:{session_id}:{message_id}",
                source.id().len(),
                session_id = source.id()
            )),
            title: text::DEFINITION.received_title.replace("{handle}", handle),
            text: message.text.clone(),
            symbol: Some(symbol.clone().unwrap_or(FrontendSymbol::Chat)),
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
            if matches!(context.author, MessageAuthor::Source { .. }) {
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
    session_id: &str,
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
mod tests {
    use crate::protocol::FrontendBlockRole;
    #[test]
    fn declared_deliveries_parse_at_the_protocol_boundary() {
        for choice in
            super::super::manifest::static_choices(&super::text::DEFINITION.settings, "delivery")
                .iter()
        {
            assert_eq!(
                choice
                    .value
                    .parse::<super::ActiveMessageDelivery>()
                    .expect("declared delivery")
                    .id(),
                choice.value
            );
        }
        assert!("other".parse::<super::ActiveMessageDelivery>().is_err());
    }

    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
    use crate::backend::checkpoint::{Checkpoint, QueuedMessage};
    use crate::middleware::{
        ActiveCommandContext, MessageQueue, MessageRouteContext, MiddlewareStack,
    };
    use crate::protocol::{MessageReply, MessageSubmission, MessageTarget, SessionFileReference};

    #[test]
    fn queue_limit_follows_the_declared_setting_bounds() {
        for (limit, valid) in [(0, false), (1, true), (1024, true), (1025, false)] {
            assert_eq!(
                Messages::new(limit, ActiveMessageDelivery::Steer).is_ok(),
                valid
            );
        }
    }

    #[test]
    fn manifest_advertises_delivery_symbols() {
        assert!(
            MANIFEST
                .feature(Default::default())
                .settings
                .iter()
                .all(|setting| !setting.composer)
        );
        assert_eq!(
            super::super::manifest::static_choices(&text::DEFINITION.settings, "delivery")
                .iter()
                .map(|choice| (choice.value.as_str(), choice.symbol.as_deref()))
                .collect::<Vec<_>>(),
            [("steer", Some("steer")), ("queue", Some("queue"))]
        );
    }

    #[test]
    fn queued_widgets_name_their_delivery() {
        let messages = Messages::default();
        let symbol = |delivery| {
            let FrontendEvent::Widget { item, .. } =
                messages.queued_widget("message-1", &MessageAuthor::User, delivery, "hello")
            else {
                panic!("queued message widget");
            };
            assert_eq!(item.text, "hello");
            assert!(matches!(
                item.action,
                Some(Op::CapabilityCommand { arguments, input: Some(input), .. })
                    if arguments == "message-1" && input == "hello"
            ));
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

    fn event(id: &str, msg: EventMsg) -> crate::protocol::Event {
        crate::protocol::Event {
            submission_id: Some(id.into()),
            msg,
        }
    }

    fn checkpoints() -> (tempfile::TempDir, SqliteCheckpoint) {
        let directory = tempfile::tempdir().expect("checkpoint directory");
        let store = SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store");
        (directory, store)
    }

    async fn route(
        stack: &MiddlewareStack,
        queued: &mut Vec<QueuedMessage>,
        message: &MessageSubmission,
        active_turn_id: Option<&str>,
    ) -> SubmissionResult {
        let (_directory, store) = checkpoints();
        route_in(&store, stack, queued, message, active_turn_id).await
    }

    async fn route_in(
        checkpoints: &dyn CheckpointStore,
        stack: &MiddlewareStack,
        queued: &mut Vec<QueuedMessage>,
        message: &MessageSubmission,
        active_turn_id: Option<&str>,
    ) -> SubmissionResult {
        let mut message = message.clone();
        stack
            .route_message(&mut MessageRouteContext {
                checkpoints,
                session_id: "session-1",
                submission_id: "message-1",
                message: &mut message,
                active_turn_id,
                queued_messages: MessageQueue::new(queued),
                events: &mut Vec::new(),
            })
            .await
            .expect("route message")
    }

    #[tokio::test]
    async fn active_user_uses_the_configured_queue_boundary() {
        let stack = MiddlewareStack::new(vec![Arc::new(
            Messages::new(4, ActiveMessageDelivery::Queue).expect("messages"),
        )])
        .expect("stack");
        let mut queued = Vec::new();

        let result = route(&stack, &mut queued, &user(None), Some("turn-1")).await;

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
    async fn immediate_turn_does_not_publish_a_queued_widget() {
        let stack = MiddlewareStack::new(vec![Arc::new(Messages::default())]).expect("stack");
        let mut queued = Vec::new();
        let mut events = Vec::new();

        let (_directory, store) = checkpoints();
        let result = stack
            .route_message(&mut MessageRouteContext {
                checkpoints: &store,
                session_id: "session-1",
                submission_id: "message-1",
                message: &mut user(None),
                active_turn_id: None,
                queued_messages: MessageQueue::new(&mut queued),
                events: &mut events,
            })
            .await
            .expect("route message");

        assert_eq!(
            result,
            SubmissionResult::Accepted {
                input_changed: false
            }
        );
        assert!(events.is_empty());
        assert_eq!(queued.len(), 1);
    }

    #[tokio::test]
    async fn queued_message_edit_preserves_retained_content_after_validation_failure() {
        let stack = MiddlewareStack::new(vec![Arc::new(Messages::default())]).expect("stack");
        let mut queued = Vec::new();
        let mut message = user(Some(ActiveMessageDelivery::Queue));
        message.attachments.push(SessionFileReference {
            id: uuid::Uuid::new_v4().to_string(),
            name: "notes.txt".into(),
            size: 8,
            media_type: "text/plain".into(),
        });
        let target = MessageTarget {
            checkpoint_sequence: 5,
            batch_item_count: 2,
        };
        message.reply = Some(MessageReply {
            target,
            text: "Earlier".into(),
        });
        let (_directory, checkpoints) = checkpoints();
        let mut session = crate::backend::checkpoint::Checkpoint::empty("session-1");
        session.session_context.owner_id = "test-bot".into();
        checkpoints
            .save(&session, &[], None)
            .await
            .expect("save session");
        checkpoints
            .append_event(
                "session-1",
                1,
                &event(
                    "earlier",
                    EventMsg::Message(MessageEvent {
                        author: MessageAuthor::User,
                        delivery: MessageDelivery::Turn,
                        text: "Earlier".into(),
                        attachments: Vec::new(),
                        reply: None,
                        message_target: Some(target),
                    }),
                ),
            )
            .await
            .expect("append quoted message");
        route_in(&checkpoints, &stack, &mut queued, &message, Some("turn-1")).await;
        let mut events = Vec::new();
        let metadata = BTreeMap::new();

        let original = queued.clone();
        for submission_id in [" ", "message-2"] {
            let result = stack
                .active_command(
                    MANIFEST.id,
                    &mut ActiveCommandContext {
                        checkpoints: &checkpoints,
                        submission_id,
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
                .await;
            if submission_id.trim().is_empty() {
                assert!(result.is_err());
                assert_eq!(queued, original);
                assert!(events.is_empty());
            } else {
                assert_eq!(
                    result.expect("edit queued message"),
                    Some(SubmissionResult::Accepted {
                        input_changed: false
                    })
                );
            }
        }

        let edited = queued[0].event();
        assert_eq!(queued[0].id(), "message-2");
        assert_eq!(
            (
                edited.text.as_str(),
                edited.delivery,
                edited.attachments,
                edited.reply
            ),
            (
                "Updated",
                MessageDelivery::Queue,
                message.attachments,
                message.reply
            )
        );
    }

    #[tokio::test]
    async fn explicitly_steered_source_remains_non_authoritative_input() {
        let stack = MiddlewareStack::new(vec![Arc::new(
            Messages::new(4, ActiveMessageDelivery::Queue).expect("messages"),
        )])
        .expect("stack");
        let peer = MessageSubmission {
            author: MessageAuthor::Source {
                message_id: "board-1".into(),
                source: crate::protocol::MessageSource::Session {
                    session_id: "peer-1".into(),
                },
                cause_id: None,
                ancestry: Vec::new(),
                handle: "worker".into(),
                symbol: None,
            },
            text: "Review this.\n\nKeep the validation.\n".into(),
            attachments: Vec::new(),
            reply: None,
            requested_delivery: Some(ActiveMessageDelivery::Steer),
            target_turn_id: None,
        };
        let mut queued = Vec::new();

        let result = route(&stack, &mut queued, &peer, Some("turn-1")).await;
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
        assert!(
            Messages::default()
                .render(&staged[0].event, "turn-1")
                .is_none(),
            "peer messages use their typed message event, not an activity block"
        );
        assert_eq!(staged[0].event.message().unwrap().text, peer.text);
    }

    #[tokio::test]
    async fn queued_external_event_waits_for_its_own_turn() {
        let stack = MiddlewareStack::new(vec![Arc::new(Messages::default())]).expect("stack");
        let source = MessageSubmission {
            author: MessageAuthor::Source {
                message_id: "report".into(),
                source: crate::protocol::MessageSource::External {
                    source_id: "routine".into(),
                    event_id: "completed".into(),
                },
                cause_id: None,
                ancestry: Vec::new(),
                handle: "monitor".into(),
                symbol: None,
            },
            text: "Monitoring completed.".into(),
            attachments: Vec::new(),
            reply: None,
            requested_delivery: Some(ActiveMessageDelivery::Queue),
            target_turn_id: None,
        };
        let mut queued = Vec::new();
        assert_eq!(
            route(&stack, &mut queued, &source, Some("user-turn")).await,
            SubmissionResult::Accepted {
                input_changed: false
            }
        );
        assert!(
            stack
                .stage_model_messages(&mut queued, "user-turn")
                .expect("stage")
                .is_empty()
        );
        let report = stack.next_turn(&mut queued).expect("next").expect("report");
        let block = Messages::default()
            .render(&report.event, "report-turn")
            .expect("external activity");
        assert_eq!(block.role, FrontendBlockRole::Activity);
        assert_eq!(block.title, "Message received from @monitor");
        assert_eq!(block.text, source.text);
        assert_eq!(block.symbol, Some(FrontendSymbol::Chat));
        assert!(matches!(report.event, EventMsg::Message(event) if event.author == source.author));
    }

    #[tokio::test]
    async fn failed_turn_promotes_unstaged_steering_to_a_queued_turn() {
        let stack = MiddlewareStack::new(vec![Arc::new(Messages::default())]).expect("stack");
        let mut queued = Vec::new();
        route(&stack, &mut queued, &user(None), Some("turn-1")).await;

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

    #[tokio::test]
    async fn reply_matches_a_durable_message_in_its_own_session() {
        let (_directory, store) = checkpoints();
        for session_id in ["current", "other"] {
            let mut checkpoint = Checkpoint::empty(session_id);
            checkpoint.session_context.owner_id = "test-bot".into();
            store
                .save(&checkpoint, &[], None)
                .await
                .expect("save session");
        }
        let attachment_target = crate::protocol::MessageTarget {
            checkpoint_sequence: 1,
            batch_item_count: 1,
        };
        store
            .append_event(
                "current",
                1,
                &event(
                    "attachment",
                    EventMsg::Message(crate::protocol::MessageEvent {
                        author: crate::protocol::MessageAuthor::User,
                        delivery: crate::protocol::MessageDelivery::Turn,
                        text: String::new(),
                        attachments: vec![crate::protocol::SessionFileReference {
                            id: "00000000-0000-0000-0000-000000000001".into(),
                            name: "clip.mov".into(),
                            size: 1,
                            media_type: "video/quicktime".into(),
                        }],
                        reply: None,
                        message_target: Some(attachment_target),
                    }),
                ),
            )
            .await
            .expect("append attachment message");
        let assistant_target = crate::protocol::MessageTarget {
            checkpoint_sequence: 1,
            batch_item_count: 2,
        };
        store
            .append_event(
                "current",
                2,
                &event(
                    "assistant",
                    EventMsg::AssistantMessage(crate::protocol::AssistantMessageEvent {
                        session_id: "current".into(),
                        turn_id: "turn".into(),
                        model_step_id: "step".into(),
                        content: vec![
                            crate::protocol::ModelStepContent {
                                output_index: 0,
                                part_index: 0,
                                phase: crate::protocol::ModelStepContentPhase::FinalAnswer,
                                text: "first part".into(),
                                annotations: Vec::new(),
                            },
                            crate::protocol::ModelStepContent {
                                output_index: 1,
                                part_index: 0,
                                phase: crate::protocol::ModelStepContentPhase::FinalAnswer,
                                text: "last part".into(),
                                annotations: Vec::new(),
                            },
                        ],
                        message_target: Some(assistant_target),
                    }),
                ),
            )
            .await
            .expect("append assistant message");
        let other_target = crate::protocol::MessageTarget {
            checkpoint_sequence: 2,
            batch_item_count: 1,
        };
        store
            .append_event(
                "other",
                1,
                &event(
                    "other",
                    EventMsg::Message(crate::protocol::MessageEvent {
                        author: crate::protocol::MessageAuthor::User,
                        delivery: crate::protocol::MessageDelivery::Turn,
                        text: "other chat".into(),
                        attachments: Vec::new(),
                        reply: None,
                        message_target: Some(other_target),
                    }),
                ),
            )
            .await
            .expect("append other message");

        let exact = MessageReply {
            target: attachment_target,
            text: "clip.mov".into(),
        };
        let changed = MessageReply {
            text: "forged quote".into(),
            ..exact.clone()
        };
        let other = MessageReply {
            target: other_target,
            text: "other chat".into(),
        };
        let assistant = MessageReply {
            target: assistant_target,
            text: "last part".into(),
        };
        let combined_assistant = MessageReply {
            target: assistant_target,
            text: "first partlast part".into(),
        };

        assert_eq!(
            (
                validate_reply(&store, "current", &exact)
                    .await
                    .expect("validate exact reply"),
                validate_reply(&store, "current", &changed)
                    .await
                    .expect("validate changed reply"),
                validate_reply(&store, "current", &other)
                    .await
                    .expect("validate cross-session reply"),
                validate_reply(&store, "current", &assistant)
                    .await
                    .expect("validate assistant reply"),
                validate_reply(&store, "current", &combined_assistant)
                    .await
                    .expect("validate combined assistant reply"),
            ),
            (
                None,
                Some(text::DEFINITION.reply_text_changed.as_str()),
                Some(text::DEFINITION.reply_target_invalid.as_str()),
                None,
                Some(text::DEFINITION.reply_text_changed.as_str()),
            )
        );
        let stack = MiddlewareStack::new(vec![Arc::new(Messages::default())]).expect("stack");
        let mut queued = Vec::new();
        let mut forged = user(None);
        forged.reply = Some(changed);
        let mut events = Vec::new();
        let result = stack
            .route_message(&mut MessageRouteContext {
                checkpoints: &store,
                session_id: "current",
                submission_id: "message-1",
                message: &mut forged,
                active_turn_id: None,
                queued_messages: MessageQueue::new(&mut queued),
                events: &mut events,
            })
            .await
            .expect("route forged reply");
        assert_eq!(
            result,
            SubmissionResult::Rejected(text::DEFINITION.reply_text_changed.clone())
        );
        assert!(queued.is_empty() && events.is_empty());
    }
}
