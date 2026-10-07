//! Gateway provider catalog, route policy, and credential availability.

use std::collections::BTreeMap;

use mobius::backend::model::provider::{
    ProviderAuth, ProviderDefinition, ReasoningPreset, provider, providers,
};
use mobius::middleware::manifest::ModelCatalogs;
use mobius::protocol::{FrontendSettingOption, FrontendTone, ModelCapability, ModelChoice};

use crate::config::{
    ConfigStore, ConfiguredProvider, CredentialStore, DEFAULT_CONTEXT_WINDOW, GatewayConfig,
    model_route_id,
};
use crate::wire::{
    ProviderAuthKind, ProviderConfig, ProviderEndpointAuth, ProviderInstance, ProviderModel,
    ProviderStatus, ReasoningChoice,
};
use crate::{Error, Result};

pub(crate) fn provider_statuses() -> Vec<ProviderStatus> {
    providers().iter().map(provider_status).collect()
}

pub(crate) fn selected_base_url<'a>(
    definition: &ProviderDefinition,
    selection: &'a ProviderConfig,
) -> Option<&'a str> {
    definition
        .configurable_base_url()
        .then(|| {
            selection
                .base_url
                .as_deref()
                .or_else(|| definition.default_base_url())
        })
        .flatten()
}

pub(crate) fn provider_instances(
    configured_providers: &BTreeMap<String, ConfiguredProvider>,
    store: &ConfigStore,
    credentials: &CredentialStore,
) -> Result<Vec<ProviderInstance>> {
    configured_providers
        .values()
        .map(|configured| {
            let definition = provider(&configured.selection.provider)?;
            let base_url = selected_base_url(definition, &configured.selection);
            Ok(ProviderInstance {
                label: configured.label.clone(),
                tint: configured.tint,
                configured: credential_is_configured(&configured.selection, store, credentials)?,
                credential_hint: credentials.hint(
                    &configured.selection.instance,
                    definition.id(),
                    base_url,
                )?,
                selection: configured.selection.clone(),
                model_ids: configured.model_ids.clone(),
                reasoning_efforts: configured.reasoning_efforts.clone(),
                image_model_ids: configured.image_model_ids.clone(),
            })
        })
        .collect()
}

/// Chat and media choices derived from one set of chat routes.
pub(crate) struct ConfiguredChoices {
    pub(crate) models: Vec<ModelChoice>,
    pub(crate) images: Vec<ModelChoice>,
    voices: Vec<ModelChoice>,
}

impl ConfiguredChoices {
    fn from_routes(gateway: &GatewayConfig, routes: Vec<CatalogRoute>) -> Result<Self> {
        let media = media_routes(&gateway.configured_providers, &routes)?;
        Ok(Self {
            images: media.images.into_iter().map(|media| media.choice).collect(),
            voices: media.voices.into_iter().map(|media| media.choice).collect(),
            models: routes.into_iter().map(|route| route.choice).collect(),
        })
    }

    pub(crate) fn validate_voice(&self, route: Option<&str>) -> Result<()> {
        if let Some(route) = route
            && !self.voices.iter().any(|choice| choice.route == route)
        {
            return Err(Error::Config(format!(
                "voice route `{route}` is not configured; select an available voice"
            )));
        }
        Ok(())
    }

    /// Borrows the lists dynamic middleware settings select from.
    pub(crate) fn catalogs(&self) -> ModelCatalogs<'_> {
        ModelCatalogs {
            models: &self.models,
            images: &self.images,
        }
    }
}

pub(crate) fn configured_model_choices(
    gateway: &GatewayConfig,
    store: &ConfigStore,
    credentials: &CredentialStore,
) -> Result<ConfiguredChoices> {
    ConfiguredChoices::from_routes(
        gateway,
        configured_model_routes(
            &gateway.configured_providers,
            gateway
                .bot_defaults
                .as_ref()
                .map(|defaults| defaults.config.provider.instance.as_str()),
            store,
            credentials,
        )?,
    )
}

pub(crate) fn configured_model_catalog(gateway: &GatewayConfig) -> Result<ConfiguredChoices> {
    let mut routes = Vec::new();
    for configured in gateway.configured_providers.values() {
        let definition = provider(&configured.selection.provider)?;
        routes.extend(catalog_routes(
            definition,
            configured,
            &configured.selection,
        ));
    }
    ConfiguredChoices::from_routes(gateway, routes)
}

pub(crate) fn configured_model_providers(
    gateway: &GatewayConfig,
    store: &ConfigStore,
    credentials: &CredentialStore,
) -> Result<BTreeMap<String, String>> {
    Ok(configured_model_routes(
        &gateway.configured_providers,
        gateway
            .bot_defaults
            .as_ref()
            .map(|defaults| defaults.config.provider.instance.as_str()),
        store,
        credentials,
    )?
    .into_iter()
    .map(|route| (route.choice.route, route.provider.instance))
    .collect())
}

pub(crate) fn configured_route_exists(gateway: &GatewayConfig, route: &str) -> Result<bool> {
    for configured in gateway.configured_providers.values() {
        let definition = provider(&configured.selection.provider)?;
        if catalog_routes(definition, configured, &configured.selection)
            .iter()
            .any(|candidate| candidate.choice.route == route)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn configured_model_routes(
    configured_providers: &BTreeMap<String, ConfiguredProvider>,
    default_instance: Option<&str>,
    store: &ConfigStore,
    credentials: &CredentialStore,
) -> Result<Vec<CatalogRoute>> {
    let mut routes = Vec::new();
    let mut configured = configured_providers.values().collect::<Vec<_>>();
    configured
        .sort_by_key(|configured| Some(configured.selection.instance.as_str()) != default_instance);
    for configured in configured {
        let definition = provider(&configured.selection.provider)?;
        if !definition
            .web_search()
            .contains(&configured.selection.web_search)
        {
            continue;
        }
        if credential_is_configured(&configured.selection, store, credentials)? {
            routes.extend(catalog_routes(
                definition,
                configured,
                &configured.selection,
            ));
        }
    }
    Ok(routes)
}

pub(crate) fn catalog_routes(
    definition: &ProviderDefinition,
    configured: &ConfiguredProvider,
    selection: &ProviderConfig,
) -> Vec<CatalogRoute> {
    let mut models = definition
        .models()
        .iter()
        .map(|preset| (preset.id.as_str(), Some(preset)))
        .collect::<Vec<_>>();
    for model in &configured.model_ids {
        if models.iter().all(|(candidate, _)| *candidate != model) {
            models.push((model, None));
        }
    }
    models.sort_by_key(|(model, _)| *model != selection.model);

    let mut routes = Vec::new();
    for (model, preset) in models {
        let mut efforts: Vec<(Option<&str>, Option<&str>)> = Vec::new();
        for reasoning in preset.into_iter().flat_map(|preset| &preset.reasoning) {
            let effort = Some(reasoning.id.as_str());
            if efforts.iter().all(|(known, _)| *known != effort) {
                efforts.push((effort, Some(reasoning.label.as_str())));
            }
        }
        if preset.is_none() {
            for reasoning in &configured.reasoning_efforts {
                let effort = Some(reasoning.as_str());
                if efforts.iter().all(|(known, _)| *known != effort) {
                    efforts.push((effort, None));
                }
            }
        }
        if efforts.is_empty() {
            efforts.push((None, None));
        }
        for (effort, variant_label) in efforts {
            let provider = ProviderConfig {
                instance: selection.instance.clone(),
                provider: selection.provider.clone(),
                base_url: selection.base_url.clone(),
                endpoint_auth: selection.endpoint_auth,
                model: model.into(),
                reasoning_effort: effort.map(str::to_string),
                service_tier: selection
                    .service_tier
                    .as_ref()
                    .or(configured.selection.service_tier.as_ref())
                    .cloned(),
                web_search: selection.web_search,
            };
            let route = model_route_id(&selection.instance, model, effort);
            routes.push(CatalogRoute {
                choice: ModelChoice {
                    route,
                    group: format!(
                        "{} · {}",
                        configured.label,
                        preset.map_or(model, |preset| preset.label.as_str())
                    ),
                    model: model.into(),
                    reasoning_effort: effort.map(str::to_string),
                    variant_label: variant_label.map(str::to_string),
                    context_window: Some(
                        preset.map_or(DEFAULT_CONTEXT_WINDOW, |preset| preset.context_window),
                    ),
                    supports_image_input: definition.supports_image_input(),
                    supports_image_generation: definition.supports_at(
                        ModelCapability::ImageGeneration,
                        selection.base_url.as_deref(),
                    ) || !configured.image_model_ids.is_empty(),
                    supports_realtime_voice: definition.supports_at(
                        ModelCapability::RealtimeVoice,
                        selection.base_url.as_deref(),
                    ),
                    tool_discovery: definition.tool_discovery(model, selection.base_url.as_deref()),
                },
                provider,
            });
        }
    }
    routes
}

pub(crate) struct CatalogRoute {
    pub(crate) choice: ModelChoice,
    pub(crate) provider: ProviderConfig,
}

/// An image or voice choice served through the transport of one chat route.
pub(crate) struct MediaRoute {
    pub(crate) choice: ModelChoice,
    pub(crate) instance: String,
    pub(crate) transport: String,
}

/// Image and voice choices for every provider instance that has a chat route.
#[derive(Default)]
pub(crate) struct MediaRoutes {
    pub(crate) images: Vec<MediaRoute>,
    pub(crate) voices: Vec<MediaRoute>,
}

/// Derives image and voice choices from the configured instances behind `routes`.
pub(crate) fn media_routes(
    configured_providers: &BTreeMap<String, ConfiguredProvider>,
    routes: &[CatalogRoute],
) -> Result<MediaRoutes> {
    let mut media = MediaRoutes::default();
    for configured in configured_providers.values() {
        let instance = configured.selection.instance.as_str();
        let Some(transport) = routes.iter().find(|route| {
            route.provider.instance == instance
                && route.provider.endpoint_auth != ProviderEndpointAuth::Credentialless
        }) else {
            continue;
        };
        let definition = provider(&configured.selection.provider)?;
        let media_choice = |model: &str,
                            label: &str,
                            variant: Option<&ReasoningPreset>,
                            voice: bool| MediaRoute {
            choice: ModelChoice {
                route: model_route_id(instance, model, variant.map(|variant| variant.id.as_str())),
                group: format!("{} · {label}", configured.label),
                model: model.into(),
                reasoning_effort: variant.map(|variant| variant.id.as_str().into()),
                variant_label: variant.map(|variant| variant.label.as_str().into()),
                context_window: None,
                supports_image_input: false,
                supports_image_generation: !voice,
                supports_realtime_voice: voice,
                tool_discovery: transport.choice.tool_discovery,
            },
            instance: instance.into(),
            transport: transport.choice.route.as_str().into(),
        };
        if definition.supports_at(
            ModelCapability::ImageGeneration,
            transport.provider.base_url.as_deref(),
        ) {
            for preset in definition.image_models() {
                for quality in with_default(&preset.variants) {
                    media
                        .images
                        .push(media_choice(&preset.id, &preset.label, quality, false));
                }
            }
        }
        // Explicit image IDs opt a compatible endpoint into its image API.
        for id in &configured.image_model_ids {
            media.images.push(media_choice(id, id, None, false));
        }
        if definition.supports_at(
            ModelCapability::RealtimeVoice,
            transport.provider.base_url.as_deref(),
        ) {
            for preset in definition.voice_models() {
                for voice in with_default(&preset.variants) {
                    media
                        .voices
                        .push(media_choice(&preset.id, &preset.label, voice, true));
                }
            }
        }
    }
    Ok(media)
}

/// Yields each variant, or a single `None` when a model has no variants.
fn with_default(variants: &[ReasoningPreset]) -> impl Iterator<Item = Option<&ReasoningPreset>> {
    variants
        .iter()
        .map(Some)
        .chain(variants.is_empty().then_some(None))
}

pub(crate) fn credential_is_configured(
    selection: &ProviderConfig,
    store: &ConfigStore,
    credentials: &CredentialStore,
) -> Result<bool> {
    let definition = provider(&selection.provider)?;
    if selection.endpoint_auth == ProviderEndpointAuth::Credentialless {
        definition.validate_credentialless_endpoint(selection.base_url.as_deref())?;
        return Ok(true);
    }
    let base_url = selected_base_url(definition, selection);
    match definition.auth() {
        ProviderAuth::ApiKey(default_env) => {
            if credentials
                .get(&selection.instance, definition.id(), base_url)?
                .is_some()
            {
                return Ok(true);
            }
            if !definition.uses_default_endpoint(base_url) {
                return Ok(false);
            }
            Ok(std::env::var(default_env).is_ok_and(|value| !value.trim().is_empty()))
        }
        ProviderAuth::Browser(auth) => auth
            .configured(&store.provider_auth_path())
            .map_err(Error::from),
    }
}

fn provider_status(definition: &ProviderDefinition) -> ProviderStatus {
    let (auth, default_api_key_env) = match definition.auth() {
        ProviderAuth::ApiKey(default_env) => (
            ProviderAuthKind::ApiKey,
            (!definition.configurable_base_url()).then(|| default_env.to_string()),
        ),
        ProviderAuth::Browser(_) => (ProviderAuthKind::DeviceCode, None),
    };
    ProviderStatus {
        provider: definition.id().into(),
        label: definition.label().into(),
        symbol: definition.symbol(),
        description: definition.description().into(),
        model_ids_configurable: definition.models().is_empty(),
        image_models: std::borrow::Cow::Borrowed(definition.image_models()),
        image_model_ids_configurable: definition.image_models().is_empty()
            && definition.supports_at(ModelCapability::ImageGeneration, None),
        voice_models: std::borrow::Cow::Borrowed(definition.voice_models()),
        auth,
        default_base_url: definition.default_base_url().map(str::to_string),
        native_custom_endpoints: definition.native_custom_endpoints(),
        default_api_key_env,
        models: definition
            .models()
            .iter()
            .map(|model| ProviderModel {
                id: model.id.clone(),
                label: model.label.clone(),
                description: model.description.clone(),
                context_window: model.context_window,
                reasoning: model
                    .reasoning
                    .iter()
                    .map(|reasoning| ReasoningChoice {
                        id: reasoning.id.clone(),
                        label: reasoning.label.clone(),
                        description: reasoning.description.clone(),
                    })
                    .collect(),
                default_reasoning: model.default_reasoning.clone(),
                tool_discovery: model.tool_discovery,
            })
            .collect(),
        web_search: definition
            .web_search()
            .iter()
            .map(|search| FrontendSettingOption {
                disables: Vec::new(),
                value: search.id().into(),
                label: search.label().into(),
                description: search.description().into(),
                symbol: None,
                tone: FrontendTone::Neutral,
            })
            .collect(),
        tool_discovery: definition.default_tool_discovery(),
        custom_endpoint_tool_discovery: definition.custom_endpoint_tool_discovery(),
        realtime_voices: definition
            .realtime_voices()
            .iter()
            .map(|voice| (*voice).into())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use mobius::backend::model::provider::provider;
    use mobius::protocol::{FrontendSymbol, FrontendTone, ToolDiscoveryMode};

    use super::*;

    #[test]
    fn selected_endpoint_uses_explicit_then_default_urls_only_for_configurable_providers() {
        for (provider_id, explicit, expected) in [
            ("openrouter", None, Some("https://openrouter.ai/api/v1")),
            (
                "openrouter",
                Some("https://custom.example/v1"),
                Some("https://custom.example/v1"),
            ),
            (
                "openai_codex",
                None,
                provider("openai_codex").expect("Codex").default_base_url(),
            ),
            (
                "openai_codex",
                Some("https://custom.example/v1"),
                Some("https://custom.example/v1"),
            ),
        ] {
            let mut selection = crate::wire::AgentComposition::default().provider;
            selection.provider = provider_id.into();
            selection.base_url = explicit.map(str::to_owned);
            assert_eq!(
                selected_base_url(provider(provider_id).expect("provider"), &selection),
                expected
            );
        }
    }

    #[test]
    fn provider_status_advertises_the_transport_voice_catalog() {
        let status = provider_status(provider("openai_socket").expect("provider"));
        assert!(!status.realtime_voices.is_empty());
        assert_eq!(
            status.realtime_voices.first().map(String::as_str),
            Some("marin")
        );
        assert!(status.realtime_voices.iter().any(|voice| voice == "cedar"));
        let codex = provider_status(provider("openai_codex").expect("provider"));
        assert_eq!(
            codex.realtime_voices.first().map(String::as_str),
            Some("cove")
        );
        assert!(!codex.realtime_voices.iter().any(|voice| voice == "cedar"));
    }

    #[test]
    fn catalog_reasoning_follows_manifest_order_even_when_another_level_is_selected() {
        let status = provider_status(provider("openai_socket").expect("provider"));
        let configured = ConfiguredProvider {
            selection: ProviderConfig {
                instance: "openai_socket".into(),
                provider: "openai_socket".into(),
                model: "gpt-6.1-sol".into(),
                reasoning_effort: Some("max".into()),
                service_tier: Some("default".into()),
                ..crate::wire::AgentComposition::default().provider
            },
            label: "OpenAI".into(),
            tint: Default::default(),
            model_ids: Vec::new(),
            reasoning_efforts: Vec::new(),
            image_model_ids: Vec::new(),
        };
        let mut selection = configured.selection.clone();
        selection.service_tier = None;
        let routes = catalog_routes(
            provider("openai_socket").expect("provider"),
            &configured,
            &selection,
        );
        assert!(
            routes
                .iter()
                .all(|route| route.provider.service_tier.as_deref() == Some("default"))
        );
        for model in &status.models {
            assert_eq!(
                routes
                    .iter()
                    .filter(|route| route.choice.model == model.id)
                    .filter_map(|route| route.choice.reasoning_effort.as_deref())
                    .collect::<Vec<_>>(),
                model
                    .reasoning
                    .iter()
                    .map(|effort| effort.id.as_str())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn provider_status_uses_manifest_defaults() {
        let status = provider_status(provider("openai_socket").expect("provider"));

        assert_eq!(status.provider, "openai_socket");
        assert_eq!(status.label, "OpenAI");
        assert_eq!(status.symbol, FrontendSymbol::Custom("chat_gpt".into()));
        assert_eq!(status.models[0].id, "gpt-6.1-sol");
        assert_eq!(status.tool_discovery, ToolDiscoveryMode::Native);
        assert_eq!(status.models[0].tool_discovery, ToolDiscoveryMode::Native);
        assert_eq!(status.default_api_key_env, None);
        assert_eq!(
            status.default_base_url.as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert!(status.native_custom_endpoints);
        assert_eq!(
            status.models[0].default_reasoning.as_deref(),
            Some("medium")
        );
        assert_eq!(status.web_search[0].value, "off");
        assert_eq!(status.web_search[0].label, "Off");
        assert_eq!(
            status.web_search[0].description,
            "Do not use provider-hosted web search"
        );
        assert_eq!(status.web_search[0].symbol, None);
        assert_eq!(status.web_search[0].tone, FrontendTone::Neutral);
    }

    #[test]
    fn compatible_provider_status_uses_manifest_defaults() {
        let custom = provider_status(provider("responses").expect("provider"));
        assert!(!custom.native_custom_endpoints);
        assert!(custom.models.is_empty());
        assert!(custom.model_ids_configurable);
        assert_eq!(
            custom.default_base_url.as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(custom.default_api_key_env, None);
        assert_eq!(custom.tool_discovery, ToolDiscoveryMode::Rebuild);

        let openrouter = provider_status(provider("openrouter").expect("provider"));
        assert!(openrouter.realtime_voices.is_empty());
        assert!(openrouter.models.is_empty());
        assert!(openrouter.model_ids_configurable);
        assert_eq!(
            openrouter.default_base_url.as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
        assert_eq!(openrouter.tool_discovery, ToolDiscoveryMode::Native);
        assert_eq!(openrouter.custom_endpoint_tool_discovery, None);
    }

    #[test]
    fn catalog_routes_resolve_model_and_endpoint_tool_discovery() {
        let anthropic = provider("anthropic").expect("anthropic");
        assert_eq!(
            anthropic.tool_discovery("claude-sonnet-5-5", anthropic.default_base_url()),
            ToolDiscoveryMode::Native
        );
        assert_eq!(
            anthropic.tool_discovery("claude-opus-5-5", anthropic.default_base_url()),
            ToolDiscoveryMode::Native
        );
        assert_eq!(
            anthropic.tool_discovery("claude-opus-5-5", Some("https://proxy.example/v1")),
            ToolDiscoveryMode::Native
        );

        let openrouter = provider("openrouter").expect("openrouter");
        assert_eq!(
            openrouter.tool_discovery("openai/gpt-5.6-luna", openrouter.default_base_url()),
            ToolDiscoveryMode::Native
        );
        assert_eq!(
            openrouter.tool_discovery("openai/gpt-5.6-luna", Some("https://proxy.example/v1")),
            ToolDiscoveryMode::Native
        );
    }

    #[test]
    fn custom_astra_model_ids_remain_available_in_configured_catalogs() {
        for (id, model) in [
            ("openrouter", "openai/gpt-6-astra"),
            ("responses", "gpt-6-astra"),
        ] {
            let definition = provider(id).expect("provider");
            let mut selection = crate::wire::AgentComposition::default().provider;
            selection.instance = id.into();
            selection.provider = id.into();
            selection.model = model.into();
            selection.base_url = definition.default_base_url().map(str::to_owned);
            selection.reasoning_effort = Some("max".into());
            let config = GatewayConfig::new("127.0.0.1:8741".parse().expect("listen"), None)
                .expect("config")
                .registering_provider(
                    selection,
                    id.into(),
                    Default::default(),
                    vec![model.into(), "custom-model".into()],
                    vec!["medium".into(), "max".into()],
                    Vec::new(),
                )
                .expect("register custom catalog");
            let configured = &config.configured_providers[id];
            let routes = catalog_routes(definition, configured, &configured.selection);
            assert_eq!(
                routes
                    .iter()
                    .filter(|route| route.choice.model == model)
                    .filter_map(|route| route.choice.reasoning_effort.as_deref())
                    .collect::<Vec<_>>(),
                ["medium", "max"]
            );
            assert!(
                routes
                    .iter()
                    .any(|route| route.choice.route == format!("{id}::{model}::max"))
            );
            assert!(
                routes
                    .iter()
                    .any(|route| route.choice.model == "custom-model")
            );
        }
    }

    #[test]
    fn media_routes_skip_compatible_endpoints_without_native_media() {
        let mut config =
            GatewayConfig::new("127.0.0.1:8741".parse().expect("listen"), None).expect("config");
        for (id, base_url, image_ids) in [
            ("openai_socket", None, Vec::new()),
            ("openai_codex", None, Vec::new()),
            ("openrouter", None, vec!["google/imagen-5".into()]),
            ("responses", Some("http://localhost:11434/v1"), Vec::new()),
        ] {
            let definition = provider(id).expect("provider");
            let mut selection = crate::wire::AgentComposition::default().provider;
            selection.instance = id.into();
            selection.provider = id.into();
            selection.model = definition.default_model().unwrap_or("local").into();
            selection.base_url = base_url.map(str::to_owned);
            selection.reasoning_effort = None;
            config = config
                .registering_provider(
                    selection,
                    id.into(),
                    Default::default(),
                    if definition.models().is_empty() {
                        vec!["local".into()]
                    } else {
                        Vec::new()
                    },
                    Vec::new(),
                    image_ids,
                )
                .expect("register");
        }
        let mut routes = Vec::new();
        for configured in config.configured_providers.values() {
            let definition = provider(&configured.selection.provider).expect("provider");
            routes.extend(catalog_routes(
                definition,
                configured,
                &configured.selection,
            ));
        }
        let media = media_routes(&config.configured_providers, &routes).expect("media");
        let images = media
            .images
            .iter()
            .map(|image| image.choice.route.as_str())
            .collect::<Vec<_>>();
        assert!(images.contains(&"openai_socket::gpt-image-2.5-sunburst::high"));
        assert!(images.contains(&"openai_socket::gpt-image-2.5-flare::low"));
        assert!(images.contains(&"openrouter::google/imagen-5::default"));
        assert!(images.iter().all(|route| !route.starts_with("responses::")));
        let voice = |instance: &str, voice: &str| {
            media.voices.iter().any(|choice| {
                choice.instance == instance
                    && choice.choice.reasoning_effort.as_deref() == Some(voice)
            })
        };
        assert!(voice("openai_socket", "cedar"));
        assert!(voice("openai_codex", "cove"));
        assert!(!voice("openai_codex", "cedar"));
        assert!(!voice("responses", "cedar"));
        assert_eq!(
            media.voices.first().expect("native voice").instance,
            "openai_codex"
        );
        let choices = configured_model_catalog(&config).expect("configured choices");
        choices
            .validate_voice(Some("openai_socket::gpt-live-1::cedar"))
            .expect("routed voice");
        for voice in [
            "cedar",
            "openai_socket::gpt-live-1::missing",
            "responses::gpt-live-1::cedar",
        ] {
            assert!(choices.validate_voice(Some(voice)).is_err(), "{voice}");
        }
        choices
            .validate_voice(None)
            .expect("first available voice is optional");
        assert!(
            media
                .voices
                .iter()
                .all(|choice| choice.instance != "openrouter")
        );
    }

    #[test]
    fn every_built_in_provider_advertises_a_discovery_mode() {
        let expected = [
            ("openai_socket", ToolDiscoveryMode::Native),
            ("openai_codex", ToolDiscoveryMode::Native),
            ("deepseek", ToolDiscoveryMode::Rebuild),
            ("kimi", ToolDiscoveryMode::Rebuild),
            ("openrouter", ToolDiscoveryMode::Native),
            ("anthropic", ToolDiscoveryMode::Native),
            ("responses", ToolDiscoveryMode::Rebuild),
        ];

        for (id, expected) in expected {
            assert_eq!(
                provider_status(provider(id).expect("provider")).tool_discovery,
                expected,
                "provider {id}"
            );
        }
    }
}
