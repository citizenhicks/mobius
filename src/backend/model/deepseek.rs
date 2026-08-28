//! DeepSeek Responses provider.

use std::sync::Arc;

use super::Model;
use super::openai::OpenAi;
use super::provider::HostedWebSearch;
use super::provider::ProviderAuth;
use super::provider::ProviderBuildConfig;
use super::provider::ProviderDefinition;
use crate::Error;
use crate::Result;

mod manifest {
    include!(concat!(
        env!("OUT_DIR"),
        "/src_backend_model_deepseek_manifest.rs"
    ));
}

const BASE_URL: &str = "https://api.deepseek.com";

pub(super) const fn provider() -> ProviderDefinition {
    ProviderDefinition::new(
        "deepseek",
        manifest::PROVIDER_LABEL,
        "deepseek",
        manifest::PROVIDER_DESCRIPTION,
        ProviderAuth::ApiKey("DEEPSEEK_API_KEY"),
        manifest::MODELS,
        manifest::DEFAULT_MODEL,
        manifest::SEARCH,
        build_provider,
    )
    .with_tool_discovery(
        manifest::TOOL_DISCOVERY,
        manifest::CUSTOM_ENDPOINT_TOOL_DISCOVERY,
    )
    .with_base_url(BASE_URL)
    .with_credentialless_endpoints()
}

fn build_provider(config: ProviderBuildConfig) -> Result<Arc<dyn Model>> {
    let base_url = config
        .base_url
        .ok_or_else(|| Error::Config("DeepSeek requires a base URL".into()))?;
    let api_key = config.credential.into_optional_api_key("deepseek")?;
    let provider =
        OpenAi::with_client(api_key, base_url, config.model, config.http)?.without_image_input();
    let provider = match config.reasoning_effort {
        Some(effort) => provider.with_reasoning_effort(effort)?,
        None => provider,
    };
    let provider = match config.web_search {
        HostedWebSearch::Off => provider,
        HostedWebSearch::Cached => {
            return Err(Error::Config(
                "DeepSeek does not support cached web search".into(),
            ));
        }
        HostedWebSearch::Live => provider.with_web_search(),
    };
    Ok(Arc::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::model::provider::ProviderCredential;
    use crate::backend::model::provider::provider as registered_provider;

    #[test]
    fn advertised_web_search_modes_build() {
        let definition = provider();
        for web_search in definition.web_search().iter().copied() {
            definition
                .build(ProviderBuildConfig {
                    credential: ProviderCredential::ApiKey("test-key".into()),
                    model: definition.default_model().expect("default model").into(),
                    base_url: Some(BASE_URL.into()),
                    reasoning_effort: None,
                    web_search,
                    http: reqwest::Client::new(),
                })
                .expect("advertised web search mode builds");
        }
    }

    #[test]
    fn registered_provider_builds_its_default_model() {
        let definition = registered_provider("deepseek").expect("registered provider");
        let model = definition
            .build(ProviderBuildConfig {
                credential: ProviderCredential::ApiKey("test-key".into()),
                model: "deepseek-v4-flash".into(),
                base_url: Some(BASE_URL.into()),
                reasoning_effort: None,
                web_search: HostedWebSearch::Off,
                http: reqwest::Client::new(),
            })
            .expect("build provider");

        assert!(matches!(
            definition.auth(),
            ProviderAuth::ApiKey("DEEPSEEK_API_KEY")
        ));
        assert_eq!(definition.models(), manifest::MODELS);
        assert_eq!(definition.web_search(), manifest::SEARCH);
        assert_eq!(model.info().reasoning_effort.as_deref(), Some("high"));
        assert!(!model.supports_image_input());
    }
}
