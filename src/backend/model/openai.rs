//! Configurable Responses provider registration.

use super::Model;
use super::provider::{ProviderBuildConfig, ProviderDefinition};
use crate::{Error, Result};

pub use super::responses::OpenAi;

pub(super) static MANIFEST: std::sync::LazyLock<super::provider::ProviderMetadata> =
    std::sync::LazyLock::new(|| crate::config::embedded(include_str!("openai_provider.toml")));
pub(super) fn generic_provider() -> ProviderDefinition {
    ProviderDefinition::from_metadata(
        "responses",
        &MANIFEST,
        MANIFEST.api_key_auth(),
        None,
        build_generic,
    )
    .with_image_input()
    .with_image_generation()
    .with_realtime_voices(&super::realtime::VOICES, &super::realtime::OPENAI_MODELS)
    .with_credentialless_endpoints()
}

fn build_generic(config: ProviderBuildConfig) -> Result<std::sync::Arc<dyn Model>> {
    let tool_discovery = config.tool_discovery.unwrap_or_else(|| {
        generic_provider().tool_discovery(&config.model, config.base_url.as_deref())
    });
    let base_url = config
        .base_url
        .ok_or_else(|| Error::Config("Responses provider requires a base URL".into()))?;
    let api_key = config.credential.into_optional_api_key("responses")?;
    let native_voice = config.capability == Some(crate::protocol::ModelCapability::RealtimeVoice)
        || generic_provider().supports_at(
            crate::protocol::ModelCapability::RealtimeVoice,
            Some(&base_url),
        );
    let provider = OpenAi::with_client(
        api_key,
        base_url,
        config.model,
        config.http,
        config.transport,
    )?
    .with_tool_discovery(tool_discovery)
    .with_service_tier(config.service_tier);
    let provider = if native_voice {
        provider.with_openai_realtime_voice()?
    } else {
        provider
    };
    let provider = match config.reasoning_effort {
        Some(effort) => provider.with_reasoning_effort(effort)?,
        None => provider,
    };
    Ok(std::sync::Arc::new(provider))
}
