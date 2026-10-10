//! Model provider interface and routing.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::io;
use std::io::Write;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::protocol::{MAX_TOOL_NAME_BYTES, TOOL_LOAD_MARKER, TokenUsage, ToolCall};
use crate::protocol::{
    ModelStepAnnotation, ModelStepContent, ModelStepContentPhase, PromptCacheMode,
    PromptCacheOutcome, ToolDiscoveryMode,
};

pub mod anthropic;
mod authorization;
mod cancellation;
mod chat_completions;
pub mod deepseek;
mod image_generation;
pub mod kimi;
pub(crate) mod media;
pub mod mistral;
pub mod openai;
pub mod openai_codex;
pub mod openai_socket;
pub mod openrouter;
pub mod provider;
pub mod realtime;
mod responses;
mod responses_socket;
mod responses_wire;
mod router;
pub use cancellation::{ModelCancellation, ModelCancellationReason};
pub(crate) use image_generation::MAX_IMAGE_PROMPT_CHARS;
pub use image_generation::{
    GeneratedImage, ImageAspect, ImageGenerationReference, ImageGenerationRequest,
};
pub use media::{ImageInputLimits, MediaPreparation};
mod transport;
pub use transport::ModelTransportSettings;
pub(crate) use transport::retry_delay;

pub use self::realtime::{
    RealtimeVoiceCall, RealtimeVoiceCommand, RealtimeVoiceEvent, RealtimeVoiceRequest,
};
pub use self::router::{ImageModel, ModelCredentialLifetime, ModelRouter};

use crate::protocol::ModelInfo;
use crate::protocol::{
    ATTACHMENTS_FIELD, INTERNAL_MESSAGE_FIELD, MESSAGE_METADATA_FIELD, MessageAuthor, MessageEvent,
    MessageSource, SessionFileReference,
};
pub(crate) use crate::protocol::{
    PROMPT_CACHE_BREAKPOINT_FIELD, REPLAY_REASONING_FIELD, TOOL_ERROR_FIELD,
};
pub use input::ModelInput;
mod input;
// Leaves room for typed lifecycle metadata inside the frontend envelope.
const MAX_MODEL_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_TOOL_CALLS: usize = 128;
const MAX_TOOL_ARGUMENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOOL_CALL_ID_BYTES: usize = 4 * 1024;
/// Stable semantic name of the core deferred-tool discovery function.
pub const TOOLS_SEARCH_NAME: &str = "tools_search";

pub(crate) fn is_provider_reasoning(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("reasoning")
}

pub(crate) fn has_provider_reasoning(item: &Value) -> bool {
    is_provider_reasoning(item)
        || item.get(REPLAY_REASONING_FIELD).is_some()
        || provider::providers()
            .iter()
            .filter_map(|provider| provider.replay_reasoning_field)
            .any(|field| item.get(field).is_some())
}

fn strip_reasoning_fields(item: &mut Value) -> bool {
    let neutral = item
        .as_object_mut()
        .is_some_and(|fields| fields.remove(REPLAY_REASONING_FIELD).is_some());
    let mut changed = neutral;
    if let Some(fields) = item.as_object_mut() {
        for field in provider::providers()
            .iter()
            .filter_map(|provider| provider.replay_reasoning_field)
        {
            changed |= fields.remove(field).is_some();
        }
    }
    changed
}

/// Removes provider-private reasoning before another model route replays context.
/// Returns whether the active input changed; visible messages and tool pairs remain intact.
pub(crate) fn strip_provider_reasoning(input: &mut Vec<Value>) -> bool {
    let mut changed = false;
    input.retain_mut(|item| {
        if is_provider_reasoning(item) {
            changed = true;
            return false;
        }
        changed |= strip_reasoning_fields(item);
        true
    });
    changed
}

/// Removes provider-private reasoning while sharing untouched durable items.
pub(crate) fn strip_shared_provider_reasoning(input: &mut Vec<Arc<Value>>) -> bool {
    let mut changed = false;
    input.retain_mut(|item| {
        if is_provider_reasoning(item) {
            changed = true;
            return false;
        }
        if has_provider_reasoning(item) {
            changed |= strip_reasoning_fields(Arc::make_mut(item));
        }
        true
    });
    changed
}

/// A function tool definition sent to a model provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Stable function name used to match model calls to a registered tool.
    /// Must satisfy the catalog's tool-name validation and be unique within that catalog.
    pub name: String,
    /// Model-facing explanation of when to call the tool and what it does.
    pub description: String,
    /// JSON Schema for the tool's argument object.
    /// Provider adapters serialize this schema into their native function-tool format;
    /// the registered tool remains responsible for validating arguments before execution.
    pub parameters: Value,
}

impl ToolCall {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.call_id.trim().is_empty() {
            return Err(Error::Provider("tool call ID cannot be empty".into()));
        }
        if self.call_id.len() > MAX_TOOL_CALL_ID_BYTES {
            return Err(Error::Provider("tool call ID exceeded size limit".into()));
        }
        if self.name.trim().is_empty() {
            return Err(Error::Provider("tool call name cannot be empty".into()));
        }
        if self.name.len() > MAX_TOOL_NAME_BYTES {
            return Err(Error::Provider("tool call name exceeded size limit".into()));
        }
        if !self.arguments.is_object() {
            return Err(Error::Provider(
                "tool call arguments must be an object".into(),
            ));
        }
        let mut writer = SizeWriter::new(MAX_TOOL_ARGUMENT_BYTES);
        serde_json::to_writer(&mut writer, &self.arguments).map_err(|error| {
            if writer.exceeded {
                Error::Provider("tool call arguments exceeded size limit".into())
            } else {
                Error::Provider(format!("tool call arguments are invalid: {error}").into())
            }
        })
    }

    pub(crate) fn replace(&mut self, name: String, arguments: Value) -> Result<()> {
        if name.trim().is_empty() {
            return Err(Error::Tool(format!(
                "tool call `{}` name is empty",
                self.call_id
            )));
        }
        if name.len() > MAX_TOOL_NAME_BYTES {
            return Err(Error::Tool(format!(
                "tool call `{}` name exceeded size limit",
                self.call_id
            )));
        }
        if !arguments.is_object() {
            return Err(Error::Tool(format!(
                "tool call `{}` arguments must be a JSON object",
                self.call_id
            )));
        }
        let mut writer = SizeWriter::new(MAX_TOOL_ARGUMENT_BYTES);
        if let Err(error) = serde_json::to_writer(&mut writer, &arguments) {
            return Err(Error::Tool(if writer.exceeded {
                format!("tool call `{}` arguments exceeded size limit", self.call_id)
            } else {
                format!(
                    "tool call `{}` arguments are invalid: {error}",
                    self.call_id
                )
            }));
        }
        self.name = name;
        self.arguments = arguments;
        Ok(())
    }
}

impl ToolDefinition {
    pub(crate) fn validate(&self) -> Result<()> {
        crate::validate_identifier("tool name", &self.name, MAX_TOOL_NAME_BYTES)?;
        if !self.parameters.is_object() {
            return Err(Error::Config(format!(
                "tool `{}` parameters must be a JSON object",
                self.name
            )));
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct StreamingToolCalls {
    call_ids: BTreeSet<String>,
    bytes: usize,
}

impl StreamingToolCalls {
    pub(crate) fn accept(&mut self, call: &ToolCall) -> Result<()> {
        call.validate()?;
        if self.call_ids.contains(&call.call_id) {
            return Err(Error::Provider(
                format!("model returned duplicate tool-call ID `{}`", call.call_id).into(),
            ));
        }
        if self.call_ids.len() >= MAX_TOOL_CALLS {
            return Err(Error::Provider(
                format!("model returned more than {MAX_TOOL_CALLS} tool calls").into(),
            ));
        }
        let remaining = MAX_MODEL_OUTPUT_BYTES.saturating_sub(self.bytes);
        let mut writer = SizeWriter::new(remaining);
        serde_json::to_writer(&mut writer, call).map_err(|error| {
            if writer.exceeded {
                Error::Provider("streamed tool calls exceeded size limit".into())
            } else {
                Error::Provider(format!("tool call could not be serialized: {error}").into())
            }
        })?;
        self.call_ids.insert(call.call_id.clone());
        self.bytes += writer.bytes;
        Ok(())
    }
}

/// Input for one model turn.
#[derive(Debug, Clone, Copy)]
pub struct ModelRequest<'a> {
    /// Local session identity used for transport continuation state.
    pub session_id: &'a str,
    /// Optional local cause recorded before an unfinished request is dropped.
    pub cancellation: Option<&'a ModelCancellation>,
    /// Optional provider-visible prompt-cache identity.
    pub prompt_cache: Option<PromptCacheIdentity<'a>>,
    /// The instructions.
    pub instructions: &'a str,
    /// The input.
    pub input: ModelInput<'a>,
    /// Revision of the active tool catalog used to validate typed tool-load controls.
    pub catalog_revision: &'a str,
    /// Schemas callable without deferred discovery for this request.
    pub tools: &'a [Arc<ToolDefinition>],
    /// Searchable schemas withheld from the model until provider-native discovery.
    pub deferred_tools: &'a [Arc<ToolDefinition>],
    /// Whether provider-hosted tools such as web search may be attached.
    pub allow_hosted_tools: bool,
    /// Whether a transport may continue a previous response for this session.
    pub allow_continuation: bool,
}

/// Provider-visible identity for one prompt-cache lineage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptCacheIdentity<'a> {
    /// Opaque, stable key. Providers must never receive the raw session ID here.
    pub key: &'a str,
    /// Active-context rewrite epoch, used to invalidate transport continuation.
    pub context_epoch: u64,
}

impl PromptCacheMode {
    fn outcome(self, usage: &TokenUsage, context_rewritten: bool) -> PromptCacheOutcome {
        if self == Self::Unsupported {
            PromptCacheOutcome::Unsupported
        } else if usage.cached_input_tokens > 0 {
            PromptCacheOutcome::Hit
        } else if context_rewritten {
            PromptCacheOutcome::ContextRewrite
        } else if usage.cache_write_input_tokens > 0 {
            PromptCacheOutcome::Write
        } else {
            PromptCacheOutcome::Miss
        }
    }
}

/// Hashes a local session identity into a stable provider cache key.
#[must_use]
pub fn prompt_cache_key(session_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"mobius/prompt-cache/v1/");
    digest.update(session_id.as_bytes());
    format!("{:x}", digest.finalize())
}

/// Fallible asynchronous callback used to forward streaming provider events.
///
/// Providers must await each callback and propagate errors instead of retrying or silently
/// dropping events. An agent-provided sink has bounded ingress and closes when
/// [`Model::respond`] completes or is canceled; retaining it does not extend
/// the response lifetime.
pub type ModelEventSink =
    Arc<dyn Fn(crate::protocol::ModelEvent) -> BoxFuture<'static, Result<()>> + Send + Sync>;

/// Completed output from a model response.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ModelOutput {
    pub(crate) output: Vec<Value>,
    pub(crate) text: String,
    pub(crate) content: Vec<ModelStepContent>,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) materialized_tools: BTreeSet<String>,
    pub(crate) end_turn: bool,
    pub(crate) usage: TokenUsage,
}

impl ModelOutput {
    /// Validates normalized output and derives its visible text and tool calls.
    /// # Errors
    ///
    /// Returns an error if the input cannot be parsed or validated.
    pub fn from_output(output: Vec<Value>, end_turn: bool, usage: TokenUsage) -> Result<Self> {
        let content = normalized_step_content(&output)?;
        Self::from_output_with_content(output, end_turn, usage, content)
    }

    pub(super) fn from_output_with_content(
        output: Vec<Value>,
        end_turn: bool,
        usage: TokenUsage,
        content: Vec<ModelStepContent>,
    ) -> Result<Self> {
        validate_provider_output(&output)?;
        if output.iter().any(|item| {
            item.get("role").is_some()
                && item.get("role").and_then(Value::as_str) != Some("assistant")
        }) {
            return Err(Error::Provider(
                "provider returned a non-assistant message".into(),
            ));
        }
        validate_usage(&usage)?;
        if output.is_empty() {
            return Err(Error::Provider("model returned no output".into()));
        }

        let text = content
            .iter()
            .filter(|content| content.phase == ModelStepContentPhase::FinalAnswer)
            .map(|content| content.text.as_str())
            .collect();

        let mut call_ids = BTreeSet::new();
        let mut tool_calls = Vec::new();
        for item in output
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        {
            if tool_calls.len() >= MAX_TOOL_CALLS {
                return Err(Error::Provider(
                    format!("model returned more than {MAX_TOOL_CALLS} tool calls").into(),
                ));
            }
            let call = decode_tool_call(item)?;
            let call_id = required_output_string(item, "call_id", MAX_TOOL_CALL_ID_BYTES)?;
            if !call_ids.insert(call_id) {
                return Err(Error::Provider(
                    format!("model returned duplicate tool-call ID `{}`", call.call_id).into(),
                ));
            }
            tool_calls.push(call);
        }

        Ok(Self {
            output,
            text,
            content,
            tool_calls,
            materialized_tools: BTreeSet::new(),
            end_turn,
            usage,
        })
    }

    /// Returns the provider-neutral output items.
    #[must_use]
    pub fn output(&self) -> &[Value] {
        &self.output
    }

    /// Returns the visible assistant text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the validated tool calls.
    #[must_use]
    pub fn tool_calls(&self) -> &[ToolCall] {
        &self.tool_calls
    }

    /// Returns deferred tools that the provider proved materialized in this response.
    #[must_use]
    pub fn materialized_tools(&self) -> &BTreeSet<String> {
        &self.materialized_tools
    }

    /// Reports whether the provider ended the turn.
    #[must_use]
    pub fn end_turn(&self) -> bool {
        self.end_turn
    }

    /// Returns the validated token usage.
    #[must_use]
    pub fn usage(&self) -> &TokenUsage {
        &self.usage
    }

    /// Returns complete, provider-neutral text normalized at the model boundary.
    #[must_use]
    pub fn content(&self) -> &[ModelStepContent] {
        &self.content
    }

    /// Consumes the response into normalized history, validated calls and usage.
    pub(crate) fn into_parts(self) -> (Vec<Value>, Vec<ToolCall>, TokenUsage) {
        (self.output, self.tool_calls, self.usage)
    }

    pub(crate) fn sync_tool_calls(&mut self) -> Result<()> {
        for (item, call) in self
            .output
            .iter_mut()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
            .zip(&self.tool_calls)
        {
            let object = item
                .as_object_mut()
                .expect("validated function call must be an object");
            object.insert("name".into(), Value::String(call.name.clone()));
            object.insert(
                "arguments".into(),
                Value::String(serde_json::to_string(&call.arguments)?),
            );
        }
        ensure_output_size(&self.output).map_err(|_| {
            Error::Tool("rewritten tool calls exceeded model output size limit".into())
        })?;
        Ok(())
    }

    pub(super) fn with_materialized_tools(
        mut self,
        names: impl IntoIterator<Item = String>,
    ) -> Result<Self> {
        for name in names {
            if name.trim().is_empty() || name.len() > MAX_TOOL_NAME_BYTES {
                return Err(Error::Provider(
                    "provider materialized an invalid tool name".into(),
                ));
            }
            self.materialized_tools.insert(name);
        }
        Ok(self)
    }
}

fn normalized_step_content(output: &[Value]) -> Result<Vec<ModelStepContent>> {
    let mut content = Vec::new();
    let final_message_index = output
        .iter()
        .rposition(|item| item.get("type").and_then(Value::as_str) == Some("message"));
    for (output_index, item) in output.iter().enumerate() {
        normalize_reasoning_content(output_index, item, &mut content);
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let precedes_hosted_search = final_message_index.is_some_and(|final_index| {
            output_index < final_index
                && output[output_index + 1..final_index].iter().any(|item| {
                    item.get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| {
                            kind == "web_search_call"
                                || provider::providers()
                                    .iter()
                                    .any(|provider| provider.web_search_type == Some(kind))
                        })
                })
        });
        let declared_phase = item.get("phase").and_then(Value::as_str);
        let phase = if declared_phase == Some("commentary")
            || (declared_phase.is_none() && precedes_hosted_search)
        {
            ModelStepContentPhase::Commentary
        } else {
            ModelStepContentPhase::FinalAnswer
        };
        for (part_index, part) in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            if part.get("type").and_then(Value::as_str) != Some("output_text") {
                continue;
            }
            let text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                Error::Provider("output text part omitted text".to_string().into())
            })?;
            if text.is_empty() {
                continue;
            }
            content.push(ModelStepContent {
                output_index,
                part_index,
                phase,
                text: text.into(),
                annotations: normalize_output_text_annotations(part)?,
            });
        }
    }
    Ok(content)
}

fn normalize_reasoning_content(
    output_index: usize,
    item: &Value,
    content: &mut Vec<ModelStepContent>,
) {
    let parts = if item.get("type").and_then(Value::as_str) == Some("reasoning") {
        ["summary", "content"]
            .into_iter()
            .filter_map(|field| item.get(field).and_then(Value::as_array))
            .find(|parts| {
                parts.iter().any(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
                })
            })
    } else {
        None
    };
    if let Some(parts) = parts {
        content.extend(parts.iter().enumerate().filter_map(|(part_index, part)| {
            part.get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| ModelStepContent {
                    output_index,
                    part_index,
                    phase: ModelStepContentPhase::Reasoning,
                    text: text.into(),
                    annotations: Vec::new(),
                })
        }));
        return;
    }
    if let Some(text) = item
        .get(REPLAY_REASONING_FIELD)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        content.push(ModelStepContent {
            output_index,
            part_index: 0,
            phase: ModelStepContentPhase::Reasoning,
            text: text.into(),
            annotations: Vec::new(),
        });
    }
}

fn normalize_output_text_annotations(part: &Value) -> Result<Vec<ModelStepAnnotation>> {
    let Some(annotations) = part.get("annotations") else {
        return Ok(Vec::new());
    };
    if annotations.is_null() {
        return Ok(Vec::new());
    }
    let annotations: Vec<OutputTextAnnotation> = serde::Deserialize::deserialize(annotations)
        .map_err(|error| {
            Error::Provider(format!("invalid output text annotation: {error}").into())
        })?;
    Ok(annotations.into_iter().map(Into::into).collect())
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum OutputTextAnnotation {
    UrlCitation {
        url: String,
        title: String,
        content: Option<String>,
        start_index: usize,
        end_index: usize,
    },
    FileCitation {
        file_id: String,
        filename: String,
        index: usize,
    },
    ContainerFileCitation {
        container_id: String,
        file_id: String,
        filename: String,
        start_index: usize,
        end_index: usize,
    },
    FilePath {
        file_id: String,
        index: usize,
    },
}

impl From<OutputTextAnnotation> for ModelStepAnnotation {
    fn from(annotation: OutputTextAnnotation) -> Self {
        match annotation {
            OutputTextAnnotation::UrlCitation {
                url,
                title,
                content,
                start_index,
                end_index,
            } => Self::UrlCitation {
                url,
                title,
                content,
                start_index,
                end_index,
            },
            OutputTextAnnotation::FileCitation {
                file_id,
                filename,
                index,
            } => Self::FileCitation {
                file_id,
                filename,
                index,
            },
            OutputTextAnnotation::ContainerFileCitation {
                container_id,
                file_id,
                filename,
                start_index,
                end_index,
            } => Self::ContainerFileCitation {
                container_id,
                file_id,
                filename,
                start_index,
                end_index,
            },
            OutputTextAnnotation::FilePath { file_id, index } => Self::FilePath { file_id, index },
        }
    }
}

/// A model provider Adapter used by the agent loop.
pub trait Model: Send + Sync {
    /// Validated operational policy for this route; custom providers inherit owner defaults.
    /// Overrides must validate settings during provider construction.
    fn transport_settings(&self) -> ModelTransportSettings {
        ModelTransportSettings::default()
    }

    /// Returns stable display metadata without exposing credentials.
    fn info(&self) -> ModelInfo {
        ModelInfo::default()
    }

    /// Reports whether this provider accepts native image input.
    fn supports_image_input(&self) -> bool {
        false
    }

    /// Reports whether this provider can generate an image through its native API.
    fn supports_image_generation(&self) -> bool {
        false
    }

    /// Generates or edits one image using provider-native image endpoints.
    fn generate_image<'a>(
        &'a self,
        _request: ImageGenerationRequest<'a>,
    ) -> BoxFuture<'a, Result<GeneratedImage>> {
        Box::pin(async {
            Err(Error::Provider(
                "image generation is unavailable for this provider".into(),
            ))
        })
    }

    /// Whether images can remain associated with their originating tool call.
    fn supports_tool_image_input(&self) -> bool {
        false
    }

    /// Reports whether this transport can negotiate a provider-owned realtime voice call.
    fn supports_realtime_voice(&self) -> bool {
        false
    }

    /// Negotiates voice media without exposing provider credentials to the frontend.
    fn start_realtime_voice(
        &self,
        _request: RealtimeVoiceRequest,
    ) -> BoxFuture<'_, Result<RealtimeVoiceCall>> {
        Box::pin(async {
            Err(Error::Provider(
                "realtime voice is unavailable for this provider".into(),
            ))
        })
    }

    /// Reports the provider's prompt-cache mode without exposing transport details.
    fn prompt_cache_capability(&self) -> PromptCacheMode {
        PromptCacheMode::Unsupported
    }

    /// Reports how this route makes deferred tool schemas callable.
    fn tool_discovery(&self) -> ToolDiscoveryMode {
        ToolDiscoveryMode::Rebuild
    }

    /// Produces one streamed response.
    ///
    /// Normalize provider wire data into [`crate::protocol::ModelEvent`] and
    /// [`ModelOutput`]. Emit only immutable, fully validated
    /// [`crate::protocol::ModelEvent::ToolCallReady`] calls, in the same order as
    /// the final output; the agent may execute them before stream EOF, so the
    /// final output must agree. Propagate [`ModelEventSink`] failures. The returned
    /// future may be dropped on cancellation; implementations own cleanup of any
    /// transport work they launch outside that future. When present, the request's
    /// [`ModelCancellation`] records the local cause before that drop.
    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>>;

    /// Prepares full logical history unless the transport can select a continuation suffix.
    fn respond_prepared<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
        media: MediaPreparation<'a>,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(async move {
            let Some(mut input) = media
                .prepare(request.session_id, request.input, self, true)
                .await?
            else {
                media::check_request_bytes(
                    self.request_size(request)?,
                    self.transport_settings().max_request_bytes,
                )?;
                return self.respond(request, events).await;
            };
            media::bound_request(
                &mut input,
                request.input,
                media.limits,
                self.transport_settings().max_request_bytes,
                true,
                |input| {
                    self.request_size(ModelRequest {
                        input: input.into(),
                        ..request
                    })
                },
            )?;
            self.respond(
                ModelRequest {
                    input: (&input).into(),
                    ..request
                },
                events,
            )
            .await
        })
    }

    /// Measures the serialized provider envelope for request admission.
    /// # Errors
    /// Returns wire conversion or serialization errors.
    fn request_size(&self, request: ModelRequest<'_>) -> Result<usize> {
        #[derive(Serialize)]
        struct Envelope<'a> {
            instructions: &'a str,
            input: ModelInput<'a>,
            tools: &'a [Arc<ToolDefinition>],
            deferred_tools: &'a [Arc<ToolDefinition>],
        }
        media::serialized_size(&Envelope {
            instructions: request.instructions,
            input: request.input,
            tools: request.tools,
            deferred_tools: request.deferred_tools,
        })
    }

    /// Switches a session to its fallback transport once, without sending a request.
    ///
    /// Called after retry exhaustion only when replaying the model request is safe.
    /// Returns whether a new transport was activated; providers without one return false.
    fn fallback_transport<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }
}

pub(crate) fn image_input<'a>(
    part: &'a Value,
    provider: &str,
) -> Result<Option<(&'a str, &'a str)>> {
    if part.get("type").and_then(Value::as_str) != Some("input_image") {
        return Ok(None);
    }
    let media_type = part
        .get("media_type")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Error::Provider(format!("{provider} image input omitted media_type").into())
        })?;
    let data = part
        .get("data")
        .and_then(Value::as_str)
        .filter(|data| !data.is_empty())
        .ok_or_else(|| Error::Provider(format!("{provider} image input omitted data").into()))?;
    let Some(subtype) = media_type.strip_prefix("image/") else {
        return Err(Error::Provider(
            format!("{provider} image input requires an image media type").into(),
        ));
    };
    if subtype.is_empty()
        || !subtype.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                )
        })
    {
        return Err(Error::Provider(
            format!("{provider} image input has an invalid media type").into(),
        ));
    }
    Ok(Some((media_type, data)))
}

pub(crate) fn image_data_url(media_type: &str, data: &str) -> String {
    format!("data:{media_type};base64,{data}")
}

pub(super) struct ImageDataUrl<'a>(pub(super) &'a str, pub(super) &'a str);

impl Serialize for ImageDataUrl<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("data:{};base64,{}", self.0, self.1))
    }
}

fn validate_usage(usage: &TokenUsage) -> Result<()> {
    if [
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.cache_write_input_tokens,
        usage.output_tokens,
        usage.reasoning_output_tokens,
        usage.total_tokens,
    ]
    .into_iter()
    .any(|tokens| tokens < 0)
    {
        return Err(Error::Provider(
            "model returned negative token usage".into(),
        ));
    }
    Ok(())
}

pub(super) fn usage_i64(
    usage: Option<&Value>,
    pointer: &str,
    provider: &str,
) -> Result<Option<i64>> {
    let Some(usage) = usage else {
        return Ok(None);
    };
    if !usage.is_object() {
        return Err(Error::Provider(
            format!("{provider} usage was not an object").into(),
        ));
    }
    let Some(value) = usage.pointer(pointer) else {
        return Ok(None);
    };
    value.as_i64().map(Some).ok_or_else(|| {
        Error::Provider(format!("{provider} usage field `{pointer}` was not an integer").into())
    })
}

pub(super) fn decode_tool_call(item: &Value) -> Result<ToolCall> {
    let call_id = required_output_string(item, "call_id", MAX_TOOL_CALL_ID_BYTES)?;
    let name = required_output_string(item, "name", MAX_TOOL_NAME_BYTES)?;
    let encoded = required_output_string(item, "arguments", MAX_TOOL_ARGUMENT_BYTES)?;
    let arguments: Value = serde_json::from_str(encoded)?;
    if !arguments.is_object() {
        return Err(Error::Provider(
            format!("tool call `{call_id}` arguments must be a JSON object").into(),
        ));
    }
    let call = ToolCall {
        call_id: call_id.to_string(),
        name: name.to_string(),
        arguments,
    };
    call.validate()?;
    Ok(call)
}

fn required_output_string<'a>(item: &'a Value, field: &str, limit: usize) -> Result<&'a str> {
    let value = item
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Error::Provider(format!("function call omitted {field}").into()))?;
    if value.len() > limit {
        return Err(Error::Provider(
            format!("function call {field} exceeded size limit").into(),
        ));
    }
    Ok(value)
}

fn ensure_output_size(output: &[Value]) -> Result<()> {
    let mut writer = SizeWriter::new(MAX_MODEL_OUTPUT_BYTES);
    match serde_json::to_writer(&mut writer, output) {
        Ok(()) => Ok(()),
        Err(_) if writer.exceeded => {
            Err(Error::Provider("model output exceeded size limit".into()))
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_provider_output(output: &[Value]) -> Result<()> {
    if output
        .iter()
        .any(|item| item.get(crate::middleware::delivery_once::FIELD).is_some())
    {
        return Err(Error::Provider(
            "provider returned an internal guidance receipt".into(),
        ));
    }
    if output
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some(TOOL_LOAD_MARKER))
    {
        return Err(Error::Provider(
            "provider returned an internal tool-load control item".into(),
        ));
    }
    ensure_output_size(output)
}

struct SizeWriter {
    bytes: usize,
    limit: usize,
    exceeded: bool,
}

impl SizeWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: 0,
            limit,
            exceeded: false,
        }
    }
}

impl Write for SizeWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.bytes.saturating_add(buffer.len()) > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("size limit exceeded"));
        }
        self.bytes += buffer.len();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Creates a Responses API user-message item.
#[must_use]
pub fn user_message(text: &str) -> Value {
    serde_json::json!({
        "role": "user",
        "content": [{"type": "input_text", "text": text}]
    })
}

/// Creates a durable user message carrying opaque uploaded-file references.
/// # Errors
///
/// Returns an error if validation or an operation required by this function fails.
pub fn user_message_with_attachments(
    text: &str,
    attachments: &[SessionFileReference],
) -> Result<Value> {
    let mut message = user_message(text);
    if !attachments.is_empty() {
        message[ATTACHMENTS_FIELD] = serde_json::to_value(attachments)?;
    }
    Ok(message)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageText {
    session: String,
    session_turn: String,
    session_steer: String,
    external: String,
}

crate::embedded_config! {
    static MESSAGE_TEXT: MessageText = include_str!("message.toml");
}

/// Creates provider-neutral model input carrying one typed conversation message.
pub(crate) fn message_input(event: &MessageEvent) -> Result<Value> {
    let text = event.reply.as_ref().map_or_else(
        || Cow::Borrowed(event.text.as_str()),
        |reply| {
            Cow::Owned(format!(
                "Replying to this earlier message:\n\n> {}\n\n{}",
                reply.text.replace('\n', "\n> "),
                event.text
            ))
        },
    );
    let mut input = match &event.author {
        MessageAuthor::User => user_message_with_attachments(&text, &event.attachments)?,
        MessageAuthor::Source { source, handle, .. } => {
            let instruction = match source {
                MessageSource::Session { .. } => Cow::Owned(format!(
                    "{} {}",
                    MESSAGE_TEXT.session,
                    match event.delivery {
                        crate::protocol::MessageDelivery::Turn
                        | crate::protocol::MessageDelivery::Queue => &MESSAGE_TEXT.session_turn,
                        crate::protocol::MessageDelivery::Steer => &MESSAGE_TEXT.session_steer,
                    }
                )),
                MessageSource::External { .. } => Cow::Borrowed(MESSAGE_TEXT.external.as_str()),
            };
            internal_user_message(
                "message_advisory",
                &format!("{}\n\n{text}", instruction.replace("{handle}", handle)),
            )
        }
    };
    input[MESSAGE_METADATA_FIELD] = serde_json::to_value(event)?;
    Ok(input)
}

pub(crate) fn has_prompt_cache_breakpoint(input: ModelInput<'_>) -> bool {
    input.iter().any(|item| {
        crate::protocol::content_parts(item).is_some_and(|content| {
            content.iter().any(|part| {
                part.get(PROMPT_CACHE_BREAKPOINT_FIELD)
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
        })
    })
}

pub(crate) fn mark_prompt_cache_breakpoint(item: &mut Value) -> bool {
    let Some(content) = crate::protocol::content_parts_mut(item) else {
        return false;
    };
    let Some(part) = content.iter_mut().rev().find(|part| {
        matches!(
            part.get("type").and_then(Value::as_str),
            Some("input_text" | "input_image")
        )
    }) else {
        return false;
    };
    part[PROMPT_CACHE_BREAKPOINT_FIELD] = Value::Bool(true);
    true
}

pub(crate) fn reset_prompt_cache_breakpoint(input: &mut [Value]) {
    for item in input.iter_mut() {
        let Some(content) = crate::protocol::content_parts_mut(item) else {
            continue;
        };
        for part in content {
            if let Some(fields) = part.as_object_mut() {
                fields.remove(PROMPT_CACHE_BREAKPOINT_FIELD);
            }
        }
    }
    for item in input.iter_mut().rev() {
        if mark_prompt_cache_breakpoint(item) {
            break;
        }
    }
}

/// Repositions cache markers without copying untouched shared history.
pub(crate) fn reset_shared_prompt_cache_breakpoint(input: &mut [Arc<Value>]) {
    let endpoint = input
        .iter()
        .enumerate()
        .rev()
        .find_map(|(item_index, item)| {
            crate::protocol::content_parts(item)?
                .iter()
                .rposition(|part| {
                    matches!(
                        part.get("type").and_then(Value::as_str),
                        Some("input_text" | "input_image")
                    )
                })
                .map(|part_index| (item_index, part_index))
        });
    for (item_index, item) in input.iter_mut().enumerate() {
        let Some(content) = crate::protocol::content_parts(item) else {
            continue;
        };
        let changed = content.iter().enumerate().any(|(part_index, part)| {
            let marker = part.get(PROMPT_CACHE_BREAKPOINT_FIELD);
            if endpoint == Some((item_index, part_index)) {
                marker != Some(&Value::Bool(true))
            } else {
                marker.is_some()
            }
        });
        if !changed {
            continue;
        }
        if let Some(content) = crate::protocol::content_parts_mut(Arc::make_mut(item)) {
            for (part_index, part) in content.iter_mut().enumerate() {
                if endpoint == Some((item_index, part_index)) {
                    part[PROMPT_CACHE_BREAKPOINT_FIELD] = Value::Bool(true);
                } else if let Some(fields) = part.as_object_mut() {
                    fields.remove(PROMPT_CACHE_BREAKPOINT_FIELD);
                }
            }
        }
    }
}

pub(crate) fn internal_user_message(kind: &str, text: &str) -> Value {
    let mut message = user_message(text);
    message[INTERNAL_MESSAGE_FIELD] = Value::String(kind.into());
    message
}

pub(crate) fn durable_visible_message_index(
    output: ModelInput<'_>,
    context: ModelInput<'_>,
    context_before: usize,
) -> Option<usize> {
    let index = output.iter().rposition(has_visible_output_text)?;
    let boundary = context_before.checked_add(index)?.checked_add(1)?;
    crate::protocol::tool_complete_boundaries(context.iter())
        .binary_search(&boundary)
        .is_ok()
        .then_some(index)
}

pub(crate) fn insert_before_open_tool_calls(output: &mut Vec<Value>, input: Vec<Value>) {
    if input.is_empty() {
        return;
    }
    let boundary = crate::protocol::tool_complete_boundaries(output.iter())
        .last()
        .copied()
        .unwrap_or_default();
    output.splice(boundary..boundary, input);
}

fn has_visible_output_text(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("message")
        && item.get("role").and_then(Value::as_str) == Some("assistant")
        && item.get("phase").and_then(Value::as_str) != Some("commentary")
        && item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|part| {
                part.get("type").and_then(Value::as_str) == Some("output_text")
                    && part
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
            })
}

/// Creates a Responses API function-call-output item.
#[must_use]
pub fn tool_output(call_id: &str, output: &crate::protocol::ToolContent, is_error: bool) -> Value {
    let mut value = serde_json::json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": output
    });
    value[TOOL_ERROR_FIELD] = Value::Bool(is_error);
    value
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
