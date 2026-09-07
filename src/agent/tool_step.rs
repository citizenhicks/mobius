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
use crate::middleware::tools::{ToolResult, execute_batch};
use crate::middleware::{PostToolUseContext, checked_image_input_bytes, model_input_image_stats};
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::ToolCallBeginEvent;
use crate::protocol::ToolCallEndEvent;

const MODEL_INPUT_IMAGE_BYTES_FIELD: &str = "_mobius_image_bytes";

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
        let drained = self.drain_submissions(inbox, turn_id).await?;
        if let Some(submission_id) = drained.interrupted {
            return Ok(Wait::Interrupted { submission_id });
        }
        let mut input_changed = drained.input_changed;
        let messages_ready = self
            .config
            .middleware
            .messages_ready(&self.state.pending_messages, turn_id)?;
        if cancel_on_input && (input_changed || messages_ready) {
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
        let results = loop {
            let drained = self.drain_submissions(inbox, turn_id).await?;
            input_changed |= drained.input_changed;
            if let Some(submission_id) = drained.interrupted {
                break Wait::Interrupted { submission_id };
            }
            if cancel_on_input && drained.input_changed {
                break Wait::Ready {
                    value: interrupted_results(
                        &callable,
                        "execution cancelled by newer input; result unknown",
                    ),
                    input_changed: true,
                };
            }
            tokio::select! {
                biased;
                results = &mut execution => {
                    let drained = self.drain_submissions(inbox, turn_id).await?;
                    input_changed |= drained.input_changed;
                    if let Some(submission_id) = drained.interrupted {
                        break Wait::Interrupted { submission_id };
                    }
                    if cancel_on_input && drained.input_changed {
                        break Wait::Ready {
                            value: interrupted_results(
                                &callable,
                                "execution cancelled by newer input; result unknown",
                            ),
                            input_changed: true,
                        };
                    }
                    executed = true;
                    break Wait::Ready { value: results, input_changed };
                }
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
        input_image_bytes: usize,
        completion: impl Into<ToolCompletion>,
    ) -> Result<()> {
        let ToolCompletion {
            mut results,
            events: hook_events,
        } = completion.into();
        if results.is_empty() && hook_events.is_empty() {
            return Ok(());
        }
        enforce_tool_image_budget(input_image_bytes, &mut results);
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
        input_image_bytes: usize,
        completion: ToolCompletion,
    ) -> Result<()> {
        let pending_approval = self.state.pending_approval.take();
        match self
            .persist_tool_results(submission_id, turn_id, input_image_bytes, completion)
            .await
        {
            Ok(()) => Ok(()),
            Err(error) => {
                self.state.pending_approval = pending_approval;
                Err(error)
            }
        }
    }

    pub(super) fn pending_tool_input_image_bytes(&self, calls: &[ToolCall]) -> Result<usize> {
        if calls.is_empty() {
            return Ok(0);
        }
        let call_ids = calls
            .iter()
            .map(|call| call.call_id.as_str())
            .collect::<BTreeSet<_>>();
        let value = self
            .state
            .context
            .iter()
            .rev()
            .find(|item| {
                item.get("type").and_then(serde_json::Value::as_str) == Some("function_call")
                    && item
                        .get("call_id")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|call_id| call_ids.contains(call_id))
            })
            .and_then(|item| item.get(MODEL_INPUT_IMAGE_BYTES_FIELD))
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                Error::Checkpoint("pending tool batch has no model image budget".into())
            })?;
        usize::try_from(value)
            .map_err(|_| Error::Checkpoint("pending tool image budget is unsupported".into()))
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

pub(in crate::agent) fn record_model_input_image_bytes(
    output: &mut [serde_json::Value],
    calls: &[ToolCall],
    bytes: usize,
) -> Result<()> {
    let bytes = u64::try_from(bytes)
        .map(serde_json::Value::from)
        .map_err(|_| Error::Checkpoint("model image budget is unsupported".into()))?;
    for call in calls {
        let item = output
            .iter_mut()
            .find(|item| {
                item.get("type").and_then(serde_json::Value::as_str) == Some("function_call")
                    && item.get("call_id").and_then(serde_json::Value::as_str)
                        == Some(&call.call_id)
            })
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| Error::Checkpoint("tool call is missing from model output".into()))?;
        item.insert(MODEL_INPUT_IMAGE_BYTES_FIELD.into(), bytes.clone());
    }
    Ok(())
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

fn enforce_tool_image_budget(mut used: usize, results: &mut [ToolResult]) {
    let mut used_images = 0_usize;
    for result in results {
        let additional = model_input_image_stats(&result.additional_input).and_then(|additional| {
            let images = used_images
                .checked_add(additional.count)
                .filter(|count| *count <= 1)
                .ok_or_else(|| {
                    Error::Tool("only one image may be added to model input per tool batch".into())
                })?;
            Ok((checked_image_input_bytes(used, additional.bytes)?, images))
        });
        match additional {
            Ok((total, images)) => {
                used = total;
                used_images = images;
            }
            Err(error) => {
                result.replace(error.to_string());
                result.is_error = true;
                result.additional_input.clear();
                result.events.clear();
            }
        }
    }
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

    #[test]
    fn excess_tool_images_become_model_visible_errors() {
        let mut results = vec![ToolResult {
            call_id: "call-1".into(),
            name: "view_image".into(),
            output: "viewed image".into(),
            is_error: false,
            handler_executed: true,
            additional_input: vec![serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "input_image",
                    "media_type": "image/png",
                    "data": "AA=="
                }]
            })],
            events: Vec::new(),
        }];

        enforce_tool_image_budget(crate::middleware::MAX_IMAGE_INPUT_BYTES, &mut results);

        assert!(results[0].is_error);
        assert!(results[0].additional_input.is_empty());
        assert!(results[0].output.contains("8 MiB"));
    }

    #[test]
    fn second_tool_image_becomes_a_model_visible_error() {
        let image = |call_id: &str| ToolResult {
            call_id: call_id.into(),
            name: "custom_image".into(),
            output: "image added".into(),
            is_error: false,
            handler_executed: true,
            additional_input: vec![serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "input_image",
                    "media_type": "image/png",
                    "data": "AA=="
                }]
            })],
            events: Vec::new(),
        };
        let mut results = vec![image("call-1"), image("call-2")];

        enforce_tool_image_budget(0, &mut results);

        assert!(!results[0].is_error);
        assert!(results[1].is_error);
        assert!(results[1].additional_input.is_empty());
        assert!(results[1].output.contains("only one image"));
    }
}
