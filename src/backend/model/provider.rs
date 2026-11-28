//! Provider-owned setup metadata and model construction.

use super::endpoints::Operation;
use std::any::Any;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;

use super::Model;
use crate::Error;
use crate::Result;
use crate::protocol::FrontendSymbol;
use crate::protocol::ModelCapability;
use crate::protocol::ToolDiscoveryMode;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderMetadata {
    pub(super) language_apis: Vec<super::endpoints::EndpointApi>,
    pub(super) image_endpoint: Option<String>,
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
    /// Documented `tool_choice` value that forces one named tool; `"{name}"` is the tool.
    /// Omitted where the provider rejects forcing, so those requests stay on `auto`.
    pub(super) required_tool_choice: Option<serde_json::Value>,
    pub(super) tool_search_type: Option<String>,
    pub(super) web_search_type: Option<String>,
    /// Path under the base URL that answers decision-model requests.
    #[serde(default)]
    pub(super) decisions_path: Option<String>,
    /// Transcription dialect and realtime models offered through this provider protocol.
    pub(super) dictation: Option<super::dictation::DictationApi>,
}

/// One request's wire `tool_choice`: `auto`, or the manifest value forcing its required tool.
#[derive(Debug, Clone, Copy)]
pub(super) struct ToolChoice<'a>(Option<(&'a serde_json::Value, &'a str)>);

impl<'a> ToolChoice<'a> {
    pub(super) const AUTO: Self = Self(None);

    /// Forces `required` when the provider manifest supplies a `template` for it.
    pub(super) fn new(template: Option<&'a serde_json::Value>, required: Option<&'a str>) -> Self {
        Self(template.zip(required))
    }
}

impl serde::Serialize for ToolChoice<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap as _, SerializeSeq as _};
        use serde_json::Value;
        let Some((template, name)) = self.0 else {
            return serializer.serialize_str("auto");
        };
        match template {
            Value::String(text) if text == "{name}" => serializer.serialize_str(name),
            Value::Array(items) => {
                let mut sequence = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    sequence.serialize_element(&Self(Some((item, name))))?;
                }
                sequence.end()
            }
            Value::Object(fields) => {
                let mut map = serializer.serialize_map(Some(fields.len()))?;
                for (key, value) in fields {
                    map.serialize_entry(key, &Self(Some((value, name))))?;
                }
                map.end()
            }
            other => other.serialize(serializer),
        }
    }
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
/// Additional trust roots for shared HTTP clients.
pub use reqwest::Certificate as HttpCertificate;
pub use reqwest::Client as HttpClient;
pub use reqwest::Response as HttpResponse;
/// Redirect policy for shared HTTP clients.
pub use reqwest::redirect::Policy as HttpRedirectPolicy;

/// Builds a reqwest client with the application's TLS provider, defaulting to ring.
pub fn http_client_builder() -> reqwest::ClientBuilder {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // Concurrent builders may race to install the process-wide provider. The first wins.
        drop(rustls::crypto::ring::default_provider().install_default());
    }
    HttpClient::builder()
}

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
#[serde(try_from = "CatalogFile")]
pub(super) struct ModelCatalog {
    pub(super) default_model: Option<String>,
    pub(super) image_input_models: Vec<String>,
    pub(super) models: Vec<ModelPreset>,
    pub(super) image_models: Vec<MediaModelPreset>,
}

/// File shape of a catalog: reasoning presets are listed once and referenced by models.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogFile {
    default_model: Option<String>,
    #[serde(default)]
    image_input_models: Vec<String>,
    #[serde(default)]
    reasoning: Vec<ReasoningPreset>,
    #[serde(default)]
    models: Vec<CatalogModel>,
    #[serde(default)]
    image_models: Vec<MediaModelPreset>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogModel {
    id: String,
    label: String,
    description: String,
    context_window: i64,
    /// Catalog preset IDs in display order; omission accepts every catalog preset.
    reasoning: Option<Vec<String>>,
    default_reasoning: Option<String>,
    tool_discovery: ToolDiscoveryMode,
}

impl TryFrom<CatalogFile> for ModelCatalog {
    type Error = Error;

    fn try_from(file: CatalogFile) -> Result<Self> {
        unique_ids(file.reasoning.iter().map(|preset| preset.id.as_str()))?;
        let preset = |id: &str| {
            file.reasoning
                .iter()
                .find(|preset| preset.id == id)
                .ok_or_else(|| Error::Internal(format!("reasoning `{id}` is not in the catalog")))
        };
        let models = file
            .models
            .into_iter()
            .map(|model| {
                // Each advertised model owns its presets; the catalog lists them once.
                let reasoning = match &model.reasoning {
                    Some(ids) => ids
                        .iter()
                        .map(|id| preset(id).cloned()) // rules-allow: each model preset owns its advertised efforts
                        .collect::<Result<_>>()?,
                    None => file.reasoning.clone(), // rules-allow: each model preset owns its advertised efforts
                };
                Ok(ModelPreset {
                    id: model.id,
                    label: model.label,
                    description: model.description,
                    context_window: model.context_window,
                    reasoning,
                    default_reasoning: model.default_reasoning,
                    tool_discovery: model.tool_discovery,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            default_model: file.default_model,
            image_input_models: file.image_input_models,
            models,
            image_models: file.image_models,
        })
    }
}

impl ModelCatalog {
    /// Loads a provider catalog, preferring a valid same-named file in the override directory.
    pub(super) fn load(name: &str, text: &str) -> Self {
        crate::config::overridable(name, text, Self::validate)
    }

    pub(super) fn validate(&self) -> Result<()> {
        unique_ids(self.image_input_models.iter().map(String::as_str))?;
        for id in &self.image_input_models {
            if !self.models.iter().any(|model| &model.id == id) {
                return Err(Error::Internal(format!(
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
                return Err(Error::Internal(format!(
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
                return Err(Error::Internal(format!(
                    "model `{}` default reasoning `{effort}` is not advertised",
                    model.id
                )));
            }
        }
        if let Some(default) = &self.default_model
            && !self.models.iter().any(|model| &model.id == default)
        {
            return Err(Error::Internal(format!(
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
            return Err(Error::Internal(format!(
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
            _ => Err(Error::Invalid(format!(
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

/// Provider-owned account authentication consumed generically by applications.
pub use super::openai_codex::BrowserAuth;
/// The concrete pending device authentication flow.
pub use super::openai_codex::DeviceLogin;

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
            Self::Browser(_) | Self::Credentialless => Err(Error::Invalid(format!(
                "provider `{provider}` requires an API key"
            ))),
        }
    }

    pub(super) fn into_optional_api_key(self, provider: &str) -> Result<Option<String>> {
        match self {
            Self::ApiKey(api_key) => Ok(Some(api_key)),
            Self::Credentialless => Ok(None),
            Self::Browser(_) => Err(Error::Invalid(format!(
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
                Err(_) => Err(Error::Invalid(format!(
                    "provider `{provider}` received wrong login; expected {}",
                    std::any::type_name::<T>()
                ))),
            },
            Self::ApiKey(_) | Self::Credentialless => Err(Error::Invalid(format!(
                "provider `{provider}` requires browser login"
            ))),
        }
    }
}

/// Constructs one model from settings with the endpoint and tool discovery already resolved.
type ProviderBuilder =
    fn(ProviderBuildConfig, BaseUrl, ToolDiscoveryMode) -> Result<Arc<dyn Model>>;

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
    voice_waits_for_session_started: bool,
    tool_discovery: ToolDiscoveryMode,
    supported_tool_discovery: &'static [ToolDiscoveryMode],
    custom_endpoint_tool_discovery: Option<ToolDiscoveryMode>,
    default_base_url: &'static BaseUrl,
    native_custom_endpoints: bool,
    credentialless_endpoints: bool,
    builder: ProviderBuilder,
    pub(super) replay_reasoning_field: Option<&'static str>,
    pub(super) web_search_type: Option<&'static str>,
    pub(super) decisions_path: Option<&'static str>,
    pub(super) max_output_tokens: Option<u64>,
    pub(super) language_apis: &'static [super::endpoints::EndpointApi],
    pub(super) image_endpoint: Option<&'static str>,
    pub(super) voice_endpoint: Option<super::realtime::VoiceApi>,
    pub(super) dictation: Option<&'static super::dictation::DictationApi>,
    pub(super) headers: &'static std::collections::BTreeMap<String, String>,
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
            decisions_path: metadata.decisions_path.as_deref(),
            max_output_tokens: metadata.max_output_tokens,
            language_apis: &metadata.language_apis,
            image_endpoint: metadata.image_endpoint.as_deref(),
            voice_endpoint: None,
            dictation: metadata.dictation.as_ref(),
            headers: &metadata.headers,
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
            voice_waits_for_session_started: false,
            tool_discovery: metadata.tool_discovery,
            supported_tool_discovery: &metadata.supported_tool_discovery,
            custom_endpoint_tool_discovery: metadata.custom_endpoint_tool_discovery,
            default_base_url: &metadata.base_url,
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

    pub(super) const fn with_voice_models(
        mut self,
        models: &'static [MediaModelPreset],
        api: super::realtime::VoiceApi,
    ) -> Self {
        self.voice_endpoint = Some(api);
        self.voice_models = models;
        self.voice_waits_for_session_started = api.waits_for_session_started();
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
    /// Whether frontends wait for the provider's session-started event before streaming a
    /// voice call's audio.
    pub const fn voice_waits_for_session_started(&self) -> bool {
        self.voice_waits_for_session_started
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
            ModelCapability::Decisions => self.decisions_path.is_some(),
        }
    }

    /// Reports a capability for the selected provider endpoint.
    #[must_use]
    pub fn supports_at(&self, capability: ModelCapability, base_url: Option<&str>) -> bool {
        (self.native_custom_endpoints || self.uses_default_endpoint(base_url))
            && self.supports(capability)
    }

    /// Whether an operator can explicitly provision this adapter at the selected endpoint.
    /// Compatible media adapters accept opt-in models even when native presets do not apply.
    #[must_use]
    pub fn can_configure_at(&self, capability: ModelCapability, base_url: Option<&str>) -> bool {
        self.supports_at(capability, base_url)
            || (self.supports_credentialless_endpoints()
                && self.supports(capability)
                && matches!(
                    capability,
                    ModelCapability::ImageGeneration | ModelCapability::RealtimeVoice
                ))
    }

    /// Built-in media presets available at the selected origin.
    #[must_use]
    pub fn media_presets_at(
        &self,
        capability: ModelCapability,
        base_url: Option<&str>,
    ) -> &'static [MediaModelPreset] {
        if !self.supports_at(capability, base_url) {
            return &[];
        }
        match capability {
            ModelCapability::ImageGeneration => self.image_models(),
            ModelCapability::RealtimeVoice => self.voice_models(),
            ModelCapability::Decisions => &[],
        }
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
            Some(self.default_base_url.as_str()),
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
            return Err(Error::Invalid(format!(
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
    /// Returns the default base URL.
    pub const fn default_base_url(&self) -> &'static BaseUrl {
        self.default_base_url
    }

    /// Reports whether a resolved base URL selects the provider's default endpoint.
    #[must_use]
    pub fn uses_default_endpoint(&self, base_url: Option<&str>) -> bool {
        uses_default_endpoint(Some(self.default_base_url.as_str()), base_url)
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
        let base_url = self.resolve_base_url(config.base_url.as_ref());
        if base_url
            .operation
            .as_ref()
            .is_some_and(|operation| operation.provider != self.id())
        {
            return Err(Error::Invalid(
                "model endpoint belongs to a different provider".into(),
            ));
        }
        if config.capability.is_none() {
            if self.locked_models && config.reasoning_effort.is_none() {
                config.reasoning_effort = self
                    .model(&config.model)
                    .and_then(|model| model.default_reasoning.as_deref())
                    .map(str::to_owned);
            }
            self.validate_model(&config.model, config.reasoning_effort.as_deref())?;
            self.validate_endpoint_web_search(&base_url, config.web_search)?;
        } else if config.model.trim().is_empty() {
            return Err(Error::Invalid(format!(
                "provider `{}` requires a model",
                self.id
            )));
        }
        if config.capability.is_none() && base_url.endpoint_tool_discovery().is_some() {
            return super::chat_completions::build_compatible(self, config, base_url);
        }
        let tool_discovery = config
            .tool_discovery
            .unwrap_or_else(|| self.tool_discovery(&config.model, Some(base_url.as_str())));
        self.validate_tool_discovery(tool_discovery)?;
        let model = (self.builder)(config, base_url, tool_discovery)?;
        if model.tool_discovery() != tool_discovery {
            return Err(Error::Internal(format!(
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
        reasoning_effort: Option<&str>,
        web_search: HostedWebSearch,
    ) -> Result<()> {
        self.model_config_is_valid(model, reasoning_effort)?;
        self.validate_web_search(web_search)
    }

    /// Validates the selected search mode against the provider and the operation's wire format.
    /// # Errors
    /// Returns an error when the language adapter cannot encode the selected hosted tool.
    pub fn validate_endpoint_web_search(
        &self,
        base: &BaseUrl,
        search: HostedWebSearch,
    ) -> Result<()> {
        self.validate_web_search(search)?;
        if search != HostedWebSearch::Off
            && base.endpoint_tool_discovery().is_some()
            && self.web_search_type.is_none()
        {
            return Err(Error::Invalid(
                "this Chat Completions API requires hosted search to be off".into(),
            ));
        }
        Ok(())
    }

    fn validate_web_search(&self, web_search: HostedWebSearch) -> Result<()> {
        if !self.web_search.contains(&web_search) {
            return Err(Error::Invalid(format!(
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
    pub fn model_config_is_valid(&self, model: &str, reasoning_effort: Option<&str>) -> Result<()> {
        self.validate_model(model, reasoning_effort)
    }

    fn validate_model(&self, model: &str, reasoning_effort: Option<&str>) -> Result<()> {
        if model.trim().is_empty() {
            return Err(Error::Invalid(format!(
                "provider `{}` requires a model",
                self.id
            )));
        }
        let preset = self.model(model);
        if self.locked_models && preset.is_none() {
            return Err(Error::Invalid(format!(
                "provider `{}` does not advertise model `{model}`",
                self.id
            )));
        }
        if self.locked_models
            && let Some(effort) = reasoning_effort
            && let Some(preset) = preset
            && !preset.reasoning.iter().any(|preset| preset.id == effort)
        {
            return Err(Error::Invalid(format!(
                "model `{model}` does not support reasoning effort `{effort}`"
            )));
        }
        Ok(())
    }

    /// Resolves an omitted base URL to this provider's default endpoint.
    #[must_use]
    pub fn resolve_base_url(&self, base_url: Option<&BaseUrl>) -> BaseUrl {
        base_url.unwrap_or(self.default_base_url).share()
    }

    /// Validates an explicitly credentialless provider endpoint.
    /// # Errors
    ///
    /// Returns an error if the supplied value is invalid.
    pub fn validate_credentialless_endpoint(&self, base_url: Option<&BaseUrl>) -> Result<()> {
        if !self.supports_credentialless_endpoints() {
            return Err(Error::Invalid(format!(
                "provider `{}` does not support credentialless endpoints",
                self.id
            )));
        }
        let base_url = base_url
            .ok_or_else(|| Error::Invalid("credentialless endpoint requires a base URL".into()))?;
        let endpoint = reqwest::Url::parse(base_url.as_str())
            .map_err(|error| Error::Invalid(format!("invalid base URL: {error}")))?;
        if endpoint.scheme() != "https" {
            return Err(Error::Invalid(
                "credentialless endpoint must use HTTPS".into(),
            ));
        }
        if reqwest::Url::parse(self.default_base_url.as_str())
            .is_ok_and(|default| same_origin(&default, &endpoint))
        {
            return Err(Error::Invalid(format!(
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
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "String")]
pub struct BaseUrl {
    root: Arc<str>,
    operation: Option<Arc<Operation>>,
}

// Operations are ephemeral adapter state. The serialized root identifies the credential scope.
impl PartialEq for BaseUrl {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
    }
}

impl Eq for BaseUrl {}

impl Serialize for BaseUrl {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl BaseUrl {
    /// Accepts HTTPS, or loopback HTTP, without credentials, a query or a fragment.
    /// # Errors
    /// Returns a configuration error naming the violated URL rule.
    pub fn new(base_url: impl Into<Arc<str>>) -> Result<Self> {
        let base_url = base_url.into();
        let url = reqwest::Url::parse(&base_url)
            .map_err(|error| Error::Invalid(format!("invalid base URL: {error}")))?;
        let host = url
            .host_str()
            .ok_or_else(|| Error::Invalid("base URL requires a host".into()))?;
        let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]");
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            return Err(Error::Invalid(
                "base URL must use HTTPS, except for loopback HTTP".into(),
            ));
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Invalid(
                "base URL cannot contain credentials, a query, or a fragment".into(),
            ));
        }
        Ok(Self {
            root: base_url,
            operation: None,
        })
    }

    /// Returns the unchanged, validated root.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.root
    }

    /// Shares this validated root without copying or checking its text again.
    #[must_use]
    pub fn share(&self) -> Self {
        Self {
            root: Arc::clone(&self.root),
            operation: self.operation.as_ref().map(Arc::clone),
        }
    }

    pub(super) fn operation_dictation_mode(&self) -> Option<super::dictation::DictationMode> {
        self.operation.as_ref()?.dictation_mode
    }

    pub(super) fn with_operation(&self, operation: Arc<Operation>) -> Self {
        Self {
            root: Arc::clone(&self.root),
            operation: Some(operation),
        }
    }

    /// Effective URL of this model's explicitly selected operation.
    #[must_use]
    pub fn model_endpoint_url(&self) -> Option<&str> {
        self.operation.as_ref().map(|op| op.url.as_str())
    }

    /// Tool discovery mode imposed by a per-model protocol selection.
    #[must_use]
    pub fn endpoint_tool_discovery(&self) -> Option<ToolDiscoveryMode> {
        self.operation
            .as_ref()
            .filter(|op| op.compatible_chat)
            .map(|_| ToolDiscoveryMode::Rebuild)
    }

    pub(super) fn operation_url(&self, path: &str) -> String {
        if let Some(operation) = self
            .operation
            .as_deref()
            .filter(|op| op.default_path == path || op.edit_path == Some(path))
        {
            // A prefixed native image API keeps its sibling edit route. A custom
            // operation name is a single endpoint for both request shapes.
            if path != operation.default_path
                && let Some(prefix) = operation
                    .url
                    .strip_prefix(self.root())
                    .and_then(|suffix| suffix.strip_suffix(operation.default_path))
                    .filter(|prefix| prefix.ends_with('/'))
            {
                return format!("{}{prefix}{path}", self.root());
            }
            return operation.url.clone(); // rules-allow: the request owns its URL
        }
        format!("{}/{path}", self.root())
    }

    pub(super) fn root(&self) -> &str {
        self.root.trim_end_matches('/')
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

static PROVIDERS: std::sync::LazyLock<[ProviderDefinition; 10]> = std::sync::LazyLock::new(|| {
    [
        super::openai_socket::provider(),
        super::openai_codex::provider(),
        super::deepseek::provider(),
        super::kimi::provider(),
        super::mistral::provider(),
        super::openrouter::provider(),
        super::a2agent::provider(),
        super::aimlapi::provider(),
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
    fn custom_image_operation_covers_edits_and_preserves_serialized_root_identity() {
        use super::super::endpoints::ProviderModelKind;
        let provider = provider("openai_socket").unwrap();
        let base = provider.default_base_url();
        let overridden = provider
            .with_group_endpoint(ProviderModelKind::Image, base, "images", "custom/images")
            .unwrap();
        for path in ["images/generations", "images/edits"] {
            assert_eq!(
                overridden.operation_url(path),
                format!("{}/custom/images", base.root())
            );
            assert_eq!(base.operation_url(path), format!("{}/{path}", base.root()));
        }
        let restored: BaseUrl =
            serde_json::from_value(serde_json::to_value(&overridden).unwrap()).unwrap();
        assert_eq!(&overridden, base);
        assert_eq!(overridden, restored);
    }

    #[test]
    fn native_image_overrides_preserve_sibling_edit_paths_under_a_prefix() {
        use super::super::endpoints::ProviderModelKind;
        for id in ["openai_socket", "openai_codex", "responses"] {
            let definition = provider(id).unwrap();
            let base = definition.default_base_url();
            for (generation, edit) in [
                ("images/generations", "images/edits"),
                ("/images/generations", "images/edits"),
                ("/v2/images/generations", "v2/images/edits"),
                ("v2/notimages/generations", "v2/notimages/generations"),
                ("custom/images", "custom/images"),
            ] {
                let bound = definition
                    .with_group_endpoint(ProviderModelKind::Image, base, "images", generation)
                    .unwrap();
                assert_eq!(
                    bound.operation_url("images/generations"),
                    format!("{}/{}", base.root(), generation.trim_start_matches('/'))
                );
                assert_eq!(
                    bound.operation_url("images/edits"),
                    format!("{}/{edit}", base.root())
                );
            }
        }
        let base = BaseUrl::new("https://proxy.example/images").unwrap();
        let bound = provider("responses")
            .unwrap()
            .with_group_endpoint(ProviderModelKind::Image, &base, "images", "generations")
            .unwrap();
        assert_eq!(
            bound.operation_url("images/edits"),
            "https://proxy.example/images/generations"
        );
    }

    #[test]
    fn current_provider_catalogs_advertise_supported_models_and_efforts() {
        assert!(
            provider("openai_socket")
                .unwrap()
                .models()
                .iter()
                .any(|model| model.id == "gpt-6.1-terra")
        );
        assert!(
            provider("anthropic")
                .unwrap()
                .models()
                .iter()
                .any(|model| model.id == "claude-fable-5-2")
        );
        let mistral = provider("mistral").unwrap();
        let large = mistral
            .models()
            .iter()
            .find(|model| model.id == "mistral-large-4-0")
            .unwrap();
        assert_eq!(
            large
                .reasoning
                .iter()
                .map(|effort| effort.id.as_str())
                .collect::<Vec<_>>(),
            ["low", "medium", "high"]
        );
        assert_eq!(large.default_reasoning.as_deref(), Some("high"));
    }

    #[test]
    fn required_tool_choice_is_each_manifest_s_documented_wire_value() {
        let choice = |manifest: &'static ProviderMetadata| {
            serde_json::to_value(ToolChoice::new(
                manifest.required_tool_choice.as_ref(),
                Some("write_handoff"),
            ))
            .expect("tool choice")
        };
        assert_eq!(
            choice(&super::super::openai_socket::MANIFEST),
            serde_json::json!({"type": "function", "name": "write_handoff"})
        );
        assert_eq!(
            choice(&super::super::mistral::MANIFEST),
            serde_json::json!({"type": "function", "function": {"name": "write_handoff"}})
        );
        // No documented way to force a tool: the request stays on `auto`.
        assert_eq!(choice(&super::super::kimi::MANIFEST), "auto");
        assert_eq!(
            serde_json::to_value(ToolChoice::new(
                super::super::mistral::MANIFEST
                    .required_tool_choice
                    .as_ref(),
                None
            ))
            .expect("tool choice"),
            "auto"
        );
    }

    #[test]
    fn configured_discovery_is_applied_by_each_api_key_transport() {
        for id in [
            "openai_socket",
            "responses",
            "openrouter",
            "aimlapi",
            "a2agent",
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
                    http: crate::backend::model::ModelTransportSettings::default()
                        .streaming_client()
                        .unwrap(),
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
    fn omitted_provider_endpoints_resolve_to_advertised_defaults() {
        for definition in providers() {
            let model = definition.default_model().unwrap_or("test-model");
            definition
                .build_config_is_valid(model, None, HostedWebSearch::Off)
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
                        http: crate::backend::model::ModelTransportSettings::default()
                            .streaming_client()
                            .unwrap(),
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
            let base_url = definition.default_base_url();
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
                    http: crate::backend::model::ModelTransportSettings::default()
                        .streaming_client()
                        .unwrap(),
                    transport: crate::backend::model::ModelTransportSettings::default(),
                })
                .expect("credentialless provider builds without a credential");
        }
    }
}
