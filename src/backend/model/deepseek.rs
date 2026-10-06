//! DeepSeek Responses provider.

use std::sync::Arc;

use super::Model;
use super::openai::OpenAi;
use super::provider::ProviderBuildConfig;
use super::provider::ProviderDefinition;
use crate::Error;
use crate::Result;

pub(super) static MANIFEST: std::sync::LazyLock<super::provider::ProviderMetadata> =
    std::sync::LazyLock::new(|| crate::config::embedded(include_str!("deepseek_provider.toml")));
pub(super) static CATALOG: std::sync::LazyLock<super::provider::ModelCatalog> =
    std::sync::LazyLock::new(|| {
        crate::config::overridable(
            "deepseek.toml",
            include_str!("deepseek.toml"),
            super::provider::ModelCatalog::validate,
        )
    });

pub(super) fn provider() -> ProviderDefinition {
    ProviderDefinition::from_metadata(
        "deepseek",
        &MANIFEST,
        MANIFEST.api_key_auth(),
        Some(&CATALOG),
        build_provider,
    )
    .with_credentialless_endpoints()
}

fn build_provider(config: ProviderBuildConfig) -> Result<Arc<dyn Model>> {
    let base_url = config
        .base_url
        .ok_or_else(|| Error::Config("DeepSeek requires a base URL".into()))?;
    let api_key = config.credential.into_optional_api_key("deepseek")?;
    let provider = OpenAi::with_client(
        api_key,
        base_url,
        config.model,
        config.http,
        config.transport,
    )?
    .with_service_tier(config.service_tier)
    .without_image_input()
    .without_realtime_voice()
    .with_image_api(None);
    let provider = match config.reasoning_effort {
        Some(effort) => provider.with_reasoning_effort(effort)?,
        None => provider,
    };
    Ok(Arc::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::model::provider::HostedWebSearch;
    use crate::backend::model::provider::ProviderAuth;
    use crate::backend::model::provider::ProviderCredential;
    use crate::backend::model::provider::provider as registered_provider;
    use crate::protocol::ModelCapability;

    #[test]
    fn advertised_web_search_modes_build() {
        let definition = provider();
        for web_search in definition.web_search().iter().copied() {
            definition
                .build(ProviderBuildConfig {
                    credential: ProviderCredential::ApiKey("test-key".into()),
                    model: definition.default_model().expect("default model").into(),
                    base_url: Some(MANIFEST.base_url.as_str().into()),
                    reasoning_effort: None,
                    service_tier: None,
                    web_search,
                    http: reqwest::Client::new(),
                    transport: crate::backend::model::ModelTransportSettings::default(),
                })
                .expect("advertised web search mode builds");
        }
    }

    #[test]
    fn registered_provider_keeps_declared_capabilities_on_each_endpoint() {
        let definition = registered_provider("deepseek").expect("registered provider");
        assert!(matches!(
            definition.auth(),
            ProviderAuth::ApiKey(key) if Some(key) == MANIFEST.credential_env.as_deref()
        ));
        assert_eq!(definition.models(), &CATALOG.models);
        assert_eq!(definition.web_search(), &MANIFEST.search);
        for base_url in [MANIFEST.base_url.as_str(), "https://custom.example/v1"] {
            let model = definition
                .build(ProviderBuildConfig {
                    credential: ProviderCredential::ApiKey("test-key".into()),
                    model: definition.default_model().expect("default model").into(),
                    base_url: Some(base_url.into()),
                    reasoning_effort: None,
                    service_tier: None,
                    web_search: HostedWebSearch::Off,
                    http: reqwest::Client::new(),
                    transport: crate::backend::model::ModelTransportSettings::default(),
                })
                .expect("build provider");

            assert_eq!(model.info().reasoning_effort.as_deref(), Some("high"));
            assert_eq!(
                (
                    model.supports_image_input(),
                    model.supports_image_generation(),
                    model.supports_realtime_voice(),
                ),
                (
                    definition.supports_image_input(),
                    definition.supports(ModelCapability::ImageGeneration),
                    definition.supports(ModelCapability::RealtimeVoice),
                ),
                "{base_url} capabilities"
            );
        }
    }
}
