//! AI/ML API Chat Completions provider.

use std::sync::{Arc, LazyLock};

use reqwest::Url;

use super::Model;
use super::chat_completions;
use super::provider::{
    BaseUrl, ModelCatalog, ProviderBuildConfig, ProviderCredential, ProviderDefinition,
    ProviderMetadata,
};
use crate::protocol::ToolDiscoveryMode;
use crate::{Error, Result};

static MANIFEST: LazyLock<ProviderMetadata> =
    LazyLock::new(|| crate::config::embedded(include_str!("aimlapi_provider.toml")));
static CATALOG: LazyLock<ModelCatalog> =
    LazyLock::new(|| ModelCatalog::load("aimlapi.toml", include_str!("aimlapi.toml")));

const API_HOST: &str = "api.aimlapi.com";

pub(super) fn provider() -> ProviderDefinition {
    ProviderDefinition::from_metadata(
        "aimlapi",
        &MANIFEST,
        MANIFEST.api_key_auth(),
        Some(&CATALOG),
        build_provider,
    )
    .with_image_input()
}

/// Builds the provider after checking that its key goes only to the AI/ML API host.
fn build_provider(
    config: ProviderBuildConfig,
    base_url: BaseUrl,
    _tool_discovery: ToolDiscoveryMode,
) -> Result<Arc<dyn Model>> {
    if matches!(config.credential, ProviderCredential::ApiKey(_))
        && !sends_key_to_api_host(&base_url)
    {
        return Err(Error::Invalid(format!(
            "provider `aimlapi` sends its API key only to https://{API_HOST}"
        )));
    }
    chat_completions::build_compatible(&provider(), config, base_url)
}

fn sends_key_to_api_host(base_url: &BaseUrl) -> bool {
    Url::parse(base_url.as_str()).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some(API_HOST)
            && url.port_or_known_default() == Some(443)
    })
}

#[cfg(test)]
#[path = "aimlapi_tests.rs"]
mod tests;
