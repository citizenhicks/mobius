//! Native Mistral Chat Completions provider.

use std::borrow::Cow;
use std::sync::Arc;

use super::transport::ApiKeyModel;
use reqwest::Client;
use serde::ser::SerializeSeq as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::chat_completions::{
    MessageSource, RequestBody, ToolCalls, WireContent, content_text, decode_usage,
};
use super::provider::{ProviderBuildConfig, ProviderDefinition};
use super::transport::{SseHandler, streaming_client};
use super::{
    Model, ModelEventSink, ModelOutput, ModelRequest, PromptCacheMode, REPLAY_REASONING_FIELD,
};
use crate::protocol::{ModelEvent, ModelInfo, TokenUsage};
use crate::{BoxFuture, Error, Result};

pub(super) static MANIFEST: std::sync::LazyLock<super::provider::ProviderMetadata> =
    std::sync::LazyLock::new(|| crate::config::embedded(include_str!("mistral_provider.toml")));
pub(super) static CATALOG: std::sync::LazyLock<super::provider::ModelCatalog> =
    std::sync::LazyLock::new(|| {
        crate::config::overridable(
            "mistral.toml",
            include_str!("mistral.toml"),
            super::provider::ModelCatalog::validate,
        )
    });

const RAW_THINKING: &str = "_mistral_thinking";
const MAX_THINKING_BLOCKS: usize = 1_024;

/// Mistral's native Chat Completions transport.
///
/// # Examples
///
/// ```
/// use mobius::backend::model::mistral::Mistral;
///
/// let provider = Mistral::new("api-key", "https://api.mistral.ai/v1", "mistral-small-2603")?
///     .with_reasoning_effort("high")?;
/// # Ok::<(), mobius::Error>(())
/// ```
#[derive(Debug)]
pub struct Mistral {
    config: ApiKeyModel,
}

impl Mistral {
    /// Creates a provider for a Mistral API root or compatible endpoint.
    /// # Errors
    /// Returns invalid credentials, endpoint or model configuration.
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self> {
        Self::with_client(Some(api_key.into()), base_url, model, streaming_client()?)
    }

    /// Creates a provider with explicit HTTP and retry policy.
    /// # Errors
    /// Returns invalid provider configuration or transport policy.
    pub fn new_with_transport(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        transport: super::ModelTransportSettings,
    ) -> Result<Self> {
        Self::with_client(
            Some(api_key.into()),
            base_url,
            model,
            transport.streaming_client()?,
        )
        .map(|mut provider| {
            provider.config.transport = transport;
            provider
        })
    }

    fn with_client(
        api_key: Option<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        client: Client,
    ) -> Result<Self> {
        let config = ApiKeyModel::new(api_key, base_url, model, client)?;
        Ok(Self { config })
    }

    /// Selects an operator-configured reasoning effort.
    /// # Errors
    /// Returns an error when the effort is empty.
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Result<Self> {
        self.config.set_reasoning(effort)?;
        Ok(self)
    }

    fn request_body<'a>(
        &'a self,
        request: ModelRequest<'a>,
    ) -> Result<RequestBody<'a, MistralMessage<'a>>> {
        RequestBody::new(
            &self.config.model,
            self.config.reasoning_effort.as_deref(),
            request,
            "Mistral",
            message_content,
        )
    }
}
impl Mistral {
    async fn send_response(
        &self,
        request: ModelRequest<'_>,
        events: ModelEventSink,
        media: Option<super::MediaPreparation<'_>>,
    ) -> Result<ModelOutput> {
        let body = super::media::encode_request(self, request, media, true, |input| {
            Ok(serde_json::to_vec(
                &self.request_body(ModelRequest { input, ..request })?,
            )?)
        })
        .await?;
        let response = super::chat_completions::post(
            &self.config.client,
            &self.config.base_url,
            self.config.api_key.as_deref(),
            body,
            "Mistral",
        )
        .await?;
        let mut stream = StreamState::default();
        super::transport::read_sse(response, "Mistral", &events, &mut stream).await?;
        stream.finish()
    }
}

impl Model for Mistral {
    fn transport_settings(&self) -> super::ModelTransportSettings {
        self.config.transport
    }
    fn info(&self) -> ModelInfo {
        self.config.info()
    }
    fn supports_image_input(&self) -> bool {
        true
    }
    fn supports_tool_image_input(&self) -> bool {
        true
    }
    fn prompt_cache_capability(&self) -> PromptCacheMode {
        PromptCacheMode::Implicit
    }
    fn request_size(&self, request: ModelRequest<'_>) -> Result<usize> {
        super::media::serialized_size(&self.request_body(request)?)
    }
    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(self.send_response(request, events, None))
    }
    fn respond_prepared<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
        media: super::MediaPreparation<'a>,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(self.send_response(request, events, Some(media)))
    }
}

#[derive(Default, Serialize)]
struct MistralMessage<'a> {
    content: MessageContent<'a>,
}

#[derive(Default, Serialize)]
#[serde(untagged)]
enum MessageContent<'a> {
    Neutral(WireContent<'a>),
    Text(Cow<'a, str>),
    Thinking(ThinkingContent<'a>),
    #[default]
    Empty,
}

fn message_content(source: MessageSource<'_>) -> Result<MistralMessage<'_>> {
    let content = match source {
        MessageSource::System(text) => MessageContent::Text(Cow::Borrowed(text)),
        MessageSource::Tool(output) => {
            MessageContent::Neutral(WireContent::tool_result(output, "Mistral")?)
        }
        MessageSource::History { role, item } => {
            if role == "assistant"
                && let Some(blocks) = item.get(RAW_THINKING).and_then(Value::as_array)
            {
                MessageContent::Thinking(ThinkingContent {
                    blocks,
                    text: content_text(item.get("content")),
                })
            } else {
                MessageContent::Neutral(WireContent::new(item.get("content"), "Mistral")?)
            }
        }
    };
    Ok(MistralMessage { content })
}

struct ThinkingContent<'a> {
    blocks: &'a [Value],
    text: Cow<'a, str>,
}

impl Serialize for ThinkingContent<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Text<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            text: &'a str,
        }
        let mut sequence = serializer
            .serialize_seq(Some(self.blocks.len() + usize::from(!self.text.is_empty())))?;
        for block in self.blocks {
            sequence.serialize_element(block)?;
        }
        if !self.text.is_empty() {
            sequence.serialize_element(&Text {
                kind: "text",
                text: &self.text,
            })?;
        }
        sequence.end()
    }
}

#[derive(Default)]
struct StreamState {
    text: String,
    reasoning: String,
    thinking: Vec<ThinkingBlock>,
    tools: ToolCalls,
    usage: TokenUsage,
    done: bool,
}

#[derive(Default, Serialize)]
struct ThinkingBlock {
    #[serde(rename = "type")]
    kind: &'static str,
    thinking: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    closed: Option<bool>,
    #[serde(skip)]
    finished: bool,
}

#[derive(Deserialize)]
struct ThinkingChunk {
    thinking: Vec<Value>,
    signature: Option<String>,
    closed: Option<bool>,
}

impl SseHandler for StreamState {
    async fn apply_data(&mut self, data: &str, events: &ModelEventSink) -> Result<()> {
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let mut chunk: Value = serde_json::from_str(data)?;
        if let Some(message) = chunk.pointer("/error/message").and_then(Value::as_str) {
            return Err(Error::Provider(
                format!("Mistral stream error: {message}").into(),
            ));
        }
        if let Some(usage) = chunk.get("usage") {
            self.usage = decode_usage(Some(usage), "Mistral")?;
        }
        if chunk
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            == Some("error")
        {
            return Err(Error::Provider("Mistral generation failed".into()));
        }
        let Some(delta) = chunk.pointer_mut("/choices/0/delta") else {
            return Ok(());
        };
        self.tools.append(delta, "Mistral")?;
        if let Some(content) = delta.get_mut("content") {
            self.append_content(content.take(), events).await?;
        }
        Ok(())
    }
}

impl StreamState {
    async fn append_content(&mut self, content: Value, events: &ModelEventSink) -> Result<()> {
        match content {
            Value::String(text) => self.append_text(&text, events).await?,
            Value::Array(parts) => {
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            self.append_text(
                                part.get("text").and_then(Value::as_str).ok_or_else(|| {
                                    Error::Provider("Mistral text chunk omitted text".into())
                                })?,
                                events,
                            )
                            .await?
                        }
                        Some("thinking") => {
                            self.append_thinking(serde_json::from_value(part)?, events)
                                .await?
                        }
                        kind => {
                            return Err(Error::Provider(
                                format!("unsupported Mistral content chunk {kind:?}").into(),
                            ));
                        }
                    }
                }
            }
            Value::Null => {}
            _ => {
                return Err(Error::Provider(
                    "Mistral content was not text or a chunk list".into(),
                ));
            }
        }
        Ok(())
    }

    async fn append_text(&mut self, text: &str, events: &ModelEventSink) -> Result<()> {
        if !text.is_empty() {
            if let Some(block) = self.thinking.last_mut() {
                block.finished = true;
            }
            self.text.push_str(text);
            events(ModelEvent::TextDelta(text.into())).await?;
        }
        Ok(())
    }

    async fn append_thinking(
        &mut self,
        chunk: ThinkingChunk,
        events: &ModelEventSink,
    ) -> Result<()> {
        if self.thinking.last().is_none_or(|block| block.finished) {
            if self.thinking.len() >= MAX_THINKING_BLOCKS {
                return Err(Error::Provider(
                    "Mistral returned too many thinking blocks".into(),
                ));
            }
            self.thinking.push(ThinkingBlock {
                kind: "thinking",
                ..ThinkingBlock::default()
            });
        }
        let block = self
            .thinking
            .last_mut()
            .expect("thinking block was just inserted");
        for part in chunk.thinking {
            if part.get("type").and_then(Value::as_str) == Some("text") {
                let text = part
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Provider("Mistral thinking text omitted text".into()))?;
                self.reasoning.push_str(text);
                if !text.is_empty() {
                    events(ModelEvent::ReasoningDelta(text.into())).await?;
                }
                if let Some(Value::String(previous)) = block
                    .thinking
                    .last_mut()
                    .filter(|previous| previous.get("type").and_then(Value::as_str) == Some("text"))
                    .and_then(|previous| previous.get_mut("text"))
                {
                    previous.push_str(text);
                    continue;
                }
            }
            block.thinking.push(part);
        }
        if let Some(signature) = chunk.signature {
            block.signature.get_or_insert_default().push_str(&signature);
        }
        if let Some(closed) = chunk.closed {
            block.closed = Some(closed);
            block.finished = closed;
        }
        Ok(())
    }

    fn finish(self) -> Result<ModelOutput> {
        if !self.done {
            return Err(Error::Provider(
                "Mistral stream ended before the [DONE] event".into(),
            ));
        }
        let calls = self.tools.finish("Mistral")?;
        let end_turn = calls.is_empty();
        let mut output = Vec::new();
        if !self.text.is_empty() || !self.thinking.is_empty() {
            let mut message = serde_json::json!({
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": self.text}],
            });
            if !self.reasoning.is_empty() {
                message[REPLAY_REASONING_FIELD] = Value::String(self.reasoning);
            }
            if !self.thinking.is_empty() {
                message[RAW_THINKING] = serde_json::to_value(self.thinking)?;
            }
            output.push(message);
        }
        output.extend(calls);
        ModelOutput::from_output(output, end_turn, self.usage)
    }
}

pub(super) fn provider() -> ProviderDefinition {
    ProviderDefinition::from_metadata(
        "mistral",
        &MANIFEST,
        MANIFEST.api_key_auth(),
        Some(&CATALOG),
        build_provider,
    )
    .with_image_input()
    .with_credentialless_endpoints()
    .with_replay_reasoning_field(RAW_THINKING)
}

fn build_provider(config: ProviderBuildConfig) -> Result<Arc<dyn Model>> {
    let base_url = config
        .base_url
        .ok_or_else(|| Error::Config("Mistral requires a base URL".into()))?;
    let api_key = config.credential.into_optional_api_key("mistral")?;
    let mut provider = Mistral::with_client(api_key, base_url, config.model, config.http)?;
    provider.config.transport = config.transport;
    if let Some(effort) = config.reasoning_effort {
        provider = provider.with_reasoning_effort(effort)?;
    }
    Ok(Arc::new(provider))
}

#[cfg(test)]
#[path = "mistral_tests.rs"]
mod tests;
