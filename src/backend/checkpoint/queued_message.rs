//! Durable queued messages awaiting delivery.

use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;

use crate::Error;
use crate::Result;
use crate::protocol::MessageAuthor;
use crate::protocol::MessageDelivery;
use crate::protocol::MessageEvent;
use crate::protocol::MessageReply;
use crate::protocol::MessageTarget;
use crate::protocol::SessionFileReference;

const MAX_QUEUED_OWNER_BYTES: usize = 256;
const MAX_QUEUED_ID_BYTES: usize = 4 * 1024;
const MAX_QUEUED_TURN_ID_BYTES: usize = 4 * 1024;

/// The single delivery boundary for one queued message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueuedMessageBoundary {
    /// Selects the turn case.
    Turn,
    /// Steers the active turn.
    Steer {
        /// The active turn identifier.
        turn_id: String,
    },
    /// Selects the queue case.
    Queue,
}

impl QueuedMessageBoundary {
    pub(crate) const fn delivery(&self) -> MessageDelivery {
        match self {
            Self::Turn => MessageDelivery::Turn,
            Self::Steer { .. } => MessageDelivery::Steer,
            Self::Queue => MessageDelivery::Queue,
        }
    }

    pub(crate) const fn starts_turn(&self) -> bool {
        matches!(self, Self::Turn | Self::Queue)
    }
}

/// One typed conversation message waiting for its delivery boundary.
///
/// Cloning shares the payload, so a staged queue snapshot costs one handle per message.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct QueuedMessage(Arc<QueuedMessageData>);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct QueuedMessageData {
    owner: String,
    id: Arc<str>,
    boundary: QueuedMessageBoundary,
    author: MessageAuthor,
    message: String,
    attachments: Vec<SessionFileReference>,
    reply: Option<MessageReply>,
}

impl QueuedMessageData {
    fn borrowed_event(&self) -> QueuedMessageEvent<'_> {
        QueuedMessageEvent {
            author: &self.author,
            delivery: self.boundary.delivery(),
            text: &self.message,
            attachments: &self.attachments,
            reply: self.reply.as_ref(),
            message_target: None,
        }
    }
}

#[derive(Serialize)]
struct QueuedMessageEvent<'a> {
    author: &'a MessageAuthor,
    delivery: MessageDelivery,
    text: &'a str,
    attachments: &'a [SessionFileReference],
    reply: Option<&'a MessageReply>,
    message_target: Option<MessageTarget>,
}

impl<'de> Deserialize<'de> for QueuedMessage {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let queued = QueuedMessageData::deserialize(deserializer)?;
        validate_queued_message(&queued).map_err(serde::de::Error::custom)?;
        Ok(Self(Arc::new(queued)))
    }
}

impl QueuedMessage {
    pub(crate) fn new(
        owner: &str,
        id: &str,
        boundary: QueuedMessageBoundary,
        event: MessageEvent,
    ) -> Result<Self> {
        let queued = Self::assemble(owner, id, boundary, event)?;
        validate_queued_message(&queued)?;
        Ok(Self(Arc::new(queued)))
    }

    /// Queues an event built from a submission already checked at ingress.
    pub(crate) fn from_validated(
        owner: &str,
        id: &str,
        boundary: QueuedMessageBoundary,
        event: MessageEvent,
    ) -> Result<Self> {
        Self::assemble(owner, id, boundary, event).map(|queued| Self(Arc::new(queued)))
    }

    fn assemble(
        owner: &str,
        id: &str,
        boundary: QueuedMessageBoundary,
        event: MessageEvent,
    ) -> Result<QueuedMessageData> {
        if event.delivery != boundary.delivery() || event.message_target.is_some() {
            return Err(Error::Config("queued message event is inconsistent".into()));
        }
        Ok(QueuedMessageData {
            owner: owner.into(),
            id: id.into(),
            boundary,
            author: event.author,
            message: event.text,
            attachments: event.attachments,
            reply: event.reply,
        })
    }

    pub(crate) fn owner(&self) -> &str {
        &self.0.owner
    }

    /// Returns the submission that owns this queued message.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.0.id
    }

    pub(crate) fn boundary(&self) -> &QueuedMessageBoundary {
        &self.0.boundary
    }

    pub(crate) fn author(&self) -> &MessageAuthor {
        &self.0.author
    }

    pub(crate) fn text(&self) -> &str {
        &self.0.message
    }

    pub(crate) fn attachments(&self) -> &[SessionFileReference] {
        &self.0.attachments
    }

    pub(crate) fn reply(&self) -> Option<&MessageReply> {
        self.0.reply.as_ref()
    }

    pub(crate) fn event(&self) -> MessageEvent {
        MessageEvent {
            author: self.0.author.clone(), // rules-allow: the queued message stays queued while its event is emitted
            delivery: self.0.boundary.delivery(),
            text: self.0.message.clone(), // rules-allow: the queued message stays queued while its event is emitted
            attachments: self.0.attachments.clone(), // rules-allow: the queued message stays queued while its event is emitted
            reply: self.0.reply.clone(), // rules-allow: the queued message stays queued while its event is emitted
            message_target: None,
        }
    }

    pub(crate) fn replace(&mut self, id: &str, event: MessageEvent) -> Result<()> {
        let boundary = self.0.boundary.clone(); // rules-allow: other queue snapshots still share this message, so its replacement owns the small boundary
        *self = Self::new(&self.0.owner, id, boundary, event)?;
        Ok(())
    }

    pub(crate) fn promote_to_next_turn(&mut self) {
        Arc::make_mut(&mut self.0).boundary = QueuedMessageBoundary::Queue;
    }

    pub(crate) fn submission_id(&self) -> &Arc<str> {
        &self.0.id
    }

    pub(crate) fn into_parts(self) -> (Arc<str>, MessageEvent) {
        let message = Arc::unwrap_or_clone(self.0);
        let event = MessageEvent {
            author: message.author,
            delivery: message.boundary.delivery(),
            text: message.message,
            attachments: message.attachments,
            reply: message.reply,
            message_target: None,
        };
        (message.id, event)
    }
}

fn validate_queued_message(message: &QueuedMessageData) -> Result<()> {
    if message.owner.trim().is_empty() || message.owner.len() > MAX_QUEUED_OWNER_BYTES {
        return Err(Error::Config("queued message owner is invalid".into()));
    }
    if message.id.trim().is_empty() || message.id.len() > MAX_QUEUED_ID_BYTES {
        return Err(Error::Config("queued message ID is invalid".into()));
    }
    if matches!(
        &message.boundary,
        QueuedMessageBoundary::Steer { turn_id }
            if turn_id.trim().is_empty() || turn_id.len() > MAX_QUEUED_TURN_ID_BYTES
    ) {
        return Err(Error::Config("queued message turn ID is invalid".into()));
    }
    crate::protocol::validate_message_content(
        &message.author,
        &message.message,
        &message.attachments,
    )?;
    if crate::serialized_len(&message.borrowed_event())
        .ok()
        .is_none_or(|bytes| bytes > crate::protocol::MAX_SERIALIZED_MESSAGE_BYTES)
    {
        return Err(Error::Config("queued message is invalid".into()));
    }
    Ok(())
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    #[test]
    fn borrowed_queue_validation_matches_the_owned_event_encoding() {
        for reply in [
            None,
            Some(MessageReply {
                target: MessageTarget {
                    checkpoint_sequence: 1,
                    batch_item_count: std::num::NonZeroUsize::MIN,
                },
                text: "quoted \"text\" 🦀".into(),
            }),
        ] {
            let event = MessageEvent {
                author: MessageAuthor::User,
                delivery: MessageDelivery::Queue,
                text: "message\n\"🦀\"".into(),
                attachments: vec![SessionFileReference {
                    id: "00000000-0000-0000-0000-000000000001".into(),
                    name: "image.png".into(),
                    media_type: "image/png".into(),
                    size: 42,
                }],
                reply,
                message_target: None,
            };
            let queued = QueuedMessage::new("messages", "id", QueuedMessageBoundary::Queue, event)
                .expect("queued");
            let wire = serde_json::to_value(&queued).expect("queued encoding");
            assert_eq!(wire["owner"], "messages");
            assert_eq!(wire["message"], queued.text());
            let mut invalid = wire;
            invalid["id"] = serde_json::json!("");
            assert!(serde_json::from_value::<QueuedMessage>(invalid).is_err());
            let owned = serde_json::to_vec(&queued.event()).expect("event");
            assert_eq!(
                serde_json::to_vec(&queued.0.borrowed_event()).expect("borrowed"),
                owned
            );
            assert_eq!(
                crate::serialized_len(&queued.0.borrowed_event()).expect("serialized length"),
                owned.len()
            );
        }
    }
}
