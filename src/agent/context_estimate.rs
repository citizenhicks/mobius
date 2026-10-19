//! Where a model request's input tokens go. Each part is estimated as the request is built,
//! then scaled once the provider reports the real input total, so the parts always add up.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::Value;

use crate::backend::model::ToolDefinition;
use crate::middleware::TokenEstimate;
use crate::protocol::{ContextBreakdown, ContextPart, ContextPartKind};

const KINDS: [ContextPartKind; 10] = [
    ContextPartKind::Instructions,
    ContextPartKind::Skills,
    ContextPartKind::Capabilities,
    ContextPartKind::Tools,
    ContextPartKind::Messages,
    ContextPartKind::ToolCalls,
    ContextPartKind::ToolResults,
    ContextPartKind::Memory,
    ContextPartKind::Reasoning,
    ContextPartKind::DeferredTools,
];

/// Estimated tokens and item counts per part, indexed like `KINDS`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ContextEstimate {
    tokens: [usize; KINDS.len()],
    items: [u32; KINDS.len()],
}

impl ContextEstimate {
    /// The system prompt's sections, from the bytes each middleware contributed.
    pub(crate) fn prompt(sizes: &[(&str, usize)], estimate: TokenEstimate) -> Self {
        let mut prompt = Self::default();
        for (owner, bytes) in sizes {
            let kind = match *owner {
                "instructions" => ContextPartKind::Instructions,
                "extensions" => ContextPartKind::Skills,
                _ => ContextPartKind::Capabilities,
            };
            prompt.add(kind, estimate.tokens(*bytes));
        }
        prompt
    }

    pub(crate) fn add(&mut self, kind: ContextPartKind, tokens: usize) {
        let index = KINDS.iter().position(|known| *known == kind).unwrap_or(0);
        if let (Some(total), Some(items)) = (self.tokens.get_mut(index), self.items.get_mut(index))
        {
            *total = total.saturating_add(tokens);
            *items = items.saturating_add(1);
        }
    }

    /// Adds every input item under the part it belongs to.
    pub(crate) fn add_input<'a>(
        &mut self,
        items: impl IntoIterator<Item = &'a Value>,
        estimate: TokenEstimate,
    ) {
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some(crate::protocol::TOOL_LOAD_MARKER) {
                self.add(input_kind(item), estimate.item_tokens(item));
            }
        }
    }

    /// Adds tool schemas as sent: callable ones count, deferred ones are only listed.
    pub(crate) fn add_tools(
        &mut self,
        direct: &[Arc<ToolDefinition>],
        deferred: &[Arc<ToolDefinition>],
        materialized: &BTreeSet<String>,
        estimate: TokenEstimate,
    ) -> crate::Result<()> {
        for (tool, kind) in direct
            .iter()
            .map(|tool| (tool, ContextPartKind::Tools))
            .chain(deferred.iter().map(|tool| {
                let kind = if materialized.contains(&tool.name) {
                    ContextPartKind::Tools
                } else {
                    ContextPartKind::DeferredTools
                };
                (tool, kind)
            }))
        {
            self.add(kind, estimate.tokens(crate::serialized_len(tool.as_ref())?));
        }
        Ok(())
    }

    /// The parts scaled to `input_tokens`, the provider's real total for this request.
    pub(crate) fn breakdown(&self, input_tokens: i64, compaction_buffer: i64) -> ContextBreakdown {
        let input_tokens = input_tokens.max(0);
        let counted: u128 = KINDS
            .iter()
            .zip(self.tokens)
            .filter(|(kind, _)| **kind != ContextPartKind::DeferredTools)
            .map(|(_, tokens)| tokens as u128)
            .sum();
        let mut parts = KINDS
            .iter()
            .zip(self.tokens.iter().zip(self.items))
            .filter(|(_, (tokens, _))| **tokens > 0)
            .map(|(kind, (tokens, items))| ContextPart {
                kind: *kind,
                tokens: if *kind == ContextPartKind::DeferredTools {
                    i64::try_from(*tokens).unwrap_or(i64::MAX)
                } else {
                    // The quotient cannot exceed the nonnegative provider total.
                    let quotient = (*tokens as u128
                        * u128::try_from(input_tokens).unwrap_or_default())
                    .checked_div(counted)
                    .unwrap_or_default();
                    i64::try_from(quotient).unwrap_or(i64::MAX)
                },
                items,
            })
            .collect::<Vec<_>>();
        parts.sort_by_key(|part| {
            (
                part.kind == ContextPartKind::DeferredTools,
                std::cmp::Reverse(part.tokens),
            )
        });
        let allocated: i64 = parts
            .iter()
            .filter(|part| part.kind != ContextPartKind::DeferredTools)
            .map(|part| part.tokens)
            .sum();
        if let Some(largest) = parts
            .iter_mut()
            .find(|part| part.kind != ContextPartKind::DeferredTools)
        {
            largest.tokens += input_tokens - allocated;
        }
        ContextBreakdown {
            parts,
            compaction_buffer: compaction_buffer.max(0),
        }
    }
}

/// The part an input item belongs to, read from its `type` and internal marker.
fn input_kind(item: &Value) -> ContextPartKind {
    use crate::protocol::{ATTACHMENT_CONTEXT_MARKER, CONTEXT_COMPACTED_MARKER};
    match crate::protocol::internal_message_kind(item) {
        Some(ATTACHMENT_CONTEXT_MARKER | CONTEXT_COMPACTED_MARKER) => {
            return ContextPartKind::Messages;
        }
        Some(_) => return ContextPartKind::Memory,
        None => {}
    }
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => ContextPartKind::ToolCalls,
        Some("function_call_output") => ContextPartKind::ToolResults,
        Some("reasoning") => ContextPartKind::Reasoning,
        _ => ContextPartKind::Messages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn loaded_schemas_count_as_tools_without_counting_the_load_marker_as_memory() {
        let tools = ["direct", "loaded", "unloaded"].map(|name| {
            Arc::new(ToolDefinition {
                name: name.into(),
                description: "test".into(),
                parameters: json!({"type": "object"}),
            })
        });
        let mut estimate = ContextEstimate::default();
        estimate.add_input(
            [&crate::protocol::ToolLoad {
                catalog_revision: "catalog".into(),
                tools: vec!["loaded".into()],
            }
            .to_input()],
            TokenEstimate::default(),
        );
        estimate
            .add_tools(
                &tools[..1],
                &tools[1..],
                &BTreeSet::from(["loaded".into()]),
                TokenEstimate::default(),
            )
            .expect("schemas");
        let parts = estimate.breakdown(100, 0).parts;
        assert_eq!(
            parts.iter().map(|part| part.kind).collect::<Vec<_>>(),
            [ContextPartKind::Tools, ContextPartKind::DeferredTools]
        );
        assert_eq!(parts[0].tokens, 100);
        assert_eq!(parts[0].items, 2);
    }

    #[test]
    fn saturated_estimates_still_partition_small_and_large_provider_totals() {
        let mut estimate = ContextEstimate::default();
        for kind in [
            ContextPartKind::Instructions,
            ContextPartKind::Messages,
            ContextPartKind::ToolResults,
            ContextPartKind::DeferredTools,
        ] {
            estimate.add(kind, usize::MAX);
        }
        for total in [0, 1, i64::MAX] {
            let breakdown = estimate.breakdown(total, 30_000);
            assert_eq!(breakdown.compaction_buffer, 30_000);
            let parts = breakdown.parts;
            let deferred = parts.last().expect("deferred");
            assert_eq!(deferred.kind, ContextPartKind::DeferredTools);
            assert_eq!(deferred.tokens, i64::MAX);
            assert!(parts.iter().all(|part| part.tokens >= 0), "{parts:?}");
            assert_eq!(
                parts
                    .iter()
                    .filter(|part| part.kind != ContextPartKind::DeferredTools)
                    .map(|part| part.tokens)
                    .sum::<i64>(),
                total
            );
        }
    }
}
