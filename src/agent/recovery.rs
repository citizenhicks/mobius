//! Seals interrupted durable work without replaying tools.

use super::TURN_RESTARTED_REASON;
use super::unix_timestamp_ms;
use crate::Error;
use crate::Result;
use crate::SessionId;
use crate::backend::checkpoint::Checkpoint;
use crate::backend::checkpoint::ExecutionOutcome;
use crate::backend::checkpoint::ExecutionRecord;
use crate::middleware::MiddlewareStack;
use crate::middleware::RuntimeContext;
use crate::middleware::TurnEndContext;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::ModelStepCompletedEvent;
use crate::protocol::ModelStepOutcome;
use crate::protocol::ToolCallEndEvent;
use crate::protocol::TurnAbortedEvent;
use serde_json::Value;
use std::sync::Arc;

#[derive(PartialEq, Eq, Copy, Clone)]
pub(super) enum Cause {
    Restart,
    Failure,
}

#[derive(Default)]
pub(super) struct Recovery {
    pub(super) delta: Vec<Arc<Value>>,
    pub(super) execution: Option<ExecutionRecord>,
    pub(super) events: Vec<Event>,
    idle: bool,
}

impl Recovery {
    pub(super) fn prepare(
        state: &mut Checkpoint,
        middleware: &MiddlewareStack,
        cause: Cause,
    ) -> Result<Self> {
        let mut recovery = Self::default();
        let outcome = match cause {
            Cause::Restart => ExecutionOutcome::Aborted,
            Cause::Failure => ExecutionOutcome::Failed,
        };
        let uncertain_tools = !state.pending_tools.is_empty()
            && (cause == Cause::Failure
                || state
                    .pending_approval
                    .as_ref()
                    .is_none_or(|pending| pending.decision_received));
        let interrupted_model_step = state.active_model_step.is_some();
        let interrupted_execution = match cause {
            Cause::Restart => uncertain_tools || interrupted_model_step,
            Cause::Failure => state.active_execution.is_some(),
        };
        recovery.idle = state
            .active_execution
            .as_ref()
            .is_some_and(|execution| execution.idle);
        if interrupted_execution && let Some(step) = state.active_model_step.take() {
            let active = state
                .active_execution
                .as_ref()
                .ok_or_else(|| Error::Checkpoint("active model step has no execution".into()))?;
            recovery.events.push(crate::agent::turn::turn_event(
                &active.submission_id,
                EventMsg::ModelStepCompleted(ModelStepCompletedEvent {
                    session_id: state.session_id.share(),
                    turn_id: Arc::clone(&active.turn_id),
                    model_step_id: step.model_step_id,
                    step_index: step.step_index,
                    started_at_ms: step.started_at_ms,
                    completed_at_ms: unix_timestamp_ms()?.max(step.started_at_ms),
                    outcome: match cause {
                        Cause::Restart => ModelStepOutcome::Interrupted,
                        Cause::Failure => ModelStepOutcome::Failed,
                    },
                    diagnostics: None,
                }),
            ));
        }
        let mut recovered_tool_calls = 0;
        if uncertain_tools {
            if interrupted_model_step {
                let calls = super::tool_step::tool_call_inputs(&state.pending_tools)?
                    .into_iter()
                    .map(Arc::new)
                    .collect::<Vec<_>>();
                Arc::make_mut(&mut state.context).extend(calls.iter().map(Arc::clone));
                recovery.delta.extend(calls);
            }
            recovered_tool_calls = u64::try_from(state.pending_tools.len()).map_err(|error| {
                Error::Checkpoint(format!("recovered tool-call count is unsupported: {error}"))
            })?;
            let recovered_turn = state
                .active_execution
                .as_ref()
                .map(|execution| Arc::clone(&execution.turn_id))
                .ok_or_else(|| {
                    Error::Checkpoint("pending tools have no active execution".into())
                })?;
            for call in std::mem::take(&mut state.pending_tools) {
                let output = match cause {
                    Cause::Restart => "execution interrupted; result unknown after restart",
                    Cause::Failure => "execution failed; result unknown",
                };
                let item = Arc::new(crate::backend::model::tool_output(
                    &call.call_id,
                    &output.into(),
                    true,
                ));
                Arc::make_mut(&mut state.context).push(Arc::clone(&item));
                recovery.delta.push(item);
                recovery.events.push(Event {
                    submission_id: state
                        .active_execution
                        .as_ref()
                        .map(|execution| Arc::clone(&execution.submission_id)),
                    msg: EventMsg::ToolCallEnd(ToolCallEndEvent {
                        turn_id: Arc::clone(&recovered_turn),
                        call_id: call.call_id,
                        name: call.name,
                        output: output.into(),
                        is_error: true,
                    }),
                });
            }
            state.pending_approval = None;
        }
        if interrupted_execution {
            let recovered_turn = state
                .active_execution
                .as_ref()
                .map(|execution| &execution.turn_id)
                .ok_or_else(|| Error::Checkpoint("active model step has no execution".into()))?;
            middleware.finish_message_turn(&mut state.pending_messages, recovered_turn, outcome)?;
            if uncertain_tools {
                let active = state.active_execution.as_mut().ok_or_else(|| {
                    Error::Checkpoint("recovery lost its active execution".into())
                })?;
                active.tool_calls = active
                    .tool_calls
                    .checked_add(recovered_tool_calls)
                    .ok_or_else(|| {
                        Error::Checkpoint("execution tool-call count overflow".into())
                    })?;
                active.failed_tool_calls = active
                    .failed_tool_calls
                    .checked_add(recovered_tool_calls)
                    .ok_or_else(|| {
                        Error::Checkpoint("execution failed-tool count overflow".into())
                    })?;
            }
            recovery.execution = Some(state.finish_execution(outcome, unix_timestamp_ms()?)?);
        }
        Ok(recovery)
    }

    pub(super) async fn finish(
        &mut self,
        middleware: Arc<MiddlewareStack>,
        session_id: SessionId,
        cause: Cause,
    ) -> Result<()> {
        let Some(execution) = &self.execution else {
            return Ok(());
        };
        let turn_id = Arc::clone(&execution.turn_id);
        let idle = self.idle;
        let terminal = super::turn::turn_event(
            &execution.submission_id,
            EventMsg::TurnAborted(TurnAbortedEvent {
                idle,
                turn_id: Arc::clone(&turn_id),
                reason: match cause {
                    Cause::Restart => TURN_RESTARTED_REASON,
                    Cause::Failure => "agent runner failed",
                }
                .into(),
            }),
        );
        let result = tokio::spawn(async move {
            let mut messages = Vec::new();
            middleware
                .turn_end(TurnEndContext {
                    session_id: &session_id,
                    turn_id: &turn_id,
                    owner: None,
                    idle,
                    events: &mut messages,
                })
                .await?;
            Ok(messages)
        })
        .await
        .map_err(|_join_error| Error::Stopped("agent turn cleanup panicked".into()))
        .and_then(|result| result);
        match result {
            Ok(messages) => {
                self.events
                    .extend(super::turn::turn_events(&execution.submission_id, messages));
                self.events.push(terminal);
                Ok(())
            }
            Err(error) => {
                self.events.push(terminal);
                Err(error)
            }
        }
    }
}

pub(super) async fn seal_failure(
    middleware: &Arc<MiddlewareStack>,
    runtime: &RuntimeContext,
    events: &super::EventRecorder,
) -> Result<()> {
    // The worker's unwound state is discarded; only its last durable checkpoint is trusted.
    events.flush().await?;
    let mut state = runtime
        .checkpoints
        .load(&runtime.session_id)
        .await?
        .ok_or_else(|| Error::Checkpoint("failed runner has no checkpoint".into()))?;
    let mut recovery = Recovery::prepare(&mut state, middleware, Cause::Failure)?;
    if recovery.execution.is_none() {
        return Ok(());
    }
    let cleanup = recovery
        .finish(
            Arc::clone(middleware),
            runtime.session_id.share(),
            Cause::Failure,
        )
        .await;
    state.sequence = state.next_sequence()?;
    let saved = events
        .save(
            Arc::new(state),
            &recovery.delta,
            recovery.execution,
            recovery.events,
        )
        .await;
    combine(cleanup, saved)
}

pub(super) fn combine(primary: Result<()>, cleanup: Result<()>) -> Result<()> {
    match (primary, cleanup) {
        (Err(primary), Err(rollback)) => Err(Error::Rollback {
            primary: Box::new(primary),
            rollback: Box::new(rollback),
        }),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}
