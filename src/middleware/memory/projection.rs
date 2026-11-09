use crate::backend::model::ModelInput;
use serde_json::Value;

use super::pages::Scope;
use super::{Basis, PROJECTION_KIND, text};
use crate::backend::model::internal_user_message;
use crate::protocol::internal_message_kind;

// Each scope's index, newest first, with short user-confirmed pages in full.
const MAX_SCOPE_INDEX_BYTES: usize = 6 * 1024;
const MAX_INLINE_BODY_BYTES: usize = 500;

pub(super) fn is_projection_item(item: &Value) -> bool {
    internal_message_kind(item) == Some(PROJECTION_KIND)
}

/// The memory index for `[global, owner]`, when it differs from the latest one in `input`.
pub(super) fn next_projection(input: ModelInput<'_>, visible: &[Scope; 2]) -> Option<Value> {
    let previous = input.iter().rev().find(|item| is_projection_item(item));
    if previous.is_none() && visible.iter().all(|scope| scope.pages.is_empty()) {
        return None;
    }
    let text = render(visible);
    if previous.and_then(|item| item.pointer("/content/0/text").and_then(Value::as_str))
        == Some(text.as_str())
    {
        return None;
    }
    Some(internal_user_message(PROJECTION_KIND, &text))
}

fn render(visible: &[Scope; 2]) -> String {
    let mut text = text::DEFINITION.prompt_projection_header.clone(); // rules-allow: the projection owns its text
    for (heading, scope) in [
        &text::DEFINITION.prompt_projection_global,
        &text::DEFINITION.prompt_projection_owner,
    ]
    .into_iter()
    .zip(visible)
    {
        text.push_str(heading);
        if scope.pages.is_empty() {
            text.push_str(&text::DEFINITION.prompt_projection_empty);
        }
        let start = text.len();
        for (shown, page) in scope.pages.iter().enumerate() {
            let mut line = format!(
                "- [[{}]] (updated_at {}) {}\n",
                page.name, page.updated_at, page.description
            );
            if page.basis == Basis::UserConfirmed && page.body.len() <= MAX_INLINE_BODY_BYTES {
                for body in page.body.lines() {
                    line.push_str("  > ");
                    line.push_str(body);
                    line.push('\n');
                }
            }
            // ponytail: newest pages win the index budget; search finds the rest.
            if text.len() - start + line.len() > MAX_SCOPE_INDEX_BYTES {
                text.push_str(
                    &text::DEFINITION
                        .prompt_projection_more
                        .replace("{count}", &(scope.pages.len() - shown).to_string()),
                );
                break;
            }
            text.push_str(&line);
        }
    }
    text.push_str(&text::DEFINITION.prompt_projection_footer);
    text
}
