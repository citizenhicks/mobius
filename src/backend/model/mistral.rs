//! Native Mistral Chat Completions provider.

use std::borrow::Cow;

use serde::ser::SerializeSeq as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::chat_completions::{
    ChatCompletions, ChatStream, Dialect, MessageSource, ToolCalls, WireContent, content_text,
    decode_usage,
};
use super::provider::ProviderDefinition;
use super::transport::SseHandler;
use super::{ModelEventSink, ModelOutput, REPLAY_REASONING_FIELD};
use crate::protocol::{ModelEvent, TokenUsage};
use crate::{Error, Result};

pub(super) static MANIFEST: std::sync::LazyLock<super::provider::ProviderMetadata> =
    std::sync::LazyLock::new(|| crate::config::embedded(include_str!("mistral_provider.toml")));
pub(super) static CATALOG: std::sync::LazyLock<super::provider::ModelCatalog> =
    std::sync::LazyLock::new(|| {
        super::provider::ModelCatalog::load("mistral.toml", include_str!("mistral.toml"))
    });

const RAW_THINKING: &str = "_mistral_thinking";
const MAX_THINKING_BLOCKS: usize = 1_024;

/// Mistral's native Chat Completions transport.
///
/// # Examples
///
/// ```
/// use mobius::backend::model::{mistral::Mistral, provider::BaseUrl};
///
/// let base_url = BaseUrl::new("https://api.mistral.ai/v1")?;
/// let provider = Mistral::new("api-key", base_url, "mistral-small-2603")?
///     .with_reasoning_effort("high")?;
/// # Ok::<(), mobius::Error>(())
/// ```
pub type Mistral = ChatCompletions<MistralDialect>;

/// Mistral's Chat Completions wire dialect: typed thinking blocks and tool-result images.
#[derive(Debug)]
pub struct MistralDialect;

impl Dialect for MistralDialect {
    const NAME: &'static str = "Mistral";
    const ID: &'static str = "mistral";
    const STREAM_USAGE: bool = false;
    const TOOL_IMAGES: bool = true;
    type Message<'a> = MistralMessage<'a>;
    type Stream = MistralStream;

    fn message(source: MessageSource<'_>) -> Result<MistralMessage<'_>> {
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
}

/// Mistral message fields.
#[derive(Default, Serialize)]
pub(super) struct MistralMessage<'a> {
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

/// Accumulates one Mistral stream.
#[derive(Default)]
pub(super) struct MistralStream {
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

impl SseHandler for MistralStream {
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

impl MistralStream {
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
                            .await?;
                        }
                        Some("thinking") => {
                            self.append_thinking(serde_json::from_value(part)?, events)
                                .await?;
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
            .ok_or_else(|| Error::Provider("Mistral thinking block was not recorded".into()))?;
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
}

impl ChatStream for MistralStream {
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
                super::set_field(
                    &mut message,
                    REPLAY_REASONING_FIELD,
                    Value::String(self.reasoning),
                );
            }
            if !self.thinking.is_empty() {
                super::set_field(
                    &mut message,
                    RAW_THINKING,
                    serde_json::to_value(self.thinking)?,
                );
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
        super::chat_completions::build::<MistralDialect>,
    )
    .with_image_input()
    .with_credentialless_endpoints()
    .with_replay_reasoning_field(RAW_THINKING)
}

#[cfg(test)]
#[path = "mistral_tests.rs"]
mod tests;
