mod model;

use uuid::Uuid;

use super::Runner;
use super::SubmissionInbox;
use super::input::{ActiveRoute, Wait};
use super::unix_timestamp_ms;
use crate::backend::checkpoint::{ActiveExecution, ExecutionOutcome, ExecutionPhase};
use crate::middleware::{PreparedMessage, TurnEndContext};
use crate::protocol::{
    ErrorEvent, Event, EventMsg, MessageTarget, TokenCountEvent, TokenUsageInfo, TurnAbortedEvent,
    TurnCompleteEvent, TurnStartedEvent,
};
use crate::{Error, Result};

impl Runner {
    pub(super) async fn stop_resumed_turn_at_session_start(&mut self) -> Result<()> {
        let Some(reason) = self.pending_session_start_stop.take() else {
            return Ok(());
        };
        let Some(execution) = self.state.active_execution.as_ref() else {
            return Ok(());
        };
        let submission_id = execution.submission_id.clone();
        let turn_id = execution.turn_id.clone();
        self.abort(&submission_id, &turn_id, &reason, ExecutionOutcome::Aborted)
            .await
    }

    pub(super) async fn ready_or_aborted<T>(
        &mut self,
        wait: Wait<T>,
        turn_id: &str,
    ) -> Result<Option<T>> {
        match wait {
            Wait::Ready { value, .. } => Ok(Some(value)),
            Wait::Interrupted { submission_id } => {
                self.abort(
                    &submission_id,
                    turn_id,
                    "interrupted",
                    ExecutionOutcome::Aborted,
                )
                .await?;
                Ok(None)
            }
        }
    }

    pub(super) async fn fail_turn(&mut self, submission_id: &str, error: Error) -> Result<()> {
        self.fail_turn_with_events(submission_id, error, Vec::new())
            .await
    }

    pub(super) async fn fail_turn_with_events(
        &mut self,
        submission_id: &str,
        error: Error,
        mut events: Vec<Event>,
    ) -> Result<()> {
        let Some(turn_id) = self
            .state
            .active_execution
            .as_ref()
            .map(|execution| execution.turn_id.clone())
        else {
            return Err(error);
        };
        let event = ErrorEvent::from_error(&error);
        let message = event.message.clone();
        events.push(turn_event(submission_id, EventMsg::Error(event)));
        self.abort_with_events(
            submission_id,
            &turn_id,
            &message,
            ExecutionOutcome::Failed,
            events,
        )
        .await
    }

    pub(super) async fn start_message_turn(
        &mut self,
        inbox: &mut SubmissionInbox,
        mut message: PreparedMessage,
    ) -> Result<()> {
        let submission_id = message.submission_id.clone();
        let turn_id = Uuid::new_v4().to_string();
        let mut hook_messages = Vec::new();
        let submitted = self
            .config
            .middleware
            .message_submit(
                self.runtime.turn_identity(&turn_id),
                &message,
                &mut hook_messages,
            )
            .await?;
        if let Some(rejection) = submitted.rejection {
            let mut pending_messages = self.state.pending_messages.clone();
            self.config
                .middleware
                .consume_next_turn(&mut pending_messages, &submission_id)?;
            self.begin_turn(&submission_id, turn_id.clone())?;
            let previous_pending_messages =
                std::mem::replace(&mut self.state.pending_messages, pending_messages);
            let mut events = vec![turn_event(
                &submission_id,
                EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: turn_id.clone(),
                    model_context_window: Some(self.config.context_window),
                }),
            )];
            events.extend(
                hook_messages
                    .into_iter()
                    .map(|message| turn_event(&submission_id, message)),
            );
            events.extend(
                message
                    .boundary_events
                    .drain(..)
                    .map(|event| turn_event(&submission_id, event)),
            );
            let result = self
                .abort_with_events(
                    &submission_id,
                    &turn_id,
                    &rejection,
                    ExecutionOutcome::Aborted,
                    events,
                )
                .await;
            if result.is_err() {
                self.state.pending_messages = previous_pending_messages;
            }
            return result;
        }
        let mut model_input = message.input;
        self.config
            .model
            .prepare_turn_input(&self.state.context, &mut model_input);
        let checkpoint_sequence = self
            .state
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::Checkpoint("checkpoint sequence overflow".into()))?;
        let mut events = vec![turn_event(
            &submission_id,
            EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: turn_id.clone(),
                model_context_window: Some(self.config.context_window),
            }),
        )];
        events.extend(
            hook_messages
                .into_iter()
                .map(|message| turn_event(&submission_id, message)),
        );
        events.extend(
            message
                .boundary_events
                .into_iter()
                .map(|event| turn_event(&submission_id, event)),
        );
        let target = message.event.message_target_mut().ok_or_else(|| {
            Error::Checkpoint("prepared input event has no message target".into())
        })?;
        *target = Some(MessageTarget {
            checkpoint_sequence,
            batch_item_count: self.transcript_delta.len() + 1,
        });
        events.push(turn_event(&submission_id, message.event));
        let mut pending_messages = self.state.pending_messages.clone();
        self.config
            .middleware
            .consume_next_turn(&mut pending_messages, &submission_id)?;
        self.begin_turn(&submission_id, turn_id.clone())?;
        let previous_pending_messages =
            std::mem::replace(&mut self.state.pending_messages, pending_messages);
        let context_len = self.state.context.len();
        let transcript_len = self.transcript_delta.len();
        let first_user_message = self.state.first_user_message.clone();
        self.state.context.extend(submitted.input);
        if self.state.first_user_message.is_none()
            && let Some(title_seed) = message.title_seed.take()
        {
            self.state.first_user_message = Some(title_seed);
        }
        self.push_context(model_input);
        if let Err(error) = self.persist_with_events(events, None).await {
            self.state.pending_messages = previous_pending_messages;
            self.state.context.truncate(context_len);
            self.transcript_delta.truncate(transcript_len);
            self.state.first_user_message = first_user_message;
            self.state.active_execution = None;
            return Err(error);
        }
        self.continue_turn(inbox, submission_id, turn_id).await
    }

    fn begin_turn(&mut self, submission_id: &str, turn_id: String) -> Result<()> {
        if self.state.active_execution.is_some() {
            return Err(Error::Checkpoint(
                "cannot start a turn while another execution is active".into(),
            ));
        }
        self.state.active_execution = Some(ActiveExecution {
            submission_id: submission_id.into(),
            turn_id: turn_id.clone(),
            started_at_ms: unix_timestamp_ms()?,
            model_calls: 0,
            tool_calls: 0,
            failed_tool_calls: 0,
            usage: crate::protocol::TokenUsage::default(),
            next_model_step: 0,
            stop_hook_active: false,
            phase: ExecutionPhase::Model,
        });
        Ok(())
    }

    async fn drain_submissions(
        &mut self,
        inbox: &mut SubmissionInbox,
        turn_id: &str,
    ) -> Result<Option<String>> {
        let cutoff = inbox.cutoff()?;
        while inbox.last_sequence < cutoff {
            let submission = inbox.recv().await.ok_or_else(|| {
                Error::Stopped("agent submission channel closed before terminal cutoff".into())
            })?;
            match self
                .route_active_submission(submission, turn_id, None)
                .await?
            {
                ActiveRoute::Interrupted { submission_id } => {
                    return Ok(Some(submission_id));
                }
                ActiveRoute::Continue { .. } | ActiveRoute::Approval { .. } => {}
            }
        }
        Ok(None)
    }

    pub(super) fn usage_event(&self, submission_id: &str) -> Option<Event> {
        let last = self.state.last_usage.clone()?;
        Some(turn_event(
            submission_id,
            EventMsg::TokenCount(TokenCountEvent {
                info: Some(TokenUsageInfo {
                    total_token_usage: self.state.total_usage.clone(),
                    last_token_usage: last,
                    model_context_window: Some(self.config.context_window),
                }),
                rate_limits: None,
            }),
        ))
    }

    async fn complete_turn(
        &mut self,
        submission_id: &str,
        turn_id: &str,
        events: Vec<Event>,
    ) -> Result<()> {
        self.finish_turn(
            submission_id,
            turn_id,
            ExecutionOutcome::Completed,
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn_id.to_string(),
            }),
            events,
        )
        .await
    }

    pub(super) async fn abort(
        &mut self,
        submission_id: &str,
        turn_id: &str,
        reason: &str,
        outcome: ExecutionOutcome,
    ) -> Result<()> {
        self.abort_with_events(submission_id, turn_id, reason, outcome, Vec::new())
            .await
    }

    async fn abort_with_events(
        &mut self,
        submission_id: &str,
        turn_id: &str,
        reason: &str,
        outcome: ExecutionOutcome,
        mut events: Vec<Event>,
    ) -> Result<()> {
        let previous_state = self.state.clone();
        let previous_transcript = self.transcript_delta.clone();
        let result = async {
            self.state.active_model_step = None;
            events.extend(self.finish_pending_tools(submission_id, turn_id, reason)?);
            self.state.pending_approval = None;
            self.finish_turn(
                submission_id,
                turn_id,
                outcome,
                EventMsg::TurnAborted(TurnAbortedEvent {
                    turn_id: turn_id.to_string(),
                    reason: reason.to_string(),
                }),
                events,
            )
            .await
        }
        .await;
        if result.is_err() {
            self.state = previous_state;
            self.transcript_delta = previous_transcript;
        }
        result
    }

    async fn finish_turn(
        &mut self,
        submission_id: &str,
        turn_id: &str,
        outcome: ExecutionOutcome,
        terminal: EventMsg,
        mut events: Vec<Event>,
    ) -> Result<()> {
        self.config.middleware.finish_message_turn(
            &mut self.state.pending_messages,
            turn_id,
            outcome,
        )?;
        events.extend(
            self.turn_end_events(submission_id, turn_id, outcome)
                .await?,
        );
        events.push(turn_event(submission_id, terminal));
        self.finish_and_persist_execution(outcome, events).await?;
        Ok(())
    }

    async fn turn_end_events(
        &mut self,
        submission_id: &str,
        turn_id: &str,
        outcome: ExecutionOutcome,
    ) -> Result<Vec<Event>> {
        if self.turn_end_turn_id.as_deref() == Some(turn_id) {
            return Ok(Vec::new());
        }
        self.turn_end_turn_id = Some(turn_id.to_owned());
        let mut messages = Vec::new();
        self.config
            .middleware
            .turn_end(TurnEndContext {
                session_id: &self.config.session_id,
                turn_id,
                outcome,
                queued_messages: &self.state.pending_messages,
                owner: None,
                events: &mut messages,
            })
            .await?;
        Ok(messages
            .into_iter()
            .map(|message| turn_event(submission_id, message))
            .collect())
    }
}

fn turn_event(submission_id: &str, msg: EventMsg) -> Event {
    Event {
        submission_id: Some(submission_id.to_string()),
        msg,
    }
}
