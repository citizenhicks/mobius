//! DeepSeek Responses provider.

use std::sync::Arc;

use super::Model;
use super::provider::BaseUrl;
use super::provider::ProviderBuildConfig;
use super::provider::ProviderDefinition;
use super::responses::OpenAi;
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

fn build_provider(config: ProviderBuildConfig, base_url: BaseUrl) -> Result<Arc<dyn Model>> {
    let image_input = CATALOG.image_input_models.contains(&config.model);
    let tool_discovery = config
        .tool_discovery
        .unwrap_or_else(|| provider().tool_discovery(&config.model, Some(base_url.as_str())));
    let api_key = config.credential.into_optional_api_key("deepseek")?;
    let provider = OpenAi::with_client(
        api_key,
        base_url,
        config.model,
        config.http,
        config.transport,
    )?
    .with_tool_discovery(tool_discovery)
    .with_service_tier(config.service_tier)
    .with_image_input(image_input)
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
    use crate::backend::model::provider::ProviderCredential;

    #[test]
    fn image_input_is_enabled_only_for_catalogued_vision_models() {
        let definition = provider();
        for base_url in [MANIFEST.base_url.as_str(), "https://custom.example/v1"] {
            for (id, supports_images) in [
                ("deepseek-flash", true),
                ("deepseek-v4-pro", false),
                ("custom-model", false),
            ] {
                let model = definition
                    .build(ProviderBuildConfig {
                        capability: None,
                        tool_discovery: None,
                        credential: ProviderCredential::ApiKey("test-key".into()),
                        model: id.into(),
                        base_url: Some(BaseUrl::new(base_url).expect("endpoint")),
                        reasoning_effort: None,
                        service_tier: None,
                        web_search: HostedWebSearch::Off,
                        http: reqwest::Client::new(),
                        transport: crate::backend::model::ModelTransportSettings::default(),
                    })
                    .expect("build provider");
                assert_eq!(model.supports_image_input(), supports_images, "{id}");
                assert_eq!(model.supports_tool_image_input(), supports_images, "{id}");
            }
        }
    }
}
