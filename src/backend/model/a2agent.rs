//! A2Agent Responses provider.

use std::sync::Arc;

use super::Model;
use super::provider::BaseUrl;
use super::provider::ProviderBuildConfig;
use super::provider::ProviderDefinition;
use super::responses::OpenAi;
use crate::Result;
use crate::protocol::ToolDiscoveryMode;

pub(super) static MANIFEST: std::sync::LazyLock<super::provider::ProviderMetadata> =
    std::sync::LazyLock::new(|| crate::config::embedded(include_str!("a2agent_provider.toml")));

pub(super) fn provider() -> ProviderDefinition {
    ProviderDefinition::from_metadata(
        "a2agent",
        &MANIFEST,
        MANIFEST.api_key_auth(),
        None,
        build_provider,
    )
}

fn build_provider(
    config: ProviderBuildConfig,
    base_url: BaseUrl,
    tool_discovery: ToolDiscoveryMode,
) -> Result<Arc<dyn Model>> {
    let api_key = config.credential.into_api_key("a2agent")?;
    let provider = OpenAi::from_api_key(
        Some(api_key),
        base_url,
        config.model,
        config.http,
        config.transport,
    )?
    .with_tool_discovery(tool_discovery)
    .with_image_input(false)
    .with_image_api(None)
    .with_max_output_tokens(MANIFEST.max_output_tokens)
    .with_service_tier(config.service_tier)
    .with_selected_effort(config.reasoning_effort)?;
    Ok(Arc::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::model::provider::HostedWebSearch;
    use crate::backend::model::provider::ProviderCredential;

    #[test]
    fn unverified_capabilities_and_credentialless_access_are_disabled() {
        let definition = provider();
        assert_eq!(definition.web_search(), &[HostedWebSearch::Off]);
        assert!(
            !definition.supports_image_input(),
            "image input is unverified"
        );
        assert!(
            !definition.supports_credentialless_endpoints(),
            "A2Agent requires an API key"
        );
        for credential in [
            ProviderCredential::ApiKey("test-key".into()),
            ProviderCredential::Credentialless,
        ] {
            let requires_key = matches!(credential, ProviderCredential::Credentialless);
            let built = definition.build(ProviderBuildConfig {
                capability: None,
                tool_discovery: None,
                credential,
                model: "fixture-model".into(),
                base_url: None,
                reasoning_effort: None,
                service_tier: None,
                web_search: HostedWebSearch::Off,
                http: reqwest::Client::new(),
                transport: super::super::ModelTransportSettings::default(),
            });
            if requires_key {
                assert!(built.is_err(), "credentialless access must be rejected");
            } else {
                let model = built.expect("API-key provider");
                assert!(
                    !model.supports_image_input(),
                    "transport must reject images"
                );
                assert!(
                    !model.supports_image_generation(),
                    "transport must not inherit image generation"
                );
            }
        }
    }
}
