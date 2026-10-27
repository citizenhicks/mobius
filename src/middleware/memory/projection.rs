use crate::backend::model::ModelInput;
use serde_json::Value;

use super::text;
use super::{MAX_INJECTION_BYTES, PROJECTION_KIND, Partition, validate_scope_budget};
use crate::backend::model::internal_user_message;
use crate::protocol::internal_message_kind;
use crate::{Error, Result};

pub(super) fn is_projection_item(item: &Value) -> bool {
    internal_message_kind(item) == Some(PROJECTION_KIND)
}

/// The memory context for `[global, owner]`, when it differs from the latest one in `input`.
pub(super) fn next_projection(
    input: ModelInput<'_>,
    visible: &[Partition; 2],
) -> Result<Option<Value>> {
    let previous = input.iter().rev().find(|item| is_projection_item(item));
    if previous.is_none()
        && visible
            .iter()
            .all(|partition| partition.memories.is_empty())
    {
        return Ok(None);
    }
    let mut text = String::new();
    text.push_str(&text::DEFINITION.prompt_projection_header);
    for (heading, partition) in [
        &text::DEFINITION.prompt_projection_global,
        &text::DEFINITION.prompt_projection_owner,
    ]
    .into_iter()
    .zip(visible)
    {
        validate_scope_budget(&partition.memories).map_err(Error::Checkpoint)?;
        text.push_str(heading);
        if partition.memories.is_empty() {
            text.push_str(&text::DEFINITION.prompt_projection_empty);
        }
        for entry in partition.memories.iter().rev() {
            text.push_str("- [");
            text.push_str(&entry.id);
            text.push_str("] ");
            text.push_str(&serde_json::to_string(&entry.text)?);
            text.push('\n');
        }
    }
    text.push_str(&text::DEFINITION.prompt_projection_footer);
    if text.len() > MAX_INJECTION_BYTES {
        return Err(Error::Checkpoint(
            "memory projection exceeds its byte limit".into(),
        ));
    }
    if previous.and_then(|item| item.pointer("/content/0/text").and_then(Value::as_str))
        == Some(text.as_str())
    {
        return Ok(None);
    }
    Ok(Some(internal_user_message(PROJECTION_KIND, &text)))
}
