//! OpenAI API-key provider registration and default endpoint.

use super::Model;
use super::provider::{BaseUrl, ProviderBuildConfig, ProviderDefinition};
use super::responses::CATALOG;
use crate::Result;
use crate::protocol::ToolDiscoveryMode;
use std::sync::Arc;

pub use super::responses_socket::OpenAiSocket;

pub(super) static MANIFEST: std::sync::LazyLock<super::provider::ProviderMetadata> =
    std::sync::LazyLock::new(|| {
        crate::config::embedded(include_str!("openai_socket_provider.toml"))
    });

impl OpenAiSocket {
    /// Creates the first-party Responses transport with HTTP fallback.
    /// # Errors
    ///
    /// Returns an error if configuration is invalid or a required resource cannot be initialized.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        Self::new_with_transport(
            api_key,
            MANIFEST.base_url.share(),
            model,
            super::ModelTransportSettings::default(),
        )
    }
}

pub(super) fn provider() -> ProviderDefinition {
    ProviderDefinition::from_metadata(
        "openai_socket",
        &MANIFEST,
        MANIFEST.api_key_auth(),
        Some(&CATALOG),
        build_provider,
    )
    .with_image_input()
    .with_image_generation()
    .with_voice_models(
        &super::realtime::OPENAI_MODELS,
        super::realtime::VoiceApi::OpenAi,
    )
}

fn build_provider(
    config: ProviderBuildConfig,
    base_url: BaseUrl,
    tool_discovery: ToolDiscoveryMode,
) -> Result<Arc<dyn Model>> {
    let api_key = config.credential.into_api_key("openai_socket")?;
    let provider = OpenAiSocket::from_api_key(
        api_key,
        base_url,
        config.model,
        config.http,
        config.transport,
    )?
    .with_tool_discovery(tool_discovery)
    .with_service_tier(config.service_tier)
    .with_selection(config.reasoning_effort, config.web_search)?;
    Ok(Arc::new(provider))
}
