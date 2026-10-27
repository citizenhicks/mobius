use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use uuid::Uuid;

use super::streaming::StreamedTools;
use super::turn_event;
use crate::agent::input::Wait;
use crate::agent::tool_step::order_results;
use crate::agent::{
    FRONTEND_DISCONNECTED_REASON, Runner, SubmissionInbox, TURN_INTERRUPTED_REASON, send_event,
    unix_timestamp_ms,
};
use crate::backend::checkpoint::{
    ActiveModelStep, ContextRewrite, ContextRewriteReason, ExecutionOutcome, ExecutionPhase,
};
use crate::backend::model::{
    MAX_TOOL_CALLS, ModelCancellation, ModelCancellationReason, ModelEventSink, ModelInput,
    ModelOutput, ModelRequest, PromptCacheIdentity, StreamingToolCalls, ToolDefinition,
    durable_visible_message_index, insert_before_open_tool_calls, internal_user_message,
    prepare_turn_input, prompt_cache_key, reset_prompt_cache_breakpoint,
    reset_shared_prompt_cache_breakpoint, strip_provider_reasoning,
    strip_shared_provider_reasoning,
};
use crate::backend::sandbox::SandboxAuthorization;
use crate::middleware::tools::{PreparedToolSet, ToolBinding, ToolResult};
use crate::middleware::{ModelContext, PreparationNotices, StagedInput, StopContext};
use crate::protocol::{
    AssistantMessageEvent, Event, EventMsg, MessageTarget, ModelEvent, ModelEventTracker,
    ModelStepCompletedEvent, ModelStepDiagnostics, ModelStepOutcome, ModelStepStartedEvent,
    SubmissionRejectedEvent, TokenUsage, ToolCall,
};
use crate::{Error, Result};

fn record_cancellation<T>(cancellation: &ModelCancellation, result: &Result<Wait<Result<T>>>) {
    let reason = match result {
        Ok(Wait::Interrupted { .. }) => ModelCancellationReason::Interrupted,
        Err(Error::Stopped(message)) if message == FRONTEND_DISCONNECTED_REASON => {
            ModelCancellationReason::FrontendDisconnected
        }
        Err(_) | Ok(Wait::Ready { value: Err(_), .. }) => ModelCancellationReason::RequestFailed,
        Ok(Wait::Ready { value: Ok(_), .. }) => return,
    };
    cancellation.record(reason);
}

/// Outcome of `Runner::prepare_model_phase`.
enum PreparedModel {
    /// An interrupt aborted the turn; `continue_turn` returns.
    Aborted,
    /// Middleware requested normal completion before another model request.
    Stopped(String),
    /// An idle turn ran its idle hooks and received no message; it completes quietly.
    Idle,
    /// Middleware queued a message during the phase; re-run it before the model call.
    Repeat(Vec<crate::backend::checkpoint::ContextRewriteReason>),
    /// Proceed to the model request.
    Ready {
        input: PreparedInput,
        tools: Box<PreparedTools>,
        rewrite_reasons: Vec<crate::backend::checkpoint::ContextRewriteReason>,
    },
}

#[expect(
    clippy::rc_buffer,
    reason = "shares the checkpoint's copy-on-write context without copying it"
)]
enum PreparedInput {
    Shared(Arc<Vec<Arc<Value>>>),
    Request(Vec<Value>),
}

impl PreparedInput {
    fn input(&self) -> ModelInput<'_> {
        match self {
            Self::Shared(input) => input.as_slice().into(),
            Self::Request(input) => input.as_slice().into(),
        }
    }
}

struct PreparedTools {
    direct: Vec<Arc<ToolDefinition>>,
    deferred: Vec<Arc<ToolDefinition>>,
    catalog: PreparedToolSet<'static>,
    allow_hosted_tools: bool,
}

struct CompletedModelStep {
    started: Arc<ModelStepStartedEvent>,
    output: ModelOutput,
    tools: PreparedToolSet<'static>,
    model_events: Arc<ModelEventTracker>,
    streamed: StreamedTools,
}

struct NormalizedModelStep {
    output: ModelOutput,
    executable_calls: Vec<(usize, ToolBinding)>,
    denied_results: Vec<ToolResult>,
    streamed: StreamedTools,
}

enum ModelStepRequest {
    Completed(Box<CompletedModelStep>),
    Restart,
    Finished,
}

impl Runner {
    async fn stage_message_input(&mut self, turn_id: &Arc<str>) -> Result<()> {
        if !self
            .config
            .middleware
            .messages_ready(&self.state.pending_messages, turn_id)?
        {
            return Ok(());
        }
        // Hooks consume a staged queue; a rejected hook or failed save must leave live input intact.
        let mut pending_messages = self.state.pending_messages.clone(); // rules-allow: hooks consume a staged queue so failure keeps live input
        let messages = self
            .config
            .middleware
            .stage_model_messages(&mut pending_messages, turn_id)?;
        if messages.is_empty() {
            return Ok(());
        }
        let checkpoint_sequence = self
            .state
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::Checkpoint("checkpoint sequence overflow".into()))?;
        let batch_before = self.transcript_delta.len();
        let mut context = StagedInput::new(Arc::clone(&self.state.context));
        // A staged batch must see earlier accepted notices without changing live state.
        let mut delivered_once = Arc::clone(&self.state.delivered_once);
        let mut transcript = Vec::with_capacity(messages.len());
        let mut events = Vec::new();
        let mut idle_origin = None;
        for mut message in messages {
            let mut hook_events = Vec::new();
            let submitted = self
                .config
                .middleware
                .message_submit(
                    self.turn_identity(turn_id)?,
                    &message,
                    &mut hook_events,
                    &delivered_once,
                )
                .await?;
            events.extend(
                hook_events
                    .into_iter()
                    .map(|msg| crate::agent::turn::turn_event(&message.submission_id, msg)),
            );
            events.extend(
                message
                    .boundary_events
                    .drain(..)
                    .map(|msg| crate::agent::turn::turn_event(&message.submission_id, msg)),
            );
            if let Some(rejection) = submitted.rejection {
                events.push(Event {
                    submission_id: Some(message.submission_id),
                    msg: EventMsg::SubmissionRejected(SubmissionRejectedEvent {
                        message: rejection,
                    }),
                });
                continue;
            }
            if idle_origin.is_none()
                && self
                    .state
                    .active_execution
                    .as_ref()
                    .is_some_and(|execution| execution.idle)
                && let EventMsg::Message(event) = &message.event
            {
                let author = Arc::new(event.author.clone()); // rules-allow: the execution keeps the first accepted author while its message moves on
                idle_origin = Some((author, Arc::clone(&message.submission_id)));
            }
            for item in submitted
                .input
                .into_iter()
                .filter(|item| crate::middleware::delivery_once::accept(&mut delivered_once, item))
            {
                context.push(Arc::new(item));
            }
            prepare_turn_input(context.input(), &mut message.input);
            let target = message.event.message_target_mut().ok_or_else(|| {
                Error::Checkpoint("prepared input event has no message target".into())
            })?;
            *target = Some(MessageTarget {
                checkpoint_sequence,
                batch_item_count: std::num::NonZeroUsize::MIN
                    .saturating_add(batch_before + transcript.len()),
            });
            let input = Arc::new(message.input);
            context.push(Arc::clone(&input));
            transcript.push(input);
            events.push(Event {
                submission_id: Some(message.submission_id),
                msg: message.event,
            });
        }
        let previous_pending_messages = std::mem::replace(
            &mut self.state.make_mut().pending_messages,
            pending_messages,
        );
        // A staged message turns an idle turn into a normal one, durably with the message.
        let was_idle = !transcript.is_empty()
            && self
                .state
                .make_mut()
                .active_execution
                .as_mut()
                .is_some_and(|execution| std::mem::take(&mut execution.idle));
        let (previous_author, promoted_submission_id) = match idle_origin {
            Some((author, submission_id)) => (
                self.state
                    .make_mut()
                    .active_execution
                    .as_mut()
                    .map(|execution| std::mem::replace(&mut execution.author, author)),
                Some(submission_id),
            ),
            None => (None, None),
        };
        if was_idle && let Some(submission_id) = promoted_submission_id {
            events.insert(
                0,
                turn_event(
                    &submission_id,
                    EventMsg::TurnStarted(crate::protocol::TurnStartedEvent {
                        idle: false,
                        turn_id: Arc::clone(turn_id),
                        model_context_window: Some(self.config.context_window),
                    }),
                ),
            );
        }
        let context_len = self.state.context.len();
        context.commit(&mut self.state.make_mut().context);
        let previous_delivered_once =
            std::mem::replace(&mut self.state.make_mut().delivered_once, delivered_once);
        let transcript_len = self.transcript_delta.len();
        self.transcript_delta.extend(transcript);
        if let Err(error) = self.persist_with_events(events, None).await {
            self.state.make_mut().pending_messages = previous_pending_messages;
            if was_idle && let Some(execution) = self.state.make_mut().active_execution.as_mut() {
                execution.idle = true;
                if let Some(author) = previous_author {
                    execution.author = author;
                }
            }
            Arc::make_mut(&mut self.state.make_mut().context).truncate(context_len);
            self.state.make_mut().delivered_once = previous_delivered_once;
            self.transcript_delta.truncate(transcript_len);
            return Err(error);
        }
        if was_idle {
            self.idle_spent = false;
        }
        Ok(())
    }

    async fn persist_model_hook_changes(
        &mut self,
        submission_id: &Arc<str>,
        mut middleware_events: Vec<EventMsg>,
        accounting_usage: Option<&TokenUsage>,
        checkpoint_changed: bool,
        provisional_target_sequence: u64,
    ) -> Result<()> {
        if checkpoint_changed {
            let durable_sequence = self
                .state
                .sequence
                .checked_add(1)
                .ok_or_else(|| Error::Checkpoint("checkpoint sequence overflow".into()))?;
            rebase_live_message_targets(
                &mut middleware_events,
                provisional_target_sequence,
                durable_sequence,
            );
        }
        let mut events = middleware_events
            .into_iter()
            .map(|message| turn_event(submission_id, message))
            .collect::<Vec<_>>();
        if accounting_usage.is_some()
            && let Some(usage) = self.usage_event(submission_id, accounting_usage)
        {
            events.push(usage);
        }
        if checkpoint_changed {
            self.persist_with_events(events, None).await?;
        } else {
            for event in events {
                send_event(&self.events, event).await?;
            }
        }
        Ok(())
    }

    /// Runs `PreModel` and `ModelRequest` with interruption routing, folds usage and
    /// events into state, and persists when anything changed.
    async fn prepare_model_phase(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &Arc<str>,
        turn_id: &Arc<str>,
        model_step: usize,
    ) -> Result<PreparedModel> {
        self.stage_message_input(turn_id).await?;
        let idle = model_step == 0
            && self
                .state
                .active_execution
                .as_ref()
                .is_some_and(|execution| execution.idle);
        let mut middleware_events = Vec::new();
        let mut middleware_usage = Vec::new();
        let provisional_target_sequence = self
            .state
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::Checkpoint("checkpoint sequence overflow".into()))?;
        let mut checkpoint_changed = false;
        let mut rewrite_reasons = Vec::new();
        let mut turn_stop = None;
        // Preparation can be cancelled; history and receipt mutations remain staged until accepted.
        let mut durable_input = StagedInput::new(Arc::clone(&self.state.context));
        let mut delivered_once = Arc::clone(&self.state.delivered_once);
        let mut transcript_delta = Vec::new();
        let mut context_epoch = self.state.context_epoch;
        let mut available_tools = self.catalog.exposed_names();
        let mut allow_hosted_tools = true;
        // Compact session-start hooks may read this queue after awaiting a provider.
        // wait_active can replace/edit the live queue meanwhile, so its Vec cannot stay borrowed.
        let queued_messages = self.state.pending_messages.clone(); // rules-allow: the live queue may change while preparation awaits
        let model = Arc::clone(&self.config.model);
        let provider = Arc::clone(&self.config.provider);
        let context_provider = self.state.context_model_route.as_ref().map(Arc::clone);
        let session_id = self.runtime.session_id.share();
        let instructions = Arc::clone(&self.system_prompt);
        let last_usage = self.state.last_usage;
        let catalog = Arc::clone(&self.catalog);
        let runtime = Arc::clone(&self.runtime);
        let middleware = Arc::clone(&self.config.middleware);
        let author = Arc::clone(self.active_author()?);
        let mut preparation_notices = PreparationNotices::new(&runtime.frontend);
        let cancellation = ModelCancellation::default();
        let control = {
            let prepare_model = middleware.prepare_model(
                ModelContext {
                    token_estimate: self.config.token_estimate,
                    author: &author,
                    model: &model,
                    provider: &provider,
                    context_provider: context_provider.as_deref().unwrap_or(&provider),
                    session_id: &session_id,
                    cancellation: Some(&cancellation),
                    session_context: &runtime.session_context,
                    metadata: &runtime.metadata,
                    turn_id,
                    model_step,
                    context_window: self.config.context_window,
                    instructions: &instructions,
                    checkpoint_sequence: self.state.sequence,
                    available_tools: &mut available_tools,
                    allow_hosted_tools: &mut allow_hosted_tools,
                    durable_input: &mut durable_input,
                    delivered_once: &mut delivered_once,
                    transcript_delta: &mut transcript_delta,
                    context_epoch: &mut context_epoch,
                    rewrite_reasons: &mut rewrite_reasons,
                    turn_stop: &mut turn_stop,
                    queued_messages,
                    last_usage: last_usage.as_ref(),
                    tools: &catalog,
                    events: &mut middleware_events,
                    usage: &mut middleware_usage,
                    checkpoint_changed: &mut checkpoint_changed,
                    runtime: &runtime,
                    hooks: &middleware,
                    preparation_notices: &mut preparation_notices.notices,
                },
                idle,
            );
            // Keep preparation alive until cancellation is recorded for its provider drop guards.
            tokio::pin!(prepare_model);
            let control = self
                .wait_active(inbox, turn_id, prepare_model.as_mut())
                .await;
            record_cancellation(&cancellation, &control);
            control
        };
        let usage_changed = !middleware_usage.is_empty();
        for (route, usage) in &middleware_usage {
            self.record_model_call()?;
            self.record_usage(route, usage).await?;
        }
        let mut hook_result = match control? {
            Wait::Ready { value, .. } => value,
            Wait::Interrupted { submission_id } => {
                let mut events = preparation_notices
                    .cancellations()
                    .map(|message| turn_event(&submission_id, message))
                    .collect::<Vec<_>>();
                if let Some(usage) = middleware_usage.last().map(|(_, usage)| usage)
                    && let Some(event) = self.usage_event(&submission_id, Some(usage))
                {
                    events.push(event);
                }
                self.abort_with_events(
                    &submission_id,
                    turn_id,
                    TURN_INTERRUPTED_REASON,
                    ExecutionOutcome::Aborted,
                    events,
                )
                .await?;
                preparation_notices.settle();
                return Ok(PreparedModel::Aborted);
            }
        };
        if let Ok(request_input) = &mut hook_result
            && turn_stop.is_none()
            && context_provider
                .as_deref()
                .is_some_and(|owner| !self.config.model.same_context_model(owner, &provider))
        {
            // Source checkpoint preparation sees its own reasoning before destination replay.
            let changed = durable_input
                .input()
                .iter()
                .any(crate::backend::model::has_provider_reasoning)
                && strip_shared_provider_reasoning(durable_input.make_mut());
            if let Some(input) = request_input {
                let request_changed = strip_provider_reasoning(input);
                if changed || request_changed {
                    reset_prompt_cache_breakpoint(input);
                }
            }
            if changed {
                if rewrite_reasons.is_empty() {
                    context_epoch = context_epoch.checked_add(1).ok_or_else(|| {
                        Error::Checkpoint("context rewrite epoch overflow".into())
                    })?;
                }
                if !rewrite_reasons.contains(&ContextRewriteReason::ModelChange) {
                    rewrite_reasons.push(ContextRewriteReason::ModelChange);
                }
                reset_shared_prompt_cache_breakpoint(durable_input.make_mut());
                checkpoint_changed = true;
            }
        }
        let preparation_accepted = hook_result.is_ok();
        preparation_notices.fail();
        let request_input = match hook_result {
            Ok(request_input) => {
                if checkpoint_changed {
                    durable_input.commit(&mut self.state.make_mut().context);
                }
                let request_input = request_input.map_or_else(
                    || PreparedInput::Shared(Arc::clone(&self.state.context)),
                    PreparedInput::Request,
                );
                Ok(request_input)
            }
            Err(error) => {
                context_epoch = self.state.context_epoch;
                transcript_delta.clear();
                rewrite_reasons.clear();
                middleware_events.clear();
                checkpoint_changed = false;
                Err(error)
            }
        };
        self.transcript_delta.extend(transcript_delta);
        if checkpoint_changed {
            self.state.make_mut().delivered_once = delivered_once;
        }
        self.state.make_mut().context_epoch = context_epoch;
        if !rewrite_reasons.is_empty() {
            self.state.make_mut().last_usage = None;
            self.state.make_mut().last_context_rewrite = Some(ContextRewrite {
                epoch: self.state.context_epoch,
                reasons: rewrite_reasons.clone(), // rules-allow: the step result also reports the rewrite reasons
            });
        }
        checkpoint_changed |= usage_changed;
        let messages_ready = self
            .config
            .middleware
            .messages_ready(&self.state.pending_messages, turn_id)?;
        middleware_events.extend(preparation_notices.completions(preparation_accepted));
        self.persist_model_hook_changes(
            submission_id,
            middleware_events,
            middleware_usage.last().map(|(_, usage)| usage),
            checkpoint_changed,
            provisional_target_sequence,
        )
        .await?;
        preparation_notices.settle();
        let request_input = request_input?;
        if messages_ready {
            return Ok(PreparedModel::Repeat(rewrite_reasons));
        }
        if idle {
            return Ok(PreparedModel::Idle);
        }
        if let Some(reason) = turn_stop {
            return Ok(PreparedModel::Stopped(reason));
        }
        Ok(PreparedModel::Ready {
            tools: Box::new(self.prepare_tools(
                request_input.input(),
                available_tools,
                allow_hosted_tools,
            )?),
            input: request_input,
            rewrite_reasons,
        })
    }

    fn prepare_tools(
        &self,
        input: ModelInput<'_>,
        available: BTreeSet<String>,
        allow_hosted_tools: bool,
    ) -> Result<PreparedTools> {
        let catalog = self.catalog.prepare(input, Cow::Owned(available))?;
        let (direct, deferred) = self.config.model.prepare_tool_definitions(
            &self.config.provider,
            catalog.direct().to_vec(),
            catalog.deferred().to_vec(),
            catalog.materialized(),
        )?;
        Ok(PreparedTools {
            direct,
            deferred,
            catalog,
            allow_hosted_tools,
        })
    }

    pub(in crate::agent) async fn live_tools(&self) -> Result<PreparedToolSet<'static>> {
        let mut available = self.catalog.exposed_names();
        let supports_tool_image_input = self
            .config
            .model
            .supports_tool_image_input(&self.config.provider)?;
        self.config
            .middleware
            .resolve_tool_exposure(
                &self.runtime.session_id,
                supports_tool_image_input,
                self.state.context.as_slice().into(),
                &mut available,
            )
            .await?;
        self.catalog
            .prepare(self.state.context.as_slice().into(), Cow::Owned(available))
    }

    fn model_step_terminal_events(
        submission_id: &Arc<str>,
        started: &ModelStepStartedEvent,
        outcome: ModelStepOutcome,
        model_events: &ModelEventTracker,
    ) -> Result<Vec<Event>> {
        let mut events = model_events
            .interrupted()?
            .into_iter()
            .filter_map(|event| {
                event
                    .into_event(
                        started.session_id.share(),
                        Arc::clone(&started.turn_id),
                        Arc::clone(&started.model_step_id),
                    )
                    .map(|event| turn_event(submission_id, event))
            })
            .collect::<Vec<_>>();
        events.push(model_step_completed_event(
            submission_id,
            started,
            outcome,
            None,
        )?);
        Ok(events)
    }

    async fn retry_model_step(
        &mut self,
        submission_id: &Arc<str>,
        started: &ModelStepStartedEvent,
        model_events: &ModelEventTracker,
    ) -> Result<()> {
        let events = Self::model_step_terminal_events(
            submission_id,
            started,
            ModelStepOutcome::Retrying,
            model_events,
        )?;
        let active_model_step = self.state.make_mut().active_model_step.take();
        match self.persist_with_events(events, None).await {
            Ok(_) => Ok(()),
            Err(error) => {
                self.state.make_mut().active_model_step = active_model_step;
                Err(error)
            }
        }
    }

    async fn fail_model_step(
        &mut self,
        submission_id: &Arc<str>,
        started: &ModelStepStartedEvent,
        model_events: &ModelEventTracker,
        error: Error,
    ) -> Result<()> {
        let events = Self::model_step_terminal_events(
            submission_id,
            started,
            ModelStepOutcome::Failed,
            model_events,
        )?;
        self.fail_turn_with_events(submission_id, error, events)
            .await
    }

    async fn interrupt_model_step(
        &mut self,
        submission_id: &Arc<str>,
        interrupt_submission_id: &Arc<str>,
        started: &ModelStepStartedEvent,
        model_events: &ModelEventTracker,
    ) -> Result<()> {
        let events = Self::model_step_terminal_events(
            submission_id,
            started,
            ModelStepOutcome::Interrupted,
            model_events,
        )?;
        self.abort_with_events(
            interrupt_submission_id,
            &started.turn_id,
            TURN_INTERRUPTED_REASON,
            ExecutionOutcome::Aborted,
            events,
        )
        .await
    }

    async fn request_model_step(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &Arc<str>,
        turn_id: &Arc<str>,
        model_step: usize,
        request_input: ModelInput<'_>,
        tools: Box<PreparedTools>,
    ) -> Result<ModelStepRequest> {
        let model = Arc::clone(&self.config.model);
        let provider = Arc::clone(&self.config.provider);
        let model_session_id = self.state.session_id.share();
        let cache_key = prompt_cache_key(&model_session_id);
        let instructions = Arc::clone(&self.system_prompt);
        let estimate = self.config.token_estimate;
        let mut context = self.prompt_estimate;
        context.add_input(request_input.iter(), estimate);
        context.add_tools(
            tools.catalog.direct(),
            tools.catalog.deferred(),
            tools.catalog.materialized(),
            estimate,
        )?;
        self.context_estimate = Some(context);
        let mut stream_retries = 0;
        loop {
            let started = Arc::new(ModelStepStartedEvent {
                session_id: model_session_id.share(),
                turn_id: Arc::clone(turn_id),
                model_step_id: Uuid::new_v4().to_string().into(),
                step_index: model_step,
                started_at_ms: unix_timestamp_ms()?,
            });
            self.record_model_call()?;
            self.state.make_mut().active_model_step = Some(ActiveModelStep {
                model_step_id: Arc::clone(&started.model_step_id),
                step_index: started.step_index,
                started_at_ms: started.started_at_ms,
            });
            self.persist_with_events(
                vec![turn_event(
                    submission_id,
                    EventMsg::ModelStepStarted(Arc::clone(&started)),
                )],
                None,
            )
            .await?;
            let event_submission_id = Arc::clone(submission_id);
            let event_turn_id = Arc::clone(&started.turn_id);
            let event_session_id = started.session_id.share();
            let event_model_step_id = Arc::clone(&started.model_step_id);
            let model_events = Arc::new(ModelEventTracker::default());
            let streamed_events = Arc::clone(&model_events);
            let catalog_revision = self.catalog.revision()?.to_owned();
            let recorder = Arc::downgrade(&self.events);
            let (response_open, response_closed) = tokio::sync::watch::channel(());
            let (ready_calls_tx, ready_calls) = tokio::sync::mpsc::channel(MAX_TOOL_CALLS);
            let ready_calls_tx = Arc::new(ready_calls_tx);
            let call_validation = Arc::new(Mutex::new(StreamingToolCalls::default()));
            let stream: ModelEventSink = Arc::new(move |event| {
                let mut response_closed = response_closed.clone(); // rules-allow: each streamed event future owns its handles
                let call_validation = Arc::clone(&call_validation);
                let ready_calls_tx = Arc::clone(&ready_calls_tx);
                let streamed_events = Arc::clone(&streamed_events);
                // Only a weak recorder handle enters this callback future, avoiding an ownership cycle.
                let recorder = std::sync::Weak::clone(&recorder);
                let event_submission_id = Arc::clone(&event_submission_id);
                let event_turn_id = Arc::clone(&event_turn_id);
                let event_session_id = event_session_id.share();
                let event_model_step_id = Arc::clone(&event_model_step_id);
                Box::pin(async move {
                    let record = async {
                        if let ModelEvent::ToolCallReady(call) = event {
                            call_validation
                                .lock()
                                .map_err(|error| {
                                    Error::Stopped(format!(
                                        "streamed tool validation unavailable: {error}"
                                    ))
                                })?
                                .accept(&call)?;
                            recorder
                                .upgrade()
                                .ok_or_else(|| Error::Stopped("event recorder stopped".into()))?
                                .flush()
                                .await?;
                            return ready_calls_tx.send(call).await.map_err(|error| {
                                Error::Stopped(format!("streamed tool queue unavailable: {error}"))
                            });
                        }
                        streamed_events.observe(&event)?;
                        let Some(msg) =
                            event.into_event(event_session_id, event_turn_id, event_model_step_id)
                        else {
                            return Ok(());
                        };
                        let recorder = recorder
                            .upgrade()
                            .ok_or_else(|| Error::Stopped("event recorder stopped".into()))?;
                        recorder
                            .record(Event {
                                submission_id: Some(event_submission_id),
                                msg,
                            })
                            .await
                    };
                    tokio::select! {
                        biased;
                        _ = response_closed.changed() => Err(Error::Stopped("model event sink closed".into())),
                        result = record => result,
                    }
                })
            });
            let cancellation = ModelCancellation::default();
            let request = ModelRequest {
                session_id: Some(&model_session_id),
                cancellation: Some(&cancellation),
                prompt_cache: Some(PromptCacheIdentity {
                    key: &cache_key,
                    context_epoch: self.state.context_epoch,
                }),
                instructions: &instructions,
                input: request_input,
                catalog_revision: &catalog_revision,
                tools: &tools.direct,
                deferred_tools: &tools.deferred,
                allow_hosted_tools: tools.allow_hosted_tools,
                allow_continuation: true,
            };
            let mut streamed = StreamedTools::default();
            let response = {
                let response = model.respond(&provider, request, stream);
                let response = async move {
                    let response = response.await;
                    drop(response_open);
                    response
                };
                // Keep the response alive until its drop guard can read the cancellation reason.
                tokio::pin!(response);
                let result = self
                    .wait_streamed_response(
                        inbox,
                        response.as_mut(),
                        ready_calls,
                        &tools.catalog,
                        &mut streamed,
                    )
                    .await;
                record_cancellation(&cancellation, &result);
                result
            };
            let response = match self.events.flush().await {
                Ok(()) => response,
                Err(error) => Err(error),
            };
            if !matches!(&response, Ok(Wait::Ready { value: Ok(_), .. })) {
                streamed.cancel();
            }
            match response {
                Ok(Wait::Ready {
                    value: Ok(output), ..
                }) => {
                    return Ok(ModelStepRequest::Completed(Box::new(CompletedModelStep {
                        started,
                        output,
                        tools: tools.catalog,
                        model_events,
                        streamed,
                    })));
                }
                Ok(Wait::Ready {
                    value: Err(Error::Provider(error)),
                    input_changed,
                }) if (error.is_stream_interrupted() || error.is_retryable())
                    && streamed.originals.is_empty() =>
                {
                    let transport = model.transport_settings_for(&provider)?;
                    let retry_limit =
                        usize::try_from(transport.stream_retry_limit).map_err(|error| {
                            Error::Config(format!(
                                "stream retry limit exceeds platform range: {error}"
                            ))
                        })?;
                    let delay = crate::backend::model::retry_delay(
                        &error,
                        stream_retries,
                        &started.model_step_id,
                        &transport,
                    );
                    let delay = if stream_retries < retry_limit {
                        stream_retries += 1;
                        delay
                    } else {
                        match model.fallback_transport(&provider, &model_session_id).await {
                            Ok(true) => {
                                stream_retries = 0;
                                if error.retry_after().is_some() {
                                    delay
                                } else {
                                    Duration::ZERO
                                }
                            }
                            outcome => {
                                let error = outcome.err().unwrap_or(Error::Provider(error));
                                self.fail_model_step(submission_id, &started, &model_events, error)
                                    .await?;
                                return Ok(ModelStepRequest::Finished);
                            }
                        }
                    };
                    self.retry_model_step(submission_id, &started, &model_events)
                        .await?;
                    if input_changed {
                        return Ok(ModelStepRequest::Restart);
                    }
                    match self
                        .wait_active(inbox, turn_id, tokio::time::sleep(delay))
                        .await?
                    {
                        Wait::Ready { input_changed, .. } => {
                            if let Some(interrupt_submission_id) =
                                self.drain_submissions(inbox, turn_id).await?.interrupted
                            {
                                self.abort(
                                    &interrupt_submission_id,
                                    turn_id,
                                    TURN_INTERRUPTED_REASON,
                                    ExecutionOutcome::Aborted,
                                )
                                .await?;
                                return Ok(ModelStepRequest::Finished);
                            }
                            if input_changed
                                || self
                                    .config
                                    .middleware
                                    .messages_ready(&self.state.pending_messages, turn_id)?
                            {
                                return Ok(ModelStepRequest::Restart);
                            }
                        }
                        Wait::Interrupted {
                            submission_id: interrupt_submission_id,
                        } => {
                            self.abort(
                                &interrupt_submission_id,
                                turn_id,
                                TURN_INTERRUPTED_REASON,
                                ExecutionOutcome::Aborted,
                            )
                            .await?;
                            return Ok(ModelStepRequest::Finished);
                        }
                    }
                }
                Ok(Wait::Ready {
                    value: Err(error), ..
                })
                | Err(error) => {
                    self.fail_model_step(submission_id, &started, &model_events, error)
                        .await?;
                    return Ok(ModelStepRequest::Finished);
                }
                Ok(Wait::Interrupted {
                    submission_id: interrupt_submission_id,
                }) => {
                    self.interrupt_model_step(
                        submission_id,
                        &interrupt_submission_id,
                        &started,
                        &model_events,
                    )
                    .await?;
                    return Ok(ModelStepRequest::Finished);
                }
            }
        }
    }

    async fn fail_completed_model_step(
        &mut self,
        submission_id: &Arc<str>,
        mut step: CompletedModelStep,
        error: Error,
    ) -> Result<Option<NormalizedModelStep>> {
        step.streamed.cancel();
        self.fail_model_step(submission_id, &step.started, &step.model_events, error)
            .await?;
        Ok(None)
    }

    async fn normalize_and_persist_model_step(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &Arc<str>,
        turn_id: &Arc<str>,
        rewrite_reasons: &[ContextRewriteReason],
        mut step: CompletedModelStep,
    ) -> Result<Option<NormalizedModelStep>> {
        let provider = Arc::clone(&self.config.provider);
        if let Err(error) = self.record_usage(&provider, &step.output.usage).await {
            return self
                .fail_completed_model_step(submission_id, step, error)
                .await;
        }
        self.state.make_mut().last_usage = Some(step.output.usage);
        if !step.output.tool_calls.starts_with(&step.streamed.originals) {
            return self
                .fail_completed_model_step(
                    submission_id,
                    step,
                    Error::Provider("completed response changed streamed tool calls".into()),
                )
                .await;
        }
        let mut tool_effects = match step.tools.accept_materialized(
            step.output.materialized_tools(),
            turn_id,
            &step.started.model_step_id,
        ) {
            Ok(effects) => effects,
            Err(error) => {
                return self
                    .fail_completed_model_step(submission_id, step, error)
                    .await;
            }
        };
        let mut calls_changed = false;
        let mut executable_calls = Vec::new();
        let mut denied_results = std::mem::take(&mut step.streamed.denied);
        let mut hook_events = std::mem::take(&mut step.streamed.hook_events);
        let mut hook_input = std::mem::take(&mut step.streamed.hook_input);
        let mut prepared_calls = std::mem::take(&mut step.streamed.calls).into_iter();
        for (index, call) in step.output.tool_calls.iter_mut().enumerate() {
            let binding = if let Some(prepared) = prepared_calls.next() {
                calls_changed |= prepared.rewritten;
                if let Some(rewritten) = prepared.call {
                    *call = rewritten;
                }
                if step.streamed.started.contains(&call.call_id)
                    || denied_results
                        .iter()
                        .any(|result| result.call_id == call.call_id)
                {
                    continue;
                }
                prepared.binding
            } else {
                let binding = match self.catalog.bind_in_place(call, &step.tools) {
                    Ok(binding) => binding,
                    Err(error) => {
                        denied_results.push(ToolResult::error(
                            call,
                            error.to_string(),
                            self.config.sandbox.output_limit(),
                        ));
                        continue;
                    }
                };
                let (denial, changed) = match self
                    .prepare_tool_call(turn_id, call, &mut hook_events, &mut hook_input)
                    .await
                {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        return self
                            .fail_completed_model_step(submission_id, step, error)
                            .await;
                    }
                };
                calls_changed |= changed;
                if let Some(denial) = denial {
                    denied_results.push(denial);
                    continue;
                }
                (!changed).then_some(binding)
            };
            // A hook rewrite is new input and is bound on its own.
            let binding =
                match binding.map_or_else(|| self.catalog.bind_in_place(call, &step.tools), Ok) {
                    Ok(binding) => binding,
                    Err(error) => {
                        denied_results.push(ToolResult::error(
                            call,
                            error.to_string(),
                            self.config.sandbox.output_limit(),
                        ));
                        continue;
                    }
                };
            executable_calls.push((index, binding));
        }
        // Repair execution arguments without changing the provider's cached history prefix.
        if calls_changed && let Err(error) = step.output.sync_tool_calls() {
            return self
                .fail_completed_model_step(submission_id, step, error)
                .await;
        }
        if let Some(interrupt_submission_id) =
            self.drain_submissions(inbox, turn_id).await?.interrupted
        {
            step.streamed.cancel();
            self.interrupt_model_step(
                submission_id,
                &interrupt_submission_id,
                &step.started,
                &step.model_events,
            )
            .await?;
            return Ok(None);
        }
        let context_before = self.state.context.len();
        let batch_before = self.transcript_delta.len();
        let mut durable_output = std::mem::take(&mut step.output.output);
        durable_output.append(&mut tool_effects.input);
        insert_before_open_tool_calls(&mut durable_output, hook_input);
        self.extend_context(durable_output);
        let message_index = durable_visible_message_index(
            self.state
                .context
                .get(context_before..)
                .unwrap_or_default()
                .into(),
            self.state.context.as_slice().into(),
            context_before,
        );
        // Recovery owns pending calls before side effects; output still supplies hooks and execution.
        self.state
            .make_mut()
            .pending_tools
            .clone_from(&step.output.tool_calls);
        self.state.make_mut().active_model_step = None;
        let next_model_step = step
            .started
            .step_index
            .checked_add(1)
            .ok_or_else(|| Error::Checkpoint("model step index overflow".into()))?;
        let active = self
            .state
            .make_mut()
            .active_execution
            .as_mut()
            .ok_or_else(|| {
                Error::Checkpoint("completed model step has no active execution".into())
            })?;
        active.next_model_step = next_model_step;
        active.phase = if step.output.end_turn && step.output.tool_calls.is_empty() {
            ExecutionPhase::Completion {
                last_assistant_message: (!step.output.text.is_empty())
                    .then(|| std::mem::take(&mut step.output.text)),
            }
        } else {
            ExecutionPhase::Model
        };
        let diagnostics = self.config.model.model_step_diagnostics(
            &self.config.provider,
            self.state.context_epoch,
            rewrite_reasons
                .iter()
                .map(|reason| reason.as_str().into())
                .collect(),
            &step.output.usage,
        )?;
        let checkpoint_sequence = self
            .state
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::Checkpoint("checkpoint sequence overflow".into()))?;
        let mut model_events = vec![model_step_completed_event(
            submission_id,
            &step.started,
            ModelStepOutcome::Completed {
                end_turn: step.output.end_turn,
                tool_call_ids: step
                    .output
                    .tool_calls
                    .iter()
                    .map(|call| Arc::clone(&call.call_id))
                    .collect(),
                usage: std::mem::take(&mut step.output.usage),
            },
            Some(diagnostics),
        )?];
        if !step.output.content().is_empty() {
            model_events.push(turn_event(
                submission_id,
                EventMsg::AssistantMessage(AssistantMessageEvent {
                    session_id: step.started.session_id.share(),
                    turn_id: Arc::clone(&step.started.turn_id),
                    model_step_id: Arc::clone(&step.started.model_step_id),
                    content: std::mem::take(&mut step.output.content),
                    message_target: message_index.map(|index| MessageTarget {
                        checkpoint_sequence,
                        batch_item_count: std::num::NonZeroUsize::MIN
                            .saturating_add(batch_before + index),
                    }),
                }),
            ));
        }
        model_events.extend(
            tool_effects
                .events
                .into_iter()
                .map(|event| turn_event(submission_id, event)),
        );
        model_events.extend(
            hook_events
                .into_iter()
                .map(|message| turn_event(submission_id, message)),
        );
        if let Some(usage) = self.usage_event(submission_id, None) {
            model_events.push(usage);
        }
        let previous_context_provider = self.state.make_mut().context_model_route.replace(provider);
        if let Err(error) = self.persist_with_events(model_events, None).await {
            self.state.make_mut().context_model_route = previous_context_provider;
            return Err(error);
        }
        Ok(Some(NormalizedModelStep {
            output: step.output,
            executable_calls,
            denied_results,
            streamed: step.streamed,
        }))
    }

    async fn resolve_turn_completion(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &Arc<str>,
        turn_id: &Arc<str>,
    ) -> Result<bool> {
        if let Some(interrupt_submission_id) =
            self.drain_submissions(inbox, turn_id).await?.interrupted
        {
            self.abort(
                &interrupt_submission_id,
                turn_id,
                TURN_INTERRUPTED_REASON,
                ExecutionOutcome::Aborted,
            )
            .await?;
            return Ok(true);
        }
        if self
            .config
            .middleware
            .messages_ready(&self.state.pending_messages, turn_id)?
        {
            self.resume_model_phase()?;
            return Ok(false);
        }

        let mut hook_events = Vec::new();
        let decision = {
            let active = self.state.active_execution.as_ref().ok_or_else(|| {
                Error::Checkpoint("turn completion has no active execution".into())
            })?;
            let ExecutionPhase::Completion {
                last_assistant_message,
            } = &active.phase
            else {
                return Err(Error::Checkpoint(
                    "turn completion resumed outside its durable phase".into(),
                ));
            };
            let mut context = StopContext {
                turn: self.turn_identity(turn_id)?,
                role: &self.runtime.role,
                stop_hook_active: active.stop_hook_active,
                last_assistant_message: last_assistant_message.as_deref(),
                events: &mut hook_events,
                continuation: None,
            };
            self.config.middleware.stop(&mut context).await?;
            context.continuation
        };
        let hook_events = hook_events
            .into_iter()
            .map(|message| turn_event(submission_id, message))
            .collect::<Vec<_>>();
        if let Some(interrupt_submission_id) =
            self.drain_submissions(inbox, turn_id).await?.interrupted
        {
            if !hook_events.is_empty() {
                self.persist_with_events(hook_events, None).await?;
            }
            self.abort(
                &interrupt_submission_id,
                turn_id,
                TURN_INTERRUPTED_REASON,
                ExecutionOutcome::Aborted,
            )
            .await?;
            return Ok(true);
        }
        if self
            .config
            .middleware
            .messages_ready(&self.state.pending_messages, turn_id)?
        {
            self.resume_model_phase()?;
            if !hook_events.is_empty() {
                self.persist_with_events(hook_events, None).await?;
            }
            return Ok(false);
        }
        if let Some(prompt) = decision {
            let active = self
                .state
                .make_mut()
                .active_execution
                .as_mut()
                .ok_or_else(|| {
                    Error::Checkpoint("stop continuation has no active execution".into())
                })?;
            active.phase = ExecutionPhase::Model;
            active.stop_hook_active = true;
            self.push_context(internal_user_message("stop_continuation", &prompt));
            self.persist_with_events(hook_events, None).await?;
            return Ok(false);
        }
        self.complete_turn(submission_id, turn_id, hook_events)
            .await?;
        Ok(true)
    }

    fn resume_model_phase(&mut self) -> Result<()> {
        let active = self
            .state
            .make_mut()
            .active_execution
            .as_mut()
            .ok_or_else(|| Error::Checkpoint("turn continuation has no active execution".into()))?;
        active.phase = ExecutionPhase::Model;
        Ok(())
    }

    async fn authorize_and_execute(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &Arc<str>,
        turn_id: &Arc<str>,
        calls: Vec<ToolCall>,
        bindings: Vec<ToolBinding>,
    ) -> Result<bool> {
        let mutation_call_ids = calls
            .iter()
            .filter(|call| self.catalog.requires_approval(&call.name))
            .map(|call| call.call_id.as_ref());
        let authorization =
            self.config
                .sandbox
                .authorize(&self.runtime.session_id, &calls, mutation_call_ids)?;
        let results = match authorization {
            SandboxAuthorization::Execute(permissions) => {
                let tools = self
                    .execute_tools(
                        inbox,
                        submission_id,
                        turn_id,
                        &calls,
                        Some(bindings),
                        permissions,
                    )
                    .await?;
                let Some(results) = self.ready_or_aborted(tools, turn_id).await? else {
                    return Ok(true);
                };
                results
            }
            SandboxAuthorization::Approval {
                request,
                permissions,
            } => {
                let Some(results) = self
                    .resolve_tool_approval(
                        inbox,
                        submission_id,
                        turn_id,
                        calls,
                        request,
                        permissions,
                    )
                    .await?
                else {
                    return Ok(true);
                };
                results
            }
        };
        self.complete_tool_step(submission_id, turn_id, results)
            .await?;
        Ok(false)
    }

    pub(in crate::agent) async fn continue_turn(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &Arc<str>,
        turn_id: &Arc<str>,
    ) -> Result<()> {
        loop {
            let phase = &self
                .state
                .active_execution
                .as_ref()
                .ok_or_else(|| Error::Checkpoint("continued turn has no active execution".into()))?
                .phase;
            if matches!(phase, ExecutionPhase::Completion { .. }) {
                if self
                    .resolve_turn_completion(inbox, submission_id, turn_id)
                    .await?
                {
                    return Ok(());
                }
                continue;
            }
            if let Some(interrupt_submission_id) =
                self.drain_submissions(inbox, turn_id).await?.interrupted
            {
                self.abort(
                    &interrupt_submission_id,
                    turn_id,
                    TURN_INTERRUPTED_REASON,
                    ExecutionOutcome::Aborted,
                )
                .await?;
                return Ok(());
            }
            let model_step = self
                .state
                .active_execution
                .as_ref()
                .ok_or_else(|| Error::Checkpoint("continued turn has no active execution".into()))?
                .next_model_step;
            if model_step >= self.config.max_model_steps {
                return Err(Error::Stopped(format!(
                    "turn reached the configured limit of {} model steps",
                    self.config.max_model_steps
                )));
            }
            let mut rewrite_reasons = Vec::new();
            let (request_input, tools) = loop {
                match self
                    .prepare_model_phase(inbox, submission_id, turn_id, model_step)
                    .await?
                {
                    PreparedModel::Aborted => return Ok(()),
                    PreparedModel::Idle => {
                        self.complete_turn(submission_id, turn_id, Vec::new())
                            .await?;
                        return Ok(());
                    }
                    PreparedModel::Stopped(reason) => {
                        self.complete_turn(
                            submission_id,
                            turn_id,
                            vec![turn_event(
                                submission_id,
                                EventMsg::Warning(crate::protocol::WarningEvent {
                                    message: reason.into(),
                                }),
                            )],
                        )
                        .await?;
                        return Ok(());
                    }
                    PreparedModel::Repeat(reasons) => {
                        extend_rewrite_reasons(&mut rewrite_reasons, reasons);
                    }
                    PreparedModel::Ready {
                        input,
                        tools,
                        rewrite_reasons: reasons,
                    } => {
                        extend_rewrite_reasons(&mut rewrite_reasons, reasons);
                        break (input, tools);
                    }
                }
            };

            let step = match self
                .request_model_step(
                    inbox,
                    submission_id,
                    turn_id,
                    model_step,
                    request_input.input(),
                    tools,
                )
                .await?
            {
                ModelStepRequest::Completed(step) => *step,
                ModelStepRequest::Restart => continue,
                ModelStepRequest::Finished => return Ok(()),
            };
            // Sampling has finished; release its history snapshot before appending output.
            drop(request_input);
            let Some(NormalizedModelStep {
                output,
                executable_calls,
                denied_results,
                streamed,
            }) = self
                .normalize_and_persist_model_step(
                    inbox,
                    submission_id,
                    turn_id,
                    &rewrite_reasons,
                    step,
                )
                .await?
            else {
                return Ok(());
            };
            if output.tool_calls.is_empty() {
                continue;
            }
            let streamed = self.wait_active(inbox, turn_id, streamed.finish()).await?;
            let Some(streamed) = self.ready_or_aborted(streamed, turn_id).await? else {
                return Ok(());
            };
            let mut completion = streamed?;
            self.post_tool_results(turn_id, &output.tool_calls, &mut completion)
                .await?;
            completion.results.extend(denied_results);
            completion.results = order_results(&output.tool_calls, completion.results);
            self.persist_tool_results(submission_id, turn_id, completion)
                .await?;
            if executable_calls.is_empty() {
                continue;
            }
            // Binding records indexes in order; move calls after result hooks finish borrowing them.
            let (indexes, bindings): (Vec<_>, Vec<_>) = executable_calls.into_iter().unzip();
            let calls = output
                .tool_calls
                .into_iter()
                .enumerate()
                .filter_map(|(index, call)| indexes.binary_search(&index).is_ok().then_some(call))
                .collect();
            if self
                .authorize_and_execute(inbox, submission_id, turn_id, calls, bindings)
                .await?
            {
                return Ok(());
            }
        }
    }
}

fn model_step_completed_event(
    submission_id: &Arc<str>,
    started: &ModelStepStartedEvent,
    outcome: ModelStepOutcome,
    diagnostics: Option<ModelStepDiagnostics>,
) -> Result<Event> {
    Ok(turn_event(
        submission_id,
        EventMsg::ModelStepCompleted(ModelStepCompletedEvent {
            session_id: started.session_id.share(),
            turn_id: Arc::clone(&started.turn_id),
            model_step_id: Arc::clone(&started.model_step_id),
            step_index: started.step_index,
            started_at_ms: started.started_at_ms,
            completed_at_ms: unix_timestamp_ms()?.max(started.started_at_ms),
            outcome,
            diagnostics,
        }),
    ))
}

fn extend_rewrite_reasons(
    collected: &mut Vec<crate::backend::checkpoint::ContextRewriteReason>,
    additional: Vec<crate::backend::checkpoint::ContextRewriteReason>,
) {
    for reason in additional {
        if !collected.contains(&reason) {
            collected.push(reason);
        }
    }
}

fn rebase_live_message_targets(events: &mut [EventMsg], provisional: u64, durable: u64) {
    for target in events
        .iter_mut()
        .filter_map(EventMsg::message_target_mut)
        .filter_map(Option::as_mut)
    {
        if target.checkpoint_sequence == provisional {
            target.checkpoint_sequence = durable;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_tool_input_stays_before_open_calls_for_anthropic_messages() {
        let mut output = vec![
            serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Checking."}]
            }),
            serde_json::json!({
                "type": "function_call",
                "call_id": "call-1",
                "name": "read_file",
                "arguments": "{}"
            }),
        ];
        insert_before_open_tool_calls(
            &mut output,
            vec![internal_user_message("pre_tool_hook", "before")],
        );
        output.push(crate::backend::model::tool_output(
            "call-1",
            &"contents".into(),
            false,
        ));

        assert_eq!(
            output
                .iter()
                .map(|item| {
                    item.get("type")
                        .and_then(Value::as_str)
                        .or_else(|| item.get("role").and_then(Value::as_str))
                })
                .collect::<Vec<_>>(),
            [
                Some("message"),
                Some("user"),
                Some("function_call"),
                Some("function_call_output"),
            ]
        );
    }

    #[test]
    fn retry_waits_never_exceed_the_configured_ceiling() {
        let transport = crate::backend::model::ModelTransportSettings::default();
        let ceiling = Duration::from_millis(transport.stream_retry_max_backoff_ms);
        for hint in [
            "4".to_owned(),
            "3600".to_owned(),
            (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc2822(),
        ] {
            let error = crate::ProviderError::http("busy", 429, Some(hint));
            assert_eq!(
                crate::backend::model::retry_delay(&error, 0, "step", &transport),
                ceiling
            );
        }
        let error = crate::ProviderError::stream_interrupted(None);
        for retry in 0..100 {
            assert!(
                crate::backend::model::retry_delay(&error, retry, "step", &transport) <= ceiling
            );
        }
    }
}
