use std::future::Future;

use crate::Error;
use crate::Result;
use crate::backend::checkpoint::{EventPageRequest, QueuedMessage};
use crate::middleware::ActiveCommandContext;
use crate::middleware::MessageQueue;
use crate::middleware::MessageRouteContext;
use crate::middleware::MiddlewareStack;
use crate::middleware::SubmissionResult;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::MessageReply;
use crate::protocol::MessageSubmission;
use crate::protocol::Op;
use crate::protocol::ReviewDecision;
use crate::protocol::Submission;
use crate::protocol::SubmissionRejectedEvent;
use crate::protocol::WarningEvent;

use super::EventRecorder;
use super::Runner;
use super::SubmissionInbox;
use super::send_event;

const REPLY_EVENT_PAGE_SIZE: usize = 256;
const INVALID_REPLY_TARGET: &str = "reply target is not a safe durable message in this chat";
const CHANGED_REPLY_TEXT: &str = "quoted message does not match its durable original";

pub(super) enum Wait<T> {
    Ready { value: T, input_changed: bool },
    Interrupted { submission_id: String },
}

pub(super) enum ActiveRoute {
    Continue {
        input_changed: bool,
    },
    Interrupted {
        submission_id: String,
    },
    Approval {
        submission_id: String,
        decision: ReviewDecision,
    },
}

struct ActiveChange {
    submission_id: String,
    pending_messages: Vec<QueuedMessage>,
    events: Vec<EventMsg>,
    input_changed: bool,
}

enum UncommittedRoute {
    Continue,
    Changed(ActiveChange),
    Interrupted {
        submission_id: String,
    },
    Approval {
        submission_id: String,
        decision: ReviewDecision,
    },
}

struct ActiveTurnRouter<'a> {
    pub middleware: &'a MiddlewareStack,
    pub session_id: &'a str,
    pub metadata: &'a std::collections::BTreeMap<String, serde_json::Value>,
    pub turn_id: &'a str,
    pub queued_messages: &'a mut Vec<QueuedMessage>,
    pub events: &'a EventRecorder,
    pub expected_approval: Option<&'a str>,
}

impl Runner {
    pub(super) async fn route_idle_message(
        &mut self,
        submission_id: String,
        message: MessageSubmission,
    ) -> Result<()> {
        if let Some(message) = validate_message_reply(
            self.config.checkpoints.as_ref(),
            &self.config.session_id,
            message.reply.as_ref(),
        )
        .await?
        {
            reject(&self.events, submission_id, message).await?;
            return Ok(());
        }
        let mut pending_messages = self.state.pending_messages.clone();
        let mut messages = Vec::new();
        let result = self
            .config
            .middleware
            .route_message(&mut MessageRouteContext {
                submission_id: &submission_id,
                message: &message,
                active_turn_id: None,
                queued_messages: MessageQueue::new(&mut pending_messages),
                events: &mut messages,
            })?;
        if let Some(change) = route_submission_result(
            &self.events,
            submission_id,
            result,
            pending_messages,
            messages,
        )
        .await?
        {
            self.persist_submission_change(change).await?;
        }
        Ok(())
    }

    pub(super) async fn wait_active<F, T>(
        &mut self,
        inbox: &mut SubmissionInbox,
        turn_id: &str,
        future: F,
    ) -> Result<Wait<T>>
    where
        F: Future<Output = T>,
    {
        tokio::pin!(future);
        let mut input_changed = false;
        loop {
            tokio::select! {
                biased;
                submission = inbox.recv() => {
                    let Some(submission) = submission else {
                        return Err(Error::Stopped("frontend disconnected".into()));
                    };
                    match self.route_active_submission(submission, turn_id, None).await? {
                        ActiveRoute::Interrupted { submission_id } => {
                            return Ok(Wait::Interrupted { submission_id });
                        }
                        ActiveRoute::Continue { input_changed: changed } => {
                            input_changed |= changed;
                        }
                        ActiveRoute::Approval { .. } => {}
                    }
                }
                value = &mut future => return Ok(Wait::Ready { value, input_changed }),
            }
        }
    }

    async fn persist_submission_change(&mut self, change: ActiveChange) -> Result<()> {
        let ActiveChange {
            submission_id,
            pending_messages,
            events,
            ..
        } = change;
        let events = events
            .into_iter()
            .map(|msg| Event {
                submission_id: Some(submission_id.clone()),
                msg,
            })
            .collect();
        let previous = std::mem::replace(&mut self.state.pending_messages, pending_messages);
        match self.persist_with_events(events, None).await {
            Ok(_) => Ok(()),
            Err(error) => {
                self.state.pending_messages = previous;
                Err(error)
            }
        }
    }

    pub(super) async fn route_active_submission(
        &mut self,
        submission: Submission,
        turn_id: &str,
        expected_approval: Option<&str>,
    ) -> Result<ActiveRoute> {
        if let Op::Message { message } = &submission.op
            && let Some(rejection) = validate_message_reply(
                self.config.checkpoints.as_ref(),
                &self.config.session_id,
                message.reply.as_ref(),
            )
            .await?
        {
            reject(&self.events, submission.id, rejection).await?;
            return Ok(ActiveRoute::Continue {
                input_changed: false,
            });
        }
        let route = (ActiveTurnRouter {
            middleware: &self.config.middleware,
            session_id: &self.config.session_id,
            metadata: &self.config.metadata,
            turn_id,
            queued_messages: &mut self.state.pending_messages,
            events: &self.events,
            expected_approval,
        })
        .route(submission)
        .await?;
        match route {
            UncommittedRoute::Changed(change) => {
                let input_changed = change.input_changed;
                self.persist_submission_change(change).await?;
                Ok(ActiveRoute::Continue { input_changed })
            }
            UncommittedRoute::Continue => Ok(ActiveRoute::Continue {
                input_changed: false,
            }),
            UncommittedRoute::Interrupted { submission_id } => {
                Ok(ActiveRoute::Interrupted { submission_id })
            }
            UncommittedRoute::Approval {
                submission_id,
                decision,
            } => Ok(ActiveRoute::Approval {
                submission_id,
                decision,
            }),
        }
    }
}

async fn validate_message_reply(
    checkpoints: &dyn crate::backend::checkpoint::CheckpointStore,
    session_id: &str,
    reply: Option<&MessageReply>,
) -> Result<Option<&'static str>> {
    let Some(reply) = reply else {
        return Ok(None);
    };
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
                    Some(original) if original == reply.text => None,
                    Some(_) => Some(CHANGED_REPLY_TEXT),
                    None => Some(INVALID_REPLY_TARGET),
                });
            }
        }
        let Some(next) = page.next_before_sequence else {
            return Ok(Some(INVALID_REPLY_TARGET));
        };
        before_sequence = Some(next);
    }
}

impl ActiveTurnRouter<'_> {
    async fn route(&mut self, submission: Submission) -> Result<UncommittedRoute> {
        let Submission { id, op } = submission;
        match op {
            Op::Message { message } => {
                let mut pending_messages = self.queued_messages.clone();
                let mut messages = Vec::new();
                let result = self.middleware.route_message(&mut MessageRouteContext {
                    submission_id: &id,
                    message: &message,
                    active_turn_id: Some(self.turn_id),
                    queued_messages: MessageQueue::new(&mut pending_messages),
                    events: &mut messages,
                })?;
                Ok(
                    route_submission_result(self.events, id, result, pending_messages, messages)
                        .await?
                        .map_or(UncommittedRoute::Continue, UncommittedRoute::Changed),
                )
            }
            Op::Interrupt { turn_id } if turn_id == self.turn_id => {
                Ok(UncommittedRoute::Interrupted { submission_id: id })
            }
            Op::Interrupt { .. } => {
                warn(self.events, id, "interrupt targeted a stale turn").await?;
                Ok(UncommittedRoute::Continue)
            }
            Op::ExecApproval {
                id: approval_id,
                decision,
            } if self.expected_approval == Some(approval_id.as_str()) => {
                Ok(UncommittedRoute::Approval {
                    submission_id: id,
                    decision,
                })
            }
            Op::ExecApproval { .. } => {
                warn(
                    self.events,
                    id,
                    "approval response targeted a stale request",
                )
                .await?;
                Ok(UncommittedRoute::Continue)
            }
            Op::CapabilityCommand {
                capability,
                command,
                arguments,
                input,
                target,
            } => {
                let mut pending_messages = self.queued_messages.clone();
                let mut messages = Vec::new();
                let result = self
                    .middleware
                    .active_command(
                        &capability,
                        &mut ActiveCommandContext {
                            submission_id: &id,
                            session_id: self.session_id,
                            metadata: self.metadata,
                            active_turn_id: self.turn_id,
                            command: &command,
                            arguments: &arguments,
                            input: input.as_deref(),
                            target,
                            queued_messages: MessageQueue::new(&mut pending_messages),
                            events: &mut messages,
                        },
                    )
                    .await?;
                let Some(result) = result else {
                    warn(
                        self.events,
                        id,
                        "command is unavailable during an active turn",
                    )
                    .await?;
                    return Ok(UncommittedRoute::Continue);
                };
                Ok(
                    route_submission_result(self.events, id, result, pending_messages, messages)
                        .await?
                        .map_or(UncommittedRoute::Continue, UncommittedRoute::Changed),
                )
            }
            Op::SetModel { .. } | Op::ResumeSession { .. } => {
                warn(
                    self.events,
                    id,
                    "operation is unavailable during an active turn",
                )
                .await?;
                Ok(UncommittedRoute::Continue)
            }
        }
    }
}

async fn route_submission_result(
    events: &EventRecorder,
    submission_id: String,
    result: SubmissionResult,
    pending_messages: Vec<QueuedMessage>,
    messages: Vec<EventMsg>,
) -> Result<Option<ActiveChange>> {
    match result {
        SubmissionResult::Accepted { input_changed } => Ok(Some(ActiveChange {
            submission_id,
            pending_messages,
            events: messages,
            input_changed,
        })),
        SubmissionResult::Handled => {
            send_messages(events, &submission_id, messages).await?;
            Ok(None)
        }
        SubmissionResult::Rejected(message) => {
            send_messages(events, &submission_id, messages).await?;
            send_event(
                events,
                Event {
                    submission_id: Some(submission_id),
                    msg: EventMsg::SubmissionRejected(SubmissionRejectedEvent { message }),
                },
            )
            .await?;
            Ok(None)
        }
    }
}

async fn send_messages(
    events: &EventRecorder,
    submission_id: &str,
    messages: Vec<EventMsg>,
) -> Result<()> {
    for msg in messages {
        send_event(
            events,
            Event {
                submission_id: Some(submission_id.to_string()),
                msg,
            },
        )
        .await?;
    }
    Ok(())
}

async fn warn(events: &EventRecorder, id: String, message: &str) -> Result<()> {
    send_event(
        events,
        Event {
            submission_id: Some(id),
            msg: EventMsg::Warning(WarningEvent {
                message: message.into(),
            }),
        },
    )
    .await
}

async fn reject(events: &EventRecorder, id: String, message: &str) -> Result<()> {
    send_event(
        events,
        Event {
            submission_id: Some(id),
            msg: EventMsg::SubmissionRejected(SubmissionRejectedEvent {
                message: message.into(),
            }),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::mpsc;

    use super::*;
    use crate::backend::checkpoint::Checkpoint;
    use crate::backend::checkpoint::CheckpointStore;
    use crate::backend::checkpoint::JournalEvent;
    use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
    use crate::middleware::Middleware;

    struct EditableMiddleware;

    #[derive(Clone, Copy)]
    enum MutatingRoute {
        Handled,
        Rejected,
        Failed,
    }

    struct MutatingMessageMiddleware(MutatingRoute);

    async fn event_recorder() -> (
        tempfile::TempDir,
        EventRecorder,
        mpsc::Receiver<JournalEvent>,
    ) {
        let directory = tempfile::tempdir().expect("checkpoint directory");
        let checkpoints = Arc::new(
            SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
                .expect("checkpoint store"),
        );
        let mut checkpoint = Checkpoint::empty("session-1");
        checkpoint.session_context.bot_id = "test-bot".into();
        checkpoints
            .save(&checkpoint, &[], None)
            .await
            .expect("initial checkpoint");
        let (recorder, events) = EventRecorder::spawn(checkpoints, "session-1".into());
        (directory, recorder, events)
    }

    #[tokio::test]
    async fn reply_matches_a_durable_message_in_its_own_session() {
        let directory = tempfile::tempdir().expect("checkpoint directory");
        let store = SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store");
        for session_id in ["current", "other"] {
            let mut checkpoint = Checkpoint::empty(session_id);
            checkpoint.session_context.bot_id = "test-bot".into();
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
                &Event {
                    submission_id: Some("attachment".into()),
                    msg: EventMsg::Message(crate::protocol::MessageEvent {
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
                },
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
                &Event {
                    submission_id: Some("assistant".into()),
                    msg: EventMsg::AssistantMessage(crate::protocol::AssistantMessageEvent {
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
                },
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
                &Event {
                    submission_id: Some("other".into()),
                    msg: EventMsg::Message(crate::protocol::MessageEvent {
                        author: crate::protocol::MessageAuthor::User,
                        delivery: crate::protocol::MessageDelivery::Turn,
                        text: "other chat".into(),
                        attachments: Vec::new(),
                        reply: None,
                        message_target: Some(other_target),
                    }),
                },
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
                validate_message_reply(&store, "current", Some(&exact))
                    .await
                    .expect("validate exact reply"),
                validate_message_reply(&store, "current", Some(&changed))
                    .await
                    .expect("validate changed reply"),
                validate_message_reply(&store, "current", Some(&other))
                    .await
                    .expect("validate cross-session reply"),
                validate_message_reply(&store, "current", Some(&assistant))
                    .await
                    .expect("validate assistant reply"),
                validate_message_reply(&store, "current", Some(&combined_assistant))
                    .await
                    .expect("validate combined assistant reply"),
            ),
            (
                None,
                Some(CHANGED_REPLY_TEXT),
                Some(INVALID_REPLY_TARGET),
                None,
                Some(CHANGED_REPLY_TEXT),
            )
        );
    }

    impl Middleware for EditableMiddleware {
        fn name(&self) -> &'static str {
            "editable"
        }

        fn active_command<'a>(
            &'a self,
            context: &'a mut ActiveCommandContext<'_>,
        ) -> crate::BoxFuture<'a, Result<Option<SubmissionResult>>> {
            Box::pin(async move {
                if context.command == "queue" {
                    context.queued_messages.enqueue(
                        context.submission_id,
                        crate::backend::checkpoint::QueuedMessageBoundary::Queue,
                        crate::protocol::MessageEvent {
                            author: crate::protocol::MessageAuthor::User,
                            delivery: crate::protocol::MessageDelivery::Queue,
                            text: "queued".into(),
                            attachments: Vec::new(),
                            reply: None,
                            message_target: None,
                        },
                    )?;
                    return Ok(Some(SubmissionResult::Accepted {
                        input_changed: false,
                    }));
                }
                if context.command == "preview" {
                    context.events.push(EventMsg::Frontend(
                        crate::protocol::FrontendEvent::Preview {
                            id: "preview".into(),
                            title: "preview".into(),
                            subtitle: String::new(),
                            page_id: "preview:latest".into(),
                            update: crate::protocol::FrontendPreviewUpdate::Replace,
                            events: Vec::new(),
                            next: None,
                        },
                    ));
                    return Ok(Some(SubmissionResult::Handled));
                }
                if context.command != "edit" {
                    return Ok(None);
                }
                assert_eq!(context.active_turn_id, "turn-1");
                assert_eq!(
                    context.target,
                    Some(crate::protocol::MessageTarget {
                        checkpoint_sequence: 7,
                        batch_item_count: 2,
                    })
                );
                if context.input.is_none() {
                    return Ok(Some(SubmissionResult::Rejected(
                        "edit requires text".into(),
                    )));
                }
                context.events.push(EventMsg::ContextCompacted);
                Ok(Some(SubmissionResult::Accepted {
                    input_changed: false,
                }))
            })
        }
    }

    impl Middleware for MutatingMessageMiddleware {
        fn name(&self) -> &'static str {
            "mutating_message"
        }

        fn handles_messages(&self) -> bool {
            true
        }

        fn route_message(
            &self,
            context: &mut crate::middleware::MessageRouteContext<'_>,
        ) -> Result<SubmissionResult> {
            context.queued_messages.enqueue(
                context.submission_id,
                crate::backend::checkpoint::QueuedMessageBoundary::Turn,
                crate::protocol::MessageEvent {
                    author: crate::protocol::MessageAuthor::User,
                    delivery: crate::protocol::MessageDelivery::Turn,
                    text: context.message.text.clone(),
                    attachments: Vec::new(),
                    reply: None,
                    message_target: None,
                },
            )?;
            match self.0 {
                MutatingRoute::Handled => Ok(SubmissionResult::Handled),
                MutatingRoute::Rejected => Ok(SubmissionResult::Rejected("rejected".into())),
                MutatingRoute::Failed => Err(Error::Config("failed".into())),
            }
        }
    }

    #[tokio::test]
    async fn active_capability_command_changes_state_without_signaling_new_input() {
        let middleware =
            MiddlewareStack::new(vec![Arc::new(EditableMiddleware)]).expect("middleware stack");
        let mut queued = Vec::new();
        let (_directory, events, _receiver) = event_recorder().await;

        let route = (ActiveTurnRouter {
            middleware: &middleware,
            session_id: "session-1",
            metadata: &std::collections::BTreeMap::new(),
            turn_id: "turn-1",
            queued_messages: &mut queued,
            events: &events,
            expected_approval: None,
        })
        .route(Submission {
            id: "edit-1".into(),
            op: Op::CapabilityCommand {
                capability: "editable".into(),
                command: "edit".into(),
                arguments: "message-1".into(),
                input: Some("edited".into()),
                target: Some(crate::protocol::MessageTarget {
                    checkpoint_sequence: 7,
                    batch_item_count: 2,
                }),
            },
        })
        .await
        .expect("route command");

        assert!(matches!(route, UncommittedRoute::Changed(_)));
        assert!(queued.is_empty());
    }

    #[tokio::test]
    async fn handled_active_command_publishes_immediately() {
        let middleware =
            MiddlewareStack::new(vec![Arc::new(EditableMiddleware)]).expect("middleware stack");
        let mut queued = Vec::new();
        let (_directory, events, mut receiver) = event_recorder().await;

        let route = (ActiveTurnRouter {
            middleware: &middleware,
            session_id: "session-1",
            metadata: &std::collections::BTreeMap::new(),
            turn_id: "turn-1",
            queued_messages: &mut queued,
            events: &events,
            expected_approval: None,
        })
        .route(Submission {
            id: "preview-1".into(),
            op: Op::CapabilityCommand {
                capability: "editable".into(),
                command: "preview".into(),
                arguments: String::new(),
                input: None,
                target: None,
            },
        })
        .await
        .expect("route command");
        let event = receiver.recv().await.expect("preview event").event;

        assert!(matches!(route, UncommittedRoute::Continue));
        assert!(matches!(
            event,
            Event {
                submission_id: Some(id),
                msg: EventMsg::Frontend(crate::protocol::FrontendEvent::Preview { title, .. }),
            } if id == "preview-1" && title == "preview"
        ));
    }

    #[tokio::test]
    async fn unavailable_active_capability_command_is_rejected_immediately() {
        let middleware =
            MiddlewareStack::new(vec![Arc::new(EditableMiddleware)]).expect("middleware stack");
        let mut queued = Vec::new();
        let (_directory, events, mut receiver) = event_recorder().await;
        let submission = Submission {
            id: "command-1".into(),
            op: Op::CapabilityCommand {
                capability: "editable".into(),
                command: "refresh".into(),
                arguments: String::new(),
                input: None,
                target: None,
            },
        };

        let route = (ActiveTurnRouter {
            middleware: &middleware,
            session_id: "session-1",
            metadata: &std::collections::BTreeMap::new(),
            turn_id: "turn-1",
            queued_messages: &mut queued,
            events: &events,
            expected_approval: None,
        })
        .route(submission.clone())
        .await
        .expect("route command");

        assert!(matches!(route, UncommittedRoute::Continue));
        assert!(matches!(
            receiver.recv().await.expect("warning").event,
            Event {
                submission_id: Some(id),
                msg: EventMsg::Warning(WarningEvent { message }),
            } if id == submission.id && message == "command is unavailable during an active turn"
        ));
    }

    #[tokio::test]
    async fn non_message_capability_cannot_enqueue_conversation_messages() {
        let middleware =
            MiddlewareStack::new(vec![Arc::new(EditableMiddleware)]).expect("middleware stack");
        let mut queued = Vec::new();
        let (_directory, events, _receiver) = event_recorder().await;

        let result = (ActiveTurnRouter {
            middleware: &middleware,
            session_id: "session-1",
            metadata: &std::collections::BTreeMap::new(),
            turn_id: "turn-1",
            queued_messages: &mut queued,
            events: &events,
            expected_approval: None,
        })
        .route(Submission {
            id: "queue-1".into(),
            op: Op::CapabilityCommand {
                capability: "editable".into(),
                command: "queue".into(),
                arguments: String::new(),
                input: None,
                target: None,
            },
        })
        .await;

        assert!(matches!(result, Err(Error::Config(_))));
        assert!(queued.is_empty());
    }

    #[tokio::test]
    async fn unaccepted_message_routes_cannot_mutate_the_durable_queue() {
        for outcome in [
            MutatingRoute::Handled,
            MutatingRoute::Rejected,
            MutatingRoute::Failed,
        ] {
            let middleware =
                MiddlewareStack::new(vec![Arc::new(MutatingMessageMiddleware(outcome))])
                    .expect("middleware stack");
            let mut queued = Vec::new();
            let (_directory, events, mut receiver) = event_recorder().await;
            let result = (ActiveTurnRouter {
                middleware: &middleware,
                session_id: "session-1",
                metadata: &std::collections::BTreeMap::new(),
                turn_id: "turn-1",
                queued_messages: &mut queued,
                events: &events,
                expected_approval: None,
            })
            .route(Submission {
                id: "message-1".into(),
                op: Op::Message {
                    message: crate::protocol::MessageSubmission {
                        author: crate::protocol::MessageAuthor::User,
                        text: "hello".into(),
                        attachments: Vec::new(),
                        reply: None,
                        requested_delivery: None,
                        target_turn_id: None,
                    },
                },
            })
            .await;

            assert!(queued.is_empty());
            assert_eq!(result.is_err(), matches!(outcome, MutatingRoute::Failed));
            if matches!(outcome, MutatingRoute::Rejected) {
                assert!(matches!(
                    receiver.recv().await.expect("rejection event").event,
                    Event {
                        submission_id: Some(id),
                        msg: EventMsg::SubmissionRejected(SubmissionRejectedEvent { message }),
                    } if id == "message-1" && message == "rejected"
                ));
            }
        }
    }
}
