//! Provider-owned setup metadata and model construction.

use std::any::Any;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;

use super::Model;
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::protocol::FrontendSymbol;
use crate::protocol::ToolDiscoveryMode;

mod text {
    include!(concat!(
        env!("OUT_DIR"),
        "/src_backend_model_provider_text.rs"
    ));
}

pub use super::transport::streaming_client;
pub use reqwest::Client as HttpClient;

/// A reasoning choice advertised for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReasoningPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

/// A model choice advertised by its backend provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub context_window: i64,
    pub reasoning: &'static [ReasoningPreset],
    pub default_reasoning: Option<&'static str>,
    pub tool_discovery: ToolDiscoveryMode,
}

/// Hosted search modes a provider may expose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedWebSearch {
    #[default]
    Off,
    Cached,
    Live,
}

impl HostedWebSearch {
    /// Returns the stable manifest value.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Cached => "cached",
            Self::Live => "live",
        }
    }

    /// Returns the user-facing setup label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => text::SEARCH_OFF_LABEL,
            Self::Cached => text::SEARCH_CACHED_LABEL,
            Self::Live => text::SEARCH_LIVE_LABEL,
        }
    }

    /// Returns the user-facing setup description.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Off => text::SEARCH_OFF_DESCRIPTION,
            Self::Cached => text::SEARCH_CACHED_DESCRIPTION,
            Self::Live => text::SEARCH_LIVE_DESCRIPTION,
        }
    }
}

impl std::str::FromStr for HostedWebSearch {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "off" => Ok(Self::Off),
            "cached" => Ok(Self::Cached),
            "live" => Ok(Self::Live),
            _ => Err(Error::Config(format!(
                "unknown hosted web-search mode `{value}`"
            ))),
        }
    }
}

/// Fully resolved settings passed to one provider constructor.
pub struct ProviderBuildConfig {
    pub credential: ProviderCredential,
    pub model: String,
    pub base_url: Option<String>,
    pub reasoning_effort: Option<String>,
    pub web_search: HostedWebSearch,
    /// Shared HTTP client; one per assembly keeps provider clones on one pool.
    pub http: HttpClient,
}

/// One provider-owned browser authentication flow.
pub trait BrowserLogin: Send {
    fn url(&self) -> &str;
    fn open_browser(&self);
    fn complete(self: Box<Self>, path: PathBuf) -> BoxFuture<'static, Result<()>>;
}

type BrowserLoginStart = fn() -> BoxFuture<'static, Result<Box<dyn BrowserLogin>>>;

/// One provider-owned device-code authentication flow.
pub trait DeviceLogin: Send {
    fn verification_url(&self) -> &str;
    fn user_code(&self) -> &str;
    fn complete(self: Box<Self>, path: PathBuf) -> BoxFuture<'static, Result<()>>;
}

type DeviceLoginStart = fn() -> BoxFuture<'static, Result<Box<dyn DeviceLogin>>>;

/// Provider-owned browser authentication hooks consumed generically by applications.
pub struct BrowserAuth {
    label: &'static str,
    configured: fn(&Path) -> Result<bool>,
    load: fn(&Path) -> Result<ProviderCredential>,
    start: BrowserLoginStart,
    start_device: Option<DeviceLoginStart>,
}

impl BrowserAuth {
    pub const fn new(
        label: &'static str,
        configured: fn(&Path) -> Result<bool>,
        load: fn(&Path) -> Result<ProviderCredential>,
        start: BrowserLoginStart,
    ) -> Self {
        Self {
            label,
            configured,
            load,
            start,
            start_device: None,
        }
    }

    /// Adds a cross-device login flow for headless provider hosts.
    #[must_use]
    pub const fn with_device_login(mut self, start: DeviceLoginStart) -> Self {
        self.start_device = Some(start);
        self
    }

    #[must_use]
    pub const fn label(&self) -> &'static str {
        self.label
    }

    pub fn configured(&self, path: &Path) -> Result<bool> {
        (self.configured)(path)
    }

    pub fn load(&self, path: &Path) -> Result<ProviderCredential> {
        (self.load)(path)
    }

    pub fn start(&self) -> BoxFuture<'static, Result<Box<dyn BrowserLogin>>> {
        (self.start)()
    }

    /// Reports whether the provider supports cross-device authentication.
    #[must_use]
    pub const fn supports_device_login(&self) -> bool {
        self.start_device.is_some()
    }

    /// Starts a cross-device login without binding a browser callback on the host.
    pub fn start_device(&self) -> BoxFuture<'static, Result<Box<dyn DeviceLogin>>> {
        match self.start_device {
            Some(start) => start(),
            None => Box::pin(async {
                Err(Error::Auth(
                    "provider does not support device-code login".into(),
                ))
            }),
        }
    }
}

/// Authentication required by a provider manifest.
#[derive(Clone, Copy)]
pub enum ProviderAuth {
    ApiKey(&'static str),
    Browser(&'static BrowserAuth),
}

/// Resolved credential passed to one provider constructor.
#[derive(Clone)]
pub enum ProviderCredential {
    ApiKey(String),
    Browser(Arc<dyn Any + Send + Sync>),
    Credentialless,
}

impl ProviderCredential {
    pub(super) fn into_api_key(self, provider: &str) -> Result<String> {
        match self {
            Self::ApiKey(api_key) => Ok(api_key),
            Self::Browser(_) | Self::Credentialless => Err(Error::Config(format!(
                "provider `{provider}` requires an API key"
            ))),
        }
    }

    pub(super) fn into_optional_api_key(self, provider: &str) -> Result<Option<String>> {
        match self {
            Self::ApiKey(api_key) => Ok(Some(api_key)),
            Self::Credentialless => Ok(None),
            Self::Browser(_) => Err(Error::Config(format!(
                "provider `{provider}` requires an API key or credentialless endpoint"
            ))),
        }
    }

    pub fn into_browser<T: Any + Send + Sync>(self, provider: &str) -> Result<Arc<T>> {
        match self {
            Self::Browser(credential) => Arc::downcast(credential)
                .map_err(|_| Error::Config(format!("provider `{provider}` received wrong login"))),
            Self::ApiKey(_) | Self::Credentialless => Err(Error::Config(format!(
                "provider `{provider}` requires browser login"
            ))),
        }
    }
}

type ProviderBuilder = fn(ProviderBuildConfig) -> Result<Arc<dyn Model>>;

/// One backend provider's setup manifest and constructor.
pub struct ProviderDefinition {
    id: &'static str,
    label: &'static str,
    symbol: &'static str,
    description: &'static str,
    auth: ProviderAuth,
    models: &'static [ModelPreset],
    default_model: Option<&'static str>,
    web_search: &'static [HostedWebSearch],
    supports_image_input: bool,
    supports_realtime_voice: bool,
    tool_discovery: ToolDiscoveryMode,
    custom_endpoint_tool_discovery: Option<ToolDiscoveryMode>,
    default_base_url: Option<&'static str>,
    credentialless_endpoints: bool,
    builder: ProviderBuilder,
}

impl ProviderDefinition {
    #[expect(
        clippy::too_many_arguments,
        reason = "a provider manifest keeps its required fields explicit at the registry entry"
    )]
    pub(crate) const fn new(
        id: &'static str,
        label: &'static str,
        symbol: &'static str,
        description: &'static str,
        auth: ProviderAuth,
        models: &'static [ModelPreset],
        default_model: Option<&'static str>,
        web_search: &'static [HostedWebSearch],
        builder: ProviderBuilder,
    ) -> Self {
        Self {
            id,
            label,
            symbol,
            description,
            auth,
            models,
            default_model,
            web_search,
            supports_image_input: false,
            supports_realtime_voice: false,
            tool_discovery: ToolDiscoveryMode::Rebuild,
            custom_endpoint_tool_discovery: None,
            default_base_url: None,
            credentialless_endpoints: false,
            builder,
        }
    }

    /// Marks a provider whose model transport accepts native image input.
    #[must_use]
    pub(crate) const fn with_image_input(mut self) -> Self {
        self.supports_image_input = true;
        self
    }

    pub(crate) const fn with_realtime_voice(mut self) -> Self {
        self.supports_realtime_voice = true;
        self
    }

    pub(crate) const fn with_base_url(mut self, default_base_url: &'static str) -> Self {
        self.default_base_url = Some(default_base_url);
        self
    }

    #[must_use]
    pub(crate) const fn with_tool_discovery(
        mut self,
        mode: ToolDiscoveryMode,
        custom_endpoint_mode: Option<ToolDiscoveryMode>,
    ) -> Self {
        self.tool_discovery = mode;
        self.custom_endpoint_tool_discovery = custom_endpoint_mode;
        self
    }

    /// Allows explicitly configured non-default endpoints to omit provider credentials.
    #[must_use]
    pub(crate) const fn with_credentialless_endpoints(mut self) -> Self {
        self.credentialless_endpoints = true;
        self
    }

    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.id
    }

    #[must_use]
    pub const fn label(&self) -> &'static str {
        self.label
    }

    #[must_use]
    pub fn symbol(&self) -> FrontendSymbol {
        FrontendSymbol::from_wire(self.symbol)
    }

    #[must_use]
    pub const fn description(&self) -> &'static str {
        self.description
    }

    #[must_use]
    pub const fn auth(&self) -> ProviderAuth {
        self.auth
    }

    #[must_use]
    pub const fn models(&self) -> &'static [ModelPreset] {
        self.models
    }

    /// Returns the model selected for a newly configured provider.
    #[must_use]
    pub const fn default_model(&self) -> Option<&'static str> {
        self.default_model
    }

    #[must_use]
    pub const fn web_search(&self) -> &'static [HostedWebSearch] {
        self.web_search
    }

    /// Reports image capability before provider credentials are resolved.
    #[must_use]
    pub const fn supports_image_input(&self) -> bool {
        self.supports_image_input
    }

    /// Reports realtime capability only for the provider's supported first-party endpoint.
    #[must_use]
    pub fn supports_realtime_voice(&self, base_url: Option<&str>) -> bool {
        self.supports_realtime_voice && self.uses_default_endpoint(base_url)
    }

    /// Resolves cache behavior for one model and endpoint selection.
    #[must_use]
    pub fn tool_discovery(&self, model: &str, base_url: Option<&str>) -> ToolDiscoveryMode {
        let mode = self
            .model(model)
            .map_or(self.tool_discovery, |preset| preset.tool_discovery);
        if self.uses_default_endpoint(base_url) {
            mode
        } else {
            self.custom_endpoint_tool_discovery.unwrap_or(mode)
        }
    }

    /// Returns the tool-discovery behavior shown before endpoint setup.
    #[must_use]
    pub fn default_tool_discovery(&self) -> ToolDiscoveryMode {
        self.tool_discovery(
            self.default_model.unwrap_or_default(),
            self.default_base_url,
        )
    }

    /// Returns an endpoint-specific override exposed during provider setup.
    #[must_use]
    pub const fn custom_endpoint_tool_discovery(&self) -> Option<ToolDiscoveryMode> {
        self.custom_endpoint_tool_discovery
    }

    #[must_use]
    pub const fn configurable_base_url(&self) -> bool {
        self.default_base_url.is_some()
    }

    #[must_use]
    pub const fn default_base_url(&self) -> Option<&'static str> {
        self.default_base_url
    }

    /// Reports whether a resolved base URL selects the provider's default endpoint.
    #[must_use]
    pub fn uses_default_endpoint(&self, base_url: Option<&str>) -> bool {
        match (self.default_base_url, base_url) {
            (None, None) => true,
            (Some(default), Some(base_url)) => {
                let Ok(default) = reqwest::Url::parse(default) else {
                    return false;
                };
                let Ok(base_url) = reqwest::Url::parse(base_url) else {
                    return false;
                };
                same_endpoint(&default, &base_url)
            }
            _ => false,
        }
    }

    /// Reports whether non-default endpoints may be configured without credentials.
    #[must_use]
    pub const fn supports_credentialless_endpoints(&self) -> bool {
        self.credentialless_endpoints
    }

    /// Returns a preset when the configured model is in this provider's picker.
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&'static ModelPreset> {
        self.models.iter().find(|model| model.id == id)
    }

    /// Builds one runtime model after validating advertised capabilities.
    pub fn build(&self, mut config: ProviderBuildConfig) -> Result<Arc<dyn Model>> {
        if matches!(config.credential, ProviderCredential::Credentialless) {
            self.validate_credentialless_endpoint(config.base_url.as_deref())?;
        }
        if config.reasoning_effort.is_none() {
            config.reasoning_effort = self
                .model(&config.model)
                .and_then(|model| model.default_reasoning)
                .map(str::to_string);
        }
        self.build_config_is_valid(
            &config.model,
            config.base_url.as_deref(),
            config.reasoning_effort.as_deref(),
            config.web_search,
        )?;
        let tool_discovery = self.tool_discovery(&config.model, config.base_url.as_deref());
        let model = (self.builder)(config)?;
        if model.tool_discovery() != tool_discovery {
            return Err(Error::Config(format!(
                "provider `{}` built a model with inconsistent tool discovery",
                self.id
            )));
        }
        Ok(model)
    }

    /// Validates provider-specific settings without resolving credentials.
    pub fn build_config_is_valid(
        &self,
        model: &str,
        base_url: Option<&str>,
        reasoning_effort: Option<&str>,
        web_search: HostedWebSearch,
    ) -> Result<()> {
        if model.trim().is_empty() {
            return Err(Error::Config(format!(
                "provider `{}` requires a model",
                self.id
            )));
        }
        let preset = self.model(model);
        if !self.models.is_empty() && preset.is_none() {
            return Err(Error::Config(format!(
                "provider `{}` does not advertise model `{model}`",
                self.id
            )));
        }
        if !self.web_search.contains(&web_search) {
            return Err(Error::Config(format!(
                "provider `{}` does not support web search mode `{}`",
                self.id,
                web_search.id()
            )));
        }
        self.validate_base_url(base_url)?;
        if let Some(effort) = reasoning_effort
            && let Some(preset) = preset
            && !preset.reasoning.iter().any(|preset| preset.id == effort)
        {
            return Err(Error::Config(format!(
                "model `{}` does not support reasoning effort `{effort}`",
                model
            )));
        }
        Ok(())
    }

    /// Validates this provider's base-URL boundary.
    pub fn validate_base_url(&self, base_url: Option<&str>) -> Result<()> {
        match (self.default_base_url, base_url) {
            (None, Some(_)) => Err(Error::Config(format!(
                "provider `{}` has a fixed API endpoint",
                self.id
            ))),
            (Some(_), None) => Err(Error::Config(format!(
                "provider `{}` requires a base URL",
                self.id
            ))),
            (Some(_), Some(base_url)) => validate_base_url(base_url),
            (None, None) => Ok(()),
        }
    }

    /// Validates an explicitly credentialless provider endpoint.
    pub fn validate_credentialless_endpoint(&self, base_url: Option<&str>) -> Result<()> {
        if !self.supports_credentialless_endpoints() {
            return Err(Error::Config(format!(
                "provider `{}` does not support credentialless endpoints",
                self.id
            )));
        }
        self.validate_base_url(base_url)?;
        let base_url = base_url
            .ok_or_else(|| Error::Config("credentialless endpoint requires a base URL".into()))?;
        let endpoint = reqwest::Url::parse(base_url)
            .map_err(|error| Error::Config(format!("invalid base URL: {error}")))?;
        if endpoint.scheme() != "https" {
            return Err(Error::Config(
                "credentialless endpoint must use HTTPS".into(),
            ));
        }
        if self.default_base_url.is_some_and(|default| {
            reqwest::Url::parse(default).is_ok_and(|default| same_origin(&default, &endpoint))
        }) {
            return Err(Error::Config(format!(
                "provider `{}` default endpoint requires provider authentication",
                self.id
            )));
        }
        Ok(())
    }
}

fn same_endpoint(left: &reqwest::Url, right: &reqwest::Url) -> bool {
    same_origin(left, right)
        && left.path().trim_end_matches('/') == right.path().trim_end_matches('/')
}

fn same_origin(left: &reqwest::Url, right: &reqwest::Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str().map(|host| host.trim_end_matches('.'))
            == right.host_str().map(|host| host.trim_end_matches('.'))
        && left.port_or_known_default() == right.port_or_known_default()
}

pub(super) fn validate_base_url(base_url: &str) -> Result<()> {
    let url = reqwest::Url::parse(base_url)
        .map_err(|error| Error::Config(format!("invalid base URL: {error}")))?;
    let host = url
        .host_str()
        .ok_or_else(|| Error::Config("base URL requires a host".into()))?;
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]");
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(Error::Config(
            "base URL must use HTTPS, except for loopback HTTP".into(),
        ));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Config(
            "base URL cannot contain credentials, a query, or a fragment".into(),
        ));
    }
    Ok(())
}

static PROVIDERS: &[ProviderDefinition] = &[
    super::openai_socket::provider(),
    super::openai_codex::provider(),
    super::deepseek::provider(),
    super::kimi::provider(),
    super::openrouter::provider(),
    super::anthropic::provider(),
    super::openai::generic_provider(),
];

/// Returns every built-in provider in setup-menu order.
#[must_use]
pub fn providers() -> &'static [ProviderDefinition] {
    PROVIDERS
}

/// Returns the provider used by an unconfigured composition.
#[must_use]
pub fn default_provider() -> &'static ProviderDefinition {
    &PROVIDERS[0]
}

/// Resolves a built-in provider by its stable manifest ID.
pub fn provider(id: &str) -> Result<&'static ProviderDefinition> {
    PROVIDERS
        .iter()
        .find(|provider| provider.id == id)
        .ok_or_else(|| Error::Unknown(format!("model provider `{id}`")))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn provider_manifest_ids_are_unique() {
        let mut ids = BTreeSet::new();

        assert!(
            providers().iter().all(|provider| ids.insert(provider.id())),
            "provider manifest contains duplicate IDs"
        );
    }

    #[test]
    fn provider_manifests_are_complete_and_internally_consistent() {
        for provider in providers() {
            assert!(!provider.id().trim().is_empty());
            assert!(!provider.label().trim().is_empty());
            assert!(!provider.symbol().as_str().trim().is_empty());
            assert!(!provider.description().trim().is_empty());
            assert_eq!(provider.web_search().first(), Some(&HostedWebSearch::Off));
            for search in provider.web_search() {
                assert!(!search.label().trim().is_empty());
                assert!(!search.description().trim().is_empty());
                assert_eq!(search.id().parse::<HostedWebSearch>().ok(), Some(*search));
            }
            assert!(
                !provider.supports_credentialless_endpoints() || provider.configurable_base_url(),
                "provider `{}` allows credentialless endpoints without a configurable base URL",
                provider.id()
            );
            assert!(
                provider.custom_endpoint_tool_discovery().is_none()
                    || provider.configurable_base_url(),
                "provider `{}` declares custom-endpoint tool discovery without a configurable base URL",
                provider.id()
            );

            assert_eq!(
                provider.default_model().is_some(),
                !provider.models().is_empty(),
                "provider `{}` must advertise exactly one default model when it has presets",
                provider.id()
            );
            assert!(
                provider
                    .default_model()
                    .is_none_or(|default| provider.model(default).is_some()),
                "provider `{}` has an unknown default model",
                provider.id()
            );

            let mut model_ids = BTreeSet::new();
            for model in provider.models() {
                assert!(model_ids.insert(model.id), "duplicate model `{}`", model.id);
                assert!(!model.label.trim().is_empty());
                assert!(!model.description.trim().is_empty());
                assert!(model.context_window > 0);

                let mut reasoning_ids = BTreeSet::new();
                for reasoning in model.reasoning {
                    assert!(reasoning_ids.insert(reasoning.id));
                    assert!(!reasoning.label.trim().is_empty());
                    assert!(!reasoning.description.trim().is_empty());
                }
                assert!(
                    model
                        .default_reasoning
                        .is_none_or(|default| reasoning_ids.contains(default))
                );
            }
        }
    }

    #[test]
    fn provider_manifests_advertise_image_input_explicitly() {
        assert!(
            !provider("deepseek")
                .expect("deepseek")
                .supports_image_input()
        );
        for id in [
            "openai_socket",
            "openai_codex",
            "kimi",
            "openrouter",
            "anthropic",
            "responses",
        ] {
            assert!(provider(id).expect("image provider").supports_image_input());
        }
    }

    #[test]
    fn astra_is_available_through_openai_codex_and_custom_responses_routes() {
        for id in ["openai_socket", "openai_codex"] {
            let definition = provider(id).expect("provider");
            assert_eq!(definition.default_model(), Some("gpt-5.6-sol"));
            let astra = definition.model("gpt-6-astra").expect("Astra preset");
            assert_eq!(astra.context_window, 1_050_000);
            assert_eq!(astra.default_reasoning, Some("medium"));
            assert_eq!(
                astra
                    .reasoning
                    .iter()
                    .map(|effort| effort.id)
                    .collect::<Vec<_>>(),
                ["low", "medium", "high", "xhigh", "max"]
            );
            assert!(
                definition
                    .build_config_is_valid("gpt-6-astra", None, Some("none"), HostedWebSearch::Off)
                    .is_err()
            );
        }
        for (id, model) in [
            ("openrouter", "openai/gpt-6-astra"),
            ("responses", "gpt-6-astra"),
        ] {
            let definition = provider(id).expect("custom provider");
            assert!(definition.models().is_empty());
            definition
                .build_config_is_valid(
                    model,
                    definition.default_base_url(),
                    Some("medium"),
                    HostedWebSearch::Off,
                )
                .expect("custom Astra route");
        }
    }

    #[test]
    fn credentialless_providers_build_without_a_credential() {
        let credentialless = providers()
            .iter()
            .filter(|definition| definition.supports_credentialless_endpoints())
            .collect::<Vec<_>>();
        assert_eq!(
            credentialless
                .iter()
                .map(|definition| definition.id())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["anthropic", "deepseek", "kimi", "openrouter", "responses"])
        );
        let error = provider("openai_socket")
            .expect("fixed-endpoint provider")
            .validate_credentialless_endpoint(Some("https://proxy.example/v1"))
            .expect_err("a provider that does not opt in is rejected");
        assert!(
            error
                .to_string()
                .contains("does not support credentialless")
        );
        for definition in credentialless {
            let base_url = definition
                .default_base_url()
                .expect("credentialless provider advertises a default base URL");
            definition
                .validate_credentialless_endpoint(Some("https://proxy.example/v1"))
                .expect("a non-default HTTPS endpoint is accepted");
            definition
                .validate_credentialless_endpoint(Some(base_url))
                .expect_err("the provider's own endpoint still requires authentication");
            definition
                .build(ProviderBuildConfig {
                    credential: ProviderCredential::Credentialless,
                    model: definition
                        .default_model()
                        .unwrap_or("test-model")
                        .to_string(),
                    base_url: Some("https://proxy.example/v1".into()),
                    reasoning_effort: None,
                    web_search: HostedWebSearch::Off,
                    http: reqwest::Client::new(),
                })
                .expect("credentialless provider builds without a credential");
        }
    }
}
