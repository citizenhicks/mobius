//! Provider-owned setup metadata and model construction.

use std::any::Any;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;

use super::Model;
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::protocol::FrontendSymbol;
use crate::protocol::ModelCapability;
use crate::protocol::ToolDiscoveryMode;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderMetadata {
    pub(super) label: String,
    pub(super) symbol: String,
    pub(super) description: String,
    pub(super) base_url: BaseUrl,
    pub(super) credential_env: Option<String>,
    pub(super) tool_discovery: ToolDiscoveryMode,
    #[serde(default = "default_supported_tool_discovery")]
    pub(super) supported_tool_discovery: Vec<ToolDiscoveryMode>,
    pub(super) custom_endpoint_tool_discovery: Option<ToolDiscoveryMode>,
    #[serde(default)]
    pub(super) native_custom_endpoints: bool,
    #[serde(default)]
    pub(super) locked_models: bool,
    pub(super) search: Vec<HostedWebSearch>,
    #[serde(default)]
    pub(super) headers: std::collections::BTreeMap<String, String>,
    pub(super) max_output_tokens: Option<u64>,
    pub(super) tool_search_type: Option<String>,
    pub(super) web_search_type: Option<String>,
}

fn default_supported_tool_discovery() -> Vec<ToolDiscoveryMode> {
    vec![ToolDiscoveryMode::Native, ToolDiscoveryMode::Rebuild]
}

impl ProviderMetadata {
    #[expect(
        clippy::expect_used,
        reason = "embedded API-key manifests always name a credential variable; provider tests build each one"
    )]
    pub(super) fn api_key_auth(&'static self) -> ProviderAuth {
        ProviderAuth::ApiKey(
            self.credential_env
                .as_deref()
                .expect("embedded API-key provider requires a credential environment name"),
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchText {
    off_label: String,
    off_description: String,
    cached_label: String,
    cached_description: String,
    live_label: String,
    live_description: String,
}

static SEARCH_TEXT: std::sync::LazyLock<SearchText> =
    std::sync::LazyLock::new(|| crate::config::embedded(include_str!("provider.toml")));
pub use super::transport::streaming_client;
/// Additional trust roots for shared HTTP clients.
pub use reqwest::Certificate as HttpCertificate;
pub use reqwest::Client as HttpClient;
pub use reqwest::Response as HttpResponse;
/// Redirect policy for shared HTTP clients.
pub use reqwest::redirect::Policy as HttpRedirectPolicy;

/// A reasoning effort, image tier or voice advertised as a variant of one model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningPreset {
    /// The identifier.
    pub id: String,
    /// The label.
    pub label: String,
    /// The description.
    #[serde(default)]
    pub description: String,
}

/// A model choice advertised by its backend provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPreset {
    /// The identifier.
    pub id: String,
    /// The label.
    pub label: String,
    /// The description.
    pub description: String,
    /// The context window.
    pub context_window: i64,
    /// The reasoning.
    pub reasoning: Vec<ReasoningPreset>,
    /// The default reasoning.
    pub default_reasoning: Option<String>,
    /// The tool discovery.
    pub tool_discovery: ToolDiscoveryMode,
}

/// An image or voice model advertised by its backend provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaModelPreset {
    /// The identifier.
    pub id: String,
    /// The label.
    pub label: String,
    /// The description.
    pub description: String,
    /// Selectable variants: image tiers with their provider model IDs, or voices.
    #[serde(default)]
    pub variants: Vec<ReasoningPreset>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelCatalog {
    pub(super) default_model: Option<String>,
    #[serde(default)]
    pub(super) image_input_models: Vec<String>,
    #[serde(default)]
    pub(super) models: Vec<ModelPreset>,
    #[serde(default)]
    pub(super) image_models: Vec<MediaModelPreset>,
}

impl ModelCatalog {
    pub(super) fn validate(&self) -> Result<()> {
        unique_ids(self.image_input_models.iter().map(String::as_str))?;
        for id in &self.image_input_models {
            if !self.models.iter().any(|model| &model.id == id) {
                return Err(Error::Config(format!(
                    "image input model `{id}` is not in the catalog"
                )));
            }
        }
        unique_ids(self.image_models.iter().map(|model| model.id.as_str()))?;
        for model in &self.image_models {
            unique_ids(model.variants.iter().map(|variant| variant.id.as_str()))?;
        }
        for (index, model) in self.models.iter().enumerate() {
            unique_ids(
                model
                    .reasoning
                    .iter()
                    .map(|reasoning| reasoning.id.as_str()),
            )?;
            if model.id.trim().is_empty()
                || self
                    .models
                    .iter()
                    .take(index)
                    .any(|other| other.id == model.id)
            {
                return Err(Error::Config(format!(
                    "model id `{}` is empty or duplicated",
                    model.id
                )));
            }
            if let Some(effort) = &model.default_reasoning
                && !model
                    .reasoning
                    .iter()
                    .any(|reasoning| &reasoning.id == effort)
            {
                return Err(Error::Config(format!(
                    "model `{}` default reasoning `{effort}` is not advertised",
                    model.id
                )));
            }
        }
        if let Some(default) = &self.default_model
            && !self.models.iter().any(|model| &model.id == default)
        {
            return Err(Error::Config(format!(
                "default model `{default}` is not in the catalog"
            )));
        }
        Ok(())
    }
}

pub(super) fn unique_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        if id.trim().is_empty() || !seen.insert(id) {
            return Err(Error::Config(format!(
                "model id `{id}` is empty or duplicated"
            )));
        }
    }
    Ok(())
}

/// One provider-reported account usage window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageLimit {
    /// The identifier.
    pub id: String,
    /// The label.
    pub label: String,
    /// The remaining fraction.
    pub remaining_fraction: f64,
    /// The window seconds.
    pub window_seconds: u64,
    /// The resets at.
    pub resets_at: Option<i64>,
}

/// Hosted search modes a provider may expose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedWebSearch {
    #[default]
    /// Selects the off case.
    Off,
    /// Selects the cached case.
    Cached,
    /// Selects the live case.
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
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => &SEARCH_TEXT.off_label,
            Self::Cached => &SEARCH_TEXT.cached_label,
            Self::Live => &SEARCH_TEXT.live_label,
        }
    }

    /// Returns the user-facing setup description.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Off => &SEARCH_TEXT.off_description,
            Self::Cached => &SEARCH_TEXT.cached_description,
            Self::Live => &SEARCH_TEXT.live_description,
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
    /// The operation this transport serves; omission constructs a chat model.
    pub capability: Option<ModelCapability>,
    /// Optional tool-discovery mode; omission uses the provider/model endpoint default.
    pub tool_discovery: Option<ToolDiscoveryMode>,
    /// The credential.
    pub credential: ProviderCredential,
    /// The model.
    pub model: String,
    /// The base URL.
    pub base_url: Option<BaseUrl>,
    /// The reasoning effort.
    pub reasoning_effort: Option<String>,
    /// Optional native Responses processing tier; omission uses the endpoint default.
    pub service_tier: Option<String>,
    /// The web search.
    pub web_search: HostedWebSearch,
    /// Shared HTTP client; one per assembly keeps provider clones on one pool.
    /// Use `ModelTransportSettings::streaming_client` or disable redirects on custom
    /// clients: authenticated provider requests must not follow another origin.
    pub http: HttpClient,
    /// Operational transport policy, validated before construction.
    pub transport: super::ModelTransportSettings,
}

impl std::fmt::Debug for ProviderBuildConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderBuildConfig")
            .field("capability", &self.capability)
            .field("tool_discovery", &self.tool_discovery)
            .field("credential", &self.credential)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("service_tier", &self.service_tier)
            .field("web_search", &self.web_search)
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

/// Concrete pending browser and device authentication flows.
pub use super::openai_codex::{BrowserLogin, DeviceLogin};

type BrowserLoginStart =
    fn(super::ModelTransportSettings) -> BoxFuture<'static, Result<BrowserLogin>>;
type DeviceLoginStart =
    fn(super::ModelTransportSettings) -> BoxFuture<'static, Result<DeviceLogin>>;
type BrowserUsageRead =
    fn(&Path, super::ModelTransportSettings) -> BoxFuture<'static, Result<Vec<UsageLimit>>>;

/// Provider-owned browser authentication hooks consumed generically by applications.
pub struct BrowserAuth {
    label: &'static str,
    configured: fn(&Path) -> Result<bool>,
    load: fn(&Path, super::ModelTransportSettings) -> Result<ProviderCredential>,
    start: BrowserLoginStart,
    start_device: Option<DeviceLoginStart>,
    usage_limits: Option<BrowserUsageRead>,
}

impl std::fmt::Debug for BrowserAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserAuth")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

impl BrowserAuth {
    /// Creates a new instance.
    pub(super) const fn new(
        label: &'static str,
        configured: fn(&Path) -> Result<bool>,
        load: fn(&Path, super::ModelTransportSettings) -> Result<ProviderCredential>,
        start: BrowserLoginStart,
    ) -> Self {
        Self {
            label,
            configured,
            load,
            start,
            start_device: None,
            usage_limits: None,
        }
    }

    /// Adds a cross-device login flow for headless provider hosts.
    #[must_use]
    pub(super) const fn with_device_login(mut self, start: DeviceLoginStart) -> Self {
        self.start_device = Some(start);
        self
    }

    /// Adds a passive account-usage reader for this browser-authenticated provider.
    #[must_use]
    pub(super) const fn with_usage_limits(mut self, usage_limits: BrowserUsageRead) -> Self {
        self.usage_limits = Some(usage_limits);
        self
    }

    #[must_use]
    /// Returns the provider label.
    pub const fn label(&self) -> &'static str {
        self.label
    }

    /// Reports whether a stored provider credential is configured.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn configured(&self, path: &Path) -> Result<bool> {
        (self.configured)(path)
    }

    /// Loads the stored provider credential.
    /// # Errors
    ///
    /// Returns an error if the resource cannot be read, decoded, or validated.
    pub fn load(&self, path: &Path) -> Result<ProviderCredential> {
        (self.load)(path, super::ModelTransportSettings::default())
    }

    /// Loads credentials with explicit authentication transport policy.
    /// # Errors
    /// Returns invalid policy or stored credential errors.
    pub fn load_with_transport(
        &self,
        path: &Path,
        settings: super::ModelTransportSettings,
    ) -> Result<ProviderCredential> {
        (self.load)(path, settings)
    }

    /// Starts browser authentication with explicit operational policy.
    pub fn start_with_transport(
        &self,
        settings: super::ModelTransportSettings,
    ) -> BoxFuture<'static, Result<BrowserLogin>> {
        (self.start)(settings)
    }

    /// Starts device authentication with explicit operational policy.
    pub fn start_device_with_transport(
        &self,
        settings: super::ModelTransportSettings,
    ) -> BoxFuture<'static, Result<DeviceLogin>> {
        match self.start_device {
            Some(start) => start(settings),
            None => Box::pin(async {
                Err(Error::Config(
                    "provider does not support device login".into(),
                ))
            }),
        }
    }

    /// Starts browser authentication.
    pub fn start(&self) -> BoxFuture<'static, Result<BrowserLogin>> {
        (self.start)(super::ModelTransportSettings::default())
    }

    /// Reports whether the provider supports cross-device authentication.
    #[must_use]
    pub const fn supports_device_login(&self) -> bool {
        self.start_device.is_some()
    }

    /// Starts a cross-device login without binding a browser callback on the host.
    pub fn start_device(&self) -> BoxFuture<'static, Result<DeviceLogin>> {
        match self.start_device {
            Some(start) => start(super::ModelTransportSettings::default()),
            None => Box::pin(async {
                Err(Error::Auth(
                    "provider does not support device-code login".into(),
                ))
            }),
        }
    }

    /// Reads provider-reported account usage, when supported.
    pub fn usage_limits(&self, path: &Path) -> Option<BoxFuture<'static, Result<Vec<UsageLimit>>>> {
        self.usage_limits_with_transport(path, super::ModelTransportSettings::default())
    }

    /// Reads account usage with explicit authentication transport policy, when supported.
    pub fn usage_limits_with_transport(
        &self,
        path: &Path,
        settings: super::ModelTransportSettings,
    ) -> Option<BoxFuture<'static, Result<Vec<UsageLimit>>>> {
        self.usage_limits
            .map(|usage_limits| usage_limits(path, settings))
    }
}

/// Authentication required by a provider manifest.
#[derive(Debug, Clone, Copy)]
pub enum ProviderAuth {
    /// Selects the API key case.
    ApiKey(&'static str),
    /// Selects the browser case.
    Browser(&'static BrowserAuth),
}

/// Resolved credential passed to one provider constructor.
#[derive(Clone)]
pub enum ProviderCredential {
    /// Selects the API key case.
    ApiKey(String),
    /// Selects the browser case.
    Browser(Arc<dyn Any + Send + Sync>),
    /// Selects the credentialless case.
    Credentialless,
}

impl std::fmt::Debug for ProviderCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ApiKey(_) => "ApiKey",
            Self::Browser(_) => "Browser",
            Self::Credentialless => "Credentialless",
        })
    }
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

    /// Consumes this value and returns the browser.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn into_browser<T: Any + Send + Sync>(self, provider: &str) -> Result<Arc<T>> {
        match self {
            Self::Browser(credential) => match Arc::downcast(credential) {
                Ok(credential) => Ok(credential),
                Err(_) => Err(Error::Config(format!(
                    "provider `{provider}` received wrong login; expected {}",
                    std::any::type_name::<T>()
                ))),
            },
            Self::ApiKey(_) | Self::Credentialless => Err(Error::Config(format!(
                "provider `{provider}` requires browser login"
            ))),
        }
    }
}

type ProviderBuilder = fn(ProviderBuildConfig, BaseUrl) -> Result<Arc<dyn Model>>;

/// Reports whether an endpoint selection uses the provider-advertised default.
/// An omitted selection uses the default endpoint.
#[must_use]
pub fn uses_default_endpoint(default: Option<&str>, selected: Option<&str>) -> bool {
    match (default, selected) {
        (_, None) => true,
        (Some(default), Some(selected)) => {
            let (Ok(default), Ok(selected)) =
                (reqwest::Url::parse(default), reqwest::Url::parse(selected))
            else {
                return false;
            };
            same_endpoint(&default, &selected)
        }
        (None, Some(_)) => false,
    }
}

/// One backend provider's setup manifest and constructor.
pub struct ProviderDefinition {
    id: &'static str,
    label: &'static str,
    symbol: &'static str,
    description: &'static str,
    auth: ProviderAuth,
    models: &'static [ModelPreset],
    locked_models: bool,
    default_model: Option<&'static str>,
    web_search: &'static [HostedWebSearch],
    supports_image_input: bool,
    supports_image_generation: bool,
    image_models: &'static [MediaModelPreset],
    voice_models: &'static [MediaModelPreset],
    tool_discovery: ToolDiscoveryMode,
    supported_tool_discovery: &'static [ToolDiscoveryMode],
    custom_endpoint_tool_discovery: Option<ToolDiscoveryMode>,
    default_base_url: Option<&'static BaseUrl>,
    native_custom_endpoints: bool,
    credentialless_endpoints: bool,
    builder: ProviderBuilder,
    pub(super) replay_reasoning_field: Option<&'static str>,
    pub(super) web_search_type: Option<&'static str>,
}

impl std::fmt::Debug for ProviderDefinition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderDefinition")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("auth", &self.auth)
            .field("default_model", &self.default_model)
            .field("default_base_url", &self.default_base_url)
            .finish_non_exhaustive()
    }
}

impl ProviderDefinition {
    pub(super) fn from_metadata(
        id: &'static str,
        metadata: &'static ProviderMetadata,
        auth: ProviderAuth,
        catalog: Option<&'static ModelCatalog>,
        builder: ProviderBuilder,
    ) -> Self {
        Self {
            replay_reasoning_field: None,
            web_search_type: metadata.web_search_type.as_deref(),
            id,
            label: &metadata.label,
            symbol: &metadata.symbol,
            description: &metadata.description,
            auth,
            models: catalog.map_or(&[], |catalog| catalog.models.as_slice()),
            locked_models: metadata.locked_models,
            default_model: catalog.and_then(|catalog| catalog.default_model.as_deref()),
            web_search: &metadata.search,
            supports_image_input: catalog
                .is_some_and(|catalog| !catalog.image_input_models.is_empty()),
            supports_image_generation: false,
            image_models: catalog.map_or(&[], |catalog| catalog.image_models.as_slice()),
            voice_models: &[],
            tool_discovery: metadata.tool_discovery,
            supported_tool_discovery: &metadata.supported_tool_discovery,
            custom_endpoint_tool_discovery: metadata.custom_endpoint_tool_discovery,
            default_base_url: Some(&metadata.base_url),
            native_custom_endpoints: metadata.native_custom_endpoints,
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

    /// Marks a provider whose default endpoint supports native image generation.
    #[must_use]
    pub(crate) const fn with_image_generation(mut self) -> Self {
        self.supports_image_generation = true;
        self
    }

    pub(crate) const fn with_voice_models(mut self, models: &'static [MediaModelPreset]) -> Self {
        self.voice_models = models;
        self
    }

    pub(super) const fn with_replay_reasoning_field(mut self, field: &'static str) -> Self {
        self.replay_reasoning_field = Some(field);
        self
    }

    /// Allows explicitly configured non-default endpoints to omit provider credentials.
    #[must_use]
    pub(crate) const fn with_credentialless_endpoints(mut self) -> Self {
        self.credentialless_endpoints = true;
        self
    }

    #[must_use]
    /// Returns the provider identifier.
    pub const fn id(&self) -> &'static str {
        self.id
    }

    #[must_use]
    /// Returns the display label.
    pub const fn label(&self) -> &'static str {
        self.label
    }

    #[must_use]
    /// Returns the frontend symbol.
    pub fn symbol(&self) -> FrontendSymbol {
        FrontendSymbol::from_wire(self.symbol)
    }

    #[must_use]
    /// Returns the provider description.
    pub const fn description(&self) -> &'static str {
        self.description
    }

    #[must_use]
    /// Returns the provider authentication method.
    pub const fn auth(&self) -> ProviderAuth {
        self.auth
    }

    #[must_use]
    /// Returns the advertised model presets.
    pub const fn models(&self) -> &'static [ModelPreset] {
        self.models
    }

    #[must_use]
    /// Returns the advertised image models; empty when instances list their own.
    pub const fn image_models(&self) -> &'static [MediaModelPreset] {
        self.image_models
    }

    #[must_use]
    /// Returns the advertised realtime voice models, default first.
    pub const fn voice_models(&self) -> &'static [MediaModelPreset] {
        self.voice_models
    }

    /// Returns the model selected for a newly configured provider.
    #[must_use]
    pub const fn default_model(&self) -> Option<&'static str> {
        self.default_model
    }

    #[must_use]
    /// Returns the web search.
    pub const fn web_search(&self) -> &'static [HostedWebSearch] {
        self.web_search
    }

    /// Reports image capability before provider credentials are resolved.
    #[must_use]
    pub const fn supports_image_input(&self) -> bool {
        self.supports_image_input
    }

    /// Reports a capability implemented by this provider's native API.
    #[must_use]
    pub const fn supports(&self, capability: ModelCapability) -> bool {
        match capability {
            ModelCapability::ImageGeneration => self.supports_image_generation,
            ModelCapability::RealtimeVoice => !self.voice_models.is_empty(),
        }
    }

    /// Reports a capability for the selected provider endpoint.
    #[must_use]
    pub fn supports_at(&self, capability: ModelCapability, base_url: Option<&str>) -> bool {
        (self.native_custom_endpoints || self.uses_default_endpoint(base_url))
            && self.supports(capability)
    }

    /// Reports whether custom roots implement this provider's native media APIs.
    #[must_use]
    pub const fn native_custom_endpoints(&self) -> bool {
        self.native_custom_endpoints
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
            self.default_base_url.map(BaseUrl::as_str),
        )
    }

    /// Returns an endpoint-specific override exposed during provider setup.
    #[must_use]
    pub const fn custom_endpoint_tool_discovery(&self) -> Option<ToolDiscoveryMode> {
        self.custom_endpoint_tool_discovery
    }

    /// Returns the tool-discovery modes implemented by this transport.
    #[must_use]
    pub const fn supported_tool_discovery(&self) -> &'static [ToolDiscoveryMode] {
        self.supported_tool_discovery
    }

    /// Reports whether this transport implements the requested discovery mode.
    #[must_use]
    pub fn supports_tool_discovery(&self, mode: ToolDiscoveryMode) -> bool {
        self.supported_tool_discovery.contains(&mode)
    }

    /// Validates a discovery selection before persisting settings or building a model.
    /// # Errors
    /// Returns an error when the transport does not implement this mode.
    pub fn validate_tool_discovery(&self, mode: ToolDiscoveryMode) -> Result<()> {
        if !self.supports_tool_discovery(mode) {
            return Err(Error::Config(format!(
                "provider `{}` does not support {mode:?} tool discovery",
                self.id
            )));
        }
        Ok(())
    }

    /// Reports whether chat model IDs and reasoning efforts must come from the advertised catalog.
    #[must_use]
    pub const fn locked_models(&self) -> bool {
        self.locked_models
    }

    #[must_use]
    /// Returns the configurable base URL.
    pub const fn configurable_base_url(&self) -> bool {
        self.default_base_url.is_some()
    }

    #[must_use]
    /// Returns the default base URL.
    pub const fn default_base_url(&self) -> Option<&'static BaseUrl> {
        self.default_base_url
    }

    /// Reports whether a resolved base URL selects the provider's default endpoint.
    #[must_use]
    pub fn uses_default_endpoint(&self, base_url: Option<&str>) -> bool {
        uses_default_endpoint(self.default_base_url.map(BaseUrl::as_str), base_url)
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

    /// Builds one runtime model from resolved provider settings.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn build(&self, mut config: ProviderBuildConfig) -> Result<Arc<dyn Model>> {
        if matches!(config.credential, ProviderCredential::Credentialless) {
            self.validate_credentialless_endpoint(config.base_url.as_ref())?;
        }
        let base_url = self.validate_base_url(config.base_url.as_ref())?;
        if config.capability.is_none() {
            if self.locked_models && config.reasoning_effort.is_none() {
                config.reasoning_effort = self
                    .model(&config.model)
                    .and_then(|model| model.default_reasoning.as_deref())
                    .map(str::to_owned);
            }
            self.validate_model(&config.model, config.reasoning_effort.as_deref())?;
            self.validate_web_search(config.web_search)?;
        } else if config.model.trim().is_empty() {
            return Err(Error::Config(format!(
                "provider `{}` requires a model",
                self.id
            )));
        }
        let tool_discovery = *config
            .tool_discovery
            .get_or_insert_with(|| self.tool_discovery(&config.model, Some(base_url.as_str())));
        self.validate_tool_discovery(tool_discovery)?;
        let model = (self.builder)(config, base_url)?;
        if model.tool_discovery() != tool_discovery {
            return Err(Error::Config(format!(
                "provider `{}` built a model with inconsistent tool discovery",
                self.id
            )));
        }
        Ok(model)
    }

    /// Validates provider-specific settings without resolving credentials.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn build_config_is_valid(
        &self,
        model: &str,
        base_url: Option<&BaseUrl>,
        reasoning_effort: Option<&str>,
        web_search: HostedWebSearch,
    ) -> Result<()> {
        self.model_config_is_valid(model, base_url, reasoning_effort)?;
        self.validate_web_search(web_search)
    }

    fn validate_web_search(&self, web_search: HostedWebSearch) -> Result<()> {
        if !self.web_search.contains(&web_search) {
            return Err(Error::Config(format!(
                "provider `{}` does not support web search mode `{}`",
                self.id,
                web_search.id()
            )));
        }
        Ok(())
    }

    /// Validates saved model settings independently of currently available hosted tools.
    /// # Errors
    ///
    /// Returns an error if the model, endpoint, or reasoning effort is invalid.
    pub fn model_config_is_valid(
        &self,
        model: &str,
        base_url: Option<&BaseUrl>,
        reasoning_effort: Option<&str>,
    ) -> Result<()> {
        self.validate_base_url(base_url)?;
        self.validate_model(model, reasoning_effort)
    }

    fn validate_model(&self, model: &str, reasoning_effort: Option<&str>) -> Result<()> {
        if model.trim().is_empty() {
            return Err(Error::Config(format!(
                "provider `{}` requires a model",
                self.id
            )));
        }
        let preset = self.model(model);
        if self.locked_models && preset.is_none() {
            return Err(Error::Config(format!(
                "provider `{}` does not advertise model `{model}`",
                self.id
            )));
        }
        if self.locked_models
            && let Some(effort) = reasoning_effort
            && let Some(preset) = preset
            && !preset.reasoning.iter().any(|preset| preset.id == effort)
        {
            return Err(Error::Config(format!(
                "model `{model}` does not support reasoning effort `{effort}`"
            )));
        }
        Ok(())
    }

    /// Validates this provider's base-URL boundary, resolving an omitted URL to its default.
    /// # Errors
    ///
    /// Returns an error if the supplied value is invalid.
    pub fn validate_base_url(&self, base_url: Option<&BaseUrl>) -> Result<BaseUrl> {
        match (self.default_base_url, base_url) {
            (None, Some(_)) => Err(Error::Config(format!(
                "provider `{}` has a fixed API endpoint",
                self.id
            ))),
            (Some(default), None) => Ok(default.share()),
            (Some(_), Some(base_url)) => Ok(base_url.share()),
            (None, None) => Err(Error::Config(format!(
                "provider `{}` has no API endpoint",
                self.id
            ))),
        }
    }

    /// Validates an explicitly credentialless provider endpoint.
    /// # Errors
    ///
    /// Returns an error if the supplied value is invalid.
    pub fn validate_credentialless_endpoint(&self, base_url: Option<&BaseUrl>) -> Result<()> {
        if !self.supports_credentialless_endpoints() {
            return Err(Error::Config(format!(
                "provider `{}` does not support credentialless endpoints",
                self.id
            )));
        }
        let base_url = base_url
            .ok_or_else(|| Error::Config("credentialless endpoint requires a base URL".into()))?;
        let endpoint = reqwest::Url::parse(base_url.as_str())
            .map_err(|error| Error::Config(format!("invalid base URL: {error}")))?;
        if endpoint.scheme() != "https" {
            return Err(Error::Config(
                "credentialless endpoint must use HTTPS".into(),
            ));
        }
        if self.default_base_url.is_some_and(|default| {
            reqwest::Url::parse(default.as_str())
                .is_ok_and(|default| same_origin(&default, &endpoint))
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

/// A provider API root checked once, preserving its exact credential-scope spelling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct BaseUrl(Arc<str>);

impl BaseUrl {
    /// Accepts HTTPS, or loopback HTTP, without credentials, a query or a fragment.
    /// # Errors
    /// Returns a configuration error naming the violated URL rule.
    pub fn new(base_url: impl Into<Arc<str>>) -> Result<Self> {
        let base_url = base_url.into();
        let url = reqwest::Url::parse(&base_url)
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
        Ok(Self(base_url))
    }

    /// Returns the unchanged, validated root.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Shares this validated root without copying or checking its text again.
    #[must_use]
    pub fn share(&self) -> Self {
        Self(Arc::clone(&self.0))
    }

    pub(super) fn root(&self) -> &str {
        self.0.trim_end_matches('/')
    }
}

impl TryFrom<String> for BaseUrl {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl std::str::FromStr for BaseUrl {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl std::ops::Deref for BaseUrl {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for BaseUrl {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for BaseUrl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

static PROVIDERS: std::sync::LazyLock<[ProviderDefinition; 8]> = std::sync::LazyLock::new(|| {
    [
        super::openai_socket::provider(),
        super::openai_codex::provider(),
        super::deepseek::provider(),
        super::kimi::provider(),
        super::mistral::provider(),
        super::openrouter::provider(),
        super::anthropic::provider(),
        super::openai::generic_provider(),
    ]
});

/// Returns every built-in provider in setup-menu order.
#[must_use]
pub fn providers() -> &'static [ProviderDefinition] {
    &*PROVIDERS
}

/// Returns the provider used by an unconfigured composition.
#[must_use]
#[expect(
    clippy::indexing_slicing,
    reason = "PROVIDERS is a non-empty static list; provider tests read its first entry"
)]
pub fn default_provider() -> &'static ProviderDefinition {
    &PROVIDERS[0]
}

/// Resolves a built-in provider by its stable manifest ID.
/// # Errors
///
/// Returns an error if validation or an operation required by this function fails.
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
    fn configured_discovery_is_applied_by_each_api_key_transport() {
        for id in [
            "openai_socket",
            "responses",
            "openrouter",
            "anthropic",
            "deepseek",
            "kimi",
            "mistral",
        ] {
            let definition = provider(id).unwrap();
            for discovery in [ToolDiscoveryMode::Native, ToolDiscoveryMode::Rebuild] {
                let built = definition.build(ProviderBuildConfig {
                    capability: None,
                    tool_discovery: Some(discovery),
                    credential: ProviderCredential::ApiKey("test-key".into()),
                    model: definition.default_model().unwrap_or("custom-model").into(),
                    base_url: None,
                    reasoning_effort: None,
                    service_tier: None,
                    web_search: HostedWebSearch::Off,
                    http: reqwest::Client::new(),
                    transport: super::super::ModelTransportSettings::default(),
                });
                if definition.supports_tool_discovery(discovery) {
                    assert_eq!(built.unwrap().tool_discovery(), discovery, "{id}");
                } else {
                    assert!(
                        built
                            .err()
                            .unwrap()
                            .to_string()
                            .contains("does not support Native tool discovery")
                    );
                }
            }
        }
        let model = &provider("anthropic").unwrap().models()[0];
        assert_eq!(
            serde_json::from_str::<ModelPreset>(&serde_json::to_string(model).unwrap()).unwrap(),
            *model
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
            let mut search_modes = Vec::new();
            for search in provider.web_search() {
                assert!(!search_modes.contains(&search.id()));
                search_modes.push(search.id());
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
                provider.default_model(),
                provider.models().first().map(|model| model.id.as_str()),
                "provider `{}` must use its first preset as the default model",
                provider.id()
            );
            let mut model_ids = BTreeSet::new();
            for model in provider.models() {
                assert!(
                    model_ids.insert(model.id.as_str()),
                    "duplicate model `{}`",
                    model.id
                );
                assert!(!model.label.trim().is_empty());
                assert!(!model.description.trim().is_empty());
                assert!(model.context_window > 0);

                let mut reasoning_ids = BTreeSet::new();
                for reasoning in &model.reasoning {
                    assert!(reasoning_ids.insert(reasoning.id.as_str()));
                    assert!(!reasoning.label.trim().is_empty());
                    assert!(!reasoning.description.trim().is_empty());
                }
                assert!(
                    model
                        .default_reasoning
                        .as_deref()
                        .is_none_or(|default| reasoning_ids.contains(default))
                );
            }
        }
    }

    #[test]
    fn omitted_provider_endpoints_resolve_to_advertised_defaults() {
        for definition in providers() {
            let model = definition.default_model().unwrap_or("test-model");
            definition
                .build_config_is_valid(model, None, None, HostedWebSearch::Off)
                .expect("omitted URL selects the advertised default");
            if matches!(definition.auth(), ProviderAuth::ApiKey(_)) {
                definition
                    .build(ProviderBuildConfig {
                        capability: None,
                        tool_discovery: None,
                        credential: ProviderCredential::ApiKey("test-secret".into()),
                        model: model.into(),
                        base_url: None,
                        reasoning_effort: None,
                        service_tier: None,
                        web_search: HostedWebSearch::Off,
                        http: reqwest::Client::new(),
                        transport: super::super::ModelTransportSettings::default(),
                    })
                    .expect("provider constructor receives its advertised default");
            }
            if definition.supports_credentialless_endpoints() {
                definition
                    .validate_credentialless_endpoint(None)
                    .expect_err("credentialless endpoints still require an explicit custom URL");
            }
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
            BTreeSet::from([
                "anthropic",
                "deepseek",
                "kimi",
                "mistral",
                "openrouter",
                "responses"
            ])
        );
        let proxy = BaseUrl::new("https://proxy.example/v1").expect("proxy URL");
        let error = provider("openai_socket")
            .expect("fixed-endpoint provider")
            .validate_credentialless_endpoint(Some(&proxy))
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
                .validate_credentialless_endpoint(Some(&proxy))
                .expect("a non-default HTTPS endpoint is accepted");
            definition
                .validate_credentialless_endpoint(Some(base_url))
                .expect_err("the provider's own endpoint still requires authentication");
            definition
                .build(ProviderBuildConfig {
                    capability: None,
                    tool_discovery: None,
                    credential: ProviderCredential::Credentialless,
                    model: definition
                        .default_model()
                        .unwrap_or("test-model")
                        .to_owned(),
                    base_url: Some(proxy.share()),
                    reasoning_effort: None,
                    service_tier: None,
                    web_search: HostedWebSearch::Off,
                    http: reqwest::Client::new(),
                    transport: crate::backend::model::ModelTransportSettings::default(),
                })
                .expect("credentialless provider builds without a credential");
        }
    }
}
