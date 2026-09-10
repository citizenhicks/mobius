use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::{apply_compaction, text};
use crate::backend::checkpoint::CheckpointStore;
use crate::backend::model::{ToolDefinition, internal_user_message};
use crate::middleware::tools::{Catalog, Tool, ToolContext, ToolExposure};
use crate::middleware::{
    ModelContext, ModelRequestContext, PostToolUseContext, RuntimeContext, SessionStartContext,
};
use crate::protocol::{
    MessageDelivery, internal_message_kind, is_internal_message, message_metadata,
    tool_complete_boundaries,
};
use crate::{BoxFuture, Error, Result};

const STATE_KEY: &str = "compaction.handoff";
const MAX_NOTE_BYTES: usize = 21_000;
const NOTES: &str = "handoff_notes";
const SAVED: &str = "handoff_saved";
const REQUEST: &str = "handoff_request";
const WARNING: &str = "handoff_warning";
const URGENT: &str = "handoff_urgent";
const RESET_ONLY: &str = "handoff_reset_only";
const CALL_ID: &str = "_mobius_handoff_call_id";
const TURN_ID: &str = "_mobius_handoff_turn_id";
const MODEL_STEP: &str = "_mobius_handoff_model_step";

pub(super) fn register(catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
    for write in [true, false] {
        catalog.register(Arc::new(HandoffTool {
            checkpoints: Arc::clone(&runtime.checkpoints),
            session_id: runtime.session_id.clone(),
            write,
        }))?;
    }
    Ok(())
}

struct HandoffTool {
    checkpoints: Arc<dyn CheckpointStore>,
    session_id: String,
    write: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteNotes {
    notes: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewContext {}

impl Tool for HandoffTool {
    fn definition(&self) -> ToolDefinition {
        if self.write {
            ToolDefinition {
                name: "write_handoff".into(),
                description: text::TOOL_WRITE_HANDOFF_DESCRIPTION.into(),
                parameters: serde_json::json!({
                    "type": "object", "properties": {"notes": {
                        "type": "string", "minLength": 1,
                        "description": format!("Working checkpoint, at most {MAX_NOTE_BYTES} UTF-8 bytes after trimming surrounding whitespace.")
                    }},
                    "required": ["notes"], "additionalProperties": false
                }),
            }
        } else {
            ToolDefinition {
                name: "new_context".into(),
                description: text::TOOL_NEW_CONTEXT_DESCRIPTION.into(),
                parameters: serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
            }
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
            if self.write {
                let args: WriteNotes = serde_json::from_value(arguments)?;
                let notes = args.notes.trim().to_owned();
                validate_notes(&notes)?;
                self.checkpoints
                    .save_state(&self.session_id, STATE_KEY, &Value::String(notes))
                    .await?;
                Ok("Saved this chat's handoff checkpoint.".into())
            } else {
                let _: NewContext = serde_json::from_value(arguments)?;
                load_notes(&self.checkpoints, &self.session_id)
                    .await?
                    .ok_or_else(|| {
                        Error::Tool(
                            "save a checkpoint with write_handoff before requesting new_context"
                                .into(),
                        )
                    })?;
                Ok("Requested a new context window after this tool batch is durably saved.".into())
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
    let size = notes.len();
    if size > MAX_NOTE_BYTES {
        return Err(Error::Tool(format!(
            "handoff notes contain {size} UTF-8 bytes; maximum {MAX_NOTE_BYTES}. Remove at least {} bytes. Checkpoint unchanged.",
            size - MAX_NOTE_BYTES
        )));
    }
    Ok(())
}

async fn load_notes(store: &Arc<dyn CheckpointStore>, session_id: &str) -> Result<Option<String>> {
    let notes = store
        .load_state(session_id, STATE_KEY)
        .await?
        .map(serde_json::from_value::<String>)
        .transpose()?;
    if let Some(notes) = &notes {
        validate_notes(notes)?;
    }
    Ok(notes)
}

pub(super) fn is_control(item: &Value) -> bool {
    matches!(
        internal_message_kind(item),
        Some(NOTES | SAVED | REQUEST | WARNING | URGENT | RESET_ONLY)
    )
}

fn note_message(notes: &str) -> Value {
    internal_user_message(NOTES, &format!("{}\n\n{notes}", text::PROMPT_RESTORED))
}

pub(super) async fn restore_notes(context: &mut SessionStartContext<'_>) -> Result<()> {
    let Some(notes) = load_notes(&context.runtime.checkpoints, &context.runtime.session_id).await?
    else {
        return Ok(());
    };
    let item = note_message(&notes);
    if context
        .input
        .iter()
        .rev()
        .find(|item| internal_message_kind(item) == Some(NOTES))
        .map(|item| &item["content"])
        != Some(&item["content"])
    {
        context.push_input(item);
    }
    Ok(())
}

pub(super) fn decorate(context: &mut ModelRequestContext<'_>) {
    let prompt = context
        .input()
        .iter()
        .rev()
        .find_map(|item| match internal_message_kind(item) {
            Some(RESET_ONLY) => Some(text::PROMPT_RESET),
            Some(URGENT) => Some(text::PROMPT_URGENT),
            Some(WARNING) => Some(text::PROMPT_WARNING),
            _ => None,
        });
    if let Some(prompt) = prompt {
        context
            .input
            .to_mut()
            .push(internal_user_message("handoff_notice", prompt));
    }
}

pub(super) fn post_tool(context: &mut PostToolUseContext<'_>) -> Result<()> {
    if context.result().is_error
        || !matches!(context.call.name.as_str(), "write_handoff" | "new_context")
    {
        return Ok(());
    }
    if context.call.name == "write_handoff" {
        context.push_input(internal_user_message(SAVED, "Working checkpoint saved."));
    } else {
        let mut request = internal_user_message(REQUEST, "Context transition requested.");
        request[CALL_ID] = Value::String(context.call.call_id.clone());
        context.push_input(request);
    }
    Ok(())
}

fn reserve_tokens(context_window: i64) -> i64 {
    (context_window.max(1) / 8).clamp(1, super::COMPACTION_RESERVE_TOKENS)
}

pub(super) fn warning_tokens(threshold: i64, context_window: i64) -> i64 {
    threshold
        .min(
            context_window
                .max(1)
                .saturating_sub(reserve_tokens(context_window).saturating_mul(3)),
        )
        .max(1)
}

pub(super) async fn prepare(context: &mut ModelContext<'_>, threshold: i64) -> Result<()> {
    if let Some(request) = context
        .input()
        .iter()
        .rev()
        .find(|item| internal_message_kind(item) == Some(REQUEST))
    {
        let call_id = request
            .get(CALL_ID)
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Checkpoint("handoff request is missing its tool call".into()))?;
        let input = fresh_input(context.input(), call_id)?;
        context.pre_compact().await?;
        if context.turn_stopped() {
            return Ok(());
        }
        apply_compaction(context, input, None).await?;
        if context.turn_stopped() {
            return Ok(());
        }
    }

    let observed = context
        .last_usage
        .map_or(0, |usage| usage.input_tokens)
        .max(context.estimated_input_tokens());
    let window = context.context_window.max(1);
    let reserve = reserve_tokens(window);
    let hard = window.saturating_sub(reserve.saturating_mul(2)).max(1);
    let warning = warning_tokens(threshold, window);
    let last = |kind| {
        context
            .input()
            .iter()
            .rposition(|item| internal_message_kind(item) == Some(kind))
    };
    let current_attempt = |index: usize| {
        context.input()[index].get(TURN_ID).and_then(Value::as_str) == Some(context.turn_id)
    };
    // A steer can repeat preparation before this model step has been sent.
    let same_step = |index: usize| {
        context.input()[index]
            .get(MODEL_STEP)
            .and_then(Value::as_u64)
            == Some(context.model_step as u64)
    };
    let reset = last(RESET_ONLY).filter(|index| current_attempt(*index));
    if observed >= window.saturating_sub(reserve).max(1)
        || reset.is_some_and(|index| !same_step(index))
    {
        return Err(Error::Stopped("context handoff could not finish within the remaining budget; the chat and saved checkpoint are preserved".into()));
    }
    let reset_only = if reset.is_some() {
        true
    } else if let Some(urgent) = last(URGENT).filter(|index| current_attempt(*index)) {
        if same_step(urgent) {
            false
        } else {
            if last(SAVED).is_none_or(|notes| notes < urgent) {
                return Err(Error::Stopped(
                    "the model did not save its required handoff; the chat is preserved".into(),
                ));
            }
            context.append_model_input(attempt_message(
                RESET_ONLY,
                "Context recovery checkpoint saved.",
                context.turn_id,
                context.model_step,
            ));
            true
        }
    } else if observed >= hard || last(URGENT).is_some() {
        context.append_model_input(attempt_message(
            URGENT,
            "Context recovery pending.",
            context.turn_id,
            context.model_step,
        ));
        false
    } else {
        if observed >= warning && last(WARNING).is_none() {
            context
                .append_model_input(internal_user_message(WARNING, "Context threshold reached."));
        }
        return Ok(());
    };
    context.disable_hosted_tools();
    context
        .available_tools
        .retain(|name| name == "new_context" || (!reset_only && name == "write_handoff"));
    Ok(())
}

fn attempt_message(kind: &str, text: &str, turn_id: &str, model_step: usize) -> Value {
    let mut item = internal_user_message(kind, text);
    item[TURN_ID] = Value::String(turn_id.into());
    item[MODEL_STEP] = Value::from(model_step);
    item
}

fn fresh_input(input: &[Value], call_id: &str) -> Result<Vec<Value>> {
    let boundaries = tool_complete_boundaries(input);
    if boundaries.last().copied() != Some(input.len()) {
        return Err(Error::Checkpoint(
            "handoff requires a completed tool batch".into(),
        ));
    }
    let call = input
        .iter()
        .rposition(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call")
                && item.get("call_id").and_then(Value::as_str) == Some(call_id)
        })
        .ok_or_else(|| Error::Checkpoint("handoff tool call is missing".into()))?;
    // Keep the entire requesting batch: other results may have arrived after the notes were written.
    let tail = boundaries
        .into_iter()
        .take_while(|boundary| *boundary <= call)
        .last()
        .unwrap_or(0);
    let active_turn = input
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
        .unwrap_or(0);
    Ok(input
        .iter()
        .enumerate()
        .filter(|(index, item)| {
            !is_control(item)
                && (*index >= tail
                    || (*index >= active_turn
                        && (item.get("role").and_then(Value::as_str) == Some("user")
                            || super::is_attachment_materialization(item))))
        })
        .map(|(_, item)| item.clone())
        .collect())
}

#[cfg(test)]
mod tests;
