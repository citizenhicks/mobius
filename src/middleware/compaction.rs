//! Portable plaintext checkpoints and context resets.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::Middleware;
use super::ModelContext;
use super::tools::Catalog;
use super::tools::loaded_tools;
use super::{PostToolUseContext, PromptSection, RuntimeContext};
use serde_json::Value;

use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::backend::checkpoint::ContextRewriteReason;
use crate::backend::model::{ModelInput, internal_user_message};
use crate::protocol::CONTEXT_COMPACTED_MARKER;
use crate::protocol::EventMsg;
use crate::protocol::FrontendBlock;
use crate::protocol::FrontendTone;
use crate::protocol::MESSAGE_METADATA_FIELD;
use crate::protocol::ToolLoad;

mod text {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Definition {
        pub(super) new_context: crate::middleware::tools::ToolSpec,
        pub(super) write_handoff: crate::middleware::tools::ToolSpec,
        #[serde(deserialize_with = "crate::middleware::manifest::deserialize_settings")]
        pub(super) settings: Vec<crate::middleware::manifest::MiddlewareSettingManifest>,
        pub(super) handoff_saved_result: String,
        pub(super) handoff_requested_result: String,
        pub(super) handoff_requested_context: String,
        pub(super) default_enabled: bool,
        pub(super) manifest_description: String,
        pub(super) manifest_label: String,
        pub(super) prompt_handoff: String,
        pub(super) prompt_prepare: String,
        pub(super) prompt_restored: String,
        pub(super) render_context_compacted: String,
        pub(super) render_context_compacting: String,
        pub(super) render_compaction_failed: String,
        pub(super) render_compaction_cancelled: String,
    }
    crate::embedded_config! { pub(super) static DEFINITION: Definition = include_str!("compaction.toml"); }
}
mod handoff;

/// Default compaction trigger for middleware instances without an override.
pub fn default_compaction_tokens() -> i64 {
    super::manifest::integer_default(&text::DEFINITION.settings, "at_tokens")
}
super::manifest::middleware_manifest! {
/// Configuration and presentation metadata for compaction.
    "compaction", text::DEFINITION, required: false, capability: None, settings: &text::DEFINITION.settings
}

/// Compacts visible context after a configurable token threshold.
pub struct Compaction {
    at_tokens: i64,
    allow_model_compaction: bool,
    reserve_tokens: i64,
}

impl Default for Compaction {
    fn default() -> Self {
        Self {
            at_tokens: default_compaction_tokens(),
            allow_model_compaction: super::manifest::string_default(
                &text::DEFINITION.settings,
                "allow_model_compaction",
            ) == "on",
            reserve_tokens: super::manifest::integer_default(
                &text::DEFINITION.settings,
                "reserve_tokens",
            ),
        }
    }
}

impl Compaction {
    /// Creates a threshold-based compaction policy.
    /// # Errors
    ///
    /// Returns an error if configuration is invalid or a required resource cannot be initialized.
    pub fn new(at_tokens: i64) -> Result<Self> {
        if at_tokens <= 0 {
            return Err(Error::Config(
                "compaction threshold must be positive".into(),
            ));
        }
        Ok(Self {
            at_tokens,
            ..Self::default()
        })
    }

    /// Sets the maximum reserve for one model response or handoff step.
    /// # Errors
    /// Returns an error unless the reserve is positive.
    pub fn reserve_tokens(mut self, tokens: i64) -> Result<Self> {
        if tokens <= 0 {
            return Err(Error::Config("compaction reserve must be positive".into()));
        }
        self.reserve_tokens = tokens;
        Ok(self)
    }

    /// Allows the model to request an early checkpoint and context reset.
    #[must_use]
    pub fn allow_model_compaction(mut self, allow: bool) -> Self {
        self.allow_model_compaction = allow;
        self
    }

    fn reserve(&self, window: i64) -> i64 {
        (window.max(1) / 8).max(1).min(self.reserve_tokens)
    }

    /// Returns the threshold with room for checkpoint preparation and a response.
    #[must_use]
    pub fn trigger_tokens(&self, context_window: i64) -> i64 {
        self.at_tokens
            .min(context_window.saturating_sub(self.reserve(context_window).saturating_mul(2)))
            .max(1)
    }
}

impl Middleware for Compaction {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, _runtime: &RuntimeContext) -> Result<()> {
        handoff::register(catalog, self.allow_model_compaction)
    }

    fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(self
            .allow_model_compaction
            .then(|| PromptSection::new(text::DEFINITION.prompt_handoff.as_str())))
    }

    fn post_tool_use<'a>(
        &'a self,
        context: &'a mut PostToolUseContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { handoff::post_tool(context) })
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        if let Some(block) = [
            &text::DEFINITION.new_context,
            &text::DEFINITION.write_handoff,
        ]
        .into_iter()
        .find_map(|spec| spec.render(event))
        {
            return Some(block);
        }
        matches!(event, EventMsg::ContextCompacted).then(|| FrontendBlock {
            id: None,
            group: None,
            update: crate::protocol::FrontendBlockUpdate::Replace,
            state: crate::protocol::FrontendBlockState::Complete,
            role: crate::protocol::FrontendBlockRole::Notice,
            title: text::DEFINITION.render_context_compacted.clone(),
            text: String::new(),
            symbol: None,
            links: Vec::new(),
            files: Vec::new(),
            content: Default::default(),
            format: crate::protocol::FrontendBlockFormat::PlainText,
            image_aspect: None,
            tone: FrontendTone::Neutral,
        })
    }

    fn pre_model<'a>(&'a self, context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { handoff::prepare(context, self).await })
    }
}

fn start_notice(context: &mut ModelContext<'_>) -> Result<()> {
    context.start_preparation_notice(
        MANIFEST.id,
        (
            &text::DEFINITION.render_context_compacting,
            FrontendTone::Neutral,
        ),
        (
            &text::DEFINITION.render_context_compacted,
            FrontendTone::Neutral,
        ),
        (
            &text::DEFINITION.render_compaction_failed,
            FrontendTone::Error,
        ),
        (
            &text::DEFINITION.render_compaction_cancelled,
            FrontendTone::Neutral,
        ),
    )
}

async fn apply_compaction(context: &mut ModelContext<'_>, output: Vec<Arc<Value>>) -> Result<()> {
    let tool_load = retained_tool_load(
        context.input(),
        context.tools.revision()?,
        &context.tools.deferred_definitions(),
    )?;
    let active_message_metadata =
        latest_turn_input(context.input()).and_then(|active| active.get(MESSAGE_METADATA_FIELD));
    let mut compacted = output;
    context
        .hooks
        .prepare_compacted_input(context.input(), &mut compacted);
    validate_active_message_metadata(compacted.as_slice().into(), active_message_metadata)?;
    if let Some(tool_load) = tool_load {
        compacted.push(Arc::new(tool_load));
    }
    context
        .model
        .validate_media(
            context.provider,
            context.session_id,
            compacted.as_slice().into(),
        )
        .await?;
    context.rewrite_input(ContextRewriteReason::Compaction, compacted)?;
    *context.compaction_count = context
        .compaction_count
        .checked_add(1)
        .ok_or_else(|| Error::Checkpoint("compaction count overflow".into()))?;
    context.record_transcript_item(internal_user_message(CONTEXT_COMPACTED_MARKER, ""));
    context.post_compact().await?;
    Ok(())
}

fn retained_tool_load(
    input: ModelInput<'_>,
    catalog_revision: &str,
    deferred_tools: &[Arc<crate::backend::model::ToolDefinition>],
) -> Result<Option<Value>> {
    let deferred = deferred_tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<BTreeSet<_>>();
    let loaded = loaded_tools(input, catalog_revision, &deferred)?;
    Ok((!loaded.is_empty()).then(|| {
        ToolLoad {
            catalog_revision: catalog_revision.into(),
            tools: loaded.into_iter().collect(),
        }
        .to_input()
    }))
}

fn latest_turn_input(input: ModelInput<'_>) -> Option<&Value> {
    input
        .iter()
        .rfind(|item| item.get(MESSAGE_METADATA_FIELD).is_some())
}

fn validate_active_message_metadata(input: ModelInput<'_>, expected: Option<&Value>) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if latest_turn_input(input).and_then(|active| active.get(MESSAGE_METADATA_FIELD))
        == Some(expected)
    {
        return Ok(());
    }
    Err(Error::Provider(
        "compaction did not preserve active message metadata".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::model::user_message;

    #[test]
    fn defaults_and_small_window_leave_preparation_space() {
        let policy = Compaction::default();
        assert_eq!(policy.at_tokens, 250_000);
        assert!(!policy.allow_model_compaction);
        assert_eq!(policy.reserve_tokens, 16_384);
        assert_eq!(policy.trigger_tokens(272_000), 239_232);
        assert_eq!(policy.trigger_tokens(8_000), 6_000);
        assert_eq!(policy.trigger_tokens(1), 1);
        assert!(Compaction::new(0).is_err());
        assert!(policy.reserve_tokens(0).is_err());
    }

    #[test]
    fn compaction_rejects_lost_active_message_metadata() {
        let peer = crate::backend::model::message_input(&crate::protocol::MessageEvent {
            author: crate::protocol::MessageAuthor::Source {
                message_id: "message".into(),
                source: crate::protocol::MessageSource::Session {
                    session_id: "peer".into(),
                },
                cause_id: None,
                ancestry: Vec::new(),
                handle: "worker".into(),
                symbol: None,
            },
            delivery: crate::protocol::MessageDelivery::Steer,
            text: "done".into(),
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        })
        .expect("peer message");
        let expected = &peer[MESSAGE_METADATA_FIELD];
        let compacted = vec![user_message("forged later input")];

        let error = validate_active_message_metadata(compacted.as_slice().into(), Some(expected))
            .expect_err("message metadata must remain active");

        assert!(error.to_string().contains("active message metadata"));
    }
}
