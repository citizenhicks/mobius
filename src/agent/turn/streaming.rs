//! Overlap completed, authorized tool calls with the remaining model stream.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use futures_util::stream::{FuturesOrdered, StreamExt};
use tokio::sync::{RwLock, mpsc};

use super::turn_event;
use crate::agent::input::Wait;
use crate::agent::tool_step::ToolCompletion;
use crate::agent::{Runner, SubmissionInbox, send_event};
use crate::backend::model::ModelOutput;
use crate::backend::sandbox::{SandboxAuthorization, SandboxPermissions};
use crate::middleware::tools::{
    ExecutionMode, PreparedToolSet, ToolBinding, ToolResult, execute_call,
};
use crate::protocol::{EventMsg, ToolCall, ToolCallBeginEvent};
use crate::{BoxFuture, Error, Result};

/// One streamed call prepared before the response completed.
pub(super) struct PreparedCall {
    /// The normalized or hook-rewritten call, when it differs from the provider output.
    pub(super) call: Option<ToolCall>,
    /// Whether middleware intentionally rewrote the durable provider output.
    pub(super) rewritten: bool,
    /// The binding of the current call; `None` when a hook rewrite is still unbound.
    pub(super) binding: Option<ToolBinding>,
}

#[derive(Default)]
pub(super) struct StreamedTools {
    pub(super) originals: Vec<ToolCall>,
    // One slot per prepared call, in provider order.
    pub(super) calls: Vec<PreparedCall>,
    pub(super) denied: Vec<ToolResult>,
    pub(super) hook_input: Vec<serde_json::Value>,
    pub(super) hook_events: Vec<EventMsg>,
    pub(super) started: BTreeSet<Arc<str>>,
    running: FuturesOrdered<BoxFuture<'static, Result<ToolResult>>>,
    results: Vec<ToolResult>,
    gate: Arc<RwLock<()>>,
    deferred: bool,
}

impl StreamedTools {
    pub(super) fn cancel(&mut self) {
        self.running.clear();
    }

    pub(super) async fn finish(mut self) -> Result<ToolCompletion> {
        while let Some(result) = self.running.next().await {
            self.results.push(result?);
        }
        Ok(self.results.into())
    }
}

impl Runner {
    pub(super) async fn wait_streamed_response(
        &mut self,
        inbox: &mut SubmissionInbox,
        response: impl Future<Output = Result<ModelOutput>>,
        mut ready_calls: mpsc::Receiver<ToolCall>,
        tools: &PreparedToolSet<'_>,
        streamed: &mut StreamedTools,
    ) -> Result<Wait<Result<ModelOutput>>> {
        enum Progress {
            Response(Result<ModelOutput>),
            Call(Option<ToolCall>),
            Tool(Result<ToolResult>),
        }
        let active = self.state.active_execution.as_ref().ok_or_else(|| {
            Error::Checkpoint("streaming response has no active execution".into())
        })?;
        let submission_id = Arc::clone(&active.submission_id);
        let turn_id = Arc::clone(&active.turn_id);
        tokio::pin!(response);
        let mut input_changed = false;
        let mut calls_open = true;
        loop {
            let progress = async {
                tokio::select! {
                    // Polling queued tool futures first admits calls through the gate in order.
                    biased;
                    Some(result) = streamed.running.next(), if !streamed.running.is_empty() => Progress::Tool(result),
                    output = &mut response => Progress::Response(output),
                    call = ready_calls.recv(), if calls_open => Progress::Call(call),
                }
            };
            let progress = match self.wait_active(inbox, &turn_id, progress).await? {
                Wait::Ready {
                    value,
                    input_changed: changed,
                } => {
                    input_changed |= changed;
                    value
                }
                Wait::Interrupted { submission_id } => {
                    return Ok(Wait::Interrupted { submission_id });
                }
            };
            match progress {
                Progress::Response(output) => {
                    return Ok(Wait::Ready {
                        value: output,
                        input_changed,
                    });
                }
                Progress::Tool(result) => streamed.results.push(result?),
                Progress::Call(None) => calls_open = false,
                Progress::Call(Some(call)) => {
                    self.start_streamed_tool(&submission_id, &turn_id, call, tools, streamed)
                        .await?;
                }
            }
        }
    }

    async fn start_streamed_tool(
        &mut self,
        submission_id: &Arc<str>,
        turn_id: &Arc<str>,
        original: ToolCall,
        tools: &PreparedToolSet<'_>,
        streamed: &mut StreamedTools,
    ) -> Result<()> {
        if streamed.deferred
            || self
                .catalog
                .cancels_on_input(std::slice::from_ref(&original))
        {
            streamed.deferred = true;
            return Ok(());
        }
        // Keep provider arguments for EOF validation while middleware may rewrite the executable call.
        let mut call = original.clone(); // rules-allow: EOF validation keeps the provider's original call
        // Native discovery proof arrives in the completed response. Never guess it.
        let Ok(binding) = self.catalog.bind_in_place(&mut call, tools) else {
            streamed.deferred = true;
            return Ok(());
        };
        let mut hook_events = Vec::new();
        let mut hook_input = Vec::new();
        // Once hooks run, this response may no longer be retried.
        streamed.originals.push(original);
        let (denial, changed) = self
            .prepare_tool_call(turn_id, &mut call, &mut hook_events, &mut hook_input)
            .await?;
        let rewritten = changed || binding.repaired;
        if denial.is_some() || !hook_input.is_empty() || !hook_events.is_empty() {
            streamed.calls.push(PreparedCall {
                call: rewritten.then_some(call),
                rewritten: changed,
                binding: (!changed).then_some(binding),
            });
            // Preserve hook ordering and atomic persistence with the complete provider output.
            streamed.hook_input.extend(hook_input);
            streamed.hook_events.extend(hook_events);
            if let Some(result) = denial {
                streamed.denied.push(result);
            }
            streamed.deferred = true;
            return Ok(());
        }
        let binding = if !changed {
            Some(binding)
        } else if self.catalog.cancels_on_input(std::slice::from_ref(&call)) {
            None
        } else {
            self.catalog.bind_in_place(&mut call, tools).ok()
        };
        let authorized = match binding {
            Some(binding) => match self.authorize_streamed_tool(&call)? {
                Some(permissions) => Ok((binding, permissions)),
                None => Err(Some(binding)),
            },
            None => Err(None),
        };
        let (binding, permissions) = match authorized {
            Ok(authorized) => authorized,
            Err(binding) => {
                streamed.calls.push(PreparedCall {
                    call: rewritten.then_some(call),
                    rewritten: changed,
                    binding,
                });
                streamed.deferred = true;
                return Ok(());
            }
        };
        // Only rewritten calls need an EOF snapshot separate from execution and recovery.
        streamed.calls.push(PreparedCall {
            call: rewritten.then(|| call.clone()), // rules-allow: a rewritten call keeps an EOF snapshot apart from execution
            rewritten: changed,
            binding: None,
        });
        // Pending calls are durable before side effects; full provider output follows at EOF.
        self.state.make_mut().pending_tools.push(call.clone()); // rules-allow: the checkpoint records the call before the task executes it
        if let Err(error) = self.save().await {
            self.state.make_mut().pending_tools.pop();
            return Err(error);
        }
        streamed.started.insert(Arc::clone(&call.call_id));
        let catalog = Arc::clone(&self.catalog);
        let parallel = catalog.execution_mode(&call.name) == ExecutionMode::Parallel;
        let sandbox = Arc::clone(&self.config.sandbox);
        let gate = Arc::clone(&streamed.gate);
        let events = Arc::clone(&self.events);
        let submission_id = Arc::clone(submission_id);
        let turn_id = Arc::clone(turn_id);
        let model_route = Arc::clone(&self.config.provider);
        let author = Arc::clone(self.active_author()?);
        let begin = ToolCallBeginEvent {
            turn_id: Arc::clone(&turn_id),
            call_id: Arc::clone(&call.call_id),
            name: Arc::clone(&call.name),
            arguments: call.arguments.clone(), // rules-allow: the begin event owns a copy of the arguments the bound call executes
        };
        let bound = binding.bind(call);
        streamed.running.push_back(Box::pin(async move {
            let _guard = if parallel {
                (Some(gate.read().await), None)
            } else {
                (None, Some(gate.write().await))
            };
            send_event(
                &events,
                turn_event(&submission_id, EventMsg::ToolCallBegin(begin)),
            )
            .await?;
            Ok(execute_call(
                &catalog,
                bound,
                &sandbox,
                &permissions,
                &turn_id,
                &model_route,
                &author,
            )
            .await)
        }));
        Ok(())
    }

    fn authorize_streamed_tool(&self, call: &ToolCall) -> Result<Option<SandboxPermissions>> {
        let mutations = self
            .catalog
            .requires_approval(&call.name)
            .then_some(call.call_id.as_ref());
        Ok(
            match self.config.sandbox.authorize(
                &self.config.session_id,
                std::slice::from_ref(call),
                mutations,
            )? {
                SandboxAuthorization::Execute(permissions) => Some(permissions),
                // Keep approval suspension/recovery on the completed-batch path.
                SandboxAuthorization::Approval { .. } => None,
            },
        )
    }
}
