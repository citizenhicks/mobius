use std::future::Future;

use crate::Error;
use crate::Result;
use crate::backend::checkpoint::QueuedMessage;
use crate::middleware::ActiveCommandContext;
use crate::middleware::MessageQueue;
use crate::middleware::MessageRouteContext;
use crate::middleware::MiddlewareStack;
use crate::middleware::SubmissionResult;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::MessageSubmission;
use crate::protocol::Op;
use crate::protocol::ReviewDecision;
use crate::protocol::Submission;
use crate::protocol::SubmissionRejectedEvent;
use crate::protocol::WarningEvent;

use super::EventRecorder;
use super::FRONTEND_DISCONNECTED_REASON;
use super::Runner;
use super::SubmissionInbox;
use super::send_event;
use super::{AdmissionReply, MessageAcceptance, ReceivedSubmission};

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
    Rejected(String),
    Changed(ActiveChange),
    Interrupted {
        submission_id: String,
    },
    Approval {
        submission_id: String,
        decision: ReviewDecision,
    },
}

enum AdmissionOutcome {
    Accepted,
    Rejected(String),
}

struct ActiveTurnRouter<'a> {
    pub checkpoints: &'a dyn crate::backend::checkpoint::CheckpointStore,
    pub middleware: &'a MiddlewareStack,
    pub session_id: &'a str,
    pub metadata: &'a std::collections::BTreeMap<String, serde_json::Value>,
    pub turn_id: &'a str,
    pub queued_messages: &'a mut Vec<QueuedMessage>,
    pub events: &'a EventRecorder,
    pub expected_approval: Option<&'a str>,
}

impl Runner {
    pub(super) async fn admit_idle_message(
        &mut self,
        submission_id: String,
        message: MessageSubmission,
        admission: Option<AdmissionReply>,
    ) -> Result<()> {
        if self.message_accepted(&submission_id).await? {
            if let Some(reply) = admission {
                let _ = reply.send(Ok(MessageAcceptance::AlreadyAccepted));
            }
            return Ok(());
        }
        let result = self.route_idle_message(submission_id, message).await;
        finish_admission(admission, result.as_ref());
        result.map(|_| ())
    }

    async fn message_accepted(&self, submission_id: &str) -> Result<bool> {
        self.config
            .checkpoints
            .message_accepted(&self.config.session_id, submission_id)
            .await
    }

    async fn route_idle_message(
        &mut self,
        submission_id: String,
        mut message: MessageSubmission,
    ) -> Result<AdmissionOutcome> {
        // Admission hooks stage edits; rejection or publication failure must retain the live queue.
        let mut pending_messages = self.state.pending_messages.clone();
        let mut messages = Vec::new();
        let result = self
            .config
            .middleware
            .route_message(&mut MessageRouteContext {
                checkpoints: self.config.checkpoints.as_ref(),
                session_id: &self.config.session_id,
                submission_id: &submission_id,
                message: &mut message,
                active_turn_id: None,
                queued_messages: MessageQueue::new(&mut pending_messages),
                events: &mut messages,
            })
            .await?;
        match route_submission_result(
            &self.events,
            submission_id,
            result,
            pending_messages,
            messages,
        )
        .await?
        {
            UncommittedRoute::Changed(change) => {
                self.persist_submission_change(change).await?;
                Ok(AdmissionOutcome::Accepted)
            }
            UncommittedRoute::Rejected(message) => Ok(AdmissionOutcome::Rejected(message)),
            _ => Ok(AdmissionOutcome::Rejected(
                "message was handled without durable admission".into(),
            )),
        }
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
            let drained = self.drain_submissions(inbox, turn_id).await?;
            input_changed |= drained.input_changed;
            if let Some(submission_id) = drained.interrupted {
                return Ok(Wait::Interrupted { submission_id });
            }
            tokio::select! {
                biased;
                value = &mut future => {
                    let drained = self.drain_submissions(inbox, turn_id).await?;
                    input_changed |= drained.input_changed;
                    if let Some(submission_id) = drained.interrupted {
                        return Ok(Wait::Interrupted { submission_id });
                    }
                    return Ok(Wait::Ready { value, input_changed });
                }
                submission = inbox.recv() => {
                    let Some(submission) = submission else {
                        return Err(Error::Stopped(FRONTEND_DISCONNECTED_REASON.into()));
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
            .map(|msg| crate::agent::turn::turn_event(&submission_id, msg))
            .collect();
        let previous = std::mem::replace(
            &mut self.state.make_mut().pending_messages,
            pending_messages,
        );
        match self.persist_with_events(events, None).await {
            Ok(_) => Ok(()),
            Err(error) => {
                self.state.make_mut().pending_messages = previous;
                Err(error)
            }
        }
    }

    pub(super) async fn route_active_submission(
        &mut self,
        received: ReceivedSubmission,
        turn_id: &str,
        expected_approval: Option<&str>,
    ) -> Result<ActiveRoute> {
        let ReceivedSubmission {
            submission,
            admission,
        } = received;
        if matches!(submission.op, Op::Message { .. })
            && self.message_accepted(&submission.id).await?
        {
            if let Some(reply) = admission {
                let _ = reply.send(Ok(MessageAcceptance::AlreadyAccepted));
            }
            return Ok(ActiveRoute::Continue {
                input_changed: false,
            });
        }
        let result = self
            .route_active_submission_inner(submission, turn_id, expected_approval)
            .await;
        finish_admission(admission, result.as_ref().map(|(_, outcome)| outcome));
        result.map(|(route, _)| route)
    }

    async fn route_active_submission_inner(
        &mut self,
        submission: Submission,
        turn_id: &str,
        expected_approval: Option<&str>,
    ) -> Result<(ActiveRoute, AdmissionOutcome)> {
        let route = (ActiveTurnRouter {
            checkpoints: self.config.checkpoints.as_ref(),
            middleware: &self.config.middleware,
            session_id: &self.config.session_id,
            metadata: &self.config.metadata,
            turn_id,
            queued_messages: &mut self.state.make_mut().pending_messages,
            events: &self.events,
            expected_approval,
        })
        .route(submission)
        .await?;
        match route {
            UncommittedRoute::Changed(change) => {
                let input_changed = change.input_changed;
                self.persist_submission_change(change).await?;
                Ok((
                    ActiveRoute::Continue { input_changed },
                    AdmissionOutcome::Accepted,
                ))
            }
            UncommittedRoute::Continue => Ok((
                ActiveRoute::Continue {
                    input_changed: false,
                },
                AdmissionOutcome::Rejected("message was handled without durable admission".into()),
            )),
            UncommittedRoute::Rejected(message) => Ok((
                ActiveRoute::Continue {
                    input_changed: false,
                },
                AdmissionOutcome::Rejected(message),
            )),
            UncommittedRoute::Interrupted { submission_id } => Ok((
                ActiveRoute::Interrupted { submission_id },
                AdmissionOutcome::Rejected("operation is not a message".into()),
            )),
            UncommittedRoute::Approval {
                submission_id,
                decision,
            } => Ok((
                ActiveRoute::Approval {
                    submission_id,
                    decision,
                },
                AdmissionOutcome::Rejected("operation is not a message".into()),
            )),
        }
    }
}

fn finish_admission(
    admission: Option<AdmissionReply>,
    result: std::result::Result<&AdmissionOutcome, &Error>,
) {
    let Some(reply) = admission else {
        return;
    };
    let accepted = match result {
        Ok(AdmissionOutcome::Accepted) => Ok(MessageAcceptance::Accepted),
        Ok(AdmissionOutcome::Rejected(message)) => Err(Error::Config(message.clone())),
        Err(error) => Err(Error::Checkpoint(error.to_string())),
    };
    let _ = reply.send(accepted);
}

impl ActiveTurnRouter<'_> {
    async fn route(&mut self, submission: Submission) -> Result<UncommittedRoute> {
        let Submission { id, op } = submission;
        match op {
            Op::Message { mut message } => {
                // Stage hook mutations until the accepted route is durably committed.
                let mut pending_messages = self.queued_messages.clone();
                let mut messages = Vec::new();
                let result = self
                    .middleware
                    .route_message(&mut MessageRouteContext {
                        checkpoints: self.checkpoints,
                        session_id: self.session_id,
                        submission_id: &id,
                        message: &mut message,
                        active_turn_id: Some(self.turn_id),
                        queued_messages: MessageQueue::new(&mut pending_messages),
                        events: &mut messages,
                    })
                    .await?;
                route_submission_result(self.events, id, result, pending_messages, messages).await
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
                // Commands can fail after modifying their queue; keep those changes provisional.
                let mut pending_messages = self.queued_messages.clone();
                let mut messages = Vec::new();
                let result = self
                    .middleware
                    .active_command(
                        &capability,
                        &mut ActiveCommandContext {
                            checkpoints: self.checkpoints,
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
                route_submission_result(self.events, id, result, pending_messages, messages).await
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
) -> Result<UncommittedRoute> {
    match result {
        SubmissionResult::Accepted { input_changed } => {
            Ok(UncommittedRoute::Changed(ActiveChange {
                submission_id,
                pending_messages,
                events: messages,
                input_changed,
            }))
        }
        SubmissionResult::Handled => {
            send_messages(events, &submission_id, messages).await?;
            Ok(UncommittedRoute::Continue)
        }
        SubmissionResult::Rejected(message) => {
            send_messages(events, &submission_id, messages).await?;
            send_event(
                events,
                Event {
                    submission_id: Some(submission_id.into()),
                    msg: EventMsg::SubmissionRejected(SubmissionRejectedEvent {
                        message: message.clone(),
                    }),
                },
            )
            .await?;
            Ok(UncommittedRoute::Rejected(message))
        }
    }
}

async fn send_messages(
    events: &EventRecorder,
    submission_id: &str,
    messages: Vec<EventMsg>,
) -> Result<()> {
    for msg in messages {
        send_event(events, crate::agent::turn::turn_event(submission_id, msg)).await?;
    }
    Ok(())
}

async fn warn(events: &EventRecorder, id: String, message: &str) -> Result<()> {
    send_event(
        events,
        Event {
            submission_id: Some(id.into()),
            msg: EventMsg::Warning(WarningEvent {
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
        Arc<dyn crate::backend::checkpoint::CheckpointStore>,
        EventRecorder,
        mpsc::Receiver<JournalEvent>,
    ) {
        let directory = tempfile::tempdir().expect("checkpoint directory");
        let checkpoints: Arc<dyn crate::backend::checkpoint::CheckpointStore> = Arc::new(
            SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
                .expect("checkpoint store"),
        );
        let mut checkpoint = Checkpoint::empty("session-1");
        checkpoint.session_context.owner_id = "test-bot".into();
        checkpoints
            .save(&checkpoint, &[], None)
            .await
            .expect("initial checkpoint");
        let (recorder, events) = crate::agent::recorder::RecorderIngress::spawn(
            Arc::clone(&checkpoints),
            "session-1".into(),
        );
        (directory, checkpoints, recorder, events)
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
                            symbol: None,
                            duration_ms: None,
                            started_at_ms: None,
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

        fn route_message<'a>(
            &'a self,
            context: &'a mut crate::middleware::MessageRouteContext<'_>,
        ) -> crate::BoxFuture<'a, Result<SubmissionResult>> {
            Box::pin(async move {
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
            })
        }
    }

    #[tokio::test]
    async fn active_capability_command_changes_state_without_signaling_new_input() {
        let middleware =
            MiddlewareStack::new(vec![Arc::new(EditableMiddleware)]).expect("middleware stack");
        let mut queued = Vec::new();
        let (_directory, checkpoints, events, _receiver) = event_recorder().await;

        let route = (ActiveTurnRouter {
            middleware: &middleware,
            checkpoints: checkpoints.as_ref(),
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
        let (_directory, checkpoints, events, mut receiver) = event_recorder().await;

        let route = (ActiveTurnRouter {
            middleware: &middleware,
            checkpoints: checkpoints.as_ref(),
            session_id: "session-1",
            metadata: &std::collections::BTreeMap::new(),
            turn_id: "turn-1",
            queued_messages: &mut queued,
            events: &events,
            expected_approval: None,
        })
        .route(Submission {
            id: "preview-1".into(),
            op: Op::command("editable", "preview", String::new()),
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
            } if id.as_ref() == "preview-1" && title == "preview"
        ));
    }

    #[tokio::test]
    async fn unavailable_active_capability_command_is_rejected_immediately() {
        let middleware =
            MiddlewareStack::new(vec![Arc::new(EditableMiddleware)]).expect("middleware stack");
        let mut queued = Vec::new();
        let (_directory, checkpoints, events, mut receiver) = event_recorder().await;
        let submission = Submission {
            id: "command-1".into(),
            op: Op::command("editable", "refresh", String::new()),
        };

        let route = (ActiveTurnRouter {
            middleware: &middleware,
            checkpoints: checkpoints.as_ref(),
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
            } if id.as_ref() == submission.id && message == "command is unavailable during an active turn"
        ));
    }

    #[tokio::test]
    async fn non_message_capability_cannot_enqueue_conversation_messages() {
        let middleware =
            MiddlewareStack::new(vec![Arc::new(EditableMiddleware)]).expect("middleware stack");
        let mut queued = Vec::new();
        let (_directory, checkpoints, events, _receiver) = event_recorder().await;

        let result = (ActiveTurnRouter {
            middleware: &middleware,
            checkpoints: checkpoints.as_ref(),
            session_id: "session-1",
            metadata: &std::collections::BTreeMap::new(),
            turn_id: "turn-1",
            queued_messages: &mut queued,
            events: &events,
            expected_approval: None,
        })
        .route(Submission {
            id: "queue-1".into(),
            op: Op::command("editable", "queue", String::new()),
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
            let (_directory, checkpoints, events, mut receiver) = event_recorder().await;
            let result = (ActiveTurnRouter {
                middleware: &middleware,
                checkpoints: checkpoints.as_ref(),
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
                    } if id.as_ref() == "message-1" && message == "rejected"
                ));
            }
        }
    }
}
