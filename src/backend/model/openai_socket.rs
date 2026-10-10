//! OpenAI API-key provider registration and default endpoint.

use super::Model;
use super::provider::{HostedWebSearch, ProviderBuildConfig, ProviderDefinition};
use super::responses::CATALOG;
use crate::Result;
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
        Self::with_client(
            api_key,
            MANIFEST.base_url.as_str(),
            model,
            super::transport::streaming_client()?,
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
    .with_realtime_voices(&super::realtime::VOICES, &super::realtime::OPENAI_MODELS)
}

fn build_provider(config: ProviderBuildConfig) -> Result<Arc<dyn Model>> {
    let tool_discovery = config
        .tool_discovery
        .unwrap_or_else(|| provider().tool_discovery(&config.model, config.base_url.as_deref()));
    let api_key = config.credential.into_api_key("openai_socket")?;
    let base_url = config
        .base_url
        .as_deref()
        .unwrap_or(MANIFEST.base_url.as_str());
    let provider = OpenAiSocket::with_client(
        api_key,
        base_url,
        config.model,
        config.http,
        config.transport,
    )?
    .with_tool_discovery(tool_discovery)
    .with_service_tier(config.service_tier);
    let provider = match config.reasoning_effort {
        Some(effort) => provider.with_reasoning_effort(effort)?,
        None => provider,
    };
    let provider = match config.web_search {
        HostedWebSearch::Off => provider,
        HostedWebSearch::Cached => provider.with_cached_web_search(),
        HostedWebSearch::Live => provider.with_web_search(),
    };
    Ok(Arc::new(provider))
}
