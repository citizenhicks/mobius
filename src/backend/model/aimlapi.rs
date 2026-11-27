//! AI/ML API Chat Completions provider.

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use super::Model;
use super::ModelEventSink;
use super::ModelOutput;
use super::chat_completions::{
    ChatCompletions, ChatStream, ChatStreamState, Dialect, MessageSource, WireContent,
};
use super::provider::{BaseUrl, ProviderBuildConfig, ProviderCredential, ProviderDefinition};
use super::transport::SseHandler;
use crate::Error;
use crate::Result;
use crate::protocol::{ModelEvent, ToolDiscoveryMode};

pub(super) static MANIFEST: std::sync::LazyLock<super::provider::ProviderMetadata> =
    std::sync::LazyLock::new(|| crate::config::embedded(include_str!("aimlapi_provider.toml")));
pub(super) static CATALOG: std::sync::LazyLock<super::provider::ModelCatalog> =
    std::sync::LazyLock::new(|| {
        super::provider::ModelCatalog::load("aimlapi.toml", include_str!("aimlapi.toml"))
    });

/// The only host that receives an AI/ML API key.
const API_HOST: &str = "api.aimlapi.com";

/// AI/ML API's Chat Completions provider.
pub type AimlApi = ChatCompletions<AimlApiDialect>;

/// AI/ML API's OpenAI-compatible Chat Completions dialect.
#[derive(Debug)]
pub struct AimlApiDialect;

impl Dialect for AimlApiDialect {
    const NAME: &'static str = "AI/ML API";
    const ID: &'static str = "aimlapi";
    const STREAM_USAGE: bool = true;
    const TOOL_IMAGES: bool = false;
    type Message<'a> = AimlApiMessage<'a>;
    type Stream = AimlApiStream;

    fn message(source: MessageSource<'_>) -> Result<AimlApiMessage<'_>> {
        let content = match source {
            MessageSource::System(text) => WireContent::Text(text.into()),
            MessageSource::Tool(output) => {
                WireContent::Text(super::chat_completions::tool_text(output)?)
            }
            MessageSource::History { item, .. } => {
                WireContent::new(item.get("content"), Self::NAME)?
            }
        };
        Ok(AimlApiMessage {
            content: Some(content),
        })
    }
}

/// AI/ML API message fields.
#[derive(Default, Serialize)]
pub(super) struct AimlApiMessage<'a> {
    content: Option<WireContent<'a>>,
}

/// Accumulates one AI/ML API stream.
#[derive(Default)]
pub(super) struct AimlApiStream {
    text: String,
    stream: ChatStreamState,
}

impl SseHandler for AimlApiStream {
    async fn apply_data(&mut self, data: &str, events: &ModelEventSink) -> Result<()> {
        let Some(chunk) = self.stream.read_chunk(data, AimlApiDialect::NAME)? else {
            return Ok(());
        };
        let Some(delta) = chunk.pointer("/choices/0/delta") else {
            return Ok(());
        };
        if let Some(reasoning) = delta
            .get("reasoning_content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            events(ModelEvent::ReasoningDelta(reasoning.to_owned())).await?;
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            self.text.push_str(text);
            if !text.is_empty() {
                events(ModelEvent::TextDelta(text.to_owned())).await?;
            }
        }
        self.stream.append_tools(delta, AimlApiDialect::NAME)?;
        Ok(())
    }
}

impl ChatStream for AimlApiStream {
    fn finish(self) -> Result<ModelOutput> {
        let (calls, usage) = self.stream.finish(AimlApiDialect::NAME)?;
        let has_tools = !calls.is_empty();
        if self.text.is_empty() && calls.is_empty() {
            return Err(Error::Provider("AI/ML API returned no output".into()));
        }
        let mut output = Vec::new();
        if !self.text.is_empty() {
            output.push(serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": self.text}]
            }));
        }
        output.extend(calls);
        ModelOutput::from_output(output, !has_tools, usage)
    }
}

pub(super) fn provider() -> ProviderDefinition {
    ProviderDefinition::from_metadata(
        "aimlapi",
        &MANIFEST,
        MANIFEST.api_key_auth(),
        Some(&CATALOG),
        build_provider,
    )
    .with_image_input()
}

/// Builds the provider after checking that its key goes only to the AI/ML API host.
fn build_provider(
    config: ProviderBuildConfig,
    base_url: BaseUrl,
    tool_discovery: ToolDiscoveryMode,
) -> Result<Arc<dyn Model>> {
    if matches!(config.credential, ProviderCredential::ApiKey(_))
        && !sends_key_to_api_host(&base_url)
    {
        return Err(Error::Invalid(format!(
            "provider `aimlapi` sends its API key only to https://{API_HOST}"
        )));
    }
    super::chat_completions::build::<AimlApiDialect>(config, base_url, tool_discovery)
}

fn sends_key_to_api_host(base_url: &BaseUrl) -> bool {
    reqwest::Url::parse(base_url.as_str()).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some(API_HOST)
            && url.port_or_known_default() == Some(443)
    })
}

#[cfg(test)]
#[path = "aimlapi_tests.rs"]
mod tests;
