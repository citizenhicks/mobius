//! Tool execution and result persistence.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::Runner;
use super::SubmissionInbox;
use super::input::ActiveRoute;
use super::input::Wait;
use crate::Error;
use crate::Result;
use crate::backend::model::ToolCall;
use crate::backend::model::tool_output;
use crate::backend::sandbox::SandboxPermissions;
use crate::middleware::PostToolUseContext;
use crate::middleware::tools::{ToolResult, execute_batch};
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::ToolCallBeginEvent;
use crate::protocol::ToolCallEndEvent;

#[derive(Default)]
pub(super) struct ToolCompletion {
    pub(super) results: Vec<ToolResult>,
    pub(super) events: Vec<EventMsg>,
}

impl From<Vec<ToolResult>> for ToolCompletion {
    fn from(results: Vec<ToolResult>) -> Self {
        Self {
            results,
            events: Vec::new(),
        }
    }
}

impl Runner {
    pub(super) async fn execute_tools(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &str,
        turn_id: &str,
        calls: &[ToolCall],
        permissions: SandboxPermissions,
    ) -> Result<Wait<ToolCompletion>> {
        let tools = self.live_tools().await?;
        let (bound_calls, mut unavailable_results) = self.catalog.bind_live_batch(calls, &tools);
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
        let cancel_on_input = catalog.cancels_on_input(&callable);
        if cancel_on_input
            && self
                .config
                .middleware
                .messages_ready(&self.state.pending_messages, turn_id)?
        {
            let mut results = interrupted_results(
                &callable,
                "execution cancelled before start because newer input is ready",
            );
            results.append(&mut unavailable_results);
            return Ok(Wait::Ready {
                value: order_results(calls, results).into(),
                input_changed: true,
            });
        }
        let execution = execute_batch(
            &catalog,
            &bound_calls,
            Arc::clone(&self.config.sandbox),
            &permissions,
            turn_id,
        );
        tokio::pin!(execution);
        let mut executed = false;
        let mut input_changed = false;
        let results = loop {
            tokio::select! {
                biased;
                submission = inbox.recv() => {
                    let Some(submission) = submission else {
                        return Err(Error::Stopped("frontend disconnected".into()));
                    };
                    match self.route_active_submission(submission, turn_id, None).await? {
                        ActiveRoute::Continue {
                            input_changed: changed,
                        } => {
                            if changed {
                                if cancel_on_input {
                                    break Wait::Ready {
                                        value: interrupted_results(
                                            &callable,
                                            "execution cancelled by newer input; result unknown",
                                        ),
                                        input_changed: true,
                                    };
                                }
                                input_changed = true;
                            }
                        }
                        ActiveRoute::Interrupted { submission_id } => {
                            break Wait::Interrupted { submission_id };
                        }
                        ActiveRoute::Approval { .. } => {}
                    }
                }
                results = &mut execution => {
                    executed = true;
                    break Wait::Ready { value: results, input_changed };
                }
            }
        };
        let (mut results, input_changed) = match results {
            Wait::Ready {
                value,
                input_changed,
            } => (value, input_changed),
            Wait::Interrupted { submission_id } => {
                return Ok(Wait::Interrupted { submission_id });
            }
        };
        results.append(&mut unavailable_results);
        results = order_results(calls, results);
        if !executed {
            return Ok(Wait::Ready {
                value: results.into(),
                input_changed,
            });
        }
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
            self.config.middleware.post_tool_use(&mut context).await?;
        }
        Ok(Wait::Ready {
            value: ToolCompletion {
                results,
                events: hook_events,
            },
            input_changed,
        })
    }

    pub(super) async fn persist_tool_results(
        &mut self,
        submission_id: &str,
        turn_id: &str,
        completion: impl Into<ToolCompletion>,
    ) -> Result<()> {
        let ToolCompletion {
            results,
            events: hook_events,
        } = completion.into();
        if results.is_empty() && hook_events.is_empty() {
            return Ok(());
        }
        let mut events = hook_events
            .into_iter()
            .map(|msg| Event {
                submission_id: Some(submission_id.to_string()),
                msg,
            })
            .collect::<Vec<_>>();
        events.extend(tool_result_events(submission_id, turn_id, &results));
        let pending_tools = self.state.pending_tools.clone();
        let active_execution = self.state.active_execution.clone();
        let context_len = self.state.context.len();
        let transcript_len = self.transcript_delta.len();
        self.append_tool_results(results)?;
        match self.persist_with_events(events, None).await {
            Ok(_) => Ok(()),
            Err(error) => {
                self.state.pending_tools = pending_tools;
                self.state.active_execution = active_execution;
                self.state.context.truncate(context_len);
                self.transcript_delta.truncate(transcript_len);
                Err(error)
            }
        }
    }

    pub(super) async fn complete_tool_step(
        &mut self,
        submission_id: &str,
        turn_id: &str,
        completion: ToolCompletion,
    ) -> Result<()> {
        let pending_approval = self.state.pending_approval.take();
        match self
            .persist_tool_results(submission_id, turn_id, completion)
            .await
        {
            Ok(()) => Ok(()),
            Err(error) => {
                self.state.pending_approval = pending_approval;
                Err(error)
            }
        }
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
            self.extend_context(std::mem::take(&mut result.additional_input));
        }
        Ok(())
    }

    pub(super) fn finish_pending_tools(
        &mut self,
        submission_id: &str,
        turn_id: &str,
        reason: &str,
    ) -> Result<Vec<Event>> {
        let calls = std::mem::take(&mut self.state.pending_tools);
        let results = interrupted_results(
            &calls,
            &format!("execution interrupted; result unknown: {reason}"),
        );
        if results.is_empty() {
            return Ok(Vec::new());
        }
        let events = tool_result_events(submission_id, turn_id, &results);
        self.append_tool_results(results)?;
        Ok(events)
    }
}

fn tool_result_events(submission_id: &str, turn_id: &str, results: &[ToolResult]) -> Vec<Event> {
    let mut events = Vec::with_capacity(results.len() * 2);
    for result in results {
        events.push(Event {
            submission_id: Some(submission_id.to_string()),
            msg: EventMsg::ToolCallEnd(ToolCallEndEvent {
                turn_id: turn_id.to_string(),
                call_id: result.call_id.clone(),
                name: result.name.clone(),
                output: result.output.clone(),
                is_error: result.is_error,
            }),
        });
        events.extend(result.events.iter().cloned().map(|msg| Event {
            submission_id: Some(submission_id.to_string()),
            msg,
        }));
    }
    events
}

fn interrupted_results(calls: &[ToolCall], message: &str) -> Vec<ToolResult> {
    calls
        .iter()
        .map(|call| ToolResult::error(call, message))
        .collect()
}

pub(super) fn order_results(calls: &[ToolCall], results: Vec<ToolResult>) -> Vec<ToolResult> {
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
