//! Context compaction policy and provider routing.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::Middleware;
use super::ModelContext;
use super::approximate_item_tokens;
use super::attachments::is_attachment_materialization;
use super::manifest::{MiddlewareManifest, MiddlewareSettingManifest};
use super::scratchpad::is_projection_item;
use serde_json::Value;
use uuid::Uuid;

use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::backend::checkpoint::ContextRewriteReason;
use crate::backend::model::CompactOutput;
use crate::backend::model::CompactRequest;
use crate::backend::model::ModelRequest;
use crate::backend::model::PromptCacheIdentity;
use crate::backend::model::ToolDefinition;
use crate::backend::model::ToolLoad;
use crate::backend::model::internal_user_message;
use crate::backend::model::prompt_cache_key;
use crate::backend::model::reset_prompt_cache_breakpoint;
use crate::backend::model::user_message;
use crate::protocol::CONTEXT_COMPACTED_MARKER;
use crate::protocol::EventMsg;
use crate::protocol::FrontendBlock;
use crate::protocol::FrontendTone;
use crate::protocol::MESSAGE_METADATA_FIELD;
use crate::protocol::internal_message_kind;
use crate::protocol::is_internal_message;
use crate::protocol::tool_complete_boundaries;

mod text {
    include!(concat!(
        env!("OUT_DIR"),
        "/src_middleware_compaction_text.rs"
    ));
}

const KEEP_RECENT_TOKENS: usize = 20_000;
const NATIVE_RETAINED_TOKENS: usize = 64_000;
const MAX_SUMMARY_TOOL_RESULT_CHARS: usize = 2_000;
const COMPACTION_RESERVE_TOKENS: i64 = 16_384;
const _: () = {
    assert!(text::DEFAULTS_COMPACTION_TOKENS >= 1);
    assert!(text::SETTING_AT_TOKENS_STEP > 0);
};
/// Default compaction trigger for middleware instances without an override.
pub const DEFAULT_COMPACTION_TOKENS: i64 = text::DEFAULTS_COMPACTION_TOKENS;
const SETTINGS: &[MiddlewareSettingManifest] = &[MiddlewareSettingManifest::Integer {
    id: "at_tokens",
    label: text::SETTING_AT_TOKENS_LABEL,
    description: text::SETTING_AT_TOKENS_DESCRIPTION,
    min: 1,
    max: None,
    step: text::SETTING_AT_TOKENS_STEP,
    default: DEFAULT_COMPACTION_TOKENS,
}];

/// Configuration and presentation metadata for compaction.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "compaction",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: false,
    default_enabled: true,
    settings: SETTINGS,
};

/// Compacts visible context after a configurable token threshold.
pub struct Compaction {
    at_tokens: i64,
}

impl Default for Compaction {
    fn default() -> Self {
        Self {
            at_tokens: DEFAULT_COMPACTION_TOKENS,
        }
    }
}

impl Compaction {
    /// Creates a threshold-based compaction policy.
    pub fn new(at_tokens: i64) -> Result<Self> {
        if at_tokens <= 0 {
            return Err(Error::Config(
                "compaction threshold must be positive".into(),
            ));
        }
        Ok(Self { at_tokens })
    }

    /// Returns the effective trigger after reserving response space.
    #[must_use]
    pub fn trigger_tokens(&self, context_window: i64) -> i64 {
        self.at_tokens
            .min(context_window.saturating_sub(COMPACTION_RESERVE_TOKENS))
            .max(1)
    }
}

impl Middleware for Compaction {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        matches!(event, EventMsg::ContextCompacted).then(|| FrontendBlock {
            id: None,
            group: None,
            update: crate::protocol::FrontendBlockUpdate::Replace,
            state: crate::protocol::FrontendBlockState::Complete,
            role: crate::protocol::FrontendBlockRole::Notice,
            title: text::RENDER_CONTEXT_COMPACTED.into(),
            text: String::new(),
            symbol: None,
            files: Vec::new(),
            format: crate::protocol::FrontendBlockFormat::PlainText,
            tone: FrontendTone::Neutral,
        })
    }

    fn pre_model<'a>(&'a self, context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let estimated = context.estimated_input_tokens();
            let observed = if contains_compaction(context.input()) {
                estimated
            } else {
                context
                    .last_usage
                    .map_or(0, |usage| usage.input_tokens)
                    .max(estimated)
            };
            if observed < self.trigger_tokens(context.context_window) || context.input().is_empty()
            {
                return Ok(());
            }
            context.pre_compact().await?;
            if context.turn_stopped() {
                return Ok(());
            }
            let catalog_revision = context.tools.revision()?;
            let tool_load = retained_tool_load(
                context.input(),
                catalog_revision,
                &context.tools.deferred_definitions(),
            )?;
            let output = if context.model.compaction_endpoint(context.provider)? {
                let tools = context
                    .tools
                    .direct_definitions()
                    .iter()
                    .filter(|tool| context.available_tools.contains(&tool.name))
                    .cloned()
                    .collect::<Vec<_>>();
                let deferred_tools = context
                    .tools
                    .deferred_definitions()
                    .iter()
                    .filter(|tool| context.available_tools.contains(&tool.name))
                    .cloned()
                    .collect::<Vec<_>>();
                let cache_key = prompt_cache_key(context.session_id);
                context
                    .model
                    .compact(
                        context.provider,
                        CompactRequest {
                            session_id: context.session_id,
                            prompt_cache: Some(PromptCacheIdentity {
                                key: &cache_key,
                                context_epoch: *context.context_epoch,
                            }),
                            instructions: context.instructions,
                            input: context.input(),
                            catalog_revision,
                            tools: &tools,
                            deferred_tools: &deferred_tools,
                        },
                    )
                    .await?
            } else {
                summarize(context).await?
            };
            if output.output.is_empty() {
                return Err(Error::Provider(
                    "compaction returned an empty context".into(),
                ));
            }
            let latest_turn_input = latest_turn_input(context.input());
            let active_message_metadata = latest_turn_input
                .as_ref()
                .and_then(|active| active.item.get(MESSAGE_METADATA_FIELD))
                .cloned();
            let mut compacted = retain_native_context(context.input(), output.output);
            compacted.retain(|item| !is_projection_item(item));
            restore_input_private_fields(&mut compacted, latest_turn_input);
            validate_active_message_metadata(&compacted, active_message_metadata.as_ref())?;
            reset_prompt_cache_breakpoint(&mut compacted);
            if let Some(tool_load) = tool_load {
                compacted.push(tool_load);
            }
            context.rewrite_input(ContextRewriteReason::Compaction, compacted)?;
            *context.compaction_count = context
                .compaction_count
                .checked_add(1)
                .ok_or_else(|| Error::Checkpoint("compaction count overflow".into()))?;
            context.record_transcript_item(internal_user_message(CONTEXT_COMPACTED_MARKER, ""));
            context.usage.push(output.usage);
            context.events.push(EventMsg::ContextCompacted);
            context.post_compact().await?;
            Ok(())
        })
    }
}

fn retained_tool_load(
    input: &[Value],
    catalog_revision: &str,
    deferred_tools: &[ToolDefinition],
) -> Result<Option<Value>> {
    let deferred = deferred_tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut loaded = BTreeSet::new();
    for item in input {
        let Some(tool_load) = ToolLoad::from_input(item)? else {
            continue;
        };
        if tool_load.catalog_revision == catalog_revision {
            loaded.extend(
                tool_load
                    .tools
                    .into_iter()
                    .filter(|name| deferred.contains(name.as_str())),
            );
        }
    }
    Ok((!loaded.is_empty()).then(|| {
        ToolLoad {
            catalog_revision: catalog_revision.into(),
            tools: loaded.into_iter().collect(),
        }
        .into_input()
    }))
}

fn retain_native_context(input: &[Value], mut compacted: Vec<Value>) -> Vec<Value> {
    if compacted.len() != 1
        || compacted[0].get("type").and_then(Value::as_str) != Some("compaction")
    {
        return compacted;
    }
    let cut = recent_cut(input, NATIVE_RETAINED_TOKENS).unwrap_or(0);
    let recent = &input[cut..];
    let mut retained = Vec::new();
    for (index, item) in recent.iter().enumerate() {
        if (!is_internal_message(item) || item.get(MESSAGE_METADATA_FIELD).is_some())
            && item.get("role").and_then(Value::as_str) == Some("user")
        {
            retained.push(item.clone());
            if let Some(materialization) = recent.get(index + 1)
                && is_attachment_materialization(materialization)
            {
                retained.push(materialization.clone());
            }
        }
    }
    retained.append(&mut compacted);
    retained
}

struct LatestTurnInput<'a> {
    item: &'a Value,
    materialization: Option<&'a Value>,
}

fn latest_turn_input(input: &[Value]) -> Option<LatestTurnInput<'_>> {
    let index = input
        .iter()
        .rposition(|item| item.get(MESSAGE_METADATA_FIELD).is_some())?;
    Some(LatestTurnInput {
        item: &input[index],
        materialization: input
            .get(index + 1)
            .filter(|item| is_attachment_materialization(item)),
    })
}

fn validate_active_message_metadata(input: &[Value], expected: Option<&Value>) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if latest_turn_input(input).and_then(|active| active.item.get(MESSAGE_METADATA_FIELD))
        == Some(expected)
    {
        return Ok(());
    }
    Err(Error::Provider(
        "compaction did not preserve active message metadata".into(),
    ))
}

fn restore_input_private_fields(
    compacted: &mut Vec<Value>,
    latest_turn_input: Option<LatestTurnInput<'_>>,
) {
    let Some(LatestTurnInput {
        item: input,
        materialization,
    }) = latest_turn_input
    else {
        return;
    };
    let Some(fields) = input.as_object() else {
        return;
    };
    let private = fields
        .iter()
        .filter(|(name, _)| name.starts_with('_'))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<Vec<_>>();
    if private.is_empty() && materialization.is_none() {
        return;
    }
    let retained_index = compacted.iter().rposition(|item| {
        item.get("role") == input.get("role") && item.get("content") == input.get("content")
    });
    let input_index = if let Some(index) = retained_index {
        if let Some(fields) = compacted[index].as_object_mut() {
            fields.extend(private);
        }
        index
    } else {
        compacted.push(input.clone());
        compacted.len() - 1
    };
    restore_attachment_materialization(compacted, input_index, materialization);
}

fn restore_attachment_materialization(
    compacted: &mut Vec<Value>,
    user_index: usize,
    materialization: Option<&Value>,
) {
    let Some(materialization) = materialization else {
        return;
    };
    match compacted.get(user_index + 1) {
        Some(retained) if retained == materialization => {}
        Some(retained) if is_attachment_materialization(retained) => {
            compacted[user_index + 1] = materialization.clone();
        }
        Some(_) | None => compacted.insert(user_index + 1, materialization.clone()),
    }
}

async fn summarize(context: &ModelContext<'_>) -> Result<CompactOutput> {
    let (prompt, recent) = prepare_summary(context.input())
        .ok_or_else(|| Error::Provider("context has no safe history boundary to compact".into()))?;
    let session_id = Uuid::new_v4().to_string();
    let cache_key = prompt_cache_key(&session_id);
    let input = [user_message(&prompt)];
    let output = context
        .model
        .respond(
            context.provider,
            ModelRequest {
                session_id: &session_id,
                prompt_cache: Some(PromptCacheIdentity {
                    key: &cache_key,
                    context_epoch: *context.context_epoch,
                }),
                instructions: text::PROMPT_SUMMARY_SYSTEM,
                input: &input,
                catalog_revision: context.tools.revision()?,
                tools: &[],
                deferred_tools: &[],
                allow_hosted_tools: false,
                allow_continuation: false,
            },
            Arc::new(|_| Ok(())),
        )
        .await?;
    let summary = output.text().trim();
    if summary.is_empty() {
        return Err(Error::Provider(
            "model compaction returned no summary".into(),
        ));
    }
    let mut compacted = Vec::with_capacity(recent.len() + 1);
    compacted.push(internal_user_message(
        "compaction",
        &format!("<compacted_context>\n{summary}\n</compacted_context>"),
    ));
    for item in recent {
        if ToolLoad::from_input(&item)?.is_none() {
            compacted.push(item);
        }
    }
    CompactOutput::from_output(compacted, output.usage().clone())
}

fn prepare_summary(input: &[Value]) -> Option<(String, Vec<Value>)> {
    let cut = recent_cut(input, KEEP_RECENT_TOKENS)?;
    let prompt = summary_prompt(&input[..cut])?;
    Some((prompt, input[cut..].to_vec()))
}

fn recent_cut(input: &[Value], keep_tokens: usize) -> Option<usize> {
    let mut accumulated = 0;
    let mut desired = None;
    for index in (0..input.len()).rev() {
        accumulated += approximate_item_tokens(&input[index]);
        if accumulated >= keep_tokens {
            desired = Some(index);
            break;
        }
    }
    let desired = desired?;
    let safe = safe_boundaries(input);
    safe.iter()
        .rev()
        .copied()
        .find(|&index| index > 0 && index <= desired)
        .or_else(|| {
            safe.iter()
                .copied()
                .find(|&index| index > desired && index < input.len())
        })
}

fn safe_boundaries(input: &[Value]) -> Vec<usize> {
    tool_complete_boundaries(input)
        .into_iter()
        .filter(|&boundary| boundary == input.len() || safe_start(&input[boundary]))
        .collect()
}

fn safe_start(item: &Value) -> bool {
    if is_attachment_materialization(item) {
        return false;
    }
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => true,
        Some("message") | None => matches!(
            item.get("role").and_then(Value::as_str),
            Some("user" | "assistant")
        ),
        Some(_) => false,
    }
}

fn summary_prompt(history: &[Value]) -> Option<String> {
    let mut conversation = Vec::new();
    let mut previous_summary = None;
    for item in history {
        if let Some(summary) = compacted_summary(item) {
            previous_summary = Some(summary);
        } else if let Some(serialized) = serialize_item(item) {
            conversation.push(serialized);
        }
    }
    if conversation.is_empty() {
        return None;
    }
    let mut prompt = format!(
        "<conversation>\n{}\n</conversation>\n",
        conversation.join("\n\n")
    );
    if let Some(summary) = previous_summary {
        prompt.push_str(&format!(
            "\n<previous_summary>\n{summary}\n</previous_summary>\n"
        ));
    }
    prompt.push_str(&format!("\n{}", text::PROMPT_SUMMARY_TASK));
    Some(prompt)
}

fn serialize_item(item: &Value) -> Option<String> {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => Some(format!(
            "[Assistant tool call]: {}({})",
            item.get("name").and_then(Value::as_str).unwrap_or("tool"),
            value_text(item.get("arguments"))
        )),
        Some("function_call_output") => Some(format!(
            "[Tool result]: {}",
            truncate_chars(
                &value_text(item.get("output")),
                MAX_SUMMARY_TOOL_RESULT_CHARS
            )
        )),
        Some("reasoning") => {
            let text = content_text(item.get("summary"));
            (!text.is_empty()).then(|| format!("[Assistant reasoning]: {text}"))
        }
        Some("message") | None => {
            let role = item.get("role").and_then(Value::as_str)?;
            let text = content_text(item.get("content"));
            (!text.is_empty()).then(|| {
                let label = if role == "assistant" {
                    "Assistant"
                } else {
                    "User"
                };
                format!("[{label}]: {text}")
            })
        }
        Some(_) => None,
    }
}

fn compacted_summary(item: &Value) -> Option<String> {
    if internal_message_kind(item) != Some("compaction") {
        return None;
    }
    let text = content_text(item.get("content"));
    text.strip_prefix("<compacted_context>")?
        .strip_suffix("</compacted_context>")
        .map(|summary| summary.trim().to_string())
}

fn contains_compaction(input: &[Value]) -> bool {
    input.iter().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("compaction")
            || compacted_summary(item).is_some()
    })
}

fn content_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .or_else(|| part.get("content"))
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn value_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    text.char_indices()
        .nth(limit)
        .map_or_else(|| text.to_string(), |(end, _)| format!("{}…", &text[..end]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::model::tool_output;

    #[test]
    fn recent_cut_keeps_parallel_calls_with_their_outputs() {
        let input = vec![
            user_message("old"),
            serde_json::json!({
                "type": "function_call",
                "call_id": "a",
                "name": "read",
                "arguments": "{}"
            }),
            serde_json::json!({
                "type": "function_call",
                "call_id": "b",
                "name": "read",
                "arguments": "{}"
            }),
            tool_output("a", &"x".repeat(200), false),
            tool_output("b", "done", false),
        ];

        assert_eq!(recent_cut(&input, 10), Some(1));
    }

    #[test]
    fn trigger_reserves_space_from_the_live_context_window() {
        let compaction = Compaction::default();

        assert_eq!(compaction.trigger_tokens(128_000), 111_616);
        assert_eq!(compaction.trigger_tokens(8_000), 1);
        assert_eq!(
            Compaction::new(4_000)
                .expect("custom threshold")
                .trigger_tokens(128_000),
            4_000
        );
    }

    #[test]
    fn compaction_restores_private_fields_on_the_retained_user() {
        let user = serde_json::json!({
            "role": "user",
            "content": [{"type": "input_text", "text": "inspect"}],
            "_middleware_state": {"id": "state"}
        });
        let mut compacted = vec![
            serde_json::json!({
                "type": "message",
                "id": "message-1",
                "role": "user",
                "status": "completed",
                "content": [{"type": "input_text", "text": "inspect"}]
            }),
            serde_json::json!({"type": "compaction", "encrypted_content": "opaque"}),
        ];

        restore_input_private_fields(
            &mut compacted,
            Some(LatestTurnInput {
                item: &user,
                materialization: None,
            }),
        );

        assert_eq!(compacted.len(), 2);
        assert_eq!(compacted[0]["id"], "message-1");
        assert_eq!(compacted[0]["status"], "completed");
        assert_eq!(
            compacted[0]["_middleware_state"],
            serde_json::json!({"id": "state"})
        );
    }

    #[test]
    fn v2_compaction_retains_the_user_before_the_marker_without_its_tool_tail() {
        let input = vec![
            serde_json::json!({
                "role": "developer",
                "content": [{"type": "input_text", "text": "stale instructions"}]
            }),
            serde_json::json!({
                "role": "user",
                "content": [{"type": "input_text", "text": "inspect"}],
                "_middleware_state": {"id": "state"}
            }),
            serde_json::json!({
                "type": "function_call",
                "call_id": "call-1",
                "name": "read",
                "arguments": "{}"
            }),
            tool_output("call-1", "large result", false),
        ];
        let compacted = vec![serde_json::json!({
            "type": "compaction",
            "encrypted_content": "opaque"
        })];
        let mut compacted = retain_native_context(&input, compacted);
        restore_input_private_fields(&mut compacted, latest_turn_input(&input));

        assert_eq!(compacted.len(), 2);
        assert_eq!(compacted[0], input[1]);
        assert_eq!(compacted[1]["type"], "compaction");
    }

    #[test]
    fn compaction_restores_an_omitted_attachment_materialization_with_its_user() {
        let user = crate::backend::model::message_input(&crate::protocol::MessageEvent {
            author: crate::protocol::MessageAuthor::User,
            delivery: crate::protocol::MessageDelivery::Turn,
            text: "inspect".into(),
            attachments: vec![crate::protocol::SessionFileReference {
                id: "upload-1".into(),
                name: "photo.png".into(),
                size: 1,
                media_type: "image/png".into(),
            }],
            reply: None,
            message_target: None,
        })
        .expect("message input");
        let materialization = internal_user_message(
            crate::protocol::ATTACHMENT_CONTEXT_MARKER,
            "attachment context",
        );
        let input = vec![user.clone(), materialization.clone()];
        let compaction = serde_json::json!({
            "type": "compaction",
            "encrypted_content": "opaque"
        });
        let mut compacted = vec![compaction.clone()];

        restore_input_private_fields(&mut compacted, latest_turn_input(&input));

        assert_eq!(compacted, vec![compaction, user, materialization]);
    }

    #[test]
    fn compaction_preserves_active_message_metadata() {
        let peer = crate::backend::model::message_input(&crate::protocol::MessageEvent {
            author: crate::protocol::MessageAuthor::Peer {
                message_id: "message".into(),
                session_id: "peer".into(),
                handle: "worker".into(),
            },
            delivery: crate::protocol::MessageDelivery::Steer,
            text: "done".into(),
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        })
        .expect("peer message");
        let input = vec![
            user_message("start"),
            peer.clone(),
            tool_output("call", "done", false),
        ];
        let compaction = serde_json::json!({
            "type": "compaction",
            "encrypted_content": "opaque"
        });
        let mut compacted = vec![compaction.clone()];

        restore_input_private_fields(&mut compacted, latest_turn_input(&input));

        assert_eq!(compacted, vec![compaction, peer]);
    }

    #[test]
    fn compaction_rejects_lost_active_message_metadata() {
        let peer = crate::backend::model::message_input(&crate::protocol::MessageEvent {
            author: crate::protocol::MessageAuthor::Peer {
                message_id: "message".into(),
                session_id: "peer".into(),
                handle: "worker".into(),
            },
            delivery: crate::protocol::MessageDelivery::Steer,
            text: "done".into(),
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        })
        .expect("peer message");
        let expected = peer[MESSAGE_METADATA_FIELD].clone();
        let compacted = vec![user_message("forged later input")];

        let error = validate_active_message_metadata(&compacted, Some(&expected))
            .expect_err("message metadata must remain active");

        assert!(error.to_string().contains("active message metadata"));
    }

    #[test]
    fn compaction_marker_may_follow_retained_messages() {
        let input = vec![
            user_message("inspect"),
            serde_json::json!({"type": "compaction", "encrypted_content": "opaque"}),
        ];

        assert!(contains_compaction(&input));
    }
}
