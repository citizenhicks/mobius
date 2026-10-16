//! OpenRouter Responses provider.

use std::sync::Arc;

use super::Model;
use super::provider::HostedWebSearch;
use super::provider::ProviderBuildConfig;
use super::provider::ProviderDefinition;
use super::responses::OpenAi;
use crate::Error;
use crate::Result;
use crate::protocol::ToolDiscoveryMode;

pub(super) static MANIFEST: std::sync::LazyLock<super::provider::ProviderMetadata> =
    std::sync::LazyLock::new(|| crate::config::embedded(include_str!("openrouter_provider.toml")));

pub(super) fn provider() -> ProviderDefinition {
    ProviderDefinition::from_metadata(
        "openrouter",
        &MANIFEST,
        MANIFEST.api_key_auth(),
        None,
        build_provider,
    )
    .with_image_input()
    .with_image_generation()
    .with_credentialless_endpoints()
}

fn build_provider(config: ProviderBuildConfig) -> Result<Arc<dyn Model>> {
    let tool_discovery = config
        .tool_discovery
        .unwrap_or_else(|| provider().tool_discovery(&config.model, config.base_url.as_deref()));
    let base_url = config
        .base_url
        .ok_or_else(|| Error::Config("OpenRouter requires a base URL".into()))?;
    let api_key = config.credential.into_optional_api_key("openrouter")?;
    let provider = OpenAi::with_client(
        api_key,
        base_url,
        config.model,
        config.http,
        config.transport,
    )?
    .with_web_search_event_type(
        MANIFEST
            .web_search_type
            .as_deref()
            .expect("web search wire type"),
    )
    .with_service_tier(config.service_tier)
    .with_image_api(Some(&super::image_generation::IMAGE_APIS["openrouter"]));
    let provider = match tool_discovery {
        ToolDiscoveryMode::Native => provider.with_inline_tool_search(
            MANIFEST
                .tool_search_type
                .as_deref()
                .expect("tool search wire type"),
        ),
        ToolDiscoveryMode::Rebuild => provider.with_tool_discovery(ToolDiscoveryMode::Rebuild),
    };
    let provider = match config.reasoning_effort {
        Some(effort) => provider.with_reasoning_effort(effort)?,
        None => provider,
    };
    let provider = match config.web_search {
        HostedWebSearch::Off => provider,
        HostedWebSearch::Cached => {
            return Err(Error::Config(
                "OpenRouter does not support cached web search".into(),
            ));
        }
        HostedWebSearch::Live => {
            provider.with_hosted_tool(serde_json::json!({"type": MANIFEST.web_search_type}))
        }
    };
    Ok(Arc::new(provider))
}
