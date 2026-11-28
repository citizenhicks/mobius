//! Configurable Responses provider registration.

use super::Model;
use super::provider::{BaseUrl, ProviderBuildConfig, ProviderDefinition};
use crate::Result;
use crate::protocol::ToolDiscoveryMode;

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
    .with_voice_models(
        &super::realtime::OPENAI_MODELS,
        super::realtime::VoiceApi::OpenAi,
    )
    .with_credentialless_endpoints()
}

fn build_generic(
    config: ProviderBuildConfig,
    base_url: BaseUrl,
    tool_discovery: ToolDiscoveryMode,
) -> Result<std::sync::Arc<dyn Model>> {
    let api_key = config.credential.into_optional_api_key("responses")?;
    let native_voice = config.capability == Some(crate::protocol::ModelCapability::RealtimeVoice)
        || generic_provider().supports_at(
            crate::protocol::ModelCapability::RealtimeVoice,
            Some(base_url.as_str()),
        );
    let provider = OpenAi::from_api_key(
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
    Ok(std::sync::Arc::new(
        provider.with_selected_effort(config.reasoning_effort)?,
    ))
}
