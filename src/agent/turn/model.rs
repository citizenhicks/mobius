use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::turn_event;
use crate::agent::input::{ActiveRoute, ActiveTurnRouter, Wait};
use crate::agent::{EventRecorder, Runner, send_event, try_send_event, unix_timestamp_ms};
use crate::backend::checkpoint::{
    ActiveModelStep, Checkpoint, ContextRewrite, ContextRewriteReason, ExecutionOutcome,
};
use crate::backend::model::{
    ModelEventSink, ModelOutput, ModelRequest, PromptCacheIdentity, STREAM_RETRY_LIMIT,
    TOOLS_SEARCH_NAME, ToolCall, ToolDefinition, ToolLoad, internal_user_message, prompt_cache_key,
};
use crate::backend::sandbox::SandboxAuthorization;
use crate::middleware::tools::{Catalog, ToolResult};
use crate::middleware::{
    ModelContext, PreToolUseContext, QueuedInputBaseline, QueuedInputQueue, StopContext,
};
use crate::protocol::{
    AgentMessageEvent, AgentMessagePhase, Event, EventMsg, MessageTarget, ModelStepCompletedEvent,
    ModelStepDiagnostics, ModelStepOutcome, ModelStepStartedEvent, Submission, WebSearchAction,
    WebSearchEndEvent, tool_complete_boundaries,
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
    /// Middleware queued input during the phase; re-run it before the model call.
    Repeat(Vec<crate::backend::checkpoint::ContextRewriteReason>),
    /// Proceed to the model request.
    Ready {
        input: Vec<Value>,
        tools: PreparedTools,
        rewrite_reasons: Vec<crate::backend::checkpoint::ContextRewriteReason>,
    },
}

enum ModelHookOutcome {
    Error(Error),
    Interrupted(String),
    Stopped(String),
    Repeat,
    Ready,
}

struct PreparedTools {
    direct: Vec<ToolDefinition>,
    deferred: Vec<ToolDefinition>,
    available: BTreeSet<String>,
    searchable: BTreeSet<String>,
    materialized: BTreeSet<String>,
}

struct CompletedModelStep {
    started: ModelStepStartedEvent,
    output: ModelOutput,
    available: BTreeSet<String>,
    searchable: BTreeSet<String>,
    materialized: BTreeSet<String>,
    pending_searches: Arc<Mutex<Vec<String>>>,
}

impl Runner {
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
        commands: &mut mpsc::Receiver<Submission>,
        submission_id: &str,
        turn_id: &str,
        model_step: usize,
    ) -> Result<PreparedModel> {
        let mut middleware_events = Vec::new();
        let mut middleware_usage = Vec::new();
        let provisional_target_sequence = self
            .state
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::Checkpoint("checkpoint sequence overflow".into()))?;
        let queued_before = QueuedInputBaseline::from_items(&self.state.pending_input);
        let had_queued_input = !self.state.pending_input.is_empty();
        let mut durable_snapshot = self.state.clone();
        let original_pending_count = durable_snapshot.pending_input.len();
        let recorder = self.events.clone();
        let active_events = self.events.clone();
        let mut checkpoint_changed = false;
        let mut rewrite_reasons = Vec::new();
        let mut turn_stop = None;
        let mut request_input = self.state.context.clone();
        let mut available_tools = exposed_tool_names(&self.catalog);
        let (control, mut queued_during_middleware, queue_changed) = {
            let mut queued_during_middleware = Vec::new();
            let mut queue_changed = false;
            let prepare_model = self.config.middleware.prepare_model(ModelContext {
                model: &self.config.model,
                provider: &self.config.provider,
                session_id: &self.config.session_id,
                session_context: &self.config.session_context,
                metadata: &self.config.metadata,
                turn_id,
                model_step,
                context_window: self.config.context_window,
                instructions: &self.system_prompt,
                checkpoint_sequence: self.state.sequence,
                request_input: &mut request_input,
                available_tools: &mut available_tools,
                durable_input: &mut self.state.context,
                transcript_delta: &mut self.transcript_delta,
                context_epoch: &mut self.state.context_epoch,
                compaction_count: &mut self.state.compaction_count,
                rewrite_reasons: &mut rewrite_reasons,
                turn_stop: &mut turn_stop,
                queued_input: QueuedInputQueue::new(
                    &mut self.state.pending_input,
                    QueuedInputBaseline::default(),
                ),
                last_usage: self.state.last_usage.as_ref(),
                tools: &self.catalog,
                events: &mut middleware_events,
                usage: &mut middleware_usage,
                checkpoint_changed: &mut checkpoint_changed,
                runtime: &self.runtime,
                hooks: &self.config.middleware,
            });
            tokio::pin!(prepare_model);
            let control = loop {
                tokio::select! {
                    output = &mut prepare_model => break Wait::Ready(output),
                    submission = commands.recv() => {
                        let Some(submission) = submission else {
                            return Err(Error::Stopped("frontend disconnected".into()));
                        };
                        let route = (ActiveTurnRouter {
                            middleware: &self.config.middleware,
                            session_id: &self.config.session_id,
                            metadata: &self.config.metadata,
                            turn_id,
                            queued_input: &mut queued_during_middleware,
                            queued_before: queued_before.clone(),
                            deferred: &mut self.deferred,
                            events: &active_events,
                            expected_approval: None,
                        })
                        .route(submission)
                        .await?;
                        match route {
                            ActiveRoute::Accepted(change) | ActiveRoute::Changed(change) => {
                                durable_snapshot.pending_input.truncate(original_pending_count);
                                durable_snapshot
                                    .pending_input
                                    .extend(queued_during_middleware.iter().cloned());
                                persist_queue_snapshot(
                                    &recorder,
                                    &mut durable_snapshot,
                                    change.into_events(),
                                )
                                .await?;
                                queue_changed = true;
                            }
                            ActiveRoute::Interrupted { submission_id } => {
                                break Wait::Interrupted { submission_id };
                            }
                            ActiveRoute::Continue | ActiveRoute::Approval { .. } => {}
                        }
                    }
                }
            };
            (control, queued_during_middleware, queue_changed)
        };
        self.state.sequence = durable_snapshot.sequence;
        self.state
            .pending_input
            .append(&mut queued_during_middleware);
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
        checkpoint_changed |= usage_changed || had_queued_input || queue_changed;
        let outcome = model_hook_outcome(
            control,
            turn_stop,
            queue_changed,
            !self.state.pending_input.is_empty(),
        )?;
        self.persist_model_hook_changes(
            submission_id,
            middleware_events,
            usage_changed,
            checkpoint_changed,
            provisional_target_sequence,
        )
        .await?;
        match outcome {
            ModelHookOutcome::Error(error) => Err(error),
            ModelHookOutcome::Interrupted(interrupt_submission_id) => {
                self.abort(
                    &interrupt_submission_id,
                    turn_id,
                    "interrupted",
                    ExecutionOutcome::Aborted,
                )
                .await?;
                Ok(PreparedModel::Aborted)
            }
            ModelHookOutcome::Stopped(reason) => Ok(PreparedModel::Stopped(reason)),
            ModelHookOutcome::Repeat => Ok(PreparedModel::Repeat(rewrite_reasons)),
            ModelHookOutcome::Ready => Ok(PreparedModel::Ready {
                tools: self.prepare_tools(&request_input, available_tools)?,
                input: request_input,
                rewrite_reasons,
            }),
        }
    }

    fn prepare_tools(
        &self,
        input: &[Value],
        mut available: BTreeSet<String>,
    ) -> Result<PreparedTools> {
        let deferred = self
            .catalog
            .deferred_definitions()
            .iter()
            .filter(|tool| available.contains(&tool.name))
            .cloned()
            .collect::<Vec<_>>();
        let searchable = deferred
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<BTreeSet<_>>();
        if searchable.is_empty() {
            available.remove(TOOLS_SEARCH_NAME);
        }
        let materialized = loaded_tools(input, self.catalog.revision()?, &searchable)?;
        let mut direct = self
            .catalog
            .direct_definitions()
            .iter()
            .filter(|tool| available.contains(&tool.name))
            .cloned()
            .collect::<Vec<_>>();
        let deferred = match self.config.model.tool_discovery(&self.config.provider)? {
            crate::protocol::ToolDiscoveryMode::Native => deferred,
            crate::protocol::ToolDiscoveryMode::Rebuild => {
                direct.extend(
                    deferred
                        .iter()
                        .filter(|tool| materialized.contains(&tool.name))
                        .cloned(),
                );
                Vec::new()
            }
        };
        Ok(PreparedTools {
            direct,
            deferred,
            available,
            searchable,
            materialized,
        })
    }

    pub(in crate::agent) async fn live_tool_sets(
        &self,
    ) -> Result<(BTreeSet<String>, BTreeSet<String>, BTreeSet<String>)> {
        let mut available = exposed_tool_names(&self.catalog);
        self.config
            .middleware
            .resolve_tool_exposure(&self.config.session_id, &self.state.context, &mut available)
            .await?;
        let searchable = self
            .catalog
            .deferred_definitions()
            .iter()
            .filter(|tool| available.contains(&tool.name))
            .map(|tool| tool.name.clone())
            .collect::<BTreeSet<_>>();
        if searchable.is_empty() {
            available.remove(TOOLS_SEARCH_NAME);
        }
        let materialized =
            loaded_tools(&self.state.context, self.catalog.revision()?, &searchable)?;
        Ok((available, searchable, materialized))
    }

    async fn fail_model_step(
        &mut self,
        submission_id: &str,
        started: &ModelStepStartedEvent,
        outcome: ModelStepOutcome,
        pending_searches: &Mutex<Vec<String>>,
    ) -> Result<()> {
        self.state.active_model_step = None;
        // A failed step can never complete the hosted searches it started, so the
        // backend closes them out instead of leaving every frontend to infer it.
        let dangling = pending_searches
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default();
        let mut events: Vec<Event> = dangling
            .into_iter()
            .map(|call_id| {
                turn_event(
                    submission_id,
                    EventMsg::WebSearchEnd(WebSearchEndEvent {
                        session_id: started.session_id.clone(),
                        turn_id: started.turn_id.clone(),
                        model_step_id: started.model_step_id.clone(),
                        call_id,
                        action: WebSearchAction::Interrupted,
                    }),
                )
            })
            .collect();
        events.push(model_step_completed_event(
            submission_id,
            started,
            outcome,
            None,
        )?);
        self.persist_with_events(events, None).await?;
        Ok(())
    }

    async fn request_model_step(
        &mut self,
        commands: &mut mpsc::Receiver<Submission>,
        submission_id: &str,
        turn_id: &str,
        model_step: usize,
        request_input: &[Value],
        tools: &PreparedTools,
    ) -> Result<Option<CompletedModelStep>> {
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
            let pending_searches = Arc::new(Mutex::new(Vec::<String>::new()));
            let tracked_searches = Arc::clone(&pending_searches);
            let catalog_revision = self.catalog.revision()?.to_owned();
            let stream: ModelEventSink = Arc::new(move |event| {
                track_pending_searches(&tracked_searches, &event);
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
            match self.wait_active(commands, turn_id, response).await {
                Ok(Wait::Ready(Ok(output))) => {
                    if !output.materialized_tools().is_subset(&tools.searchable) {
                        self.fail_model_step(
                            submission_id,
                            &started,
                            ModelStepOutcome::Failed,
                            &pending_searches,
                        )
                        .await?;
                        return Err(Error::Provider(
                            "provider materialized a tool outside the searchable catalog".into(),
                        ));
                    }
                    let mut materialized = tools.materialized.clone();
                    materialized.extend(output.materialized_tools().iter().cloned());
                    return Ok(Some(CompletedModelStep {
                        started,
                        output,
                        available: tools.available.clone(),
                        searchable: tools.searchable.clone(),
                        materialized,
                        pending_searches,
                    }));
                }
                Ok(Wait::Ready(Err(Error::Provider(error))))
                    if error.is_stream_interrupted() && stream_retries < STREAM_RETRY_LIMIT =>
                {
                    let delay = stream_retry_delay(&error, stream_retries, &started.model_step_id);
                    self.fail_model_step(
                        submission_id,
                        &started,
                        ModelStepOutcome::Retrying,
                        &pending_searches,
                    )
                    .await?;
                    stream_retries += 1;
                    match self
                        .wait_active(commands, turn_id, tokio::time::sleep(delay))
                        .await?
                    {
                        Wait::Ready(()) => {
                            if let Some(interrupt_submission_id) =
                                self.drain_commands(commands, turn_id).await?
                            {
                                self.abort(
                                    &interrupt_submission_id,
                                    turn_id,
                                    "interrupted",
                                    ExecutionOutcome::Aborted,
                                )
                                .await?;
                                return Ok(None);
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
                            return Ok(None);
                        }
                    }
                }
                Ok(Wait::Ready(Err(error))) | Err(error) => {
                    self.fail_model_step(
                        submission_id,
                        &started,
                        ModelStepOutcome::Failed,
                        &pending_searches,
                    )
                    .await?;
                    return Err(error);
                }
                Ok(Wait::Interrupted {
                    submission_id: interrupt_submission_id,
                }) => {
                    self.fail_model_step(
                        submission_id,
                        &started,
                        ModelStepOutcome::Interrupted,
                        &pending_searches,
                    )
                    .await?;
                    self.abort(
                        &interrupt_submission_id,
                        turn_id,
                        "interrupted",
                        ExecutionOutcome::Aborted,
                    )
                    .await?;
                    return Ok(None);
                }
            }
        }
    }

    async fn normalize_and_persist_model_step(
        &mut self,
        submission_id: &str,
        turn_id: &str,
        rewrite_reasons: &[ContextRewriteReason],
        mut step: CompletedModelStep,
    ) -> Result<(ModelOutput, Vec<ToolCall>, Vec<ToolResult>)> {
        let provider = self.config.provider.clone();
        if let Err(error) = self.record_usage(&provider, &step.output.usage) {
            self.fail_model_step(
                submission_id,
                &step.started,
                ModelStepOutcome::Failed,
                &step.pending_searches,
            )
            .await?;
            return Err(error);
        }
        self.state.last_usage = Some(step.output.usage.clone());
        let original_tool_calls = step.output.tool_calls.clone();
        let mut executable_calls = Vec::new();
        let mut denied_results = Vec::new();
        let mut hook_events = Vec::new();
        let mut hook_input = Vec::new();
        for call in &mut step.output.tool_calls {
            if let Err(error) = bind_prepared_call(
                &self.catalog,
                call.clone(),
                &step.available,
                &step.materialized,
                &step.searchable,
            ) {
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
                self.fail_model_step(
                    submission_id,
                    &step.started,
                    ModelStepOutcome::Failed,
                    &step.pending_searches,
                )
                .await?;
                return Err(error);
            }
            let denial = context.denial().map(str::to_owned);
            hook_input.append(&mut context.input);
            if let Some(reason) = denial {
                denied_results.push(ToolResult::error(
                    context.call(),
                    format!("tool call denied: {reason}"),
                ));
            } else {
                match bind_prepared_call(
                    &self.catalog,
                    call.clone(),
                    &step.available,
                    &step.materialized,
                    &step.searchable,
                ) {
                    Ok(call) => executable_calls.push(call),
                    Err(error) => {
                        denied_results.push(ToolResult::error(call, error.to_string()));
                    }
                }
            }
        }
        if step.output.tool_calls != original_tool_calls
            && let Err(error) = step.output.sync_tool_calls()
        {
            self.fail_model_step(
                submission_id,
                &step.started,
                ModelStepOutcome::Failed,
                &step.pending_searches,
            )
            .await?;
            return Err(error);
        }
        let context_before = self.state.context.len();
        let batch_before = self.transcript_delta.len();
        let mut durable_output = step.output.output.clone();
        if !step.output.materialized_tools().is_empty() {
            durable_output.push(
                ToolLoad {
                    catalog_revision: self.catalog.revision()?.into(),
                    tools: step.output.materialized_tools().iter().cloned().collect(),
                }
                .into_input(),
            );
        }
        insert_pre_tool_input(&mut durable_output, hook_input);
        let message_index = durable_output.iter().rposition(has_visible_output_text);
        self.extend_context(durable_output);
        let message_boundary = message_index.map(|index| context_before + index + 1);
        let message_is_safe = message_boundary.is_some_and(|boundary| {
            tool_complete_boundaries(&self.state.context)
                .binary_search(&boundary)
                .is_ok()
        });
        self.state.pending_tools.clone_from(&step.output.tool_calls);
        self.state.active_model_step = None;
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
                content: step.output.content().to_vec(),
            },
            Some(diagnostics),
        )?];
        model_events.extend(
            hook_events
                .into_iter()
                .map(|message| turn_event(submission_id, message)),
        );
        if !step.output.text.is_empty() {
            model_events.push(turn_event(
                submission_id,
                EventMsg::AgentMessage(AgentMessageEvent {
                    session_id: self.state.session_id.clone(),
                    turn_id: turn_id.to_string(),
                    model_step_id: step.started.model_step_id.clone(),
                    message: step.output.text.clone(),
                    phase: AgentMessagePhase::FinalAnswer,
                    message_target: message_index.filter(|_| message_is_safe).map(|index| {
                        MessageTarget {
                            checkpoint_sequence,
                            batch_item_count: batch_before + index + 1,
                        }
                    }),
                }),
            ));
        }
        if let Some(usage) = self.usage_event(submission_id) {
            model_events.push(usage);
        }
        self.persist_with_events(model_events, None).await?;
        Ok((step.output, executable_calls, denied_results))
    }

    async fn finish_toolless_step(
        &mut self,
        commands: &mut mpsc::Receiver<Submission>,
        submission_id: &str,
        turn_id: &str,
        output: &ModelOutput,
        stop_continued: &mut bool,
    ) -> Result<bool> {
        if !output.end_turn {
            return Ok(false);
        }
        if let Some(interrupt_submission_id) = self.drain_commands(commands, turn_id).await? {
            self.abort(
                &interrupt_submission_id,
                turn_id,
                "interrupted",
                ExecutionOutcome::Aborted,
            )
            .await?;
            return Ok(true);
        }
        if !self.state.pending_input.is_empty() {
            return Ok(false);
        }

        let mut hook_events = Vec::new();
        let decision = {
            let mut context = StopContext {
                turn: self.runtime.turn_identity(turn_id),
                role: &self.runtime.role,
                stop_hook_active: *stop_continued,
                last_assistant_message: (!output.text().is_empty()).then_some(output.text()),
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
        if let Some(interrupt_submission_id) = self.drain_commands(commands, turn_id).await? {
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
        if !self.state.pending_input.is_empty() {
            if !hook_events.is_empty() {
                self.persist_with_events(hook_events, None).await?;
            }
            return Ok(false);
        }
        if let Some(prompt) = decision {
            *stop_continued = true;
            self.push_context(internal_user_message("stop_continuation", &prompt));
            self.persist_with_events(hook_events, None).await?;
            return Ok(false);
        }
        self.complete_turn(submission_id, turn_id, hook_events)
            .await?;
        Ok(true)
    }

    async fn authorize_and_execute(
        &mut self,
        commands: &mut mpsc::Receiver<Submission>,
        submission_id: &str,
        turn_id: &str,
        calls: Vec<ToolCall>,
    ) -> Result<bool> {
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
                    .execute_tools(commands, submission_id, turn_id, &calls, permissions)
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
                    .pause_and_resolve(
                        commands,
                        submission_id,
                        turn_id,
                        calls,
                        request,
                        permissions,
                        Vec::new(),
                    )
                    .await?
                else {
                    return Ok(true);
                };
                results
            }
            SandboxAuthorization::Review(review) => {
                let Some(results) = self
                    .review_and_resolve(commands, submission_id, turn_id, calls, review)
                    .await?
                else {
                    return Ok(true);
                };
                results
            }
        };
        self.state.pending_approval = None;
        self.persist_tool_results(submission_id, turn_id, results)
            .await?;
        Ok(false)
    }

    pub(in crate::agent) async fn continue_turn(
        &mut self,
        commands: &mut mpsc::Receiver<Submission>,
        submission_id: String,
        turn_id: String,
    ) -> Result<()> {
        let mut model_step = 0;
        let mut stop_continued = false;
        loop {
            if let Some(interrupt_submission_id) = self.drain_commands(commands, &turn_id).await? {
                self.abort(
                    &interrupt_submission_id,
                    &turn_id,
                    "interrupted",
                    ExecutionOutcome::Aborted,
                )
                .await?;
                return Ok(());
            }
            if model_step >= self.config.max_model_steps {
                return Err(Error::Stopped(format!(
                    "turn reached the configured limit of {} model steps",
                    self.config.max_model_steps
                )));
            }
            let mut rewrite_reasons = Vec::new();
            let request_input = loop {
                match self
                    .prepare_model_phase(commands, &submission_id, &turn_id, model_step)
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

            let Some(step) = self
                .request_model_step(
                    commands,
                    &submission_id,
                    &turn_id,
                    model_step,
                    &request_input.0,
                    &request_input.1,
                )
                .await?
            else {
                return Ok(());
            };
            let (output, executable_calls, denied_results) = self
                .normalize_and_persist_model_step(&submission_id, &turn_id, &rewrite_reasons, step)
                .await?;
            model_step += 1;
            if output.tool_calls.is_empty() {
                if self
                    .finish_toolless_step(
                        commands,
                        &submission_id,
                        &turn_id,
                        &output,
                        &mut stop_continued,
                    )
                    .await?
                {
                    return Ok(());
                }
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
                .authorize_and_execute(commands, &submission_id, &turn_id, executable_calls)
                .await?
            {
                return Ok(());
            }
        }
    }
}

fn loaded_tools(
    input: &[Value],
    catalog_revision: &str,
    searchable: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let mut loaded = BTreeSet::new();
    for item in input {
        let Some(selection) = ToolLoad::from_input(item)? else {
            continue;
        };
        if selection.catalog_revision == catalog_revision {
            loaded.extend(
                selection
                    .tools
                    .into_iter()
                    .filter(|name| searchable.contains(name)),
            );
        }
    }
    Ok(loaded)
}

fn model_hook_outcome(
    control: Wait<Result<()>>,
    turn_stop: Option<String>,
    queue_changed: bool,
    has_pending_input: bool,
) -> Result<ModelHookOutcome> {
    match control {
        Wait::Ready(Err(error)) => Ok(ModelHookOutcome::Error(error)),
        Wait::Interrupted { submission_id } => Ok(ModelHookOutcome::Interrupted(submission_id)),
        Wait::Ready(Ok(())) => {
            if let Some(reason) = turn_stop {
                Ok(ModelHookOutcome::Stopped(reason))
            } else if queue_changed {
                Ok(ModelHookOutcome::Repeat)
            } else if has_pending_input {
                Err(Error::Config(
                    "queued active input was not consumed by its middleware".into(),
                ))
            } else {
                Ok(ModelHookOutcome::Ready)
            }
        }
    }
}

fn track_pending_searches(pending: &Mutex<Vec<String>>, event: &crate::protocol::ModelEvent) {
    if let Ok(mut pending) = pending.lock() {
        match event {
            crate::protocol::ModelEvent::WebSearchStarted { call_id } => {
                pending.push(call_id.clone());
            }
            crate::protocol::ModelEvent::WebSearchCompleted { call_id, .. } => {
                pending.retain(|open| open != call_id);
            }
            _ => {}
        }
    }
}

fn exposed_tool_names(catalog: &Catalog) -> BTreeSet<String> {
    catalog
        .direct_definitions()
        .iter()
        .chain(catalog.deferred_definitions().iter())
        .map(|tool| tool.name.clone())
        .collect()
}

fn bind_prepared_call(
    catalog: &Catalog,
    call: ToolCall,
    available: &BTreeSet<String>,
    materialized: &BTreeSet<String>,
    searchable: &BTreeSet<String>,
) -> Result<ToolCall> {
    if !available.contains(&call.name) {
        return Err(Error::Tool(format!(
            "tool `{}` is unavailable for this model step",
            call.name
        )));
    }
    catalog
        .bind_call(call, materialized, searchable)
        .map(crate::middleware::tools::BoundToolCall::into_call)
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
    for target in events.iter_mut().filter_map(|event| match event {
        EventMsg::UserMessage(message) => message.message_target.as_mut(),
        EventMsg::AgentMessage(message) => message.message_target.as_mut(),
        _ => None,
    }) {
        if target.checkpoint_sequence == provisional {
            target.checkpoint_sequence = durable;
        }
    }
}

async fn persist_queue_snapshot(
    recorder: &EventRecorder,
    checkpoint: &mut Checkpoint,
    events: Vec<Event>,
) -> Result<()> {
    let previous_sequence = checkpoint.sequence;
    checkpoint.sequence = checkpoint
        .sequence
        .checked_add(1)
        .ok_or_else(|| Error::Checkpoint("checkpoint sequence overflow".into()))?;
    if let Err(error) = recorder.save(checkpoint, &[], None, events).await {
        checkpoint.sequence = previous_sequence;
        return Err(error);
    }
    Ok(())
}

fn has_visible_output_text(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("message")
        && item.get("role").and_then(Value::as_str) == Some("assistant")
        && item.get("phase").and_then(Value::as_str) != Some("commentary")
        && item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|part| {
                part.get("type").and_then(Value::as_str) == Some("output_text")
                    && part
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
            })
}

fn insert_pre_tool_input(output: &mut Vec<Value>, input: Vec<Value>) {
    if input.is_empty() {
        return;
    }
    let boundary = tool_complete_boundaries(output.iter())
        .last()
        .copied()
        .unwrap_or_default();
    output.splice(boundary..boundary, input);
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
        insert_pre_tool_input(
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
