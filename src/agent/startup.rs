use std::sync::Arc;

use super::Agent;
use super::AgentConfig;
use super::EventRecorder;
use super::Runner;
use super::SUBMISSION_QUEUE_CAPACITY;
use super::send_event;
use super::submission_channel;
use super::try_send_event;
use super::unix_timestamp_ms;
use crate::Error;
use crate::Result;
use crate::backend::checkpoint::CHECKPOINT_VERSION;
use crate::backend::checkpoint::Checkpoint;
use crate::backend::checkpoint::ExecutionOutcome;
use crate::backend::checkpoint::ExecutionRecord;
use crate::backend::checkpoint::TranscriptPageRequest;
use crate::middleware::FrontendExtensions;
use crate::middleware::RuntimeContext;
use crate::middleware::SessionStartSource;
use crate::middleware::TurnEndContext;
use crate::protocol::ErrorEvent;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::ModelChangedEvent;
use crate::protocol::ModelStepCompletedEvent;
use crate::protocol::ModelStepOutcome;
use crate::protocol::SessionConfiguredEvent;
use crate::protocol::SessionHistoryEvent;
use crate::protocol::TokenCountEvent;
use crate::protocol::TokenUsageInfo;
use crate::protocol::ToolCallEndEvent;
use crate::protocol::TurnAbortedEvent;
use crate::protocol::replay_events;

fn initial_hook_source(is_new: bool) -> SessionStartSource {
    if is_new {
        SessionStartSource::Startup
    } else {
        SessionStartSource::Resume
    }
}

async fn failed_start(config: &AgentConfig, runtime: &RuntimeContext, primary: Error) -> Error {
    match config.middleware.session_end(runtime).await {
        Err(rollback) => Error::Rollback {
            primary: Box::new(primary),
            rollback: Box::new(rollback),
        },
        Ok(()) => primary,
    }
}

/// Validates capabilities, restores a checkpoint, and starts the agent loop.
pub async fn create_agent(mut config: AgentConfig) -> Result<Agent> {
    if config.context_window <= 0 {
        return Err(Error::Config("context window must be positive".into()));
    }
    if config.system_prompt.trim().is_empty() {
        return Err(Error::Config("system prompt cannot be empty".into()));
    }
    if config.max_model_steps == 0 {
        return Err(Error::Config("maximum model steps must be positive".into()));
    }
    config.middleware = config
        .middleware
        .with_sandbox(Arc::clone(&config.sandbox))?;
    config.middleware.require_message_handler()?;
    let (mut state, is_new) = match config.checkpoints.load(&config.session_id).await? {
        Some(state) => (state, false),
        None => (Checkpoint::empty(&config.session_id), true),
    };
    if state.version != CHECKPOINT_VERSION || state.session_id != config.session_id {
        return Err(Error::Checkpoint(
            "checkpoint does not match the requested session".into(),
        ));
    }
    config
        .middleware
        .validate_message_queue(&state.pending_messages)?;
    let mut metadata_changed = false;
    if is_new {
        state.session_context.clone_from(&config.session_context);
        state.metadata.clone_from(&config.metadata);
        state.catalog_visible = config.catalog_visible;
    } else {
        config.session_context.clone_from(&state.session_context);
        if config.metadata_configured {
            metadata_changed = config.metadata != state.metadata;
            state.metadata.clone_from(&config.metadata);
        } else {
            config.metadata.clone_from(&state.metadata);
        }
    }
    let (mut replay, next_before_sequence) = if is_new || config.initial_replay_batches == 0 {
        (Vec::new(), None)
    } else {
        let page = config
            .checkpoints
            .transcript_page(
                &config.session_id,
                TranscriptPageRequest {
                    before_sequence: None,
                    max_batches: config.initial_replay_batches,
                },
            )
            .await?;
        let next_before_sequence = page.next_before_sequence;
        let transcript = page.into_positioned_items_chronological();
        (
            replay_events(&transcript, &config.session_id),
            next_before_sequence,
        )
    };
    if let Some(turn_id) = state
        .active_execution
        .as_ref()
        .map(|execution| &execution.turn_id)
    {
        for pending in &state.pending_tools {
            if let Some(call) = replay.iter_mut().rev().find_map(|event| match event {
                EventMsg::ToolCallBegin(call) if call.call_id == pending.call_id => Some(call),
                _ => None,
            }) {
                call.turn_id.clone_from(turn_id);
            }
        }
    }
    let mut recovery_delta = Vec::new();
    let mut recovery_execution: Option<ExecutionRecord> = None;
    let mut recovery_events = Vec::new();
    let route = if config.model_route_configured || is_new {
        config.provider.clone()
    } else {
        state
            .model_route
            .clone()
            .ok_or_else(|| Error::Checkpoint("saved session has no model route".into()))?
    };
    let choice = config.select_model(&route)?;
    let route = choice.route.clone();
    let model = crate::protocol::ModelInfo {
        model: choice.model,
        reasoning_effort: choice.reasoning_effort,
    };
    let (sender, inbox) = submission_channel(SUBMISSION_QUEUE_CAPACITY);
    let (event_tx, event_rx) =
        EventRecorder::spawn(Arc::clone(&config.checkpoints), config.session_id.clone());
    let session = SessionConfiguredEvent {
        session_id: config.session_id.clone(),
        context: config.session_context.clone(),
        model: ModelChangedEvent {
            route: route.clone(),
            model: model.model.clone(),
            reasoning_effort: model.reasoning_effort.clone(),
            model_context_window: Some(config.context_window),
        },
    };
    let session_event = Event {
        submission_id: None,
        msg: EventMsg::SessionConfigured(session.clone()),
    };
    let middleware_events = event_tx.clone();
    let pending_frontend = Arc::new(std::sync::Mutex::new(Some(Vec::new())));
    let queued_frontend = Arc::clone(&pending_frontend);
    let runtime = RuntimeContext {
        sender: sender.downgrade(),
        checkpoints: Arc::clone(&config.checkpoints),
        session_id: config.session_id.clone(),
        model_route: route.clone(),
        model: model.model.clone(),
        approval_policy: config.sandbox.approval_policy(),
        session_context: config.session_context.clone(),
        metadata: config.metadata.clone(),
        role: config.role.clone(),
        frontend: Arc::new(move |update| {
            let mut queued = queued_frontend
                .lock()
                .map_err(|_| Error::Stopped("middleware frontend queue poisoned".into()))?;
            if let Some(events) = queued.as_mut() {
                if events.len() >= super::EVENT_QUEUE_CAPACITY {
                    return Err(Error::Stopped("event recorder queue is full".into()));
                }
                events.push(update);
                return Ok(());
            }
            drop(queued);
            try_send_event(
                &middleware_events,
                Event {
                    submission_id: None,
                    msg: EventMsg::Frontend(update),
                },
            )
        }),
    };
    let system_prompt: Arc<str> = config
        .middleware
        .system_prompt(&config.system_prompt, &runtime)?
        .into();
    let catalog = config.middleware.catalog(&runtime)?;
    let tool_count = catalog.registered_definitions().len();
    let frontend = FrontendExtensions::new(config.middleware.clone(), config.session_id.clone())?;
    let mut state_changed =
        metadata_changed || state.model_route.as_deref() != Some(route.as_str());
    state.model_route = Some(route);
    let uncertain_tools = !state.pending_tools.is_empty()
        && state
            .pending_approval
            .as_ref()
            .is_none_or(|pending| pending.decision_received);
    let interrupted_model_step = state.active_model_step.is_some();
    let interrupted_execution = uncertain_tools || interrupted_model_step;
    if interrupted_execution && let Some(step) = state.active_model_step.take() {
        let active = state
            .active_execution
            .as_ref()
            .ok_or_else(|| Error::Checkpoint("active model step has no execution".into()))?;
        recovery_events.push(Event {
            submission_id: Some(active.submission_id.clone()),
            msg: EventMsg::ModelStepCompleted(ModelStepCompletedEvent {
                session_id: state.session_id.clone(),
                turn_id: active.turn_id.clone(),
                model_step_id: step.model_step_id,
                step_index: step.step_index,
                started_at_ms: step.started_at_ms,
                completed_at_ms: unix_timestamp_ms()?.max(step.started_at_ms),
                outcome: ModelStepOutcome::Interrupted,
                diagnostics: None,
            }),
        });
    }
    if uncertain_tools {
        let recovered_tool_calls = u64::try_from(state.pending_tools.len())
            .map_err(|_| Error::Checkpoint("recovered tool-call count is unsupported".into()))?;
        let recovered_turn = state
            .active_execution
            .as_ref()
            .map(|execution| execution.turn_id.clone())
            .ok_or_else(|| Error::Checkpoint("pending tools have no active execution".into()))?;
        for call in std::mem::take(&mut state.pending_tools) {
            let output = "execution interrupted; result unknown after restart";
            let item = crate::backend::model::tool_output(&call.call_id, output, true);
            state.context.push(item.clone());
            recovery_delta.push(item);
            recovery_events.push(Event {
                submission_id: state
                    .active_execution
                    .as_ref()
                    .map(|execution| execution.submission_id.clone()),
                msg: EventMsg::ToolCallEnd(ToolCallEndEvent {
                    turn_id: recovered_turn.clone(),
                    call_id: call.call_id,
                    name: call.name,
                    output: output.into(),
                    is_error: true,
                }),
            });
        }
        state.pending_approval = None;
        config.middleware.finish_message_turn(
            &mut state.pending_messages,
            &recovered_turn,
            ExecutionOutcome::Aborted,
        )?;
        let active = state
            .active_execution
            .as_mut()
            .ok_or_else(|| Error::Checkpoint("recovery lost its active execution".into()))?;
        active.tool_calls = active
            .tool_calls
            .checked_add(recovered_tool_calls)
            .ok_or_else(|| Error::Checkpoint("execution tool-call count overflow".into()))?;
        active.failed_tool_calls = active
            .failed_tool_calls
            .checked_add(recovered_tool_calls)
            .ok_or_else(|| Error::Checkpoint("execution failed-tool count overflow".into()))?;
        recovery_execution =
            Some(state.finish_execution(ExecutionOutcome::Aborted, unix_timestamp_ms()?)?);
        state_changed = true;
    } else if interrupted_model_step {
        let recovered_turn = state
            .active_execution
            .as_ref()
            .map(|execution| execution.turn_id.clone())
            .ok_or_else(|| Error::Checkpoint("active model step has no execution".into()))?;
        config.middleware.finish_message_turn(
            &mut state.pending_messages,
            &recovered_turn,
            ExecutionOutcome::Aborted,
        )?;
        recovery_execution =
            Some(state.finish_execution(ExecutionOutcome::Aborted, unix_timestamp_ms()?)?);
        state_changed = true;
    }
    let start_source = initial_hook_source(is_new);
    let session_start = config
        .middleware
        .session_start(
            &runtime,
            &state.pending_messages,
            start_source,
            &mut state.context,
        )
        .await?;
    state_changed |= session_start.input_changed;
    let pending_session_start_stop = session_start.stop_reason;
    if let Some(execution) = &recovery_execution {
        let mut messages = Vec::new();
        if let Err(error) = config
            .middleware
            .turn_end(TurnEndContext {
                session_id: &config.session_id,
                turn_id: &execution.turn_id,
                outcome: ExecutionOutcome::Aborted,
                queued_messages: &state.pending_messages,
                owner: None,
                events: &mut messages,
            })
            .await
        {
            return Err(failed_start(&config, &runtime, error).await);
        }
        recovery_events.extend(messages.into_iter().map(|msg| Event {
            submission_id: Some(execution.submission_id.clone()),
            msg,
        }));
        recovery_events.push(Event {
            submission_id: Some(execution.submission_id.clone()),
            msg: EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: execution.turn_id.clone(),
                reason: "interrupted by restart".into(),
            }),
        });
    }
    let finish_start = async {
        if is_new || state_changed {
            if !is_new {
                state.sequence = state
                    .sequence
                    .checked_add(1)
                    .ok_or_else(|| Error::Checkpoint("checkpoint sequence overflow".into()))?;
            }
            let mut startup_events = vec![session_event];
            startup_events.append(&mut recovery_events);
            event_tx
                .save(
                    &state,
                    &recovery_delta,
                    recovery_execution.as_ref(),
                    startup_events,
                )
                .await?;
        } else {
            send_event(&event_tx, session_event).await?;
        }
        let frontend_events = pending_frontend
            .lock()
            .map_err(|_| Error::Stopped("middleware frontend queue poisoned".into()))?
            .take()
            .ok_or_else(|| Error::Stopped("middleware frontend queue already drained".into()))?;
        for update in frontend_events {
            send_event(
                &event_tx,
                Event {
                    submission_id: None,
                    msg: EventMsg::Frontend(update),
                },
            )
            .await?;
        }
        if !replay.is_empty() {
            try_send_event(
                &event_tx,
                Event {
                    submission_id: None,
                    msg: EventMsg::SessionHistory(SessionHistoryEvent { events: replay }),
                },
            )?;
        }
        if let Some(last_token_usage) = state.last_usage.clone() {
            try_send_event(
                &event_tx,
                Event {
                    submission_id: None,
                    msg: EventMsg::TokenCount(TokenCountEvent {
                        info: Some(TokenUsageInfo {
                            total_token_usage: state.total_usage.clone(),
                            last_token_usage,
                            model_context_window: Some(config.context_window),
                        }),
                        rate_limits: None,
                    }),
                },
            )?;
        }
        event_tx.flush().await
    }
    .await;
    if let Err(error) = finish_start {
        return Err(failed_start(&config, &runtime, error).await);
    }
    let model_choices = config.model.choices().cloned().collect();
    let model_router = Arc::clone(&config.model);
    let mut runner = Runner {
        config,
        runtime,
        system_prompt,
        catalog,
        state,
        review_session_id: uuid::Uuid::new_v4().to_string(),
        transcript_delta: Vec::new(),
        pending_session_start_stop,
        turn_end_turn_id: None,
        events: event_tx.clone(),
    };
    tokio::spawn(async move {
        let run = runner.run(inbox).await;
        let session_end = runner.config.middleware.session_end(&runner.runtime).await;
        let error = match (run, session_end) {
            (Err(primary), Err(rollback)) => Some(Error::Rollback {
                primary: Box::new(primary),
                rollback: Box::new(rollback),
            }),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Some(error),
            (Ok(()), Ok(())) => None,
        };
        if let Some(error) = error {
            let _ = send_event(
                &event_tx,
                Event {
                    submission_id: None,
                    msg: EventMsg::Error(ErrorEvent::from_error(&error)),
                },
            )
            .await;
        }
    });
    Ok(Agent {
        sender,
        events: event_rx,
        model_router,
        frontend,
        session,
        model,
        model_choices,
        tool_count,
        next_before_sequence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_checkpoint_starts_before_it_resumes() {
        assert_eq!(initial_hook_source(true), SessionStartSource::Startup);
        assert_eq!(initial_hook_source(false), SessionStartSource::Resume);
    }
}
