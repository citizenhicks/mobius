use std::collections::BTreeMap;
use std::sync::Arc;

use super::*;
use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
use crate::backend::checkpoint::{Checkpoint, QueuedMessage};
use crate::middleware::{ActiveCommandContext, MessageQueue, MessageRouteContext, MiddlewareStack};
use crate::protocol::{MessageReply, MessageSubmission, MessageTarget, SessionFileReference};

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

async fn route(
    stack: &MiddlewareStack,
    queued: &mut Vec<QueuedMessage>,
    message: &MessageSubmission,
    active_turn_id: Option<&str>,
) -> SubmissionResult {
    let (_directory, store) = SqliteCheckpoint::temporary();
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
            session_id: &"session-1".parse().expect("session ID"),
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
        batch_item_count: std::num::NonZeroUsize::new(2).unwrap(),
    };
    message.reply = Some(MessageReply {
        target,
        text: "Earlier".into(),
    });
    let (_directory, checkpoints) = SqliteCheckpoint::temporary();
    let session = crate::backend::checkpoint::Checkpoint::for_test_owner("session-1", "test-bot");
    checkpoints
        .save(session, Vec::new(), None)
        .await
        .expect("save session");
    checkpoints
        .append_event(
            &"session-1".parse().expect("session ID"),
            1,
            event(
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
                    session_id: &"session-1".parse().expect("session ID"),
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
    let stack = MiddlewareStack::new(vec![Arc::new(Messages::new(
        std::num::NonZeroUsize::new(4).expect("positive"),
        ActiveMessageDelivery::Queue,
    ))])
    .expect("stack");
    let peer = MessageSubmission {
        author: MessageAuthor::Source {
            message_id: "board-1".into(),
            source: crate::protocol::MessageSource::Session {
                session_id: "peer-1".parse().expect("session ID"),
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
    let (_directory, store) = SqliteCheckpoint::temporary();
    for session_id in ["current", "other"] {
        let checkpoint = Checkpoint::for_test_owner(session_id, "test-bot");
        store
            .save(checkpoint, Vec::new(), None)
            .await
            .expect("save session");
    }
    let attachment_target = crate::protocol::MessageTarget {
        checkpoint_sequence: 1,
        batch_item_count: std::num::NonZeroUsize::MIN,
    };
    store
        .append_event(
            &"current".parse().expect("session ID"),
            1,
            event(
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
        batch_item_count: std::num::NonZeroUsize::new(2).unwrap(),
    };
    store
        .append_event(
            &"current".parse().expect("session ID"),
            2,
            event(
                "assistant",
                EventMsg::AssistantMessage(crate::protocol::AssistantMessageEvent {
                    session_id: "current".parse().expect("session ID"),
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
        batch_item_count: std::num::NonZeroUsize::MIN,
    };
    store
        .append_event(
            &"other".parse().expect("session ID"),
            1,
            event(
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
            validate_reply(&store, &"current".parse().expect("session ID"), &exact)
                .await
                .expect("validate exact reply"),
            validate_reply(&store, &"current".parse().expect("session ID"), &changed)
                .await
                .expect("validate changed reply"),
            validate_reply(&store, &"current".parse().expect("session ID"), &other)
                .await
                .expect("validate cross-session reply"),
            validate_reply(&store, &"current".parse().expect("session ID"), &assistant)
                .await
                .expect("validate assistant reply"),
            validate_reply(
                &store,
                &"current".parse().expect("session ID"),
                &combined_assistant
            )
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
            session_id: &"current".parse().expect("session ID"),
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
