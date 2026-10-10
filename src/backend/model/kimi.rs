//! Native Kimi Chat Completions provider.

use std::sync::Arc;

use super::transport::ApiKeyModel;
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;

use super::Model;
use super::ModelEventSink;
use super::ModelOutput;
use super::ModelRequest;
use super::PromptCacheMode;
use super::REPLAY_REASONING_FIELD;
use super::chat_completions::{MessageSource, RequestBody, StreamOptions, ToolCalls, WireContent};
use super::provider::{ProviderBuildConfig, ProviderDefinition};
use super::transport::{SseHandler, streaming_client};
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::protocol::ModelEvent;
use crate::protocol::ModelInfo;
use crate::protocol::TokenUsage;

pub(super) static MANIFEST: std::sync::LazyLock<super::provider::ProviderMetadata> =
    std::sync::LazyLock::new(|| crate::config::embedded(include_str!("kimi_provider.toml")));
pub(super) static CATALOG: std::sync::LazyLock<super::provider::ModelCatalog> =
    std::sync::LazyLock::new(|| {
        crate::config::overridable(
            "kimi.toml",
            include_str!("kimi.toml"),
            super::provider::ModelCatalog::validate,
        )
    });

/// Kimi's native Chat Completions provider.
#[derive(Debug)]
pub struct Kimi {
    config: ApiKeyModel,
}

#[derive(Default, Serialize)]
struct KimiMessage<'a> {
    content: Option<WireContent<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<&'a str>,
}

fn message_content(source: MessageSource<'_>) -> Result<KimiMessage<'_>> {
    let mut reasoning_content = None;
    let content = match source {
        MessageSource::System(text) => WireContent::Text(text.into()),
        MessageSource::Tool(output) => {
            WireContent::Text(super::chat_completions::tool_text(output)?)
        }
        MessageSource::History { role, item } => {
            if role == "assistant" {
                reasoning_content = item
                    .get(REPLAY_REASONING_FIELD)
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty());
            }
            WireContent::new(item.get("content"), "Kimi")?
        }
    };
    Ok(KimiMessage {
        content: Some(content),
        reasoning_content,
    })
}

impl Kimi {
    /// Creates a provider for a Moonshot Kimi endpoint.
    /// # Errors
    ///
    /// Returns an error if configuration is invalid or a required resource cannot be initialized.
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self> {
        Self::with_client(Some(api_key.into()), base_url, model, streaming_client()?)
    }

    /// Creates a provider with explicit HTTP, socket and retry policy.
    /// # Errors
    /// Returns invalid provider policy or HTTP client construction errors.
    pub fn new_with_transport(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        settings: super::ModelTransportSettings,
    ) -> Result<Self> {
        Self::with_client(
            Some(api_key.into()),
            base_url,
            model,
            settings.streaming_client()?,
        )?
        .with_transport_settings(settings)
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

    /// Applies validated operational transport policy.
    /// # Errors
    /// Returns an error for invalid operational settings.
    pub fn with_transport_settings(
        mut self,
        settings: super::ModelTransportSettings,
    ) -> Result<Self> {
        settings.validate()?;
        self.config.transport = settings;
        Ok(self)
    }

    /// Selects an operator-configured effort for this Kimi model.
    /// # Errors
    /// Returns an error when the effort is empty.
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Result<Self> {
        self.config.set_reasoning(effort)?;
        Ok(self)
    }

    async fn send_response(
        &self,
        request: ModelRequest<'_>,
        events: ModelEventSink,
        media: Option<super::MediaPreparation<'_>>,
    ) -> Result<ModelOutput> {
        let body = super::media::encode_request(self, request, media, true, |input| {
            Ok(serde_json::to_vec(
                &self.request_body(&ModelRequest { input, ..request })?,
            )?)
        })
        .await?;
        let response = super::chat_completions::post(
            &self.config.client,
            &self.config.base_url,
            self.config.api_key.as_deref(),
            body,
            "Kimi",
        )
        .await?;
        let mut stream = StreamState::default();
        super::transport::read_sse(response, "Kimi", &events, &mut stream).await?;
        stream.finish()
    }

    fn request_body<'a>(
        &'a self,
        request: &ModelRequest<'a>,
    ) -> Result<RequestBody<'a, KimiMessage<'a>>> {
        let mut body = RequestBody::new(
            &self.config.model,
            self.config.reasoning_effort.as_deref(),
            *request,
            "Kimi",
            message_content,
        )?;
        body.stream_options = Some(StreamOptions {
            include_usage: true,
        });
        Ok(body)
    }

    #[cfg(test)]
    fn request_body_value(&self, request: &ModelRequest<'_>) -> Result<Value> {
        Ok(serde_json::to_value(self.request_body(request)?)?)
    }
}

impl Model for Kimi {
    fn transport_settings(&self) -> super::ModelTransportSettings {
        self.config.transport
    }

    fn info(&self) -> ModelInfo {
        self.config.info()
    }

    fn supports_image_input(&self) -> bool {
        true
    }

    fn prompt_cache_capability(&self) -> PromptCacheMode {
        PromptCacheMode::Implicit
    }

    fn respond_prepared<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
        media: super::MediaPreparation<'a>,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(self.send_response(request, events, Some(media)))
    }

    fn request_size(&self, request: ModelRequest<'_>) -> Result<usize> {
        super::media::serialized_size(&self.request_body(&request)?)
    }

    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(self.send_response(request, events, None))
    }
}

#[derive(Default)]
struct StreamState {
    text: String,
    reasoning: String,
    tools: ToolCalls,
    usage: TokenUsage,
    done: bool,
}

impl SseHandler for StreamState {
    async fn apply_data(&mut self, data: &str, events: &ModelEventSink) -> Result<()> {
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let chunk: Value = serde_json::from_str(data)?;
        if let Some(message) = chunk.pointer("/error/message").and_then(Value::as_str) {
            return Err(Error::Provider(
                format!("Kimi stream error: {message}").into(),
            ));
        }
        if let Some(usage) = chunk.get("usage") {
            self.usage = decode_usage(Some(usage))?;
        }
        let Some(choice) = chunk.pointer("/choices/0") else {
            return Ok(());
        };
        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };
        if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
            self.reasoning.push_str(reasoning);
            if !reasoning.is_empty() {
                events(ModelEvent::ReasoningDelta(reasoning.to_string())).await?;
            }
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            self.text.push_str(text);
            if !text.is_empty() {
                events(ModelEvent::TextDelta(text.to_string())).await?;
            }
        }
        self.tools.append(delta, "Kimi")?;
        Ok(())
    }
}

impl StreamState {
    fn finish(self) -> Result<ModelOutput> {
        if !self.done {
            return Err(Error::Provider(
                "Kimi stream ended before the [DONE] event".into(),
            ));
        }
        let calls = self.tools.finish("Kimi")?;
        let has_tools = !calls.is_empty();
        if self.text.is_empty() && self.reasoning.is_empty() && calls.is_empty() {
            return Err(Error::Provider("Kimi returned no output".into()));
        }
        let mut output = Vec::new();
        if !self.text.is_empty() || !self.reasoning.is_empty() {
            output.push(serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": self.text
                }],
                (REPLAY_REASONING_FIELD): self.reasoning
            }));
        }
        output.extend(calls);
        ModelOutput::from_output(output, !has_tools, self.usage)
    }
}

fn decode_usage(usage: Option<&Value>) -> Result<TokenUsage> {
    super::chat_completions::decode_usage(usage, "Kimi")
}

pub(super) fn provider() -> ProviderDefinition {
    ProviderDefinition::from_metadata(
        "kimi",
        &MANIFEST,
        MANIFEST.api_key_auth(),
        Some(&CATALOG),
        build_provider,
    )
    .with_image_input()
    .with_credentialless_endpoints()
}

fn build_provider(config: ProviderBuildConfig) -> Result<Arc<dyn Model>> {
    let base_url = config
        .base_url
        .ok_or_else(|| Error::Config("Kimi requires a base URL".into()))?;
    let api_key = config.credential.into_optional_api_key("kimi")?;
    let provider = Kimi::with_client(api_key, base_url, config.model, config.http)?
        .with_transport_settings(config.transport)?;
    let provider = match config.reasoning_effort {
        Some(effort) => provider.with_reasoning_effort(effort)?,
        None => provider,
    };
    Ok(Arc::new(provider))
}

#[cfg(test)]
#[path = "kimi_tests.rs"]
mod tests;
