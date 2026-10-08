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
    BoundToolCall, ExecutionMode, PreparedToolSet, ToolResult, execute_call,
};
use crate::protocol::{EventMsg, ToolCall, ToolCallBeginEvent};
use crate::{BoxFuture, Error, Result};

#[derive(Default)]
pub(super) struct StreamedTools {
    pub(super) originals: Vec<ToolCall>,
    // One slot per prepared call; unchanged calls reuse the validated provider output at EOF.
    pub(super) calls: Vec<Option<ToolCall>>,
    pub(super) denied: Vec<ToolResult>,
    pub(super) hook_input: Vec<serde_json::Value>,
    pub(super) hook_events: Vec<EventMsg>,
    pub(super) started: BTreeSet<String>,
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
        let active = self.state.active_execution.as_ref().ok_or_else(|| {
            Error::Checkpoint("streaming response has no active execution".into())
        })?;
        let submission_id = active.submission_id.clone();
        let turn_id = active.turn_id.clone();
        enum Progress {
            Response(Result<ModelOutput>),
            Call(Option<ToolCall>),
            Tool(Result<ToolResult>),
        }
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
        submission_id: &str,
        turn_id: &str,
        original: ToolCall,
        tools: &PreparedToolSet<'_>,
        streamed: &mut StreamedTools,
    ) -> Result<()> {
        if streamed.deferred {
            return Ok(());
        }
        // Native discovery proof arrives in the completed response. Never guess it.
        if self.catalog.validate_prepared(&original, tools).is_err()
            || self
                .catalog
                .cancels_on_input(std::slice::from_ref(&original))
        {
            streamed.deferred = true;
            return Ok(());
        }
        // Keep provider arguments for EOF validation while middleware may rewrite the executable call.
        let mut call = original.clone();
        let mut hook_events = Vec::new();
        let mut hook_input = Vec::new();
        // Once hooks run, this response may no longer be retried.
        streamed.originals.push(original);
        let (denial, changed) = self
            .prepare_tool_call(turn_id, &mut call, &mut hook_events, &mut hook_input)
            .await?;
        if denial.is_some() || !hook_input.is_empty() || !hook_events.is_empty() {
            streamed.calls.push(changed.then_some(call));
            // Preserve hook ordering and atomic persistence with the complete provider output.
            streamed.hook_input.extend(hook_input);
            streamed.hook_events.extend(hook_events);
            if let Some(result) = denial {
                streamed.denied.push(result);
            }
            streamed.deferred = true;
            return Ok(());
        }
        let Some((bound, permissions)) =
            self.authorize_streamed_tool(&call, tools, changed).await?
        else {
            streamed.calls.push(changed.then_some(call));
            streamed.deferred = true;
            return Ok(());
        };
        // Only rewritten calls need an EOF snapshot separate from execution and recovery.
        streamed.calls.push(changed.then(|| call.clone()));
        // Pending calls are durable before side effects; full provider output follows at EOF.
        self.state.make_mut().pending_tools.push(call.clone());
        if let Err(error) = self.save().await {
            self.state.make_mut().pending_tools.pop();
            return Err(error);
        }
        streamed.started.insert(call.call_id.clone());
        let catalog = Arc::clone(&self.catalog);
        let parallel = catalog.execution_mode(&call.name) == ExecutionMode::Parallel;
        let sandbox = Arc::clone(&self.config.sandbox);
        let gate = Arc::clone(&streamed.gate);
        let events = Arc::clone(&self.events);
        let submission_id = submission_id.to_owned();
        let turn_id = turn_id.to_owned();
        let model_route = self.config.provider.clone();
        let author = self.active_author()?.clone();
        streamed.running.push_back(Box::pin(async move {
            let _read;
            let _write;
            if parallel {
                _read = Some(gate.read().await);
                _write = None;
            } else {
                _read = None;
                _write = Some(gate.write().await);
            }
            send_event(
                &events,
                turn_event(
                    &submission_id,
                    EventMsg::ToolCallBegin(ToolCallBeginEvent {
                        turn_id: turn_id.clone(),
                        call_id: call.call_id,
                        name: call.name,
                        arguments: call.arguments,
                    }),
                ),
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

    async fn authorize_streamed_tool(
        &self,
        call: &ToolCall,
        tools: &PreparedToolSet<'_>,
        changed: bool,
    ) -> Result<Option<(BoundToolCall, SandboxPermissions)>> {
        if self.catalog.cancels_on_input(std::slice::from_ref(call)) {
            return Ok(None);
        }
        if changed && self.catalog.validate_prepared(call, tools).is_err() {
            return Ok(None);
        }
        let live = self.live_tools().await?;
        let Ok(bound) = self.catalog.bind_prepared(call.clone(), &live) else {
            return Ok(None);
        };
        let mutations = self
            .catalog
            .requires_approval(&call.name)
            .then_some(call.call_id.as_str());
        Ok(
            match self.config.sandbox.authorize(
                &self.config.session_id,
                std::slice::from_ref(call),
                mutations,
            )? {
                SandboxAuthorization::Execute(permissions) => Some((bound, permissions)),
                // Keep approval suspension/recovery on the completed-batch path.
                SandboxAuthorization::Approval { .. } => None,
            },
        )
    }
}
