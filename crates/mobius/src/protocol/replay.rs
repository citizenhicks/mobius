//! Converts neutral model history into frontend presentation events.

use crate::SessionId;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::Deserialize as _;
use serde_json::Value;

use crate::protocol::AssistantMessageEvent;
use crate::protocol::EventMsg;
use crate::protocol::MessageEvent;
use crate::protocol::MessageTarget;
use crate::protocol::ModelStepContent;
use crate::protocol::ModelStepContentPhase;
use crate::protocol::ToolCallBeginEvent;
use crate::protocol::ToolCallEndEvent;
use crate::protocol::ToolLoad;
use crate::protocol::ToolLoadEvent;

pub(crate) const INTERNAL_MESSAGE_FIELD: &str = "_mobius_internal";
pub(crate) const CONTEXT_COMPACTED_MARKER: &str = "context_compacted";
pub(crate) const ATTACHMENT_CONTEXT_MARKER: &str = "attachments";
pub(crate) const MESSAGE_METADATA_FIELD: &str = "_mobius_message";
pub(crate) const ATTACHMENTS_FIELD: &str = "_mobius_attachments";
pub(crate) const REPLAY_REASONING_FIELD: &str = "_mobius_reasoning";
pub(crate) const PROMPT_CACHE_BREAKPOINT_FIELD: &str = "_mobius_prompt_cache_breakpoint";
pub(crate) const TOOL_ERROR_FIELD: &str = "_mobius_is_error";

pub(crate) fn internal_message_kind(message: &Value) -> Option<&str> {
    message.get(INTERNAL_MESSAGE_FIELD)?.as_str()
}

pub(crate) fn is_internal_message(message: &Value) -> bool {
    internal_message_kind(message).is_some()
}

const FORKED_ATTACHMENT_PLACEHOLDER: &str = "[Attachment unavailable in this fork]";

pub(crate) fn strip_attachment_references(items: &mut Vec<Arc<Value>>) {
    for item in items.iter_mut() {
        if item.get(ATTACHMENTS_FIELD).is_none()
            && item
                .get(MESSAGE_METADATA_FIELD)
                .and_then(|metadata| metadata.get("attachments"))
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty)
        {
            continue;
        }
        let item = Arc::make_mut(item);
        let needs_placeholder = item.get("role").and_then(Value::as_str) == Some("user")
            && item
                .get(ATTACHMENTS_FIELD)
                .and_then(|value| {
                    <Vec<crate::protocol::SessionFileReference> as serde::Deserialize>::deserialize(
                        value,
                    )
                    .ok()
                })
                .is_some_and(|files| !files.is_empty())
            && !has_message_text(item);
        if let Some(object) = item.as_object_mut() {
            object.remove(ATTACHMENTS_FIELD);
            if needs_placeholder {
                object.insert(
                    "content".into(),
                    serde_json::json!([{
                        "type": "input_text",
                        "text": FORKED_ATTACHMENT_PLACEHOLDER
                    }]),
                );
            }
            if let Some(mut message) = object
                .get(MESSAGE_METADATA_FIELD)
                .and_then(|value| <MessageEvent as serde::Deserialize>::deserialize(value).ok())
            {
                message.attachments.clear();
                if needs_placeholder {
                    message.text = FORKED_ATTACHMENT_PLACEHOLDER.into();
                }
                if let Ok(metadata) = serde_json::to_value(message) {
                    object.insert(MESSAGE_METADATA_FIELD.into(), metadata);
                }
            }
        }
    }
    items.retain_mut(|item| {
        if internal_message_kind(item) != Some(ATTACHMENT_CONTEXT_MARKER) {
            return true;
        }
        let is_image =
            |part: &Value| part.get("type").and_then(Value::as_str) == Some("input_image");
        if !item
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| parts.iter().any(is_image))
        {
            return false;
        }
        let images = if let Some(item) = Arc::get_mut(item) {
            let mut parts = item
                .get_mut("content")
                .and_then(Value::as_array_mut)
                .map_or_default(std::mem::take);
            parts.retain(is_image);
            parts
        } else {
            // A fork owns its rewritten item; copy only retained images, never attachment text.
            item.get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|part| is_image(part))
                .cloned() // rules-allow: a shared item keeps its images, so the fork copies them
                .collect()
        };
        let inherited = serde_json::json!({
            "role": "user",
            (INTERNAL_MESSAGE_FIELD): "inherited_media",
            "content": images
        });
        *item = Arc::new(inherited);
        true
    });
}

fn has_message_text(value: &Value) -> bool {
    match value.get("content") {
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(parts)) => parts.iter().any(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
        }),
        _ => false,
    }
}

/// Returns one-based context boundaries with no unfinished tool calls.
pub(crate) fn tool_complete_boundaries<'a>(
    input: impl IntoIterator<Item = &'a Value>,
) -> Vec<usize> {
    let mut open_calls = BTreeSet::new();
    let mut complete = Vec::new();
    for (index, item) in input.into_iter().enumerate() {
        match item.get("type").and_then(Value::as_str) {
            Some(kind @ ("function_call" | "tool_search_call"))
                if kind == "function_call" || item["execution"].as_str() != Some("server") =>
            {
                let Some(call_id) = super::nonempty_string(item, "call_id") else {
                    // No later output can identify this unfinished call.
                    break;
                };
                open_calls.insert(call_id);
            }
            Some("function_call_output" | "tool_search_output") => {
                if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                    open_calls.remove(call_id);
                }
            }
            Some(_) | None => {}
        }
        if open_calls.is_empty() {
            complete.push(index + 1);
        }
    }
    complete
}

/// Reconstructs frontend-neutral events from positioned durable transcript items.
#[must_use]
pub fn events<V: std::borrow::Borrow<Value>>(
    context: &[(MessageTarget, V)],
    session_id: &SessionId,
) -> Vec<EventMsg> {
    let mut events = Vec::new();
    let mut tools = BTreeMap::new();
    let complete = tool_complete_boundaries(context.iter().map(|(_, value)| value.borrow()));
    for (index, (target, value)) in context.iter().enumerate() {
        let value = value.borrow();
        if internal_message_kind(value) == Some(CONTEXT_COMPACTED_MARKER) {
            events.push(EventMsg::ContextCompacted);
            continue;
        }
        let message_target = complete
            .binary_search(&(index + 1))
            .is_ok()
            .then_some(*target);
        let item_id: Arc<str> = replay_id(target).into();
        if let Some(mut message) = message_metadata(value) {
            message.message_target = message_target;
            events.push(EventMsg::Message(message));
            continue;
        }
        if value.get("role").and_then(Value::as_str) == Some("assistant") {
            let content = assistant_content(value);
            if !content.is_empty() {
                events.push(EventMsg::AssistantMessage(AssistantMessageEvent {
                    session_id: session_id.share(),
                    turn_id: Arc::clone(&item_id),
                    model_step_id: Arc::clone(&item_id),
                    content,
                    message_target,
                }));
            }
            continue;
        }
        match value.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                if let Some(text) = reasoning_text(value) {
                    events.push(EventMsg::AssistantMessage(AssistantMessageEvent {
                        session_id: session_id.share(),
                        turn_id: Arc::clone(&item_id),
                        model_step_id: item_id,
                        content: vec![ModelStepContent {
                            output_index: 0,
                            part_index: 0,
                            phase: ModelStepContentPhase::Reasoning,
                            text,
                            annotations: Vec::new(),
                        }],
                        message_target: None,
                    }));
                }
            }
            Some("function_call") => {
                let call_id = string(value, "call_id");
                let name = string(value, "name");
                if call_id.is_empty() || name.is_empty() {
                    continue;
                }
                tools.insert(
                    Arc::clone(&call_id),
                    (Arc::clone(&name), Arc::clone(&item_id)),
                );
                events.push(EventMsg::ToolCallBegin(ToolCallBeginEvent {
                    model_step_id: None,
                    turn_id: item_id,
                    call_id,
                    name,
                    arguments: arguments(value.get("arguments")),
                }));
            }
            Some("function_call_output") => {
                let call_id = string(value, "call_id");
                let Some(output) = value.get("output").and_then(|output| {
                    output
                        .as_array()?
                        .iter()
                        .map(|part| {
                            let fields = part
                                .as_object()?
                                .iter()
                                .filter(|(key, _)| key.as_str() != PROMPT_CACHE_BREAKPOINT_FIELD)
                                .map(|(key, value)| (key.as_str(), value));
                            super::ContentPart::deserialize(serde::de::value::MapDeserializer::new(
                                fields,
                            ))
                            .ok()
                        })
                        .collect::<Option<Vec<_>>>()
                        .map(super::ToolContent)
                }) else {
                    continue;
                };
                let (name, turn_id) = tools.get(&call_id).map_or_else(
                    || ("tool".into(), item_id),
                    |(name, turn_id)| (Arc::clone(name), Arc::clone(turn_id)),
                );
                events.push(EventMsg::ToolCallEnd(ToolCallEndEvent {
                    model_step_id: None,
                    turn_id,
                    name,
                    call_id,
                    is_error: value
                        .get(TOOL_ERROR_FIELD)
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    output,
                }));
            }
            Some("tool_load") => {
                let Ok(Some(load)) = ToolLoad::from_input(value) else {
                    continue;
                };
                events.push(EventMsg::ToolLoad(ToolLoadEvent {
                    load_id: item_id.to_string(),
                    turn_id: item_id,
                    catalog_revision: load.catalog_revision,
                    tools: load.tools,
                }));
            }
            Some(_) | None => {}
        }
    }
    events
}

pub(crate) fn message_metadata(value: &Value) -> Option<MessageEvent> {
    MessageEvent::deserialize(value.get(MESSAGE_METADATA_FIELD)?).ok()
}

/// The user-visible text that session search and memory can recover from one history item.
pub(crate) fn history_text(item: &Value) -> Option<(&'static str, Cow<'_, str>)> {
    if let Some(message) = message_metadata(item) {
        return Some(match message.author {
            super::MessageAuthor::User => {
                let mut text = message.text;
                for file in message.attachments {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&stored_file_text(&serde_json::to_value(file).ok()?));
                }
                ("user", Cow::Owned(text))
            }
            super::MessageAuthor::Source { source, handle, .. } => (
                "assistant",
                Cow::Owned(format!("@{handle} ({}): {}", source.id(), message.text)),
            ),
        });
    }
    if is_internal_message(item) {
        let attachments = super::content_parts(item)?.iter().filter(|part| {
            matches!(
                part.get("type").and_then(Value::as_str),
                Some("input_image" | "file")
            )
        });
        return Some(("user", joined_text(attachments)?));
    }
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            let name = item.get("name")?.as_str()?;
            let arguments = item.get("arguments")?;
            let text = match arguments.as_str() {
                Some(raw) => format!("{name}\n{raw}"),
                None => format!("{name}\n{arguments}"),
            };
            Some(("tool_call", Cow::Owned(text)))
        }
        Some("function_call_output") => Some((
            "tool_result",
            joined_text(item.get("output")?.as_array()?.iter()).unwrap_or_default(),
        )),
        Some("reasoning" | "compaction") => None,
        _ => {
            let kind = match item.get("role")?.as_str()? {
                "user" => "user",
                "assistant" => "assistant",
                _ => return None,
            };
            let text = match item.get("content")? {
                Value::String(text) => Cow::Borrowed(text.as_str()),
                Value::Array(parts) => joined_text(parts.iter()).unwrap_or_default(),
                Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => return None,
            };
            Some((kind, text))
        }
    }
}

fn joined_text<'a>(parts: impl Iterator<Item = &'a Value>) -> Option<Cow<'a, str>> {
    let mut parts = parts.filter_map(content_part_text);
    let mut text = parts.next()?;
    for part in parts {
        let text = text.to_mut();
        text.push('\n');
        text.push_str(&part);
    }
    Some(text)
}

fn stored_file_text(file: &Value) -> String {
    format!("Stored file: {file}")
}

/// Readable durable evidence, including references that can be reopened after compaction.
fn content_part_text(part: &Value) -> Option<Cow<'_, str>> {
    if let Some(text) = part
        .get("text")
        .or_else(|| part.get("content"))
        .and_then(Value::as_str)
    {
        return Some(Cow::Borrowed(text));
    }
    match part.get("type").and_then(Value::as_str) {
        Some("input_image") => part
            .get("image")
            .map(|image| Cow::Owned(format!("Image: {image}"))),
        Some("file") => part
            .get("file")
            .map(|file| Cow::Owned(stored_file_text(file))),
        _ => None,
    }
}

fn replay_id(target: &MessageTarget) -> String {
    format!(
        "history-{}-{}",
        target.checkpoint_sequence, target.batch_item_count
    )
}

fn assistant_content(value: &Value) -> Vec<ModelStepContent> {
    let mut content = reasoning_text(value)
        .filter(|text| !text.trim().is_empty())
        .map(|text| ModelStepContent {
            output_index: 0,
            part_index: 0,
            phase: ModelStepContentPhase::Reasoning,
            text,
            annotations: Vec::new(),
        })
        .into_iter()
        .collect::<Vec<_>>();
    let phase = if value.get("phase").and_then(Value::as_str) == Some("commentary") {
        ModelStepContentPhase::Commentary
    } else {
        ModelStepContentPhase::FinalAnswer
    };
    match value.get("content") {
        Some(Value::String(text)) if !text.is_empty() => content.push(ModelStepContent {
            output_index: 0,
            part_index: 0,
            phase,
            text: text.clone(), // rules-allow: replay builds owned content from a borrowed item
            annotations: Vec::new(),
        }),
        Some(Value::Array(parts)) => {
            content.extend(parts.iter().enumerate().filter_map(|(part_index, part)| {
                let text = part.get("text").and_then(Value::as_str)?;
                if text.is_empty() {
                    return None;
                }
                Some(ModelStepContent {
                    output_index: 0,
                    part_index,
                    phase,
                    text: text.into(),
                    annotations: part
                        .get("annotations")
                        .and_then(|annotations| Vec::deserialize(annotations).ok())
                        .unwrap_or_default(),
                })
            }));
        }
        Some(_) | None => {}
    }
    content
}

fn reasoning_text(value: &Value) -> Option<String> {
    value
        .get(REPLAY_REASONING_FIELD)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn string(value: &Value, field: &str) -> Arc<str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .into()
}

fn arguments(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(value)) => {
            serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.clone())) // rules-allow: replay builds owned arguments from a borrowed item
        }
        Some(value) => value.clone(), // rules-allow: replay builds owned arguments from a borrowed item
        None => serde_json::json!({}),
    }
}

#[cfg(test)]
mod tests;
