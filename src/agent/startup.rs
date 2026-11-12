use crate::SessionId;
use std::sync::Arc;

use super::Agent;
use super::AgentConfig;
use super::ChildAgents;
use super::Runner;
use super::SUBMISSION_QUEUE_CAPACITY;
use super::recovery;
use super::recovery::Cause;
use super::recovery::Recovery;
use super::submission_channel;
use crate::Error;
use crate::Result;
use crate::backend::checkpoint::CHECKPOINT_VERSION;
use crate::backend::checkpoint::Checkpoint;
use crate::backend::checkpoint::TranscriptPageRequest;
use crate::middleware::FrontendExtensions;
use crate::middleware::RuntimeContext;
use crate::middleware::SessionStartSource;
use crate::protocol::ErrorEvent;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::ModelChangedEvent;
use crate::protocol::SessionConfiguredEvent;
use crate::protocol::SessionHistoryEvent;
use crate::protocol::TokenCountEvent;
use crate::protocol::TokenUsageInfo;
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
/// # Errors
///
/// Returns an error if validation or an operation required by this function fails.
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
    let session_id = config.session_id.take().unwrap_or_else(SessionId::generate);
    let (children, root_config) = if let Some(children) = config.children.take() {
        (children, None)
    } else {
        // Children start from the root's unstarted composition, which startup consumes below.
        let root = Arc::new(config.child_configuration());
        (ChildAgents(Arc::downgrade(&root)), Some(root))
    };
    config.middleware = Arc::new(
        config
            .middleware
            .with_sandbox(Arc::clone(&config.sandbox))?,
    );
    config.middleware.require_message_handler()?;
    let (mut state, is_new) = match config.checkpoints.load(&session_id).await? {
        Some(state) => (state, false),
        None => (Checkpoint::empty(session_id.share()), true),
    };
    if state.version != CHECKPOINT_VERSION || state.session_id != session_id {
        return Err(Error::Checkpoint(
            "checkpoint does not match the requested session".into(),
        ));
    }
    config
        .middleware
        .validate_message_queue(&state.pending_messages)?;
    let mut metadata_changed = false;
    if is_new {
        state.session_context = Arc::clone(&config.session_context);
        state.metadata.clone_from(&config.metadata);
        state.catalog_visible = config.catalog_visible;
    } else {
        config.session_context = Arc::clone(&state.session_context);
        if config.metadata_configured {
            metadata_changed = config.metadata != state.metadata;
            state.metadata.clone_from(&config.metadata);
        } else {
            config.metadata.clone_from(&state.metadata);
        }
    }
    state.session_context.validate()?;
    let (mut replay, next_before_sequence) = if is_new || config.initial_replay_batches == 0 {
        (Vec::new(), None)
    } else {
        let page = config
            .checkpoints
            .transcript_page(
                &session_id,
                TranscriptPageRequest {
                    before_sequence: None,
                    max_batches: config.initial_replay_batches,
                },
            )
            .await?;
        let next_before_sequence = page.next_before_sequence;
        let transcript = page.into_positioned_items_chronological();
        (
            replay_events(&transcript, &session_id),
            next_before_sequence,
        )
    };
    if let Some(turn_id) = state
        .active_execution
        .as_ref()
        .map(|execution| Arc::clone(&execution.turn_id))
    {
        for pending in &state.pending_tools {
            if let Some(call) = replay.iter_mut().rev().find_map(|event| match event {
                EventMsg::ToolCallBegin(call) if call.call_id == pending.call_id => Some(call),
                _ => None,
            }) {
                call.turn_id = Arc::clone(&turn_id);
            }
        }
    }
    let route = if config.model_route_configured || is_new {
        std::mem::take(&mut config.provider)
    } else {
        state
            .model_route
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| Error::Checkpoint("saved session has no model route".into()))?
    };
    let choice = config.select_model(&route)?;
    let route = choice.route;
    let model = crate::protocol::ModelInfo {
        model: choice.model,
        reasoning_effort: choice.reasoning_effort,
    };
    let (sender, inbox) = submission_channel(SUBMISSION_QUEUE_CAPACITY);
    let (event_tx, event_rx) = crate::agent::recorder::RecorderIngress::spawn(
        Arc::clone(&config.checkpoints),
        session_id.share(),
    );
    let session = Arc::new(SessionConfiguredEvent {
        session_id: session_id.share(),
        context: Arc::clone(&config.session_context),
        model: ModelChangedEvent {
            route: Arc::clone(&route),
            model: Arc::clone(&model.model),
            reasoning_effort: model.reasoning_effort.as_ref().map(Arc::clone),
            model_context_window: Some(config.context_window),
        },
    });
    let session_event = Event {
        submission_id: None,
        msg: EventMsg::SessionConfigured(Arc::clone(&session)),
    };
    let middleware_events = Arc::clone(&event_tx);
    let pending_frontend = Arc::new(std::sync::Mutex::new(Some(Vec::new())));
    let queued_frontend = Arc::clone(&pending_frontend);
    let runtime = RuntimeContext {
        sender: sender.downgrade(),
        checkpoints: Arc::clone(&config.checkpoints),
        session_id: session_id.share(),
        model_route: Arc::clone(&route),
        model: Arc::clone(&model.model),
        approval_policy: config.sandbox.approval_policy(),
        session_context: Arc::clone(&config.session_context),
        metadata: std::mem::take(&mut config.metadata),
        role: std::mem::take(&mut config.role),
        children,
        frontend: Arc::new(move |update| {
            let mut queued = queued_frontend.lock().map_err(|error| {
                Error::Stopped(format!("middleware frontend queue poisoned: {error}"))
            })?;
            if let Some(events) = queued.as_mut() {
                if events.len() >= super::EVENT_QUEUE_CAPACITY {
                    return Err(Error::Stopped("event recorder queue is full".into()));
                }
                events.push(update);
                return Ok(());
            }
            drop(queued);
            middleware_events.try_record(Event {
                submission_id: None,
                msg: EventMsg::Frontend(update),
            })
        }),
    };
    let (system_prompt, prompt_sizes) = config
        .middleware
        .system_prompt(&config.system_prompt, &runtime)?;
    let system_prompt: Arc<str> = system_prompt.into();
    let prompt_estimate =
        super::context_estimate::ContextEstimate::prompt(&prompt_sizes, config.token_estimate);
    let catalog = config.middleware.catalog(&runtime)?;
    let frontend = FrontendExtensions::new(Arc::clone(&config.middleware), session_id.share())?;
    let mut state_changed = metadata_changed || state.model_route.as_ref() != Some(&route);
    state.model_route = Some(route);
    let mut recovery = Recovery::prepare(&mut state, &config.middleware, Cause::Restart)?;
    state_changed |= recovery.execution.is_some();
    let start_source = initial_hook_source(is_new);
    let session_start = config
        .middleware
        .session_start(
            &runtime,
            &state.pending_messages,
            start_source,
            Arc::make_mut(&mut state.context),
            &mut state.delivered_once,
        )
        .await?;
    state_changed |= session_start.input_changed;
    let pending_session_start_stop = session_start.stop_reason;
    if let Err(error) = recovery
        .finish(
            Arc::clone(&config.middleware),
            session_id.share(),
            Cause::Restart,
        )
        .await
    {
        return Err(failed_start(&config, &runtime, error).await);
    }
    let mut state = super::LiveCheckpoint(Arc::new(state));
    let finish_start = async {
        if is_new || state_changed {
            if !is_new {
                state.make_mut().sequence = state.next_sequence()?;
            }
            let mut startup_events = vec![session_event];
            startup_events.append(&mut recovery.events);
            event_tx
                .save(
                    Arc::clone(&state.0),
                    &recovery.delta,
                    recovery.execution,
                    startup_events,
                )
                .await?;
        } else {
            event_tx.record(session_event).await?;
        }
        let frontend_events = pending_frontend
            .lock()
            .map_err(|error| {
                Error::Stopped(format!("middleware frontend queue poisoned: {error}"))
            })?
            .take()
            .ok_or_else(|| Error::Stopped("middleware frontend queue already drained".into()))?;
        for update in frontend_events {
            event_tx
                .record(Event {
                    submission_id: None,
                    msg: EventMsg::Frontend(update),
                })
                .await?;
        }
        if !replay.is_empty() {
            event_tx.try_record(Event {
                submission_id: None,
                msg: EventMsg::SessionHistory(SessionHistoryEvent { events: replay }),
            })?;
        }
        let last_usage = state.last_usage;
        if let Some(last_token_usage) = last_usage {
            event_tx.try_record(Event {
                submission_id: None,
                msg: EventMsg::TokenCount(TokenCountEvent {
                    info: Some(TokenUsageInfo {
                        context: None,
                        total_token_usage: state.total_usage,
                        last_token_usage,
                        model_context_window: Some(config.context_window),
                    }),
                    rate_limits: None,
                }),
            })?;
        }
        event_tx.flush().await
    }
    .await;
    if let Err(error) = finish_start {
        return Err(failed_start(&config, &runtime, error).await);
    }
    let model_router = Arc::clone(&config.model);
    let weak_frontend = Arc::downgrade(&runtime.frontend);
    let frontend_sink: crate::middleware::FrontendEventSink = Arc::new(move |event| {
        let frontend = weak_frontend
            .upgrade()
            .ok_or_else(|| Error::Stopped("agent frontend stopped".into()))?;
        frontend(event)
    });
    let middleware = Arc::clone(&config.middleware);
    let runtime = Arc::new(runtime);
    let fallback_runtime = Arc::clone(&runtime);
    let events = Arc::clone(&event_tx);
    let worker = tokio::spawn(async move {
        let mut runner = Runner {
            config,
            runtime,
            system_prompt,
            prompt_estimate,
            context_estimate: None,
            tool_schema_estimate: super::context_estimate::ToolSchemaEstimate::default(),
            catalog: Arc::new(catalog),
            state,
            transcript_delta: Vec::new(),
            pending_session_start_stop,
            turn_end_turn_id: None,
            idle: crate::middleware::IdleSchedule::default(),
            events: event_tx,
        };
        let result = runner.run(inbox).await;
        (result, runner.runtime)
    });
    tokio::spawn(async move {
        let (run, runtime) = match worker.await {
            Ok(result) => result,
            Err(_join_error) => (
                Err(Error::Stopped("agent runner panicked".into())),
                fallback_runtime,
            ),
        };
        let recovered = if run.is_err() {
            let middleware = Arc::clone(&middleware);
            let runtime = Arc::clone(&runtime);
            let events = Arc::clone(&events);
            tokio::spawn(
                async move { recovery::seal_failure(&middleware, &runtime, &events).await },
            )
            .await
            .unwrap_or_else(|_| Err(Error::Stopped("agent recovery panicked".into())))
        } else {
            Ok(())
        };
        let session_end = tokio::spawn(async move { middleware.session_end(&runtime).await })
            .await
            .unwrap_or_else(|_| Err(Error::Stopped("agent session cleanup panicked".into())));
        drop(root_config);
        let result = recovery::combine(recovery::combine(run, recovered), session_end);
        if let Err(error) = result
            && let Err(send_error) = events
                .record(Event {
                    submission_id: None,
                    msg: EventMsg::Error(ErrorEvent::from_error(&error)),
                })
                .await
        {
            tracing::warn!(%error, %send_error, "agent runtime failed and its error event could not be recorded");
        }
    });
    Ok(Agent {
        sender,
        events: event_rx,
        model_router,
        frontend,
        frontend_sink,
        session,
        next_before_sequence,
    })
}
