use std::sync::Arc;

use crate::backend::model::ModelInput;
use serde_json::Value;

use super::text;
use super::{MAX_INJECTION_BYTES, PROJECTION_KIND, Snapshot, validate_scope_budget};
use crate::backend::model::internal_user_message;
use crate::protocol::internal_message_kind;
use crate::{Error, Result};

pub(super) fn is_projection_item(item: &Value) -> bool {
    internal_message_kind(item) == Some(PROJECTION_KIND)
}

pub(super) fn without_projection_items(input: ModelInput<'_>) -> Option<Vec<Arc<Value>>> {
    input.iter().any(is_projection_item).then(|| {
        input
            .iter()
            .enumerate()
            .filter(|(_, item)| !is_projection_item(item))
            .map(|(index, item)| {
                input
                    .shared_item(index)
                    .map_or_else(|| Arc::new(item.to_owned()), Arc::clone)
            })
            .collect()
    })
}

pub(super) fn next_projection(input: ModelInput<'_>, snapshot: &Snapshot) -> Result<Option<Value>> {
    let previous = input.iter().rev().find(|item| is_projection_item(item));
    if previous.is_none() && snapshot.global.is_empty() {
        return Ok(None);
    }
    let mut text = text::DEFINITION.prompt_projection_header.clone();
    {
        let entries = &snapshot.global;
        validate_scope_budget(entries).map_err(Error::Checkpoint)?;
        text.push_str(&text::DEFINITION.prompt_projection_global);
        if entries.is_empty() {
            text.push_str(&text::DEFINITION.prompt_projection_empty);
        }
        for entry in entries.iter().rev() {
            text.push_str("- ");
            text.push_str(&serde_json::to_string(&entry.note)?);
            text.push('\n');
        }
    }
    text.push_str(&text::DEFINITION.prompt_projection_footer);
    if text.len() > MAX_INJECTION_BYTES {
        return Err(Error::Checkpoint(
            "shared scratchpad projection exceeds its byte limit".into(),
        ));
    }
    if previous.and_then(|item| item["content"][0]["text"].as_str()) == Some(text.as_str()) {
        return Ok(None);
    }
    Ok(Some(internal_user_message(PROJECTION_KIND, &text)))
}
