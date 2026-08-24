//! Durable masking of stale tool output in active model context.

use serde_json::Value;

use super::manifest::{MiddlewareManifest, MiddlewareSettingManifest};
use super::{Middleware, ModelContext, approximate_item_tokens};
use crate::backend::checkpoint::ContextRewriteReason;
use crate::protocol::{TOOL_ERROR_FIELD, is_internal_message, tool_complete_boundaries};
use crate::{BoxFuture, Error, Result};

mod text {
    include!(concat!(
        env!("OUT_DIR"),
        "/src_middleware_context_offloading_text.rs"
    ));
}

const MASKED_TOOL_OUTPUT: &str = "[offloaded]";

fn high_water_tokens(stale_after_tokens: usize) -> usize {
    stale_after_tokens.saturating_add((stale_after_tokens / 2).max(1))
}

const _: () = {
    assert!(text::DEFAULTS_STALE_AFTER_TOKENS >= 1);
    assert!(text::SETTING_STALE_AFTER_TOKENS_STEP > 0);
};
/// Default trailing token window retained by context offloading.
pub const DEFAULT_STALE_AFTER_TOKENS: i64 = text::DEFAULTS_STALE_AFTER_TOKENS;
const SETTINGS: &[MiddlewareSettingManifest] = &[MiddlewareSettingManifest::Integer {
    id: "stale_after_tokens",
    label: text::SETTING_STALE_AFTER_TOKENS_LABEL,
    description: text::SETTING_STALE_AFTER_TOKENS_DESCRIPTION,
    min: 1,
    max: None,
    step: text::SETTING_STALE_AFTER_TOKENS_STEP,
    default: DEFAULT_STALE_AFTER_TOKENS,
}];

/// Configuration and presentation metadata for context offloading.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "context_offloading",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: false,
    default_enabled: true,
    settings: SETTINGS,
};

/// Masks successful tool output older than a trailing token window.
pub struct ContextOffloading {
    stale_after_tokens: usize,
}

impl ContextOffloading {
    /// Creates a tool-output retention policy.
    pub fn new(stale_after_tokens: i64) -> Result<Self> {
        let stale_after_tokens = usize::try_from(stale_after_tokens)
            .map_err(|_| Error::Config("context offloading threshold must be positive".into()))?;
        if stale_after_tokens == 0 {
            return Err(Error::Config(
                "context offloading threshold must be positive".into(),
            ));
        }
        Ok(Self { stale_after_tokens })
    }
}

impl Middleware for ContextOffloading {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn pre_model<'a>(&'a self, context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(input) = mask_stale_outputs(context.input(), self.stale_after_tokens) {
                context.rewrite_input(ContextRewriteReason::ContextOffloading, input)?;
            }
            Ok(())
        })
    }
}

fn mask_stale_outputs(input: &[Value], stale_after_tokens: usize) -> Option<Vec<Value>> {
    let latest_user = input.iter().rposition(|item| {
        item.get("role").and_then(Value::as_str) == Some("user") && !is_internal_message(item)
    })?;
    let total_tokens = input
        .iter()
        .map(approximate_item_tokens)
        .fold(0, usize::saturating_add);
    if total_tokens <= high_water_tokens(stale_after_tokens) {
        return None;
    }

    let mut suffix_tokens = vec![0_usize; input.len() + 1];
    for index in (0..input.len()).rev() {
        suffix_tokens[index] =
            suffix_tokens[index + 1].saturating_add(approximate_item_tokens(&input[index]));
    }
    let boundaries = tool_complete_boundaries(&input[..latest_user]);
    let block_end = boundaries
        .iter()
        .copied()
        .find(|&boundary| suffix_tokens[boundary] <= stale_after_tokens)
        .or_else(|| boundaries.last().copied())?;
    let mut masked = input.to_vec();
    let mut changed = false;
    for item in &mut masked[..block_end] {
        if successful_tool_output(item).is_some() {
            item["output"] = Value::String(MASKED_TOOL_OUTPUT.into());
            changed = true;
        }
    }
    changed.then_some(masked)
}

fn successful_tool_output(item: &Value) -> Option<&str> {
    if item.get("type").and_then(Value::as_str) != Some("function_call_output")
        || item.get(TOOL_ERROR_FIELD).and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    item.get("output")
        .and_then(Value::as_str)
        .filter(|output| *output != MASKED_TOOL_OUTPUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::model::{tool_output, user_message};

    #[test]
    fn masks_parallel_calls_as_one_complete_block() {
        let input = vec![
            user_message("old turn"),
            serde_json::json!({
                "type": "function_call", "call_id": "a", "name": "read", "arguments": "{}"
            }),
            serde_json::json!({
                "type": "function_call", "call_id": "b", "name": "read", "arguments": "{}"
            }),
            tool_output("a", &"a".repeat(100), false),
            tool_output("b", &"b".repeat(100), false),
            user_message("latest turn"),
        ];

        let masked = mask_stale_outputs(&input, 10).expect("stale parallel block");

        assert_eq!(masked[1], input[1]);
        assert_eq!(masked[2], input[2]);
        assert_eq!(masked[3]["output"], MASKED_TOOL_OUTPUT);
        assert_eq!(masked[4]["output"], MASKED_TOOL_OUTPUT);
    }

    #[test]
    fn preserves_failed_outputs_and_waits_for_safe_boundary() {
        let incomplete = vec![
            user_message("old turn"),
            serde_json::json!({
                "type": "function_call",
                "call_id": "open",
                "name": "read",
                "arguments": "{}"
            }),
            user_message("latest turn"),
        ];
        assert!(mask_stale_outputs(&incomplete, 1).is_none());

        let input = vec![
            user_message("old turn"),
            serde_json::json!({
                "type": "function_call", "call_id": "failed", "name": "read", "arguments": "{}"
            }),
            tool_output("failed", &"failed".repeat(100), true),
            serde_json::json!({
                "type": "function_call", "call_id": "stale", "name": "read", "arguments": "{}"
            }),
            tool_output("stale", &"stale".repeat(100), false),
            user_message("latest turn"),
        ];

        let masked = mask_stale_outputs(&input, 10).expect("stale complete block");

        assert_eq!(masked[2], input[2]);
        assert_eq!(masked[4]["output"], MASKED_TOOL_OUTPUT);
    }

    #[test]
    fn uses_high_water_trigger_and_low_water_retention() {
        let low_input = vec![
            user_message("old turn"),
            serde_json::json!({
                "type": "function_call", "call_id": "stale", "name": "read", "arguments": "{}"
            }),
            tool_output("stale", "stale", false),
            user_message("latest turn"),
        ];
        let low = low_input
            .iter()
            .map(approximate_item_tokens)
            .sum::<usize>()
            .saturating_sub(1);
        assert!(low_input.iter().map(approximate_item_tokens).sum::<usize>() > low);
        assert!(
            low_input.iter().map(approximate_item_tokens).sum::<usize>() <= high_water_tokens(low)
        );
        assert!(mask_stale_outputs(&low_input, low).is_none());

        let mut over_high = vec![
            user_message("old turn"),
            serde_json::json!({
                "type": "function_call", "call_id": "stale", "name": "read", "arguments": "{}"
            }),
            tool_output("stale", &"stale".repeat(20), false),
            serde_json::json!({"role": "assistant", "content": "padding".repeat(20)}),
            user_message("latest turn"),
        ];
        over_high.insert(
            4,
            serde_json::json!({"role": "assistant", "content": "more".repeat(20)}),
        );
        let masked = mask_stale_outputs(&over_high, low).expect("high-water rewrite");
        assert_eq!(masked[2]["output"], MASKED_TOOL_OUTPUT);
        assert_eq!(masked.last(), over_high.last());
    }

    #[test]
    fn is_idempotent_after_one_block_rewrite() {
        let input = vec![
            user_message("old turn"),
            serde_json::json!({
                "type": "function_call", "call_id": "stale", "name": "read", "arguments": "{}"
            }),
            tool_output("stale", &"stale".repeat(100), false),
            user_message("latest turn"),
        ];
        let masked = mask_stale_outputs(&input, 10).expect("stale block");
        assert!(mask_stale_outputs(&masked, 10).is_none());
    }
}
