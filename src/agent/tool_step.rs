//! Tool execution and result persistence.

use std::borrow::Borrow;
use std::collections::BTreeSet;
use std::sync::Arc;

use super::FRONTEND_DISCONNECTED_REASON;
use super::Runner;
use super::SubmissionInbox;
use super::input::ActiveRoute;
use super::input::Wait;
use crate::Error;
use crate::Result;
use crate::backend::model::tool_output;
use crate::backend::sandbox::SandboxPermissions;
use crate::middleware::tools::{ToolResult, execute_batch};
use crate::middleware::{PostToolUseContext, PreToolUseContext};
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::TokenUsage;
use crate::protocol::ToolCall;
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
    pub(super) async fn prepare_tool_call(
        &self,
        turn_id: &str,
        call: &mut ToolCall,
        events: &mut Vec<EventMsg>,
        input: &mut Vec<serde_json::Value>,
    ) -> Result<(Option<ToolResult>, bool)> {
        let mut context = PreToolUseContext {
            delivery_once: crate::middleware::delivery_once::DeliveryOnce::new(
                &self.state.delivered_once,
            ),
            turn: self.turn_identity(turn_id)?,
            events,
            tools: &self.catalog,
            call,
            changed: false,
            input: Vec::new(),
            denial: None,
        };
        self.config.middleware.pre_tool_use(&mut context).await?;
        let changed = context.changed;
        let denial = context.denial.take();
        input.append(&mut context.input);
        if let Some(reason) = denial {
            return Ok((
                Some(ToolResult::error(
                    call,
                    format!("tool call denied: {reason}"),
                    self.config.sandbox.output_limit(),
                )),
                changed,
            ));
        }
        Ok((None, changed))
    }

    pub(super) async fn post_tool_results(
        &self,
        turn_id: &str,
        calls: &[impl Borrow<ToolCall> + Sync],
        completion: &mut ToolCompletion,
    ) -> Result<()> {
        for result in &mut completion.results {
            let call = calls
                .iter()
                .map(Borrow::borrow)
                .find(|call| call.call_id == result.call_id)
                .ok_or_else(|| Error::Tool("tool result has no matching call".into()))?;
            if !result.handler_executed {
                continue;
            }
            let mut context = PostToolUseContext {
                delivery_once: crate::middleware::delivery_once::DeliveryOnce::new(
                    &self.state.delivered_once,
                ),
                turn: self.turn_identity(turn_id)?,
                call,
                events: &mut completion.events,
                tools: &self.catalog,
                result,
            };
            self.config.middleware.post_tool_use(&mut context).await?;
        }
        Ok(())
    }

    pub(super) async fn execute_tools(
        &mut self,
        inbox: &mut SubmissionInbox,
        submission_id: &str,
        turn_id: &str,
        calls: &[impl Borrow<ToolCall> + Sync],
        permissions: SandboxPermissions,
    ) -> Result<Wait<ToolCompletion>> {
        let tools = self.live_tools().await?;
        let (bound_calls, callable, mut unavailable_results) = self.catalog.bind_live_batch(
            calls.iter().map(Borrow::borrow),
            &tools,
            self.config.sandbox.output_limit(),
        );
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
        let catalog = Arc::clone(&self.catalog);
        let cancel_on_input = catalog.cancels_on_input(callable.iter().copied());
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
                callable.iter().copied(),
                "execution cancelled before start because newer input is ready",
                self.config.sandbox.output_limit(),
            );
            results.append(&mut unavailable_results);
            return Ok(Wait::Ready {
                value: order_results(calls, results).into(),
                input_changed: true,
            });
        }
        let model_route = self.config.provider.clone();
        let author = self.active_author()?.clone();
        let execution = execute_batch(
            &catalog,
            bound_calls,
            Arc::clone(&self.config.sandbox),
            &permissions,
            turn_id,
            &model_route,
            &author,
        );
        let output_limit = self.config.sandbox.output_limit();
        let cancelled_results = || {
            interrupted_results(
                callable.iter().copied(),
                "execution cancelled by newer input; result unknown",
                output_limit,
            )
        };
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
                    value: cancelled_results(),
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
                            value: cancelled_results(),
                            input_changed: true,
                        };
                    }
                    executed = true;
                    break Wait::Ready { value: results, input_changed };
                }
                submission = inbox.recv() => {
                    let Some(submission) = submission else {
                        return Err(Error::Stopped(FRONTEND_DISCONNECTED_REASON.into()));
                    };
                    match self.route_active_submission(submission, turn_id, None).await? {
                        ActiveRoute::Continue {
                            input_changed: changed,
                        } => {
                            if changed {
                                if cancel_on_input {
                                    break Wait::Ready {
                                        value: cancelled_results(),
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
        let mut completion = results.into();
        self.post_tool_results(turn_id, calls, &mut completion)
            .await?;
        Ok(Wait::Ready {
            value: completion,
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
            mut results,
            events: hook_events,
        } = completion.into();
        if results.is_empty() && hook_events.is_empty() {
            return Ok(());
        }
        let mut events = hook_events
            .into_iter()
            .map(|msg| crate::agent::turn::turn_event(submission_id, msg))
            .collect::<Vec<_>>();
        let tool_usage = batch_usage(&results)?;
        // Failed persistence restores calls removed by the tentative result application.
        let pending_tools = self.state.pending_tools.clone();
        let active_accounting = self.state.active_execution.as_ref().map(|active| {
            (
                active.tool_calls,
                active.failed_tool_calls,
                active.usage.clone(),
            )
        });
        // Paid-tool totals must roll back with the same failed checkpoint.
        let total_usage = self.state.total_usage.clone();
        let context_len = self.state.context.len();
        let transcript_len = self.transcript_delta.len();
        let outcome = async {
            self.append_tool_results(&mut results).await?;
            events.extend(tool_result_events(submission_id, turn_id, results));
            if tool_usage.is_some()
                && let Some(usage) = self.usage_event(submission_id, tool_usage.as_ref())
            {
                events.push(usage);
            }
            self.persist_with_events(events, None).await?;
            Ok(())
        }
        .await;
        match outcome {
            Ok(_) => Ok(()),
            Err(error) => {
                self.state.make_mut().pending_tools = pending_tools;
                if let Some((tool_calls, failed_tool_calls, usage)) = active_accounting
                    && let Some(active) = self.state.make_mut().active_execution.as_mut()
                {
                    active.tool_calls = tool_calls;
                    active.failed_tool_calls = failed_tool_calls;
                    active.usage = usage;
                }
                self.state.make_mut().total_usage = total_usage;
                let state = self.state.make_mut();
                crate::middleware::delivery_once::rollback(
                    &mut state.delivered_once,
                    &state.context[context_len..],
                );
                Arc::make_mut(&mut self.state.make_mut().context).truncate(context_len);
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
        let pending_approval = self.state.make_mut().pending_approval.take();
        match self
            .persist_tool_results(submission_id, turn_id, completion)
            .await
        {
            Ok(()) => Ok(()),
            Err(error) => {
                self.state.make_mut().pending_approval = pending_approval;
                Err(error)
            }
        }
    }

    async fn append_tool_results(&mut self, results: &mut [ToolResult]) -> Result<()> {
        let tool_calls = u64::try_from(results.len())
            .map_err(|_| Error::Checkpoint("execution tool-call count is unsupported".into()))?;
        let failed_tool_calls = u64::try_from(
            results.iter().filter(|result| result.is_error).count(),
        )
        .map_err(|_| Error::Checkpoint("execution failed-tool count is unsupported".into()))?;
        self.record_tools(tool_calls, failed_tool_calls)?;
        for (route, usage) in results.iter().filter_map(|result| result.usage.as_ref()) {
            self.record_usage(route, usage).await?;
        }
        let completed = results
            .iter()
            .map(|result| result.call_id.as_str())
            .collect::<BTreeSet<_>>();
        self.state
            .make_mut()
            .pending_tools
            .retain(|call| !completed.contains(call.call_id.as_str()));
        for result in results {
            self.push_context(tool_output(
                &result.call_id,
                &result.output,
                result.is_error,
            ));
            self.extend_context(std::mem::take(&mut result.additional_input));
        }
        Ok(())
    }

    pub(super) async fn finish_pending_tools(
        &mut self,
        submission_id: &str,
        turn_id: &str,
        reason: &str,
    ) -> Result<Vec<Event>> {
        let calls = std::mem::take(&mut self.state.make_mut().pending_tools);
        if self.state.active_model_step.is_some() {
            self.extend_context(tool_call_inputs(&calls)?);
        }
        let mut results = interrupted_results(
            &calls,
            &format!("execution interrupted; result unknown: {reason}"),
            self.config.sandbox.output_limit(),
        );
        if results.is_empty() {
            return Ok(Vec::new());
        }
        self.append_tool_results(&mut results).await?;
        Ok(tool_result_events(submission_id, turn_id, results))
    }
}

fn batch_usage(results: &[ToolResult]) -> Result<Option<TokenUsage>> {
    let mut total = None;
    for (_, usage) in results.iter().filter_map(|result| result.usage.as_ref()) {
        total
            .get_or_insert_with(TokenUsage::default)
            .checked_add(usage)
            .ok_or_else(|| {
                Error::Provider("provider token usage exceeds the supported range".into())
            })?;
    }
    Ok(total)
}

pub(super) fn tool_call_inputs(calls: &[ToolCall]) -> Result<Vec<serde_json::Value>> {
    calls
        .iter()
        .map(|call| {
            Ok(serde_json::json!({
                "type": "function_call",
                "call_id": call.call_id,
                "name": call.name,
                "arguments": serde_json::to_string(&call.arguments)?,
            }))
        })
        .collect()
}

fn tool_result_events(submission_id: &str, turn_id: &str, results: Vec<ToolResult>) -> Vec<Event> {
    let mut events = Vec::with_capacity(results.len() * 2);
    for result in results {
        events.push(crate::agent::turn::turn_event(
            submission_id,
            EventMsg::ToolCallEnd(ToolCallEndEvent {
                turn_id: turn_id.to_string(),
                call_id: result.call_id,
                name: result.name,
                output: result.output,
                is_error: result.is_error,
            }),
        ));
        events.extend(
            result
                .events
                .into_iter()
                .map(|msg| crate::agent::turn::turn_event(submission_id, msg)),
        );
    }
    events
}

fn interrupted_results<'a>(
    calls: impl IntoIterator<Item = &'a ToolCall>,
    message: &str,
    output_limit: usize,
) -> Vec<ToolResult> {
    calls
        .into_iter()
        .map(|call| ToolResult::error(call, message, output_limit))
        .collect()
}

pub(super) fn order_results(
    calls: &[impl Borrow<ToolCall>],
    results: Vec<ToolResult>,
) -> Vec<ToolResult> {
    let mut results = results
        .into_iter()
        .map(|result| (result.call_id.clone(), result))
        .collect::<std::collections::BTreeMap<_, _>>();
    calls
        .iter()
        .map(Borrow::borrow)
        .filter_map(|call| results.remove(&call.call_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_usage_includes_every_paid_tool_call() {
        let mut results = (0..2)
            .map(|index| {
                ToolResult::error(
                    &ToolCall {
                        call_id: index.to_string(),
                        name: "paid_tool".into(),
                        arguments: serde_json::Value::Null,
                    },
                    "",
                    crate::backend::sandbox::default_tool_output_limit(),
                )
            })
            .collect::<Vec<_>>();
        for (result, tokens) in results.iter_mut().zip([7, 5]) {
            result.usage = Some((
                "route".into(),
                TokenUsage {
                    total_tokens: tokens,
                    ..TokenUsage::default()
                },
            ));
        }
        assert_eq!(batch_usage(&results).unwrap().unwrap().total_tokens, 12);
    }

    #[test]
    fn interrupted_results_do_not_claim_tools_were_denied() {
        let calls = [ToolCall {
            call_id: "call-1".into(),
            name: "write".into(),
            arguments: serde_json::json!({}),
        }];

        let results = interrupted_results(
            &calls,
            "execution interrupted; result unknown",
            crate::backend::sandbox::default_tool_output_limit(),
        );

        assert_eq!(
            results[0].output.text(),
            "execution interrupted; result unknown"
        );
    }
}
