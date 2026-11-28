//! Tool execution and result persistence.

use std::borrow::Borrow;
use std::collections::BTreeSet;
use std::sync::Arc;

use super::Runner;
use super::SubmissionInbox;
use super::input::Wait;
use crate::Error;
use crate::Result;
use crate::backend::model::tool_output;
use crate::backend::sandbox::SandboxPermissions;
use crate::middleware::tools::{Catalog, ToolBinding, ToolResult, ToolStep, execute_batch};
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
        turn_id: &Arc<str>,
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
        turn_id: &Arc<str>,
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
        submission_id: &Arc<str>,
        turn_id: &Arc<str>,
        calls: &[impl Borrow<ToolCall> + Sync],
        bindings: Option<Vec<ToolBinding>>,
        permissions: SandboxPermissions,
    ) -> Result<Wait<ToolCompletion>> {
        let output_limit = self.config.sandbox.output_limit();
        let calls_iter = calls.iter().map(Borrow::borrow);
        // Calls bound for this step execute directly; after an approval wait they bind again.
        let (bound_calls, callable, mut unavailable_results) = if let Some(bindings) = bindings {
            let mut bindings = bindings.into_iter();
            Catalog::bind_batch(
                calls_iter,
                |_| {
                    bindings
                        .next()
                        .ok_or_else(|| Error::Tool("tool call has no binding".into()))
                },
                output_limit,
            )
        } else {
            let tools = self.live_tools().await?;
            Catalog::bind_batch(
                calls_iter,
                |call| self.catalog.bind_in_place(call, &tools),
                output_limit,
            )
        };
        for call in &callable {
            self.emit(
                Arc::clone(submission_id),
                EventMsg::ToolCallBegin(ToolCallBeginEvent {
                    model_step_id: self.state.tool_step_id().map(Arc::clone),
                    turn_id: Arc::clone(turn_id),
                    call_id: Arc::clone(&call.call_id),
                    name: Arc::clone(&call.name),
                    arguments: call.arguments.clone(), // rules-allow: the begin event owns the borrowed call's fields
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
        let input_changed = drained.input_changed;
        let messages_ready = self
            .config
            .middleware
            .messages_ready(&self.state.pending_messages, turn_id)?;
        if cancel_on_input && (input_changed || messages_ready) {
            let mut results = interrupted_results(
                callable.iter().copied(),
                "execution cancelled before start because newer input is ready",
                output_limit,
            );
            results.append(&mut unavailable_results);
            return Ok(Wait::Ready {
                value: order_results(calls, results).into(),
                input_changed: true,
            });
        }
        let model_route = Arc::clone(&self.config.provider);
        let author = Arc::clone(self.active_author()?);
        let tool_step_id = self.state.tool_step_id().map(Arc::clone);
        let step = ToolStep {
            turn_id,
            model_step_id: tool_step_id.as_ref(),
            model_route: &model_route,
            author: &author,
        };
        let execution = execute_batch(
            &catalog,
            bound_calls,
            Arc::clone(&self.config.sandbox),
            &permissions,
            &step,
        );
        let cancelled_results = || {
            interrupted_results(
                callable.iter().copied(),
                "execution cancelled by newer input; result unknown",
                output_limit,
            )
        };
        let execution = async { (execution.await, true) };
        let wait = self
            .wait_active(inbox, turn_id, execution, || {
                cancel_on_input.then(|| (cancelled_results(), false))
            })
            .await?;
        let ((mut results, executed), input_changed) = match wait {
            Wait::Ready {
                value,
                input_changed: changed,
            } => (value, input_changed || changed),
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
        submission_id: &Arc<str>,
        turn_id: &Arc<str>,
        completion: impl Into<ToolCompletion>,
    ) -> Result<()> {
        let ToolCompletion {
            mut results,
            events: hook_events,
        } = completion.into();
        if results.is_empty() && hook_events.is_empty() {
            return Ok(());
        }
        let mut events =
            crate::agent::turn::turn_events(submission_id, hook_events).collect::<Vec<_>>();
        // Validate the whole batch before usage observers can see a partial result.
        let has_tool_usage = validate_batch_usage(&results)?;
        let mut removed = Vec::new();
        let active_accounting = self
            .state
            .active_execution
            .as_ref()
            .map(|active| (active.tool_calls, active.failed_tool_calls, active.usage));
        // Paid-tool totals must roll back with the same failed checkpoint.
        let total_usage = self.state.total_usage;
        let context_len = self.state.context.len();
        let transcript_len = self.transcript_delta.len();
        let outcome = async {
            self.append_tool_results(&mut results).await?;
            if !results.is_empty() && !self.state.pending_tools.is_empty() {
                let completed: BTreeSet<_> = results.iter().map(|result| &result.call_id).collect();
                let pending = &mut self.state.make_mut().pending_tools;
                for (index, call) in std::mem::take(pending).into_iter().enumerate() {
                    if completed.contains(&call.call_id) {
                        removed.push((index, call));
                    } else {
                        pending.push(call);
                    }
                }
            }
            events.extend(tool_result_events(
                submission_id,
                turn_id,
                self.state.tool_step_id(),
                results,
            ));
            if has_tool_usage {
                events.push(self.usage_event(submission_id));
            }
            self.persist_with_events(events, None).await?;
            Ok(())
        }
        .await;
        match outcome {
            Ok(()) => Ok(()),
            Err(error) => {
                let pending = &mut self.state.make_mut().pending_tools;
                for (index, call) in removed {
                    pending.insert(index, call);
                }
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
                    state.context.get(context_len..).unwrap_or_default(),
                );
                Arc::make_mut(&mut self.state.make_mut().context).truncate(context_len);
                self.transcript_delta.truncate(transcript_len);
                Err(error)
            }
        }
    }

    pub(super) async fn complete_tool_step(
        &mut self,
        submission_id: &Arc<str>,
        turn_id: &Arc<str>,
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
        let tool_calls = u64::try_from(results.len()).map_err(|error| {
            Error::Checkpoint(format!("execution tool-call count is unsupported: {error}"))
        })?;
        let failed_tool_calls = u64::try_from(
            results.iter().filter(|result| result.is_error).count(),
        )
        .map_err(|error| {
            Error::Checkpoint(format!(
                "execution failed-tool count is unsupported: {error}"
            ))
        })?;
        self.record_tools(tool_calls, failed_tool_calls)?;
        for reported in results.iter().filter_map(|result| result.usage.as_ref()) {
            self.record_usage(&reported.route, &reported.usage).await?;
        }
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
        submission_id: &Arc<str>,
        turn_id: &Arc<str>,
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
        Ok(tool_result_events(
            submission_id,
            turn_id,
            self.state.tool_step_id(),
            results,
        ))
    }
}

fn validate_batch_usage(results: &[ToolResult]) -> Result<bool> {
    let mut total = None;
    for reported in results.iter().filter_map(|result| result.usage.as_ref()) {
        total
            .get_or_insert_with(TokenUsage::default)
            .checked_add(&reported.usage)
            .ok_or_else(|| {
                Error::Provider("provider token usage exceeds the supported range".into())
            })?;
    }
    Ok(total.is_some())
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

fn tool_result_events(
    submission_id: &Arc<str>,
    turn_id: &Arc<str>,
    model_step_id: Option<&Arc<str>>,
    results: Vec<ToolResult>,
) -> Vec<Event> {
    let mut events = Vec::with_capacity(results.len() * 2);
    for result in results {
        events.push(crate::agent::turn::turn_event(
            submission_id,
            EventMsg::ToolCallEnd(ToolCallEndEvent {
                model_step_id: model_step_id.map(Arc::clone),
                turn_id: Arc::clone(turn_id),
                call_id: result.call_id,
                name: result.name,
                output: result.output,
                is_error: result.is_error,
            }),
        ));
        events.extend(crate::agent::turn::turn_events(
            submission_id,
            result.events,
        ));
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
    mut results: Vec<ToolResult>,
) -> Vec<ToolResult> {
    if results.len() <= 1 {
        results.retain(|result| {
            calls
                .iter()
                .any(|call| call.borrow().call_id == result.call_id)
        });
        return results;
    }
    // A linear scan wins for small batches; indexing pays off for larger mixed completions.
    if results.len() >= 32 {
        let mut by_id: std::collections::HashMap<_, _> = results
            .iter()
            .enumerate()
            .map(|(index, result)| (result.call_id.as_ref(), index))
            .collect();
        if by_id.len() == results.len() {
            if calls.len() == results.len()
                && calls
                    .iter()
                    .zip(&results)
                    .all(|(call, result)| call.borrow().call_id == result.call_id)
            {
                return results;
            }
            let order: Vec<_> = calls
                .iter()
                .filter_map(|call| by_id.remove(call.borrow().call_id.as_ref()))
                .collect();
            let mut slots: Vec<_> = results.into_iter().map(Some).collect();
            return order
                .into_iter()
                .filter_map(|index| slots.get_mut(index)?.take())
                .collect();
        }
    }
    // Duplicate replies follow the same last-match rule as small batches.
    calls
        .iter()
        .filter_map(|call| {
            let index = results
                .iter()
                .rposition(|result| result.call_id == call.borrow().call_id)?;
            Some(results.swap_remove(index))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::tools::ReportedToolUsage;

    fn call(index: usize) -> ToolCall {
        ToolCall {
            call_id: index.to_string().into(),
            name: "paid_tool".into(),
            arguments: serde_json::Value::Null,
        }
    }

    fn result(call: &ToolCall) -> ToolResult {
        ToolResult::error(
            call,
            "",
            crate::backend::sandbox::default_tool_output_limit(),
        )
    }

    #[test]
    fn large_partial_completion_keeps_call_order() {
        let calls: Vec<_> = (0..96).map(call).collect();
        let completed = calls.iter().step_by(2);
        let results = completed.clone().rev().map(result).collect();
        let ordered = order_results(&calls, results);
        assert!(
            ordered
                .iter()
                .map(|result| &result.call_id)
                .eq(completed.map(|call| &call.call_id))
        );
    }

    #[test]
    fn paid_tool_batches_reject_overflow_before_recording_usage() {
        let mut results: Vec<_> = (0..2).map(|index| result(&call(index))).collect();
        assert!(
            !validate_batch_usage(&results).unwrap(),
            "unpaid calls need no usage event"
        );
        for (result, tokens) in results.iter_mut().zip([7, 5]) {
            result.usage = Some(ReportedToolUsage {
                route: "route".into(),
                usage: TokenUsage {
                    total_tokens: tokens,
                    ..TokenUsage::default()
                },
            });
        }
        assert!(
            validate_batch_usage(&results).unwrap(),
            "paid calls need a usage event"
        );
        results[0].usage.as_mut().unwrap().usage.total_tokens = i64::MAX;
        assert!(
            matches!(validate_batch_usage(&results), Err(Error::Provider(_))),
            "overflow must fail before any observer is called"
        );
    }
}
