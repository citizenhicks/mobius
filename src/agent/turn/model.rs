use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use uuid::Uuid;

use super::turn_event;
use crate::agent::input::Wait;
use crate::agent::{Runner, SubmissionInbox, send_event, try_send_event, unix_timestamp_ms};
use crate::backend::checkpoint::{
    ActiveModelStep, ContextRewrite, ContextRewriteReason, ExecutionOutcome, ExecutionPhase,
};
use crate::backend::model::{
    ModelEventSink, ModelOutput, ModelRequest, PromptCacheIdentity, STREAM_RETRY_LIMIT, ToolCall,
    ToolDefinition, durable_visible_message_index, insert_before_open_tool_calls,
    internal_user_message, prompt_cache_key,
};
use crate::backend::sandbox::SandboxAuthorization;
use crate::middleware::tools::{PreparedToolSet, ToolResult};
use crate::middleware::{ModelContext, PreToolUseContext, StopContext};
use crate::protocol::{
    AssistantMessageEvent, Event, EventMsg, MessageTarget, ModelEventTracker,
    ModelStepCompletedEvent, ModelStepDiagnostics, ModelStepOutcome, ModelStepStartedEvent,
    SubmissionRejectedEvent,
};
use crate::{Error, Result};

const STREAM_RETRY_BASE_DELAY_MS: u64 = 200;
const STREAM_RETRY_MAX_DELAY_MS: u64 = 3_200;

/// Outcome of `Runner::prepare_model_phase`.
enum PreparedModel {
    /// An interrupt aborted the turn; `continue_turn` returns.
    Aborted,
    /// Middleware requested normal completion before another model request.
    Stopped(String),
    /// Middleware queued a message during the phase; re-run it before the model call.
    Repeat(Vec<crate::backend::checkpoint::ContextRewriteReason>),
    /// Proceed to the model request.
    Ready {
        input: Vec<Value>,
        tools: Box<PreparedTools>,
        rewrite_reasons: Vec<crate::backend::checkpoint::ContextRewriteReason>,
    },
}

struct PreparedTools {
    direct: Vec<ToolDefinition>,
    deferred: Vec<ToolDefinition>,
    catalog: PreparedToolSet,
}

struct CompletedModelStep {
    started: ModelStepStartedEvent,
    output: ModelOutput,
    tools: PreparedToolSet,
    model_events: ModelEventTracker,
}

enum ModelStepRequest {
    Completed(Box<CompletedModelStep>),
    Restart,
    Finished,
}

impl Runner {
    async fn stage_message_input(&mut self, turn_id: &str) -> Result<()> {
        let mut pending_messages = self.state.pending_messages.clone();
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
        let mut context = self.state.context.clone();
        let mut transcript = Vec::with_capacity(messages.len());
        let mut events = Vec::new();
        for mut message in messages {
            let mut hook_events = Vec::new();
            let submitted = self
                .config
                .middleware
                .message_submit(
                    self.runtime.turn_identity(turn_id),
                    &message,
                    &mut hook_events,
                )
                .await?;
            events.extend(hook_events.into_iter().map(|msg| Event {
                submission_id: Some(message.submission_id.clone()),
                msg,
            }));
            events.extend(message.boundary_events.drain(..).map(|msg| Event {
                submission_id: Some(message.submission_id.clone()),
                msg,
            }));
            if let Some(rejection) = submitted.rejection {
                events.push(Event {
                    submission_id: Some(message.submission_id),
                    msg: EventMsg::SubmissionRejected(SubmissionRejectedEvent {
                        message: rejection,
                    }),
                });
                continue;
            }
            context.extend(submitted.input);
            self.config
                .model
                .prepare_turn_input(&context, &mut message.input);
            let target = message.event.message_target_mut().ok_or_else(|| {
                Error::Checkpoint("prepared input event has no message target".into())
            })?;
            *target = Some(MessageTarget {
                checkpoint_sequence,
                batch_item_count: batch_before + transcript.len() + 1,
            });
            context.push(message.input.clone());
            transcript.push(message.input);
            events.push(Event {
                submission_id: Some(message.submission_id),
                msg: message.event,
            });
        }
        let previous_pending_messages =
            std::mem::replace(&mut self.state.pending_messages, pending_messages);
        let previous_context = std::mem::replace(&mut self.state.context, context);
        let transcript_len = self.transcript_delta.len();
        self.transcript_delta.extend(transcript);
        if let Err(error) = self.persist_with_events(events, None).await {
            self.state.pending_messages = previous_pending_messages;
            self.state.context = previous_context;
            self.transcript_delta.truncate(transcript_len);
            return Err(error);
        }
        Ok(())
    }

    async fn persist_model_hook_changes(
        &mut self,
        submission_id: &str,
        mut middleware_events: Vec<EventMsg>,
        usage_changed: bool,
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
        if usage_changed && let Some(usage) = self.usage_event(submission_id) {
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
        submission_id: &str,
        turn_id: &str,
        model_step: usize,
    ) -> Result<PreparedModel> {
        self.stage_message_input(turn_id).await?;
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
        let mut request_input = self.state.context.clone();
        let mut durable_input = self.state.context.clone();
        let mut transcript_delta = self.transcript_delta.clone();
        let mut context_epoch = self.state.context_epoch;
        let mut compaction_count = self.state.compaction_count;
        let mut available_tools = self.catalog.exposed_names();
        let queued_messages = self.state.pending_messages.clone();
        let model = Arc::clone(&self.config.model);
        let provider = self.config.provider.clone();
        let session_id = self.config.session_id.clone();
        let session_context = self.config.session_context.clone();
        let metadata = self.config.metadata.clone();
        let instructions = Arc::clone(&self.system_prompt);
        let last_usage = self.state.last_usage.clone();
        let catalog = self.catalog.clone();
        let runtime = self.runtime.clone();
        let middleware = self.config.middleware.clone();
        let prepare_model = middleware.prepare_model(ModelContext {
            model: &model,
            provider: &provider,
            session_id: &session_id,
            session_context: &session_context,
            metadata: &metadata,
            turn_id,
            model_step,
            context_window: self.config.context_window,
            instructions: &instructions,
            checkpoint_sequence: self.state.sequence,
            request_input: &mut request_input,
            available_tools: &mut available_tools,
            durable_input: &mut durable_input,
            transcript_delta: &mut transcript_delta,
            context_epoch: &mut context_epoch,
            compaction_count: &mut compaction_count,
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
        });
        let control = self.wait_active(inbox, turn_id, prepare_model).await?;
        let hook_result = match control {
            Wait::Ready { value, .. } => value,
            Wait::Interrupted { submission_id } => {
                self.abort(
                    &submission_id,
                    turn_id,
                    "interrupted",
                    ExecutionOutcome::Aborted,
                )
                .await?;
                return Ok(PreparedModel::Aborted);
            }
        };
        self.state.context = durable_input;
        self.transcript_delta = transcript_delta;
        self.state.context_epoch = context_epoch;
        self.state.compaction_count = compaction_count;
        let usage_changed = !middleware_usage.is_empty();
        if !rewrite_reasons.is_empty() {
            self.state.last_context_rewrite = Some(ContextRewrite {
                epoch: self.state.context_epoch,
                reasons: rewrite_reasons.clone(),
            });
        }
        if usage_changed {
            let route = self.config.provider.clone();
            for usage in &middleware_usage {
                self.record_usage(&route, usage)?;
                self.state.last_usage = Some(usage.clone());
            }
        }
        checkpoint_changed |= usage_changed;
        let messages_ready = self
            .config
            .middleware
            .messages_ready(&self.state.pending_messages, turn_id)?;
        self.persist_model_hook_changes(
            submission_id,
            middleware_events,
            usage_changed,
            checkpoint_changed,
            provisional_target_sequence,
        )
        .await?;
        hook_result?;
        if messages_ready {
            return Ok(PreparedModel::Repeat(rewrite_reasons));
        }
        if let Some(reason) = turn_stop {
            return Ok(PreparedModel::Stopped(reason));
        }
        Ok(PreparedModel::Ready {
            tools: Box::new(self.prepare_tools(&request_input, available_tools)?),
            input: request_input,
            rewrite_reasons,
        })
    }

    fn prepare_tools(&self, input: &[Value], available: BTreeSet<String>) -> Result<PreparedTools> {
        let catalog = self.catalog.prepare(input, available)?;
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
        })
    }

    pub(in crate::agent) async fn live_tools(&self) -> Result<PreparedToolSet> {
        let mut available = self.catalog.exposed_names();
        self.config
            .middleware
            .resolve_tool_exposure(&self.config.session_id, &self.state.context, &mut available)
            .await?;
        self.catalog.prepare(&self.state.context, available)
    }

    fn model_step_terminal_events(
        submission_id: &str,
        started: &ModelStepStartedEvent,
        outcome: ModelStepOutcome,
        model_events: &ModelEventTracker,
    ) -> Result<Vec<Event>> {
        let mut events = model_events
            .interrupted()?
            .into_iter()
            .map(|event| {
                turn_event(
                    submission_id,
                    event.into_event(
                        &started.session_id,
                        &started.turn_id,
                        &started.model_step_id,
                    ),
                )
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
        submission_id: &str,
        started: &ModelStepStartedEvent,
        model_events: &ModelEventTracker,
    ) -> Result<()> {
        let events = Self::model_step_terminal_events(
            submission_id,
            started,
            ModelStepOutcome::Retrying,
            model_events,
        )?;
        let active_model_step = self.state.active_model_step.take();
        match self.persist_with_events(events, None).await {
            Ok(_) => Ok(()),
            Err(error) => {
                self.state.active_model_step = active_model_step;
                Err(error)
            }
        }
    }

    async fn fail_model_step(
        &mut self,
        submission_id: &str,
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
        submission_id: &str,
        interrupt_submission_id: &str,
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
            "interrupted",
            ExecutionOutcome::Aborted,
            events,
        )
        .await
    }

    async fn request_model_step(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &str,
        turn_id: &str,
        model_step: usize,
        request_input: &[Value],
        tools: &PreparedTools,
    ) -> Result<ModelStepRequest> {
        let model = Arc::clone(&self.config.model);
        let provider = self.config.provider.clone();
        let model_session_id = self.state.session_id.clone();
        let cache_key = prompt_cache_key(&model_session_id);
        let instructions = Arc::clone(&self.system_prompt);
        let mut stream_retries = 0;
        loop {
            let started = ModelStepStartedEvent {
                session_id: self.state.session_id.clone(),
                turn_id: turn_id.to_string(),
                model_step_id: Uuid::new_v4().to_string(),
                step_index: model_step,
                started_at_ms: unix_timestamp_ms()?,
            };
            self.record_model_call()?;
            self.state.active_model_step = Some(ActiveModelStep {
                model_step_id: started.model_step_id.clone(),
                step_index: started.step_index,
                started_at_ms: started.started_at_ms,
            });
            self.persist_with_events(
                vec![turn_event(
                    submission_id,
                    EventMsg::ModelStepStarted(started.clone()),
                )],
                None,
            )
            .await?;
            let events = self.events.clone();
            let event_submission_id = submission_id.to_string();
            let event_turn_id = turn_id.to_string();
            let event_session_id = self.state.session_id.clone();
            let event_model_step_id = started.model_step_id.clone();
            let model_events = ModelEventTracker::default();
            let streamed_events = model_events.clone();
            let catalog_revision = self.catalog.revision()?.to_owned();
            let stream: ModelEventSink = Arc::new(move |event| {
                streamed_events.observe(&event)?;
                let msg = event.into_event(&event_session_id, &event_turn_id, &event_model_step_id);
                try_send_event(
                    &events,
                    Event {
                        submission_id: Some(event_submission_id.clone()),
                        msg,
                    },
                )
            });
            let response = model.respond(
                &provider,
                ModelRequest {
                    session_id: &model_session_id,
                    prompt_cache: Some(PromptCacheIdentity {
                        key: &cache_key,
                        context_epoch: self.state.context_epoch,
                    }),
                    instructions: &instructions,
                    input: request_input,
                    catalog_revision: &catalog_revision,
                    tools: &tools.direct,
                    deferred_tools: &tools.deferred,
                    allow_hosted_tools: true,
                    allow_continuation: true,
                },
                stream,
            );
            match self.wait_active(inbox, turn_id, response).await {
                Ok(Wait::Ready {
                    value: Ok(output), ..
                }) => {
                    return Ok(ModelStepRequest::Completed(Box::new(CompletedModelStep {
                        started,
                        output,
                        tools: tools.catalog.clone(),
                        model_events,
                    })));
                }
                Ok(Wait::Ready {
                    value: Err(Error::Provider(error)),
                    input_changed,
                }) if error.is_stream_interrupted() && stream_retries < STREAM_RETRY_LIMIT => {
                    let delay = stream_retry_delay(&error, stream_retries, &started.model_step_id);
                    self.retry_model_step(submission_id, &started, &model_events)
                        .await?;
                    stream_retries += 1;
                    if input_changed {
                        return Ok(ModelStepRequest::Restart);
                    }
                    match self
                        .wait_active(inbox, turn_id, tokio::time::sleep(delay))
                        .await?
                    {
                        Wait::Ready { input_changed, .. } => {
                            if let Some(interrupt_submission_id) =
                                self.drain_submissions(inbox, turn_id).await?
                            {
                                self.abort(
                                    &interrupt_submission_id,
                                    turn_id,
                                    "interrupted",
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
                                "interrupted",
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

    async fn normalize_and_persist_model_step(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &str,
        turn_id: &str,
        rewrite_reasons: &[ContextRewriteReason],
        mut step: CompletedModelStep,
    ) -> Result<Option<(ModelOutput, Vec<ToolCall>, Vec<ToolResult>)>> {
        let provider = self.config.provider.clone();
        if let Err(error) = self.record_usage(&provider, &step.output.usage) {
            self.fail_model_step(submission_id, &step.started, &step.model_events, error)
                .await?;
            return Ok(None);
        }
        self.state.last_usage = Some(step.output.usage.clone());
        let mut tool_effects = match step.tools.accept_materialized(
            step.output.materialized_tools(),
            turn_id,
            &step.started.model_step_id,
        ) {
            Ok(effects) => effects,
            Err(error) => {
                self.fail_model_step(submission_id, &step.started, &step.model_events, error)
                    .await?;
                return Ok(None);
            }
        };
        let original_tool_calls = step.output.tool_calls.clone();
        let mut executable_calls = Vec::new();
        let mut denied_results = Vec::new();
        let mut hook_events = Vec::new();
        let mut hook_input = Vec::new();
        for call in &mut step.output.tool_calls {
            if let Err(error) = self.catalog.bind_prepared(call.clone(), &step.tools) {
                denied_results.push(ToolResult::error(call, error.to_string()));
                continue;
            }
            let mut context = PreToolUseContext {
                turn: self.runtime.turn_identity(turn_id),
                events: &mut hook_events,
                tools: &self.catalog,
                call,
                input: Vec::new(),
                denial: None,
            };
            if let Err(error) = self.config.middleware.pre_tool_use(&mut context).await {
                self.fail_model_step(submission_id, &step.started, &step.model_events, error)
                    .await?;
                return Ok(None);
            }
            let denial = context.denial().map(str::to_owned);
            hook_input.append(&mut context.input);
            if let Some(reason) = denial {
                denied_results.push(ToolResult::error(
                    context.call(),
                    format!("tool call denied: {reason}"),
                ));
            } else {
                match self.catalog.bind_prepared(call.clone(), &step.tools) {
                    Ok(call) => executable_calls.push(call.into_call()),
                    Err(error) => {
                        denied_results.push(ToolResult::error(call, error.to_string()));
                    }
                }
            }
        }
        if step.output.tool_calls != original_tool_calls
            && let Err(error) = step.output.sync_tool_calls()
        {
            self.fail_model_step(submission_id, &step.started, &step.model_events, error)
                .await?;
            return Ok(None);
        }
        if let Some(interrupt_submission_id) = self.drain_submissions(inbox, turn_id).await? {
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
        let mut durable_output = step.output.output.clone();
        durable_output.append(&mut tool_effects.input);
        insert_before_open_tool_calls(&mut durable_output, hook_input);
        self.extend_context(durable_output);
        let message_index = durable_visible_message_index(
            &self.state.context[context_before..],
            &self.state.context,
            context_before,
        );
        self.state.pending_tools.clone_from(&step.output.tool_calls);
        self.state.active_model_step = None;
        let next_model_step = step
            .started
            .step_index
            .checked_add(1)
            .ok_or_else(|| Error::Checkpoint("model step index overflow".into()))?;
        let active = self.state.active_execution.as_mut().ok_or_else(|| {
            Error::Checkpoint("completed model step has no active execution".into())
        })?;
        active.next_model_step = next_model_step;
        active.phase = if step.output.end_turn && step.output.tool_calls.is_empty() {
            ExecutionPhase::Completion {
                last_assistant_message: (!step.output.text.is_empty())
                    .then(|| step.output.text.clone()),
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
                    .map(|call| call.call_id.clone())
                    .collect(),
                usage: step.output.usage.clone(),
            },
            Some(diagnostics),
        )?];
        if !step.output.content().is_empty() {
            model_events.push(turn_event(
                submission_id,
                EventMsg::AssistantMessage(AssistantMessageEvent {
                    session_id: self.state.session_id.clone(),
                    turn_id: turn_id.to_string(),
                    model_step_id: step.started.model_step_id.clone(),
                    content: step.output.content().to_vec(),
                    message_target: message_index.map(|index| MessageTarget {
                        checkpoint_sequence,
                        batch_item_count: batch_before + index + 1,
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
        if let Some(usage) = self.usage_event(submission_id) {
            model_events.push(usage);
        }
        self.persist_with_events(model_events, None).await?;
        Ok(Some((step.output, executable_calls, denied_results)))
    }

    async fn resolve_turn_completion(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &str,
        turn_id: &str,
    ) -> Result<bool> {
        let (last_assistant_message, stop_hook_active) = {
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
            (last_assistant_message.clone(), active.stop_hook_active)
        };
        if let Some(interrupt_submission_id) = self.drain_submissions(inbox, turn_id).await? {
            self.abort(
                &interrupt_submission_id,
                turn_id,
                "interrupted",
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
            let mut context = StopContext {
                turn: self.runtime.turn_identity(turn_id),
                role: &self.runtime.role,
                stop_hook_active,
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
        if let Some(interrupt_submission_id) = self.drain_submissions(inbox, turn_id).await? {
            if !hook_events.is_empty() {
                self.persist_with_events(hook_events, None).await?;
            }
            self.abort(
                &interrupt_submission_id,
                turn_id,
                "interrupted",
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
            let active = self.state.active_execution.as_mut().ok_or_else(|| {
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
        let active =
            self.state.active_execution.as_mut().ok_or_else(|| {
                Error::Checkpoint("turn continuation has no active execution".into())
            })?;
        active.phase = ExecutionPhase::Model;
        Ok(())
    }

    async fn authorize_and_execute(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &str,
        turn_id: &str,
        calls: Vec<ToolCall>,
    ) -> Result<bool> {
        let live_tools = self.live_tools().await?;
        let (live_calls, unavailable_results) = self.catalog.bind_live_batch(&calls, &live_tools);
        if !unavailable_results.is_empty() {
            self.persist_tool_results(submission_id, turn_id, unavailable_results)
                .await?;
        }
        let calls = live_calls
            .into_iter()
            .map(|call| call.into_call())
            .collect::<Vec<_>>();
        if calls.is_empty() {
            return Ok(false);
        }
        let mutation_call_ids = calls
            .iter()
            .filter(|call| self.catalog.requires_approval(&call.name))
            .map(|call| call.call_id.clone())
            .collect::<Vec<_>>();
        let authorization =
            self.config
                .sandbox
                .authorize(&self.config.session_id, &calls, &mutation_call_ids)?;
        let results = match authorization {
            SandboxAuthorization::Execute(permissions) => {
                let tools = self
                    .execute_tools(inbox, submission_id, turn_id, &calls, permissions)
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
        submission_id: String,
        turn_id: String,
    ) -> Result<()> {
        loop {
            let phase = self
                .state
                .active_execution
                .as_ref()
                .ok_or_else(|| Error::Checkpoint("continued turn has no active execution".into()))?
                .phase
                .clone();
            if matches!(phase, ExecutionPhase::Completion { .. }) {
                if self
                    .resolve_turn_completion(inbox, &submission_id, &turn_id)
                    .await?
                {
                    return Ok(());
                }
                continue;
            }
            if let Some(interrupt_submission_id) = self.drain_submissions(inbox, &turn_id).await? {
                self.abort(
                    &interrupt_submission_id,
                    &turn_id,
                    "interrupted",
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
            let request_input = loop {
                match self
                    .prepare_model_phase(inbox, &submission_id, &turn_id, model_step)
                    .await?
                {
                    PreparedModel::Aborted => return Ok(()),
                    PreparedModel::Stopped(reason) => {
                        self.complete_turn(
                            &submission_id,
                            &turn_id,
                            vec![turn_event(
                                &submission_id,
                                EventMsg::Warning(crate::protocol::WarningEvent {
                                    message: reason,
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
                    &submission_id,
                    &turn_id,
                    model_step,
                    &request_input.0,
                    &request_input.1,
                )
                .await?
            {
                ModelStepRequest::Completed(step) => *step,
                ModelStepRequest::Restart => continue,
                ModelStepRequest::Finished => return Ok(()),
            };
            let Some((output, executable_calls, denied_results)) = self
                .normalize_and_persist_model_step(
                    inbox,
                    &submission_id,
                    &turn_id,
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
            if !denied_results.is_empty() {
                self.persist_tool_results(&submission_id, &turn_id, denied_results)
                    .await?;
            }
            if executable_calls.is_empty() {
                continue;
            }
            if self
                .authorize_and_execute(inbox, &submission_id, &turn_id, executable_calls)
                .await?
            {
                return Ok(());
            }
        }
    }
}

fn model_step_completed_event(
    submission_id: &str,
    started: &ModelStepStartedEvent,
    outcome: ModelStepOutcome,
    diagnostics: Option<ModelStepDiagnostics>,
) -> Result<Event> {
    Ok(turn_event(
        submission_id,
        EventMsg::ModelStepCompleted(ModelStepCompletedEvent {
            session_id: started.session_id.clone(),
            turn_id: started.turn_id.clone(),
            model_step_id: started.model_step_id.clone(),
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

fn stream_retry_delay(error: &crate::ProviderError, retry: usize, model_step_id: &str) -> Duration {
    let exponential_ms = STREAM_RETRY_BASE_DELAY_MS
        .saturating_mul(1_u64 << retry.min(4))
        .min(STREAM_RETRY_MAX_DELAY_MS);
    let jitter = model_step_id.bytes().fold(retry as u64, |value, byte| {
        value.wrapping_mul(16_777_619).wrapping_add(u64::from(byte))
    });
    let jitter_percent = 80 + jitter % 41;
    let backoff = Duration::from_millis(exponential_ms.saturating_mul(jitter_percent) / 100);
    error
        .retry_after()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .map_or(backoff, |retry_after| retry_after.max(backoff))
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
            "call-1", "contents", false,
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
    fn stream_retry_delay_respects_server_seconds() {
        let error = crate::ProviderError::stream_interrupted(Some("30".into()));

        assert_eq!(
            stream_retry_delay(&error, 0, "step-1"),
            Duration::from_secs(30)
        );
    }
}
