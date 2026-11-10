//! Native Kimi Chat Completions provider.

use serde::Serialize;
use serde_json::Value;

use super::ModelEventSink;
use super::ModelOutput;
use super::REPLAY_REASONING_FIELD;
use super::chat_completions::{
    ChatCompletions, ChatStream, Dialect, MessageSource, ToolCalls, WireContent,
};
use super::provider::ProviderDefinition;
use super::transport::SseHandler;
use crate::Error;
use crate::Result;
use crate::protocol::ModelEvent;
use crate::protocol::TokenUsage;

pub(super) static MANIFEST: std::sync::LazyLock<super::provider::ProviderMetadata> =
    std::sync::LazyLock::new(|| crate::config::embedded(include_str!("kimi_provider.toml")));
pub(super) static CATALOG: std::sync::LazyLock<super::provider::ModelCatalog> =
    std::sync::LazyLock::new(|| {
        super::provider::ModelCatalog::load("kimi.toml", include_str!("kimi.toml"))
    });

/// Kimi's native Chat Completions provider.
pub type Kimi = ChatCompletions<KimiDialect>;

/// Kimi's Chat Completions wire dialect: reasoning replay and a requested usage chunk.
#[derive(Debug)]
pub struct KimiDialect;

impl Dialect for KimiDialect {
    const NAME: &'static str = "Kimi";
    const ID: &'static str = "kimi";
    const STREAM_USAGE: bool = true;
    const TOOL_IMAGES: bool = false;
    type Message<'a> = KimiMessage<'a>;
    type Stream = KimiStream;

    fn message(source: MessageSource<'_>) -> Result<KimiMessage<'_>> {
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
}

/// Kimi message fields.
#[derive(Default, Serialize)]
pub(super) struct KimiMessage<'a> {
    content: Option<WireContent<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<&'a str>,
}

/// Accumulates one Kimi stream.
#[derive(Default)]
pub(super) struct KimiStream {
    text: String,
    reasoning: String,
    tools: ToolCalls,
    usage: TokenUsage,
    done: bool,
}

impl SseHandler for KimiStream {
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
                events(ModelEvent::ReasoningDelta(reasoning.to_owned())).await?;
            }
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            self.text.push_str(text);
            if !text.is_empty() {
                events(ModelEvent::TextDelta(text.to_owned())).await?;
            }
        }
        self.tools.append(delta, "Kimi")?;
        Ok(())
    }
}

impl ChatStream for KimiStream {
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
        super::chat_completions::build::<KimiDialect>,
    )
    .with_image_input()
    .with_credentialless_endpoints()
}

#[cfg(test)]
#[path = "kimi_tests.rs"]
mod tests;
