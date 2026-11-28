//! Native Kimi Chat Completions provider.

use serde::Serialize;
use serde_json::Value;

use super::REPLAY_REASONING_FIELD;
use super::chat_completions::{ChatCompletions, Dialect, MessageSource, TextStream, WireContent};
use super::provider::ProviderDefinition;
use crate::Result;

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
    type Stream = TextStream;

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
