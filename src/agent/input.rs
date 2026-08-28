use std::collections::VecDeque;
use std::future::Future;

use tokio::sync::mpsc;

use crate::Error;
use crate::Result;
use crate::backend::checkpoint::QueuedInput;
use crate::middleware::ActiveCommandContext;
use crate::middleware::ActiveSubmissionContext;
use crate::middleware::ActiveSubmissionResult;
use crate::middleware::MiddlewareStack;
use crate::middleware::QueuedInputBaseline;
use crate::middleware::QueuedInputQueue;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::Op;
use crate::protocol::ReviewDecision;
use crate::protocol::Submission;
use crate::protocol::WarningEvent;

use super::EventRecorder;
use super::MAX_DEFERRED_SUBMISSIONS;
use super::Runner;
use super::send_event;

pub(super) enum Wait<T> {
    Ready(T),
    Interrupted { submission_id: String },
}

pub(super) enum ActiveRoute {
    Continue,
    Accepted(ActiveChange),
    Changed(ActiveChange),
    Interrupted {
        submission_id: String,
    },
    Approval {
        submission_id: String,
        decision: ReviewDecision,
    },
}

pub(super) struct ActiveChange {
    submission_id: String,
    events: Vec<EventMsg>,
}

impl ActiveChange {
    pub(super) fn into_events(self) -> Vec<Event> {
        self.events
            .into_iter()
            .map(|msg| Event {
                submission_id: Some(self.submission_id.clone()),
                msg,
            })
            .collect()
    }
}

pub(super) struct ActiveTurnRouter<'a> {
    pub middleware: &'a MiddlewareStack,
    pub session_id: &'a str,
    pub metadata: &'a std::collections::BTreeMap<String, serde_json::Value>,
    pub turn_id: &'a str,
    pub queued_input: &'a mut Vec<QueuedInput>,
    pub queued_before: QueuedInputBaseline,
    pub deferred: &'a mut VecDeque<Submission>,
    pub events: &'a EventRecorder,
    pub expected_approval: Option<&'a str>,
}

impl Runner {
    pub(super) async fn wait_active<F, T>(
        &mut self,
        commands: &mut mpsc::Receiver<Submission>,
        turn_id: &str,
        future: F,
    ) -> Result<Wait<T>>
    where
        F: Future<Output = T>,
    {
        tokio::pin!(future);
        loop {
            tokio::select! {
                output = &mut future => return Ok(Wait::Ready(output)),
                submission = commands.recv() => {
                    let Some(submission) = submission else {
                        return Err(Error::Stopped("frontend disconnected".into()));
                    };
                    let route = (ActiveTurnRouter {
                        middleware: &self.config.middleware,
                        session_id: &self.config.session_id,
                        metadata: &self.config.metadata,
                        turn_id,
                        queued_input: &mut self.state.pending_input,
                        queued_before: QueuedInputBaseline::default(),
                        deferred: &mut self.deferred,
                        events: &self.events,
                        expected_approval: None,
                    })
                    .route(submission)
                    .await?;
                    match route {
                        ActiveRoute::Accepted(change) | ActiveRoute::Changed(change) => {
                            self.persist_active_change(change).await?;
                        }
                        ActiveRoute::Interrupted { submission_id } => {
                            return Ok(Wait::Interrupted { submission_id });
                        }
                        ActiveRoute::Continue | ActiveRoute::Approval { .. } => {}
                    }
                }
            }
        }
    }

    pub(super) async fn persist_active_change(&mut self, change: ActiveChange) -> Result<()> {
        self.persist_with_events(change.into_events(), None).await?;
        Ok(())
    }
}

impl ActiveTurnRouter<'_> {
    pub async fn route(&mut self, submission: Submission) -> Result<ActiveRoute> {
        let Submission { id, op } = submission;
        match op {
            op @ (Op::UserInput { .. } | Op::PeerInput { .. }) => {
                defer_submission(self.deferred, self.events, Submission { id, op }).await?;
                Ok(ActiveRoute::Continue)
            }
            Op::Interrupt { turn_id } if turn_id == self.turn_id => {
                Ok(ActiveRoute::Interrupted { submission_id: id })
            }
            Op::Interrupt { .. } => {
                warn(self.events, id, "interrupt targeted a stale turn").await?;
                Ok(ActiveRoute::Continue)
            }
            Op::ExecApproval {
                id: approval_id,
                decision,
            } if self.expected_approval == Some(approval_id.as_str()) => {
                Ok(ActiveRoute::Approval {
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
                Ok(ActiveRoute::Continue)
            }
            Op::CapabilityCommand {
                capability,
                command,
                arguments,
                input,
                target,
            } => {
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
                            queued_input: QueuedInputQueue::new(
                                self.queued_input,
                                self.queued_before.clone(),
                            ),
                            events: &mut messages,
                        },
                    )
                    .await?;
                let Some(result) = result else {
                    defer_submission(
                        self.deferred,
                        self.events,
                        Submission {
                            id,
                            op: Op::CapabilityCommand {
                                capability,
                                command,
                                arguments,
                                input,
                                target,
                            },
                        },
                    )
                    .await?;
                    return Ok(ActiveRoute::Continue);
                };
                match result {
                    ActiveSubmissionResult::Accepted => Ok(ActiveRoute::Changed(ActiveChange {
                        submission_id: id,
                        events: messages,
                    })),
                    ActiveSubmissionResult::Handled => {
                        send_messages(self.events, &id, messages).await?;
                        Ok(ActiveRoute::Continue)
                    }
                    ActiveSubmissionResult::Rejected(message) => {
                        send_messages(self.events, &id, messages).await?;
                        warn(self.events, id, &message).await?;
                        Ok(ActiveRoute::Continue)
                    }
                }
            }
            op @ (Op::SetModel { .. } | Op::ResumeSession { .. }) => {
                defer_submission(self.deferred, self.events, Submission { id, op }).await?;
                Ok(ActiveRoute::Continue)
            }
            Op::ActiveInput {
                operation,
                turn_id,
                text,
            } => {
                let mut messages = Vec::new();
                let result = self
                    .middleware
                    .active_submission(&mut ActiveSubmissionContext {
                        submission_id: &id,
                        operation: &operation,
                        active_turn_id: self.turn_id,
                        target_turn_id: &turn_id,
                        text: &text,
                        queued_input: QueuedInputQueue::new(
                            self.queued_input,
                            self.queued_before.clone(),
                        ),
                        events: &mut messages,
                    })?;
                match result {
                    Some(ActiveSubmissionResult::Accepted) => {
                        Ok(ActiveRoute::Accepted(ActiveChange {
                            submission_id: id,
                            events: messages,
                        }))
                    }
                    Some(ActiveSubmissionResult::Handled) => {
                        send_messages(self.events, &id, messages).await?;
                        Ok(ActiveRoute::Continue)
                    }
                    Some(ActiveSubmissionResult::Rejected(message)) => {
                        send_messages(self.events, &id, messages).await?;
                        warn(self.events, id, &message).await?;
                        Ok(ActiveRoute::Continue)
                    }
                    None => {
                        warn(
                            self.events,
                            id,
                            "active operation middleware is not installed",
                        )
                        .await?;
                        Ok(ActiveRoute::Continue)
                    }
                }
            }
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

pub(super) async fn defer_submission(
    deferred: &mut VecDeque<Submission>,
    events: &EventRecorder,
    submission: Submission,
) -> Result<()> {
    if deferred.len() >= MAX_DEFERRED_SUBMISSIONS {
        warn(events, submission.id, "deferred command queue is full").await?;
        return Ok(());
    }
    deferred.push_back(submission);
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::backend::checkpoint::Checkpoint;
    use crate::backend::checkpoint::CheckpointStore;
    use crate::backend::checkpoint::JournalEvent;
    use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
    use crate::middleware::Middleware;

    struct EditableMiddleware;

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
        checkpoints
            .save(&Checkpoint::empty("session-1"), &[], None)
            .await
            .expect("initial checkpoint");
        let (recorder, events) = EventRecorder::spawn(checkpoints, "session-1".into());
        (directory, recorder, events)
    }

    impl Middleware for EditableMiddleware {
        fn name(&self) -> &'static str {
            "editable"
        }

        fn active_command<'a>(
            &'a self,
            context: &'a mut ActiveCommandContext<'_>,
        ) -> crate::BoxFuture<'a, Result<Option<ActiveSubmissionResult>>> {
            Box::pin(async move {
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
                    return Ok(Some(ActiveSubmissionResult::Handled));
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
                    return Ok(Some(ActiveSubmissionResult::Rejected(
                        "edit requires text".into(),
                    )));
                }
                if context.queued_input.take(context.arguments)?.is_some() {
                    Ok(Some(ActiveSubmissionResult::Accepted))
                } else {
                    Ok(Some(ActiveSubmissionResult::Rejected("stale edit".into())))
                }
            })
        }
    }

    #[tokio::test]
    async fn active_capability_command_changes_state_without_signaling_new_input() {
        let middleware =
            MiddlewareStack::new(vec![Arc::new(EditableMiddleware)]).expect("middleware stack");
        let mut queued =
            vec![QueuedInput::new("editable", "message-1", "original").expect("queued input")];
        let mut deferred = VecDeque::new();
        let (_directory, events, _receiver) = event_recorder().await;

        let route = (ActiveTurnRouter {
            middleware: &middleware,
            session_id: "session-1",
            metadata: &std::collections::BTreeMap::new(),
            turn_id: "turn-1",
            queued_input: &mut queued,
            queued_before: QueuedInputBaseline::default(),
            deferred: &mut deferred,
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

        assert!(matches!(route, ActiveRoute::Changed(_)));
        assert!(queued.is_empty());
        assert!(deferred.is_empty());
    }

    #[tokio::test]
    async fn handled_active_command_publishes_without_being_deferred() {
        let middleware =
            MiddlewareStack::new(vec![Arc::new(EditableMiddleware)]).expect("middleware stack");
        let mut queued = Vec::new();
        let mut deferred = VecDeque::new();
        let (_directory, events, mut receiver) = event_recorder().await;

        let route = (ActiveTurnRouter {
            middleware: &middleware,
            session_id: "session-1",
            metadata: &std::collections::BTreeMap::new(),
            turn_id: "turn-1",
            queued_input: &mut queued,
            queued_before: QueuedInputBaseline::default(),
            deferred: &mut deferred,
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

        assert!(matches!(route, ActiveRoute::Continue));
        assert!(deferred.is_empty());
        assert!(matches!(
            event,
            Event {
                submission_id: Some(id),
                msg: EventMsg::Frontend(crate::protocol::FrontendEvent::Preview { title, .. }),
            } if id == "preview-1" && title == "preview"
        ));
    }

    #[tokio::test]
    async fn unrelated_capability_command_remains_deferred() {
        let middleware =
            MiddlewareStack::new(vec![Arc::new(EditableMiddleware)]).expect("middleware stack");
        let mut queued = Vec::new();
        let mut deferred = VecDeque::new();
        let (_directory, events, _receiver) = event_recorder().await;
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
            queued_input: &mut queued,
            queued_before: QueuedInputBaseline::default(),
            deferred: &mut deferred,
            events: &events,
            expected_approval: None,
        })
        .route(submission.clone())
        .await
        .expect("route command");

        assert!(matches!(route, ActiveRoute::Continue));
        assert_eq!(deferred, VecDeque::from([submission]));
    }
}
