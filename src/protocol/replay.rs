//! Converts neutral model history into frontend presentation events.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::protocol::AgentMessageEvent;
use crate::protocol::AgentMessagePhase;
use crate::protocol::AgentReasoningContentDeltaEvent;
use crate::protocol::EventMsg;
use crate::protocol::MessageTarget;
use crate::protocol::ToolCallBeginEvent;
use crate::protocol::ToolCallEndEvent;
use crate::protocol::UserMessageEvent;

pub(crate) const INTERNAL_MESSAGE_FIELD: &str = "_mobius_internal";
pub(crate) const CONTEXT_COMPACTED_MARKER: &str = "context_compacted";
pub(crate) const ATTACHMENT_CONTEXT_MARKER: &str = "attachments";
pub(crate) const ATTACHMENTS_FIELD: &str = "_mobius_attachments";
pub(crate) const REPLAY_REASONING_FIELD: &str = "_mobius_reasoning";
pub(crate) const TOOL_ERROR_FIELD: &str = "_mobius_is_error";
const FORKED_ATTACHMENT_PLACEHOLDER: &str = "[Attachment unavailable in this fork]";

pub(crate) fn strip_attachment_references(items: &mut Vec<Value>) {
    for item in items.iter_mut() {
        let needs_placeholder = item.get("role").and_then(Value::as_str) == Some("user")
            && !attachment_references(item).is_empty()
            && message_text(item, "user").is_none_or(|text| text.trim().is_empty());
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
        }
    }
    items.retain(|item| internal_message_kind(item) != Some(ATTACHMENT_CONTEXT_MARKER));
}

pub(crate) fn internal_message_kind(message: &Value) -> Option<&str> {
    message.get(INTERNAL_MESSAGE_FIELD)?.as_str()
}

pub(crate) fn is_internal_message(message: &Value) -> bool {
    internal_message_kind(message).is_some()
}

/// Returns one-based context boundaries with no unfinished tool calls.
pub(crate) fn tool_complete_boundaries<'a>(
    input: impl IntoIterator<Item = &'a Value>,
) -> Vec<usize> {
    let mut open_calls = BTreeSet::new();
    let mut complete = Vec::new();
    for (index, item) in input.into_iter().enumerate() {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|call_id| !call_id.is_empty())
                    .map_or_else(|| format!("missing-{index}"), str::to_string);
                open_calls.insert(call_id);
            }
            Some("function_call_output") => {
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
pub fn events(context: &[(MessageTarget, Value)], session_id: &str) -> Vec<EventMsg> {
    let mut events = Vec::new();
    let mut tools = BTreeMap::new();
    let complete = tool_complete_boundaries(context.iter().map(|(_, value)| value));
    for (index, (target, value)) in context.iter().enumerate() {
        if internal_message_kind(value) == Some(CONTEXT_COMPACTED_MARKER) {
            events.push(EventMsg::ContextCompacted);
            continue;
        }
        let message_target = complete
            .binary_search(&(index + 1))
            .is_ok()
            .then_some(*target);
        let item_id = replay_id(target);
        if value.get("role").and_then(Value::as_str) == Some("user") {
            let attachments = attachment_references(value);
            let message = message_text(value, "user").unwrap_or_default();
            if !is_internal_message(value) && (!message.is_empty() || !attachments.is_empty()) {
                events.push(EventMsg::UserMessage(UserMessageEvent {
                    message,
                    attachments,
                    message_target,
                }));
            }
            continue;
        }
        if value.get("role").and_then(Value::as_str) == Some("assistant") {
            push_reasoning(&mut events, reasoning_text(value), session_id, &item_id);
            if let Some(message) = message_text(value, "assistant") {
                let phase = if value.get("phase").and_then(Value::as_str) == Some("commentary") {
                    AgentMessagePhase::Commentary
                } else {
                    AgentMessagePhase::FinalAnswer
                };
                events.push(EventMsg::AgentMessage(AgentMessageEvent {
                    session_id: session_id.into(),
                    turn_id: item_id.clone(),
                    model_step_id: item_id.clone(),
                    message,
                    phase,
                    message_target,
                }));
            }
            continue;
        }
        match value.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                push_reasoning(&mut events, reasoning_text(value), session_id, &item_id);
            }
            Some("function_call") => {
                let call_id = string(value, "call_id");
                let name = string(value, "name");
                if call_id.is_empty() || name.is_empty() {
                    continue;
                }
                tools.insert(call_id.clone(), (name.clone(), item_id.clone()));
                events.push(EventMsg::ToolCallBegin(ToolCallBeginEvent {
                    turn_id: item_id,
                    call_id,
                    name,
                    arguments: arguments(value.get("arguments")),
                }));
            }
            Some("function_call_output") => {
                let call_id = string(value, "call_id");
                let output = value_text(value.get("output"));
                let (name, turn_id) = tools
                    .get(&call_id)
                    .cloned()
                    .unwrap_or_else(|| ("tool".into(), item_id));
                events.push(EventMsg::ToolCallEnd(ToolCallEndEvent {
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
            Some(_) | None => {}
        }
    }
    events
}

fn attachment_references(value: &Value) -> Vec<crate::protocol::SessionFileReference> {
    value
        .get(ATTACHMENTS_FIELD)
        .cloned()
        .map(serde_json::from_value)
        .and_then(std::result::Result::ok)
        .unwrap_or_default()
}

fn push_reasoning(
    events: &mut Vec<EventMsg>,
    reasoning: Option<String>,
    session_id: &str,
    item_id: &str,
) {
    let Some(delta) = reasoning.filter(|reasoning| !reasoning.trim().is_empty()) else {
        return;
    };
    events.push(EventMsg::AgentReasoningContentDelta(
        AgentReasoningContentDeltaEvent {
            session_id: session_id.into(),
            turn_id: item_id.into(),
            model_step_id: item_id.into(),
            delta,
        },
    ));
}

fn replay_id(target: &MessageTarget) -> String {
    format!(
        "history-{}-{}",
        target.checkpoint_sequence, target.batch_item_count
    )
}

fn message_text(value: &Value, role: &str) -> Option<String> {
    if value.get("role").and_then(Value::as_str) != Some(role) {
        return None;
    }
    let content = value.get("content")?;
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text: String = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn reasoning_text(value: &Value) -> Option<String> {
    value
        .get(REPLAY_REASONING_FIELD)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn string(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn arguments(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(value)) => {
            serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.clone()))
        }
        Some(value) => value.clone(),
        None => serde_json::json!({}),
    }
}

fn value_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::model::internal_user_message;
    use crate::backend::model::user_message;

    #[test]
    fn replay_uses_only_neutral_reasoning_and_hides_internal_messages() {
        let history = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({
                "role": "assistant",
                "content": "done",
                "_mobius_reasoning": "neutral",
                "_anthropic_content": "provider-private"
            }),
            internal_user_message("compaction", "hidden"),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            (
                MessageTarget {
                    checkpoint_sequence: 4,
                    batch_item_count: index + 1,
                },
                item,
            )
        })
        .collect::<Vec<_>>();

        let replayed = events(&history, "session");

        assert_eq!(replayed.len(), 3);
        assert!(matches!(&replayed[0], EventMsg::UserMessage(event) if event.message == "hello"));
        assert!(matches!(
            &replayed[1],
            EventMsg::AgentReasoningContentDelta(event)
                if event.delta == "neutral"
                    && event.turn_id == "history-4-2"
                    && event.model_step_id == "history-4-2"
        ));
        assert!(matches!(
            &replayed[2],
            EventMsg::AgentMessage(event) if event.message == "done"
        ));
        assert!(matches!(
            &replayed[0],
            EventMsg::UserMessage(event)
                if event.message_target == Some(MessageTarget {
                    checkpoint_sequence: 4,
                    batch_item_count: 1,
                })
        ));
    }

    #[test]
    fn replay_emits_repeated_compaction_markers_in_transcript_order() {
        let history = vec![
            user_message("first"),
            internal_user_message(CONTEXT_COMPACTED_MARKER, ""),
            serde_json::json!({"role": "assistant", "content": "answer"}),
            internal_user_message(CONTEXT_COMPACTED_MARKER, ""),
            user_message("second"),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            (
                MessageTarget {
                    checkpoint_sequence: index as u64 + 1,
                    batch_item_count: 1,
                },
                item,
            )
        })
        .collect::<Vec<_>>();

        let replayed = events(&history, "session");

        assert!(matches!(
            replayed.as_slice(),
            [
                EventMsg::UserMessage(first),
                EventMsg::ContextCompacted,
                EventMsg::AgentMessage(answer),
                EventMsg::ContextCompacted,
                EventMsg::UserMessage(second),
            ] if first.message == "first"
                && answer.message == "answer"
                && second.message == "second"
        ));
    }

    #[test]
    fn replay_preserves_attachment_only_user_messages() {
        let item = serde_json::json!({
            "role": "user",
            "content": [{"type": "input_text", "text": ""}],
            "_mobius_attachments": [{
                "id": "3d46beff-7e84-46ea-859a-e66b4614a79b",
                "name": "photo.png",
                "size": 4,
                "media_type": "image/png"
            }]
        });
        let events = events(
            &[(
                MessageTarget {
                    checkpoint_sequence: 1,
                    batch_item_count: 1,
                },
                item,
            )],
            "session",
        );

        assert!(matches!(
            events.as_slice(),
            [EventMsg::UserMessage(message)] if message.message.is_empty()
                && message.attachments[0].name == "photo.png"
        ));
    }

    #[test]
    fn replay_preserves_commentary_phase() {
        let history = [(
            MessageTarget {
                checkpoint_sequence: 2,
                batch_item_count: 1,
            },
            serde_json::json!({
                "type": "message",
                "role": "assistant",
                "phase": "commentary",
                "content": [{"type": "output_text", "text": "Checking the workspace."}]
            }),
        )];

        let replayed = events(&history, "session");

        assert!(matches!(
            replayed.as_slice(),
            [EventMsg::AgentMessage(message)]
                if message.message == "Checking the workspace."
                    && message.phase == AgentMessagePhase::Commentary
        ));
    }

    #[test]
    fn stripped_attachment_only_messages_keep_a_neutral_fork_placeholder() {
        let mut items = vec![
            serde_json::json!({
                "role": "user",
                "content": [{"type": "input_text", "text": ""}],
                "_mobius_attachments": [{
                    "id": "3d46beff-7e84-46ea-859a-e66b4614a79b",
                    "name": "photo.png",
                    "size": 4,
                    "media_type": "image/png"
                }]
            }),
            internal_user_message(ATTACHMENT_CONTEXT_MARKER, "private blob context"),
        ];

        strip_attachment_references(&mut items);
        assert_eq!(items.len(), 1);
        let context = [(
            MessageTarget {
                checkpoint_sequence: 1,
                batch_item_count: 1,
            },
            items.remove(0),
        )];
        let replayed = events(&context, "fork");

        assert!(matches!(
            replayed.as_slice(),
            [EventMsg::UserMessage(message)]
                if message.message == FORKED_ATTACHMENT_PLACEHOLDER
                    && message.attachments.is_empty()
        ));
    }

    #[test]
    fn replay_keeps_tool_identity_stable_across_durable_batches() {
        let history = vec![
            (
                MessageTarget {
                    checkpoint_sequence: 7,
                    batch_item_count: 1,
                },
                serde_json::json!({
                    "type": "function_call",
                    "call_id": "call-1",
                    "name": "read_file",
                    "arguments": "{}"
                }),
            ),
            (
                MessageTarget {
                    checkpoint_sequence: 9,
                    batch_item_count: 1,
                },
                serde_json::json!({
                    "type": "function_call_output",
                    "call_id": "call-1",
                    "output": "done"
                }),
            ),
        ];

        let replayed = events(&history, "session");

        assert!(matches!(
            replayed.as_slice(),
            [EventMsg::ToolCallBegin(begin), EventMsg::ToolCallEnd(end)]
                if begin.turn_id == "history-7-1" && end.turn_id == begin.turn_id
        ));
    }
}
