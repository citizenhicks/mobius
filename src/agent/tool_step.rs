//! Tool execution and result persistence.

use std::collections::BTreeSet;
use std::sync::Arc;

use tokio::sync::mpsc;

use super::Runner;
use super::input::ActiveRoute;
use super::input::ActiveTurnRouter;
use super::input::Wait;
use crate::Error;
use crate::Result;
use crate::backend::model::ToolCall;
use crate::backend::model::ToolLoad;
use crate::backend::model::tool_output;
use crate::backend::sandbox::SandboxPermissions;
use crate::middleware::PostToolUseContext;
use crate::middleware::QueuedInputBaseline;
use crate::middleware::tools::{BoundToolCall, Catalog, ToolResult, execute_batch};
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::Submission;
use crate::protocol::ToolCallBeginEvent;
use crate::protocol::ToolCallEndEvent;

impl Runner {
    pub(super) async fn execute_tools(
        &mut self,
        commands: &mut mpsc::Receiver<Submission>,
        submission_id: &str,
        turn_id: &str,
        calls: &[ToolCall],
        permissions: SandboxPermissions,
    ) -> Result<Wait<Vec<ToolResult>>> {
        let (available, searchable, materialized) = self.live_tool_sets().await?;
        let (bound_calls, mut unavailable_results) =
            bind_live_calls(&self.catalog, calls, &available, &searchable, &materialized);
        let callable = bound_calls
            .iter()
            .map(|call| call.as_call().clone())
            .collect::<Vec<_>>();
        for call in &callable {
            self.emit(
                submission_id,
                EventMsg::ToolCallBegin(ToolCallBeginEvent {
                    turn_id: turn_id.to_string(),
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                }),
            )
            .await?;
        }
        let catalog = self.catalog.clone();
        let interrupt_on_active_input = catalog.interrupts_on_active_input(&callable);
        let execution = execute_batch(
            &catalog,
            &bound_calls,
            Arc::clone(&self.config.sandbox),
            &permissions,
            turn_id,
        );
        tokio::pin!(execution);
        let mut executed = false;
        let results = loop {
            tokio::select! {
                results = &mut execution => {
                    executed = true;
                    break Wait::Ready(results);
                }
                submission = commands.recv() => {
                    let Some(submission) = submission else {
                        return Err(Error::Stopped("frontend disconnected".into()));
                    };
                    match (ActiveTurnRouter {
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
                    .await?
                    {
                        ActiveRoute::Accepted(change) => {
                            self.persist_active_change(change).await?;
                            if interrupt_on_active_input {
                                break Wait::Ready(interrupted_results(
                                    &callable,
                                    "execution interrupted; result unknown after active input",
                                ));
                            }
                        }
                        ActiveRoute::Changed(change) => {
                            self.persist_active_change(change).await?;
                        }
                        ActiveRoute::Interrupted { submission_id } => {
                            break Wait::Interrupted { submission_id };
                        }
                        _ => {}
                    }
                }
            }
        };
        let mut results = match results {
            Wait::Ready(results) => results,
            Wait::Interrupted { submission_id } => {
                return Ok(Wait::Interrupted { submission_id });
            }
        };
        results.append(&mut unavailable_results);
        results = order_results(calls, results);
        if !executed {
            return Ok(Wait::Ready(results));
        }
        let raw_results = results.clone();
        let mut hook_events = Vec::new();
        for result in &mut results {
            let call = calls
                .iter()
                .find(|call| call.call_id == result.call_id)
                .ok_or_else(|| Error::Tool("tool result has no matching call".into()))?;
            if !result.handler_executed {
                continue;
            }
            let mut context = PostToolUseContext {
                turn: self.runtime.turn_identity(turn_id),
                call,
                events: &mut hook_events,
                tools: &self.catalog,
                result,
            };
            if let Err(error) = self.config.middleware.post_tool_use(&mut context).await {
                self.persist_tool_results(submission_id, turn_id, raw_results)
                    .await?;
                return Err(error);
            }
        }
        for message in hook_events {
            if let Err(error) = self.emit(submission_id, message).await {
                self.persist_tool_results(submission_id, turn_id, results)
                    .await?;
                return Err(error);
            }
        }
        Ok(Wait::Ready(results))
    }

    pub(super) async fn persist_tool_results(
        &mut self,
        submission_id: &str,
        turn_id: &str,
        results: Vec<ToolResult>,
    ) -> Result<()> {
        if results.is_empty() {
            return Ok(());
        }
        let events = tool_result_events(submission_id, turn_id, &results);
        self.append_tool_results(results)?;
        self.persist_with_events(events, None).await?;
        Ok(())
    }

    pub(super) fn append_tool_results(&mut self, results: Vec<ToolResult>) -> Result<()> {
        let tool_calls = u64::try_from(results.len())
            .map_err(|_| Error::Checkpoint("execution tool-call count is unsupported".into()))?;
        let failed_tool_calls = u64::try_from(
            results.iter().filter(|result| result.is_error).count(),
        )
        .map_err(|_| Error::Checkpoint("execution failed-tool count is unsupported".into()))?;
        self.record_tools(tool_calls, failed_tool_calls)?;
        let completed = results
            .iter()
            .map(|result| result.call_id.as_str())
            .collect::<BTreeSet<_>>();
        self.state
            .pending_tools
            .retain(|call| !completed.contains(call.call_id.as_str()));
        for mut result in results {
            self.push_context(tool_output(
                &result.call_id,
                &result.output,
                result.is_error,
            ));
            if !result.loaded_tools.is_empty() {
                self.push_context(
                    ToolLoad {
                        catalog_revision: self.catalog.revision()?.into(),
                        tools: std::mem::take(&mut result.loaded_tools),
                    }
                    .into_input(),
                );
            }
            self.extend_context(std::mem::take(&mut result.additional_input));
        }
        Ok(())
    }

    pub(super) async fn finish_pending_tools(
        &mut self,
        submission_id: &str,
        turn_id: &str,
        reason: &str,
    ) -> Result<()> {
        let calls = std::mem::take(&mut self.state.pending_tools);
        let results = interrupted_results(
            &calls,
            &format!("execution interrupted; result unknown: {reason}"),
        );
        if results.is_empty() {
            return Ok(());
        }
        self.persist_tool_results(submission_id, turn_id, results)
            .await
    }
}

fn bind_live_calls(
    catalog: &Catalog,
    calls: &[ToolCall],
    available: &BTreeSet<String>,
    searchable: &BTreeSet<String>,
    materialized: &BTreeSet<String>,
) -> (Vec<BoundToolCall>, Vec<ToolResult>) {
    let mut bound = Vec::with_capacity(calls.len());
    let mut rejected = Vec::new();
    for call in calls {
        if !available.contains(&call.name) {
            rejected.push(ToolResult::error(
                call,
                format!("tool `{}` is no longer available", call.name),
            ));
            continue;
        }
        match catalog.bind_call(call.clone(), materialized, searchable) {
            Ok(call) => bound.push(call),
            Err(error) => rejected.push(ToolResult::error(call, error.to_string())),
        }
    }
    (bound, rejected)
}

fn tool_result_events(submission_id: &str, turn_id: &str, results: &[ToolResult]) -> Vec<Event> {
    results
        .iter()
        .map(|result| Event {
            submission_id: Some(submission_id.to_string()),
            msg: EventMsg::ToolCallEnd(ToolCallEndEvent {
                turn_id: turn_id.to_string(),
                call_id: result.call_id.clone(),
                name: result.name.clone(),
                output: result.output.clone(),
                is_error: result.is_error,
            }),
        })
        .collect()
}

fn interrupted_results(calls: &[ToolCall], message: &str) -> Vec<ToolResult> {
    calls
        .iter()
        .map(|call| ToolResult::error(call, message))
        .collect()
}

fn order_results(calls: &[ToolCall], results: Vec<ToolResult>) -> Vec<ToolResult> {
    let mut results = results
        .into_iter()
        .map(|result| (result.call_id.clone(), result))
        .collect::<std::collections::BTreeMap<_, _>>();
    calls
        .iter()
        .filter_map(|call| results.remove(&call.call_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_results_do_not_claim_tools_were_denied() {
        let calls = [ToolCall {
            call_id: "call-1".into(),
            name: "write".into(),
            arguments: serde_json::json!({}),
        }];

        let results = interrupted_results(&calls, "execution interrupted; result unknown");

        assert_eq!(results[0].output, "execution interrupted; result unknown");
    }
}
