use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::{Compaction, apply_compaction, text};
use crate::backend::model::{
    ModelInput, ModelRequest, PromptCacheIdentity, ToolDefinition, internal_user_message,
    prompt_cache_key, tool_output,
};
use crate::middleware::tools::{Catalog, Tool, ToolContext, ToolExposure};
use crate::middleware::{ModelContext, PostToolUseContext};
use crate::protocol::{
    EventMsg, MessageDelivery, ToolCallBeginEvent, ToolCallEndEvent, internal_message_kind,
    is_internal_message, message_metadata, tool_complete_boundaries,
};
use crate::{BoxFuture, Error, Result};

const MAX_NOTE_BYTES: usize = 21_000;
const NOTES: &str = "handoff_notes";
const REQUEST: &str = "handoff_request";

pub(super) fn register(catalog: &mut Catalog, allow: bool) -> Result<()> {
    for write in [true, false] {
        catalog.register(Arc::new(HandoffTool { write, allow }))?;
    }
    Ok(())
}

struct HandoffTool {
    write: bool,
    allow: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteNotes<'a> {
    notes: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewContext {}

fn write_definition() -> ToolDefinition {
    let mut tool = text::DEFINITION.write_handoff.tool.clone();
    tool.parameters["properties"]["notes"]["description"] =
        text::DEFINITION.write_handoff.tool.parameters["properties"]["notes"]["description"]
            .as_str()
            .expect("handoff description")
            .replace("{max_bytes}", &MAX_NOTE_BYTES.to_string())
            .into();
    tool
}

impl Tool for HandoffTool {
    fn definition(&self) -> ToolDefinition {
        if self.write {
            write_definition()
        } else {
            text::DEFINITION.new_context.tool.clone()
        }
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            if !self.allow {
                return Err(Error::Tool("model-requested compaction is disabled".into()));
            }
            if self.write {
                let args = WriteNotes::deserialize(&arguments)?;
                validate_notes(args.notes)?;
                Ok(text::DEFINITION.handoff_saved_result.as_str().into())
            } else {
                let _: NewContext = serde_json::from_value(arguments)?;
                Ok(text::DEFINITION.handoff_requested_result.as_str().into())
            }
        })
    }
}

fn validate_notes(notes: &str) -> Result<()> {
    if notes.trim().is_empty() {
        return Err(Error::Tool(
            "handoff notes must contain non-whitespace text. Checkpoint unchanged.".into(),
        ));
    }
    if notes.len() > MAX_NOTE_BYTES {
        return Err(Error::Tool(format!(
            "handoff notes contain {} UTF-8 bytes; maximum {MAX_NOTE_BYTES}. Remove at least {} bytes. Checkpoint unchanged.",
            notes.len(),
            notes.len() - MAX_NOTE_BYTES
        )));
    }
    Ok(())
}

pub(super) fn is_control(item: &Value) -> bool {
    matches!(internal_message_kind(item), Some(NOTES | REQUEST))
}

fn note_message(notes: &str) -> Value {
    internal_user_message(
        NOTES,
        &format!("{}\n\n{notes}", text::DEFINITION.prompt_restored),
    )
}

pub(super) fn post_tool(context: &mut PostToolUseContext<'_>) -> Result<()> {
    if context.result().is_error {
        return Ok(());
    }
    if context.call.name == "write_handoff" {
        let notes = WriteNotes::deserialize(&context.call.arguments)?.notes;
        context.push_input(note_message(notes));
    } else if context.call.name == "new_context" {
        context.push_input(internal_user_message(
            REQUEST,
            &text::DEFINITION.handoff_requested_context,
        ));
    }
    Ok(())
}

pub(super) async fn prepare(context: &mut ModelContext<'_>, policy: &Compaction) -> Result<()> {
    if !policy.allow_model_compaction {
        context.available_tools.remove("new_context");
        context.available_tools.remove("write_handoff");
    }
    // Requests belong to their originating turn; a failed reset must not replay on later turns.
    let requested = policy.allow_model_compaction && requested_in_active_turn(context.input());
    let estimated = context.estimated_input_tokens();
    let switching = !context
        .model
        .same_context_model(context.context_provider, context.provider);
    let threshold = if switching {
        context
            .context_window
            .saturating_sub(policy.reserve(context.context_window))
            .max(1)
    } else {
        policy.trigger_tokens(context.context_window)
    };
    let observed = estimated.max(if switching {
        0
    } else {
        context.last_usage.map_or(0, |usage| usage.input_tokens)
    });
    if !requested && observed < threshold {
        return Ok(());
    }
    context.pre_compact().await?;
    if context.turn_stopped() {
        return Ok(());
    }
    super::start_notice(context)?;
    let attempts = tool_complete_boundaries(context.input().iter())
        .len()
        .max(1);
    let fit = context
        .context_window
        .saturating_sub(policy.reserve(context.context_window))
        .max(1);
    let mut previous = context.estimated_input_tokens();
    for _ in 0..attempts {
        let (input, partial) = prepare_checkpoint(context, policy, switching).await?;
        apply_compaction(context, input).await?;
        if context.turn_stopped() {
            return Ok(());
        }
        let remaining = context.estimated_input_tokens();
        if remaining < fit {
            return Ok(());
        }
        if !partial || remaining >= previous {
            break;
        }
        previous = remaining;
    }
    Err(Error::Stopped("prepared checkpoint does not fit the selected model; a preserved input or complete tool batch exceeds its budget; the chat and pending input are preserved".into()))
}

async fn prepare_checkpoint(
    context: &mut ModelContext<'_>,
    policy: &Compaction,
    switching: bool,
) -> Result<(Vec<Arc<Value>>, bool)> {
    let pending = if switching {
        active_turn(context.input())
    } else {
        context.input().len()
    };
    let definitions = context.tools.direct_definitions();
    let definition = definitions
        .iter()
        .find(|tool| tool.name == "write_handoff")
        .ok_or_else(|| Error::Config("checkpoint writer is not registered".into()))?;
    let tools = std::slice::from_ref(definition);
    let source = context.context_provider;
    let result = prepare_response(context, policy, source, context.input(), pending, tools).await;
    let (output, summarized, route) = match result {
        Ok((output, summarized)) => (output, summarized, source),
        Err(error)
            if switching
                && matches!(
                    error,
                    Error::Provider(_)
                        | Error::Auth(_)
                        | Error::Unknown(_)
                        | Error::Http(_)
                        | Error::Io(_)
                ) =>
        {
            let mut portable = (0..context.input().len())
                .map(|index| {
                    context
                        .input()
                        .shared_item(index)
                        .map(Arc::clone)
                        .ok_or_else(|| {
                            Error::Checkpoint(
                                "checkpoint preparation requires shared history".into(),
                            )
                        })
                })
                .collect::<Result<Vec<_>>>()?;
            crate::backend::model::strip_shared_provider_reasoning(&mut portable);
            let pending = active_turn(portable.as_slice().into());
            let (output, summarized) = prepare_response(
                context,
                policy,
                context.provider,
                portable.as_slice().into(),
                pending,
                tools,
            )
            .await?;
            // Top-level reasoning is omitted; map partial progress to its durable boundary.
            let summarized = if summarized == pending {
                active_turn(context.input())
            } else {
                context
                    .input()
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| !crate::backend::model::is_provider_reasoning(item))
                    .nth(summarized - 1)
                    .map(|(index, _)| index + 1)
                    .ok_or_else(|| {
                        Error::Checkpoint("checkpoint preparation lost its source boundary".into())
                    })?
            };
            (output, summarized, context.provider)
        }
        Err(error) => return Err(error),
    };
    let (output, mut calls, usage) = output.into_parts();
    context.record_usage(route, usage);
    if calls.len() != 1 || calls[0].name != "write_handoff" {
        return Err(Error::Provider(
            "checkpoint preparation must return one write_handoff call; the chat is preserved"
                .into(),
        ));
    }
    let call = calls.pop().expect("one validated checkpoint call");
    let notes = WriteNotes::deserialize(&call.arguments)?.notes;
    validate_notes(notes)?;
    let mut fresh = retained_input_until(context.input(), summarized)?;
    fresh.insert(0, Arc::new(note_message(notes)));
    // Journal the genuine preparation output without retaining the checkpoint tool's duplicate notes.
    for item in output {
        context.record_transcript_item(item);
    }
    let result = tool_output(
        &call.call_id,
        &text::DEFINITION.handoff_saved_result.as_str().into(),
        false,
    );
    context.record_transcript_item(result);
    context
        .events
        .push(EventMsg::ToolCallBegin(ToolCallBeginEvent {
            turn_id: context.turn_id.into(),
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            arguments: call.arguments,
        }));
    context.events.push(EventMsg::ToolCallEnd(ToolCallEndEvent {
        turn_id: context.turn_id.into(),
        call_id: call.call_id,
        name: call.name,
        output: text::DEFINITION.handoff_saved_result.as_str().into(),
        is_error: false,
    }));
    Ok((fresh, summarized < pending))
}

async fn prepare_response(
    context: &ModelContext<'_>,
    policy: &Compaction,
    route: &str,
    history: ModelInput<'_>,
    pending: usize,
    tools: &[Arc<ToolDefinition>],
) -> Result<(crate::backend::model::ModelOutput, usize)> {
    let source_window = context
        .model
        .resolve_choice(route, None)?
        .context_window
        .unwrap_or(context.context_window);
    let guidance = internal_user_message("handoff_preparation", &text::DEFINITION.prompt_prepare);
    let overhead = context
        .token_estimate
        .tokens(
            context
                .instructions
                .len()
                .saturating_add(serde_json::to_vec(&tools)?.len()),
        )
        .saturating_add(context.token_estimate.item_tokens(&guidance));
    let budget = usize::try_from(
        source_window
            .saturating_sub(policy.reserve(source_window))
            .max(1),
    )
    .unwrap_or(usize::MAX);
    let mut estimate = overhead;
    let mut fitted = 0;
    let boundaries = tool_complete_boundaries(history.iter());
    for (index, item) in history.iter().take(pending).enumerate() {
        estimate = estimate.saturating_add(context.token_estimate.item_tokens(item));
        if estimate >= budget {
            break;
        }
        if boundaries.binary_search(&(index + 1)).is_ok() {
            fitted = index + 1;
        }
    }
    if fitted == 0 {
        return Err(Error::Provider("checkpoint preparation cannot fit one complete input or tool batch; the chat and pending input are preserved".into()));
    }
    let input = history
        .prefix(fitted)
        .with_appended(std::slice::from_ref(&guidance).into())?;
    let cache_key = prompt_cache_key(context.session_id);
    let request = ModelRequest {
        session_id: context.session_id,
        cancellation: context.cancellation,
        prompt_cache: Some(PromptCacheIdentity {
            key: &cache_key,
            context_epoch: *context.context_epoch,
        }),
        instructions: context.instructions,
        input,
        catalog_revision: context.tools.revision()?,
        tools,
        deferred_tools: &[],
        allow_hosted_tools: false,
        allow_continuation: false,
    };
    let transport = context.model.transport_settings_for(route)?;
    let retry_limit = usize::try_from(transport.stream_retry_limit)
        .map_err(|_| Error::Config("stream retry limit exceeds platform range".into()))?;
    let mut retries = 0;
    let mut fallback_attempted = false;
    loop {
        let error = match context
            .model
            .respond(route, request, Arc::new(|_| Box::pin(async { Ok(()) })))
            .await
        {
            Ok(output) => return Ok((output, fitted)),
            Err(Error::Provider(error))
                if error.is_stream_interrupted() || error.is_retryable() =>
            {
                error
            }
            Err(error) => return Err(error),
        };
        let mut delay =
            crate::backend::model::retry_delay(&error, retries, context.turn_id, &transport);
        if retries < retry_limit {
            retries += 1;
        } else {
            if fallback_attempted {
                return Err(Error::Provider(error));
            }
            fallback_attempted = true;
            if !context
                .model
                .fallback_transport(route, context.session_id)
                .await?
            {
                return Err(Error::Provider(error));
            }
            retries = 0;
            if error.retry_after().is_none() {
                delay = std::time::Duration::ZERO;
            }
        }
        tokio::time::sleep(delay).await;
    }
}

fn requested_in_active_turn(input: ModelInput<'_>) -> bool {
    input
        .suffix(active_turn(input))
        .iter()
        .any(|item| internal_message_kind(item) == Some(REQUEST))
}

fn active_turn(input: ModelInput<'_>) -> usize {
    input
        .iter()
        .rposition(|item| {
            message_metadata(item).is_some_and(|message| message.delivery != MessageDelivery::Steer)
        })
        .or_else(|| {
            input.iter().rposition(|item| {
                !is_internal_message(item)
                    && item.get("role").and_then(Value::as_str) == Some("user")
            })
        })
        .unwrap_or(0)
}

#[cfg(test)]
fn retained_input(input: ModelInput<'_>) -> Result<Vec<Arc<Value>>> {
    retained_input_until(input, input.len())
}

fn retained_input_until(input: ModelInput<'_>, summarized: usize) -> Result<Vec<Arc<Value>>> {
    let boundaries = tool_complete_boundaries(input.iter());
    if boundaries.last().copied() != Some(input.len()) {
        return Err(Error::Checkpoint(
            "handoff requires a completed tool batch".into(),
        ));
    }
    let active = active_turn(input);
    // Preserve the latest complete batch; other tool results may be newer than the notes.
    let tail = input
        .iter()
        .rposition(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        .filter(|call| *call >= active)
        .map(|call| {
            boundaries
                .iter()
                .copied()
                .take_while(|boundary| *boundary <= call)
                .last()
                .unwrap_or(0)
        })
        .unwrap_or(input.len());
    input
        .iter()
        .enumerate()
        .filter(|(index, item)| {
            internal_message_kind(item) != Some(REQUEST)
                && (!is_control(item) || *index >= summarized)
                && (*index >= summarized
                    || *index >= tail
                    || (*index >= active
                        && item.get("role").and_then(Value::as_str) == Some("user")))
        })
        .map(|(index, _)| {
            input.shared_item(index).map(Arc::clone).ok_or_else(|| {
                Error::Checkpoint("checkpoint retention requires shared durable history".into())
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
