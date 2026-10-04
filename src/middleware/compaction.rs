//! Context compaction policy and provider routing.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::Middleware;
use super::ModelContext;
use super::TokenEstimate;
use super::manifest::{
    MiddlewareManifest, MiddlewareSettingChoice, MiddlewareSettingChoices,
    MiddlewareSettingManifest,
};
use super::tools::Catalog;
use super::tools::loaded_tools;
use super::{
    ModelRequestContext, PostToolUseContext, PromptSection, RuntimeContext, SessionStartContext,
};
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
use crate::backend::model::internal_user_message;
use crate::backend::model::prompt_cache_key;
use crate::backend::model::user_message;
use crate::protocol::CONTEXT_COMPACTED_MARKER;
use crate::protocol::EventMsg;
use crate::protocol::FrontendBlock;
use crate::protocol::FrontendTone;
use crate::protocol::MESSAGE_METADATA_FIELD;
use crate::protocol::ToolLoad;
use crate::protocol::internal_message_kind;
use crate::protocol::is_internal_message;
use crate::protocol::tool_complete_boundaries;

mod text {
    use super::CompactionMode;
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Definition {
        pub(super) summary_tool_call_label: String,
        pub(super) summary_tool_result_label: String,
        pub(super) summary_reasoning_label: String,
        pub(super) summary_assistant_label: String,
        pub(super) summary_user_label: String,
        pub(super) prompt_visual_evidence: String,
        pub(super) prompt_compacted_context: String,
        pub(super) handoff_notes_parameter_description: String,
        pub(super) handoff_saved_result: String,
        pub(super) handoff_requested_result: String,
        pub(super) handoff_saved_context: String,
        pub(super) handoff_requested_context: String,
        pub(super) defaults_keep_recent_tokens: i64,
        pub(super) setting_keep_recent_tokens_label: String,
        pub(super) setting_keep_recent_tokens_description: String,
        pub(super) defaults_native_retained_tokens: i64,
        pub(super) setting_native_retained_tokens_label: String,
        pub(super) setting_native_retained_tokens_description: String,
        pub(super) defaults_reserve_tokens: i64,
        pub(super) setting_reserve_tokens_label: String,
        pub(super) setting_reserve_tokens_description: String,
        pub(super) defaults_handoff_reserve_divisor: i64,
        pub(super) setting_handoff_reserve_divisor_label: String,
        pub(super) setting_handoff_reserve_divisor_description: String,
        pub(super) defaults_handoff_warning_reserves: i64,
        pub(super) setting_handoff_warning_reserves_label: String,
        pub(super) setting_handoff_warning_reserves_description: String,
        pub(super) defaults_handoff_urgent_reserves: i64,
        pub(super) setting_handoff_urgent_reserves_label: String,
        pub(super) setting_handoff_urgent_reserves_description: String,
        pub(super) default_enabled: bool,
        pub(super) default_mode: CompactionMode,
        pub(super) defaults_compaction_tokens: i64,
        pub(super) manifest_description: String,
        pub(super) manifest_label: String,
        pub(super) mode_automatic_description: String,
        pub(super) mode_automatic_label: String,
        pub(super) mode_handoff_description: String,
        pub(super) mode_handoff_label: String,
        pub(super) prompt_handoff: String,
        pub(super) prompt_reset: String,
        pub(super) prompt_restored: String,
        pub(super) prompt_summary_system: String,
        pub(super) prompt_summary_task: String,
        pub(super) prompt_urgent: String,
        pub(super) prompt_warning: String,
        pub(super) render_context_compacted: String,
        pub(super) setting_at_tokens_description: String,
        pub(super) setting_at_tokens_label: String,
        pub(super) setting_at_tokens_step: i64,
        pub(super) setting_mode_description: String,
        pub(super) setting_mode_label: String,
        pub(super) tool_new_context_description: String,
        pub(super) tool_write_handoff_description: String,
    }
    pub(super) static DEFINITION: std::sync::LazyLock<Definition> =
        std::sync::LazyLock::new(|| {
            let definition: Definition = crate::config::embedded(include_str!("compaction.toml"));

            assert!(definition.defaults_compaction_tokens >= 1);
            assert!(definition.defaults_keep_recent_tokens > 0);
            assert!(definition.defaults_native_retained_tokens > 0);
            assert!(definition.defaults_reserve_tokens > 0);
            assert!(definition.defaults_handoff_reserve_divisor > 0);
            assert!(definition.defaults_handoff_urgent_reserves > 0);
            assert!(
                definition.defaults_handoff_warning_reserves
                    > definition.defaults_handoff_urgent_reserves
            );
            assert!(definition.setting_at_tokens_step > 0);

            definition
        });
}
mod handoff;

const MAX_SUMMARY_TOOL_RESULT_CHARS: usize = 2_000;

/// Default compaction trigger for middleware instances without an override.
pub fn default_compaction_tokens() -> i64 {
    text::DEFINITION.defaults_compaction_tokens
}
const HANDOFF_EXCLUDES: &[&str] = &["context_offloading"];
static MODES: std::sync::LazyLock<Vec<MiddlewareSettingChoice>> = std::sync::LazyLock::new(|| {
    vec![
        MiddlewareSettingChoice {
            disables: &[],
            value: "automatic",
            label: text::DEFINITION.mode_automatic_label.as_str(),
            description: text::DEFINITION.mode_automatic_description.as_str(),
            symbol: None,
            tone: FrontendTone::Neutral,
        },
        MiddlewareSettingChoice {
            disables: HANDOFF_EXCLUDES,
            value: "handoff",
            label: text::DEFINITION.mode_handoff_label.as_str(),
            description: text::DEFINITION.mode_handoff_description.as_str(),
            symbol: None,
            tone: FrontendTone::Neutral,
        },
    ]
});
static SETTINGS: std::sync::LazyLock<Vec<MiddlewareSettingManifest>> =
    std::sync::LazyLock::new(|| {
        vec![
            MiddlewareSettingManifest::Select {
                id: "mode",
                label: text::DEFINITION.setting_mode_label.as_str(),
                description: text::DEFINITION.setting_mode_description.as_str(),
                choices: MiddlewareSettingChoices::Static(&MODES),
                unset_label: None,
                default: Some(text::DEFINITION.default_mode.id()),
                max_bytes: 9,
                composer: false,
            },
            MiddlewareSettingManifest::Integer {
                id: "at_tokens",
                label: text::DEFINITION.setting_at_tokens_label.as_str(),
                description: text::DEFINITION.setting_at_tokens_description.as_str(),
                min: 1,
                max: None,
                step: text::DEFINITION.setting_at_tokens_step,
                default: default_compaction_tokens(),
            },
            MiddlewareSettingManifest::Integer {
                id: "keep_recent_tokens",
                label: text::DEFINITION.setting_keep_recent_tokens_label.as_str(),
                description: text::DEFINITION
                    .setting_keep_recent_tokens_description
                    .as_str(),
                min: 1,
                max: None,
                step: 1,
                default: text::DEFINITION.defaults_keep_recent_tokens,
            },
            MiddlewareSettingManifest::Integer {
                id: "native_retained_tokens",
                label: text::DEFINITION
                    .setting_native_retained_tokens_label
                    .as_str(),
                description: text::DEFINITION
                    .setting_native_retained_tokens_description
                    .as_str(),
                min: 1,
                max: None,
                step: 1,
                default: text::DEFINITION.defaults_native_retained_tokens,
            },
            MiddlewareSettingManifest::Integer {
                id: "reserve_tokens",
                label: text::DEFINITION.setting_reserve_tokens_label.as_str(),
                description: text::DEFINITION.setting_reserve_tokens_description.as_str(),
                min: 1,
                max: None,
                step: 1,
                default: text::DEFINITION.defaults_reserve_tokens,
            },
            MiddlewareSettingManifest::Integer {
                id: "handoff_reserve_divisor",
                label: text::DEFINITION
                    .setting_handoff_reserve_divisor_label
                    .as_str(),
                description: text::DEFINITION
                    .setting_handoff_reserve_divisor_description
                    .as_str(),
                min: 1,
                max: None,
                step: 1,
                default: text::DEFINITION.defaults_handoff_reserve_divisor,
            },
            MiddlewareSettingManifest::Integer {
                id: "handoff_warning_reserves",
                label: text::DEFINITION
                    .setting_handoff_warning_reserves_label
                    .as_str(),
                description: text::DEFINITION
                    .setting_handoff_warning_reserves_description
                    .as_str(),
                min: 1,
                max: None,
                step: 1,
                default: text::DEFINITION.defaults_handoff_warning_reserves,
            },
            MiddlewareSettingManifest::Integer {
                id: "handoff_urgent_reserves",
                label: text::DEFINITION
                    .setting_handoff_urgent_reserves_label
                    .as_str(),
                description: text::DEFINITION
                    .setting_handoff_urgent_reserves_description
                    .as_str(),
                min: 1,
                max: None,
                step: 1,
                default: text::DEFINITION.defaults_handoff_urgent_reserves,
            },
        ]
    });

/// Policy used when a conversation reaches its context threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMode {
    /// Use the provider's native compaction capability, or a model summary.
    Automatic,
    /// Let the model save working notes and request a new context window.
    Handoff,
}

impl Default for CompactionMode {
    fn default() -> Self {
        text::DEFINITION.default_mode
    }
}

impl CompactionMode {
    const fn id(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Handoff => "handoff",
        }
    }
}

impl std::str::FromStr for CompactionMode {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        serde::Deserialize::deserialize(
            serde::de::value::StrDeserializer::<serde::de::value::Error>::new(value),
        )
        .map_err(|_| Error::Config("unsupported compaction mode".into()))
    }
}

/// Configuration and presentation metadata for compaction.
pub static MANIFEST: std::sync::LazyLock<MiddlewareManifest> =
    std::sync::LazyLock::new(|| MiddlewareManifest {
        id: "compaction",
        label: text::DEFINITION.manifest_label.as_str(),
        description: text::DEFINITION.manifest_description.as_str(),
        required: false,
        default_enabled: text::DEFINITION.default_enabled,
        required_model_capability: None,
        settings: &SETTINGS,
    });

/// Compacts visible context after a configurable token threshold.
pub struct Compaction {
    at_tokens: i64,
    mode: CompactionMode,
    keep_recent_tokens: usize,
    native_retained_tokens: usize,
    reserve_tokens: i64,
    handoff_reserve_divisor: i64,
    handoff_warning_reserves: i64,
    handoff_urgent_reserves: i64,
}

impl Default for Compaction {
    fn default() -> Self {
        Self {
            at_tokens: default_compaction_tokens(),
            mode: CompactionMode::default(),
            keep_recent_tokens: usize::try_from(text::DEFINITION.defaults_keep_recent_tokens)
                .expect("bundled recent budget must fit"),
            native_retained_tokens: usize::try_from(
                text::DEFINITION.defaults_native_retained_tokens,
            )
            .expect("bundled native budget must fit"),
            reserve_tokens: text::DEFINITION.defaults_reserve_tokens,
            handoff_reserve_divisor: text::DEFINITION.defaults_handoff_reserve_divisor,
            handoff_warning_reserves: text::DEFINITION.defaults_handoff_warning_reserves,
            handoff_urgent_reserves: text::DEFINITION.defaults_handoff_urgent_reserves,
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

    /// Sets the trailing complete conversation budget retained after summary compaction.
    /// # Errors
    /// Returns an error for a zero budget or one outside the supported integer range.
    pub fn keep_recent_tokens(mut self, tokens: usize) -> Result<Self> {
        positive_token_budget(tokens)?;
        self.keep_recent_tokens = tokens;
        Ok(self)
    }

    /// Sets the recent user-message budget retained beside native compaction output.
    /// # Errors
    /// Returns an error for a zero budget or one outside the supported integer range.
    pub fn native_retained_tokens(mut self, tokens: usize) -> Result<Self> {
        positive_token_budget(tokens)?;
        self.native_retained_tokens = tokens;
        Ok(self)
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

    /// Selects the context-window fraction and warning/urgent margins for handoffs.
    /// # Errors
    /// Returns an error unless all values are positive and warning exceeds urgent.
    pub fn handoff_policy(
        mut self,
        reserve_divisor: i64,
        warning_reserves: i64,
        urgent_reserves: i64,
    ) -> Result<Self> {
        if reserve_divisor <= 0 || urgent_reserves <= 0 || warning_reserves <= urgent_reserves {
            return Err(Error::Config("handoff policy requires a positive divisor and warning reserves greater than positive urgent reserves".into()));
        }
        self.handoff_reserve_divisor = reserve_divisor;
        self.handoff_warning_reserves = warning_reserves;
        self.handoff_urgent_reserves = urgent_reserves;
        Ok(self)
    }

    /// Selects the context continuation policy for every model using this middleware.
    #[must_use]
    pub fn mode(mut self, mode: CompactionMode) -> Self {
        self.mode = mode;
        self
    }

    /// Returns the effective trigger after reserving response space.
    #[must_use]
    pub fn trigger_tokens(&self, context_window: i64) -> i64 {
        match self.mode {
            CompactionMode::Automatic => self
                .at_tokens
                .min(context_window.saturating_sub(self.reserve_tokens))
                .max(1),
            CompactionMode::Handoff => handoff::warning_tokens(self, context_window),
        }
    }
}

impl Middleware for Compaction {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn incompatible_middleware(&self) -> &'static [&'static str] {
        match self.mode {
            CompactionMode::Automatic => &[],
            CompactionMode::Handoff => HANDOFF_EXCLUDES,
        }
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        if self.mode == CompactionMode::Handoff {
            handoff::register(catalog, runtime)?;
        }
        Ok(())
    }

    fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok((self.mode == CompactionMode::Handoff)
            .then(|| PromptSection::new(text::DEFINITION.prompt_handoff.as_str())))
    }

    fn post_tool_use<'a>(
        &'a self,
        context: &'a mut PostToolUseContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.mode == CompactionMode::Handoff {
                handoff::post_tool(context)?;
            }
            Ok(())
        })
    }

    fn model_request<'a>(
        &'a self,
        context: &'a mut ModelRequestContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.mode == CompactionMode::Handoff {
                handoff::decorate(context);
            }
            Ok(())
        })
    }

    fn prepare_compacted_input(&self, _original: &[Value], compacted: &mut Vec<Value>) {
        compacted.retain(|item| !handoff::is_control(item));
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.mode == CompactionMode::Handoff {
                handoff::restore_notes(context).await?;
            }
            Ok(())
        })
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        if let Some(block) = super::tools::render_tool_event(
            event,
            |name| matches!(name, "write_handoff" | "new_context"),
            |name, arguments| super::tools::labeled_tool_heading(name, "notes", arguments),
        ) {
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
        Box::pin(async move {
            if self.mode == CompactionMode::Handoff {
                return handoff::prepare(context, self).await;
            }
            let estimated = context.estimated_input_tokens();
            let observed = context
                .last_usage
                .map_or(0, |usage| usage.input_tokens)
                .max(estimated);
            if observed < self.trigger_tokens(context.context_window) || context.input().is_empty()
            {
                return Ok(());
            }
            context.pre_compact().await?;
            if context.turn_stopped() {
                return Ok(());
            }
            let catalog_revision = context.tools.revision()?;
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
                let request = CompactRequest {
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
                };
                context.model.compact(context.provider, request).await?
            } else {
                summarize(context, self.keep_recent_tokens).await?
            };
            if output.output.is_empty() {
                return Err(Error::Provider(
                    "compaction returned an empty context".into(),
                ));
            }
            apply_compaction(
                context,
                output.output,
                Some(output.usage),
                self.native_retained_tokens,
            )
            .await?;
            Ok(())
        })
    }
}

async fn apply_compaction(
    context: &mut ModelContext<'_>,
    output: Vec<Value>,
    usage: Option<crate::protocol::TokenUsage>,
    native_retained_tokens: usize,
) -> Result<()> {
    let tool_load = retained_tool_load(
        context.input(),
        context.tools.revision()?,
        &context.tools.deferred_definitions(),
    )?;
    let latest_turn_input = latest_turn_input(context.input());
    let active_message_metadata = latest_turn_input
        .as_ref()
        .and_then(|active| active.get(MESSAGE_METADATA_FIELD))
        .cloned();
    let mut compacted = retain_native_context(
        context.input(),
        output,
        native_retained_tokens,
        context.token_estimate,
    );
    restore_input_private_fields(&mut compacted, latest_turn_input);
    context
        .hooks
        .prepare_compacted_input(context.input(), &mut compacted);
    validate_active_message_metadata(&compacted, active_message_metadata.as_ref())?;
    if let Some(tool_load) = tool_load {
        compacted.push(tool_load);
    }
    context
        .model
        .validate_media(context.provider, context.session_id, &compacted)
        .await?;
    context.rewrite_input(ContextRewriteReason::Compaction, compacted)?;
    *context.compaction_count = context
        .compaction_count
        .checked_add(1)
        .ok_or_else(|| Error::Checkpoint("compaction count overflow".into()))?;
    context.record_transcript_item(internal_user_message(CONTEXT_COMPACTED_MARKER, ""));
    if let Some(usage) = usage {
        context.usage.push(usage);
    }
    context.events.push(EventMsg::ContextCompacted);
    context.post_compact().await?;
    Ok(())
}

fn retained_tool_load(
    input: &[Value],
    catalog_revision: &str,
    deferred_tools: &[ToolDefinition],
) -> Result<Option<Value>> {
    let deferred = deferred_tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<BTreeSet<_>>();
    let loaded = loaded_tools(input, catalog_revision, &deferred)?;
    Ok((!loaded.is_empty()).then(|| {
        ToolLoad {
            catalog_revision: catalog_revision.into(),
            tools: loaded.into_iter().collect(),
        }
        .into_input()
    }))
}

fn retain_native_context(
    input: &[Value],
    mut compacted: Vec<Value>,
    native_retained_tokens: usize,
    estimate: TokenEstimate,
) -> Vec<Value> {
    if compacted.len() != 1
        || compacted[0].get("type").and_then(Value::as_str) != Some("compaction")
    {
        return compacted;
    }
    let cut = recent_cut(input, native_retained_tokens, estimate).unwrap_or(0);
    let recent = &input[cut..];
    let mut retained = Vec::new();
    for item in recent {
        if (!is_internal_message(item) || item.get(MESSAGE_METADATA_FIELD).is_some())
            && item.get("role").and_then(Value::as_str) == Some("user")
        {
            retained.push(item.clone());
        }
    }
    retained.append(&mut compacted);
    retained
}

fn latest_turn_input(input: &[Value]) -> Option<&Value> {
    input
        .iter()
        .rfind(|item| item.get(MESSAGE_METADATA_FIELD).is_some())
}

fn validate_active_message_metadata(input: &[Value], expected: Option<&Value>) -> Result<()> {
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

fn restore_input_private_fields(compacted: &mut Vec<Value>, latest_turn_input: Option<&Value>) {
    let Some(input) = latest_turn_input else {
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
    if private.is_empty() {
        return;
    }
    let retained_index = compacted.iter().rposition(|item| {
        item.get("role") == input.get("role") && item.get("content") == input.get("content")
    });
    if let Some(index) = retained_index {
        if let Some(fields) = compacted[index].as_object_mut() {
            fields.extend(private);
        }
    } else {
        compacted.push(input.clone());
    }
}

async fn summarize(context: &ModelContext<'_>, keep_recent_tokens: usize) -> Result<CompactOutput> {
    let (prompt, recent) =
        prepare_summary(context.input(), keep_recent_tokens, context.token_estimate).ok_or_else(
            || Error::Provider("context has no safe history boundary to compact".into()),
        )?;
    let session_id = Uuid::new_v4().to_string();
    let cache_key = prompt_cache_key(&session_id);
    let cut = context.input().len() - recent.len();
    let mut input = vec![user_message(&prompt)];
    for item in &context.input()[..cut] {
        let images = crate::protocol::content_parts(item)
            .into_iter()
            .flatten()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("input_image"))
            .flat_map(|part| {
                let label = crate::protocol::content_part_text(part).unwrap_or_default();
                [
                    serde_json::json!({"type":"input_text", "text":label}),
                    part.clone(),
                ]
            })
            .collect::<Vec<_>>();
        if !images.is_empty() {
            let label = text::DEFINITION.prompt_visual_evidence.replace(
                "{item}",
                &item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .unwrap_or(&Value::Null)
                    .to_string(),
            );
            let mut evidence = user_message(&label);
            evidence["content"]
                .as_array_mut()
                .ok_or_else(|| Error::Checkpoint("invalid summary evidence content".into()))?
                .extend(images);
            input.push(evidence);
        }
    }
    let request = ModelRequest {
        session_id: context.session_id,
        prompt_cache: Some(PromptCacheIdentity {
            key: &cache_key,
            context_epoch: *context.context_epoch,
        }),
        instructions: text::DEFINITION.prompt_summary_system.as_str(),
        input: &input,
        catalog_revision: context.tools.revision()?,
        tools: &[],
        deferred_tools: &[],
        allow_hosted_tools: false,
        allow_continuation: false,
    };
    let output = context
        .model
        .respond(
            context.provider,
            request,
            Arc::new(|_| Box::pin(async { Ok(()) })),
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
        &text::DEFINITION
            .prompt_compacted_context
            .replace("{summary}", summary),
    ));
    for item in recent {
        if ToolLoad::from_input(&item)?.is_none() {
            compacted.push(item);
        }
    }
    CompactOutput::from_output(compacted, output.usage().clone())
}

fn prepare_summary(
    input: &[Value],
    keep_recent_tokens: usize,
    estimate: TokenEstimate,
) -> Option<(String, Vec<Value>)> {
    let cut = recent_cut(input, keep_recent_tokens, estimate)?;
    let prompt = summary_prompt(&input[..cut])?;
    Some((prompt, input[cut..].to_vec()))
}

fn recent_cut(input: &[Value], keep_tokens: usize, estimate: TokenEstimate) -> Option<usize> {
    let mut accumulated = 0usize;
    let mut desired = None;
    for index in (0..input.len()).rev() {
        accumulated = accumulated.saturating_add(estimate.item_tokens(&input[index]));
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
    if is_internal_message(item) && item.get(MESSAGE_METADATA_FIELD).is_none() {
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
    prompt.push_str(&format!(
        "\n{}",
        text::DEFINITION.prompt_summary_task.as_str()
    ));
    Some(prompt)
}

fn serialize_item(item: &Value) -> Option<String> {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => Some(format!(
            "[{}]: {}({})",
            text::DEFINITION.summary_tool_call_label,
            item.get("name").and_then(Value::as_str).unwrap_or("tool"),
            value_text(item.get("arguments"))
        )),
        Some("function_call_output") => Some(format!(
            "[{}]: {}",
            text::DEFINITION.summary_tool_result_label,
            truncate_chars(
                &content_text(item.get("output")),
                MAX_SUMMARY_TOOL_RESULT_CHARS
            )
        )),
        Some("reasoning") => {
            let text = content_text(item.get("summary"));
            (!text.is_empty())
                .then(|| format!("[{}]: {text}", text::DEFINITION.summary_reasoning_label))
        }
        Some("message") | None => {
            let role = item.get("role").and_then(Value::as_str)?;
            let text = content_text(item.get("content"));
            (!text.is_empty()).then(|| {
                let label = if role == "assistant" {
                    text::DEFINITION.summary_assistant_label.as_str()
                } else {
                    text::DEFINITION.summary_user_label.as_str()
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

fn content_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(crate::protocol::content_part_text)
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

fn positive_token_budget(tokens: usize) -> Result<()> {
    if tokens == 0 || i64::try_from(tokens).is_err() {
        return Err(Error::Config(
            "retained token budget must be a positive signed integer".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_local_defaults_preserve_retention_and_handoff_policy() {
        let policy = Compaction::default();
        assert_eq!(policy.at_tokens, 250_000);
        assert_eq!(policy.keep_recent_tokens, 20_000);
        assert_eq!(policy.native_retained_tokens, 64_000);
        assert_eq!(policy.reserve_tokens, 16_384);
        assert_eq!(policy.handoff_reserve_divisor, 8);
        assert_eq!(policy.handoff_warning_reserves, 3);
        assert_eq!(policy.handoff_urgent_reserves, 2);
        assert_eq!(policy.mode, CompactionMode::Automatic);
    }

    #[test]
    fn configured_compaction_reserves_and_retention_change_behavior() {
        let policy = Compaction::new(900)
            .unwrap()
            .reserve_tokens(100)
            .unwrap()
            .keep_recent_tokens(1)
            .unwrap()
            .native_retained_tokens(1)
            .unwrap()
            .handoff_policy(10, 4, 2)
            .unwrap();
        assert_eq!(policy.trigger_tokens(1000), 900);
        assert_eq!(
            policy.mode(CompactionMode::Handoff).trigger_tokens(1000),
            600
        );
        assert!(Compaction::default().handoff_policy(8, 2, 3).is_err());
        let input = vec![user_message("old request"), user_message("latest request")];
        let native =
            vec![serde_json::json!({"type":"compaction", "encrypted_content":"checkpoint"})];
        let retained = retain_native_context(&input, native, 1, TokenEstimate::default());
        assert_eq!(retained.len(), 2);
        assert_eq!(retained[0], input[1]);
        assert!(prepare_summary(&input, 1000, TokenEstimate::default()).is_none());
        assert!(prepare_summary(&input, 1, TokenEstimate::default()).is_some());
    }

    use crate::backend::model::tool_output;

    #[test]
    fn configured_modes_parse_at_the_owner_boundary() {
        for choice in MODES.iter() {
            assert_eq!(
                choice
                    .value
                    .parse::<CompactionMode>()
                    .expect("declared mode")
                    .id(),
                choice.value
            );
        }
        assert!("other".parse::<CompactionMode>().is_err());
    }

    #[test]
    fn handoff_event_shows_written_notes() {
        let notes = "Goal: finish the UI update.\nVerified: regression tests pass.";
        let block = Compaction::default()
            .render(
                &EventMsg::ToolCallBegin(crate::protocol::ToolCallBeginEvent {
                    turn_id: "turn".into(),
                    call_id: "save".into(),
                    name: "write_handoff".into(),
                    arguments: serde_json::json!({"notes": notes}),
                }),
                "session",
            )
            .expect("handoff event");
        assert_eq!(block.text, notes);
    }

    #[test]
    fn handoff_and_offloading_cannot_share_a_stack_in_either_order() {
        use crate::middleware::{MiddlewareStack, context_offloading::ContextOffloading};
        for reverse in [false, true] {
            let mut entries: Vec<Arc<dyn Middleware>> = vec![
                Arc::new(Compaction::default().mode(CompactionMode::Handoff)),
                Arc::new(ContextOffloading::new(50_000).expect("offloading")),
            ];
            if reverse {
                entries.reverse();
            }
            assert!(MiddlewareStack::new(entries).is_err());
        }
    }

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
            tool_output("a", "x".repeat(200), false),
            tool_output("b", "done", false),
        ];

        assert_eq!(recent_cut(&input, 10, TokenEstimate::default()), Some(1));
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
        let handoff = compaction.mode(CompactionMode::Handoff);
        assert_eq!(handoff.trigger_tokens(272_000), 222_848);
        assert_eq!(handoff.trigger_tokens(8_000), 5_000);
        assert_eq!(handoff.trigger_tokens(1), 1);
        assert_eq!(
            Compaction::new(4_000)
                .expect("custom threshold")
                .mode(CompactionMode::Handoff)
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

        restore_input_private_fields(&mut compacted, Some(&user));

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
        let mut compacted = retain_native_context(
            &input,
            compacted,
            Compaction::default().native_retained_tokens,
            TokenEstimate::default(),
        );
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
        let files = tempfile::tempdir().expect("files");
        let attachments = super::super::attachments::Attachments::new(
            crate::backend::session_files::SessionFileStore::new(files.path(), None),
        );
        attachments.prepare_compacted_input(&input, &mut compacted);
        assert_eq!(compacted, vec![compaction, user, materialization]);
    }

    #[test]
    fn compaction_preserves_active_message_metadata() {
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
        let expected = peer[MESSAGE_METADATA_FIELD].clone();
        let compacted = vec![user_message("forged later input")];

        let error = validate_active_message_metadata(&compacted, Some(&expected))
            .expect_err("message metadata must remain active");

        assert!(error.to_string().contains("active message metadata"));
    }
}
