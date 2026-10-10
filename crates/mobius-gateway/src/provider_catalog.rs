//! Gateway provider catalog, route policy, and credential availability.

use std::collections::BTreeMap;

use mobius::backend::model::provider::{
    ModelPreset, ProviderAuth, ProviderDefinition, ReasoningPreset, provider, providers,
};
use mobius::middleware::manifest::ModelCatalogs;
use mobius::protocol::{FrontendSettingOption, FrontendTone, ModelCapability, ModelChoice};

use crate::config::{
    ConfigStore, ConfiguredModel, ConfiguredProvider, CredentialStore, DEFAULT_CONTEXT_WINDOW,
    GatewayConfig, model_route_id,
};
use crate::wire::{
    ProviderAuthKind, ProviderConfig, ProviderEndpointAuth, ProviderInstance, ProviderModel,
    ProviderStatus,
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
                models: configured_models(definition, configured),
                image_model_ids: configured.image_model_ids.clone(),
            })
        })
        .collect()
}

/// Independent chat, image and voice catalogs.
pub(crate) struct ConfiguredChoices {
    pub(crate) models: Vec<ModelChoice>,
    pub(crate) images: Vec<ModelChoice>,
    voices: Vec<ModelChoice>,
}

impl ConfiguredChoices {
    fn from_routes(routes: Vec<CatalogRoute>, media: MediaRoutes) -> Self {
        Self {
            images: media.images.into_iter().map(|media| media.choice).collect(),
            voices: media.voices.into_iter().map(|media| media.choice).collect(),
            models: routes.into_iter().map(|route| route.choice).collect(),
        }
    }

    /// Borrows the lists dynamic middleware settings select from.
    pub(crate) fn catalogs(&self) -> ModelCatalogs<'_> {
        ModelCatalogs {
            models: &self.models,
            images: &self.images,
            voices: &self.voices,
        }
    }
}

pub(crate) fn configured_model_choices(
    gateway: &GatewayConfig,
    store: &ConfigStore,
    credentials: &CredentialStore,
) -> Result<ConfiguredChoices> {
    Ok(ConfiguredChoices::from_routes(
        configured_model_routes(
            &gateway.configured_providers,
            gateway
                .bot_defaults
                .as_ref()
                .map(|defaults| defaults.config.provider.instance.as_str()),
            store,
            credentials,
        )?,
        media_routes(&gateway.configured_providers, Some((store, credentials)))?,
    ))
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
    Ok(ConfiguredChoices::from_routes(
        routes,
        media_routes(&gateway.configured_providers, None)?,
    ))
}

pub(crate) fn configured_model_providers(
    gateway: &GatewayConfig,
    store: &ConfigStore,
    credentials: &CredentialStore,
) -> Result<BTreeMap<String, String>> {
    let mut providers: BTreeMap<_, _> = configured_model_routes(
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
    .collect();
    let media = media_routes(&gateway.configured_providers, Some((store, credentials)))?;
    providers.extend(
        media
            .images
            .into_iter()
            .chain(media.voices)
            .map(|route| (route.choice.route, route.provider.instance)),
    );
    Ok(providers)
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

#[cfg(test)]
pub(crate) fn configured_model_input(model: &ModelPreset) -> ConfiguredModel {
    ConfiguredModel {
        id: model.id.clone(),
        reasoning_efforts: Some(
            model
                .reasoning
                .iter()
                .map(|effort| effort.id.clone())
                .collect(),
        ),
        default_reasoning: model.default_reasoning.clone(),
        ..Default::default()
    }
}

pub(crate) fn model_context_window(definition: &ProviderDefinition, id: &str) -> i64 {
    definition
        .model(id)
        .or_else(|| {
            definition
                .default_model()
                .and_then(|id| definition.model(id))
        })
        .map_or(DEFAULT_CONTEXT_WINDOW, |model| model.context_window)
}

pub(crate) fn prepare_configured_models(
    definition: &ProviderDefinition,
    submitted: Vec<ConfiguredModel>,
    mut previous: Vec<ModelPreset>,
) -> Vec<ModelPreset> {
    submitted
        .into_iter()
        .map(|input| {
            let mut model =
                if let Some(index) = previous.iter().position(|model| model.id == input.id) {
                    previous.swap_remove(index)
                } else if let Some(preset) = definition.model(&input.id) {
                    preset.clone()
                } else {
                    ModelPreset {
                        context_window: model_context_window(definition, &input.id),
                        tool_discovery: definition.tool_discovery(&input.id, None),
                        label: input.id.clone(),
                        id: input.id,
                        description: String::new(),
                        reasoning: Vec::new(),
                        default_reasoning: None,
                    }
                };
            if let Some(efforts) = input.reasoning_efforts {
                let mut previous_efforts = std::mem::take(&mut model.reasoning);
                model.reasoning = efforts
                    .into_iter()
                    .map(|id| {
                        previous_efforts
                            .iter()
                            .position(|effort| effort.id == id)
                            .map_or_else(
                                || ReasoningPreset {
                                    label: id.clone(),
                                    id,
                                    description: String::new(),
                                },
                                |index| previous_efforts.swap_remove(index),
                            )
                    })
                    .collect();
                model.default_reasoning = input.default_reasoning;
            } else if input.default_reasoning.is_some() {
                model.default_reasoning = input.default_reasoning;
            }
            if let Some(label) = input.label {
                model.label = label;
            }
            if let Some(description) = input.description {
                model.description = description;
            }
            if let Some(window) = input.context_window {
                model.context_window = window;
            }
            model
        })
        .collect()
}

pub(crate) fn models<'a>(
    definition: &ProviderDefinition,
    configured: &'a ConfiguredProvider,
) -> &'a [ModelPreset] {
    if definition.locked_models() {
        definition.models()
    } else {
        &configured.models
    }
}

pub(crate) fn catalog_routes(
    definition: &ProviderDefinition,
    configured: &ConfiguredProvider,
    selection: &ProviderConfig,
) -> Vec<CatalogRoute> {
    if selection.model.is_empty() {
        return Vec::new();
    }
    let models = models(definition, configured);
    let ordered = models
        .iter()
        .filter(|model| model.id == selection.model)
        .chain(models.iter().filter(|model| model.id != selection.model));
    let mut routes = Vec::new();
    for model in ordered {
        let (id, label, context_window) = (
            model.id.as_str(),
            model.label.as_str(),
            model.context_window,
        );
        for variant in with_default(&model.reasoning) {
            let effort = variant.map(|variant| variant.id.as_str());
            let variant_label = variant.map(|variant| variant.label.as_str());
            let provider = ProviderConfig {
                instance: selection.instance.clone(),
                provider: selection.provider.clone(),
                base_url: configured.selection.base_url.clone(),
                endpoint_auth: configured.selection.endpoint_auth,
                model: id.into(),
                reasoning_effort: effort.map(str::to_string),
                service_tier: selection
                    .service_tier
                    .as_ref()
                    .or(configured.selection.service_tier.as_ref())
                    .cloned(),
                web_search: configured.selection.web_search,
                tool_discovery: configured.selection.tool_discovery,
            };
            let route = model_route_id(&selection.instance, id, effort);
            routes.push(CatalogRoute {
                choice: ModelChoice {
                    route,
                    group: format!("{} · {}", configured.label, label),
                    model: id.into(),
                    reasoning_effort: effort.map(str::to_string),
                    variant_label: variant_label.map(str::to_string),
                    context_window: Some(context_window),
                    supports_image_input: definition.supports_image_input(),
                    supports_image_generation: definition.supports_at(
                        ModelCapability::ImageGeneration,
                        configured.selection.base_url.as_deref(),
                    ) || !configured.image_model_ids.is_empty(),
                    supports_realtime_voice: definition.supports_at(
                        ModelCapability::RealtimeVoice,
                        configured.selection.base_url.as_deref(),
                    ),
                    tool_discovery: effective_tool_discovery(definition, &provider, model),
                },
                provider,
                capability: None,
            });
        }
    }
    routes
}

pub(crate) struct CatalogRoute {
    pub(crate) choice: ModelChoice,
    pub(crate) provider: ProviderConfig,
    pub(crate) capability: Option<ModelCapability>,
}

/// Image and voice choices, independent of the chat catalog.
#[derive(Default)]
pub(crate) struct MediaRoutes {
    pub(crate) images: Vec<CatalogRoute>,
    pub(crate) voices: Vec<CatalogRoute>,
}

/// Derives media choices directly from configured providers, optionally checking credentials.
pub(crate) fn media_routes(
    configured_providers: &BTreeMap<String, ConfiguredProvider>,
    credentials: Option<(&ConfigStore, &CredentialStore)>,
) -> Result<MediaRoutes> {
    let mut media = MediaRoutes::default();
    for configured in configured_providers.values() {
        let instance = configured.selection.instance.as_str();
        if let Some((store, credentials)) = credentials
            && !credential_is_configured(&configured.selection, store, credentials)?
        {
            continue;
        }
        let definition = provider(&configured.selection.provider)?;
        let media_choice = |model: &str,
                            label: &str,
                            variant: Option<&ReasoningPreset>,
                            voice: bool| CatalogRoute {
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
                tool_discovery: definition
                    .tool_discovery(model, configured.selection.base_url.as_deref()),
            },
            provider: ProviderConfig {
                instance: instance.into(),
                provider: configured.selection.provider.as_str().into(),
                model: model.into(),
                base_url: configured.selection.base_url.as_deref().map(Into::into),
                endpoint_auth: configured.selection.endpoint_auth,
                reasoning_effort: None,
                service_tier: None,
                web_search: Default::default(),
                tool_discovery: None,
            },
            capability: Some(if voice {
                ModelCapability::RealtimeVoice
            } else {
                ModelCapability::ImageGeneration
            }),
        };
        if configured.image_model_ids.is_empty()
            && (configured.image_models.is_some()
                || definition.supports_at(
                    ModelCapability::ImageGeneration,
                    configured.selection.base_url.as_deref(),
                ))
        {
            for preset in configured
                .image_models
                .as_deref()
                .unwrap_or_else(|| definition.image_models())
            {
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
        if configured.voice_models.is_some()
            || definition.supports_at(
                ModelCapability::RealtimeVoice,
                configured.selection.base_url.as_deref(),
            )
        {
            for preset in configured
                .voice_models
                .as_deref()
                .unwrap_or_else(|| definition.voice_models())
            {
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

pub(crate) fn effective_tool_discovery(
    definition: &ProviderDefinition,
    selection: &ProviderConfig,
    model: &ModelPreset,
) -> mobius::protocol::ToolDiscoveryMode {
    selection.tool_discovery.unwrap_or_else(|| {
        if definition.uses_default_endpoint(selection.base_url.as_deref()) {
            model.tool_discovery
        } else {
            definition
                .custom_endpoint_tool_discovery()
                .unwrap_or(model.tool_discovery)
        }
    })
}

fn configured_models(
    definition: &ProviderDefinition,
    configured: &ConfiguredProvider,
) -> Vec<ProviderModel> {
    if configured.selection.model.is_empty() {
        return Vec::new();
    }
    configured
        .models
        .iter()
        .map(|model| {
            let mut value = model.clone();
            value.tool_discovery =
                effective_tool_discovery(definition, &configured.selection, model);
            value
        })
        .collect()
}

fn provider_models(definition: &ProviderDefinition) -> Vec<ProviderModel> {
    definition.models().to_vec()
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
        model_ids_configurable: !definition.locked_models(),
        image_models: std::borrow::Cow::Borrowed(definition.image_models()),
        image_model_ids_configurable: definition.image_models().is_empty()
            && definition.supports_at(ModelCapability::ImageGeneration, None),
        voice_models: std::borrow::Cow::Borrowed(definition.voice_models()),
        auth,
        default_base_url: definition.default_base_url().map(str::to_string),
        native_custom_endpoints: definition.native_custom_endpoints(),
        default_api_key_env,
        models: provider_models(definition),
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
        supported_tool_discovery: definition.supported_tool_discovery().into(),
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
        let voices = |id| {
            provider_status(provider(id).expect("provider"))
                .voice_models
                .iter()
                .flat_map(|model| &model.variants)
                .map(|voice| voice.id.to_string())
                .collect::<Vec<_>>()
        };
        let openai = voices("openai_socket");
        assert_eq!(openai.first().map(String::as_str), Some("marin"));
        assert!(openai.iter().any(|voice| voice == "cedar"));
        let codex = voices("openai_codex");
        assert_eq!(codex.first().map(String::as_str), Some("cove"));
        assert!(!codex.iter().any(|voice| voice == "cedar"));
    }

    #[test]
    fn native_text_catalogs_stay_locked_and_media_only_instances_hide_them() {
        for id in ["openai_socket", "openai_codex"] {
            let definition = provider(id).unwrap();
            let mut selection = crate::wire::AgentComposition::default().provider;
            selection.instance = id.into();
            selection.provider = id.into();
            selection.model = definition.default_model().unwrap().into();
            selection.base_url = definition.default_base_url().map(Into::into);
            let mut configured = ConfiguredProvider {
                selection,
                label: definition.label().into(),
                tint: Default::default(),
                models: Vec::new(),
                image_model_ids: Vec::new(),
                image_models: None,
                voice_models: None,
            };
            assert!(!provider_status(definition).model_ids_configurable);
            assert!(configured_models(definition, &configured).is_empty());
            assert!(!catalog_routes(definition, &configured, &configured.selection).is_empty());
            configured.selection.model.clear();
            configured.selection.reasoning_effort = None;
            assert!(configured_models(definition, &configured).is_empty());
            assert!(catalog_routes(definition, &configured, &configured.selection).is_empty());
            let media = media_routes(&BTreeMap::from([(id.into(), configured)]), None).unwrap();
            assert!(!media.images.is_empty());
            assert!(!media.voices.is_empty());
        }
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
            models: Vec::new(),
            image_model_ids: Vec::new(),
            image_models: None,
            voice_models: None,
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
        assert!(openrouter.voice_models.is_empty());
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
                    vec![
                        crate::wire::ConfiguredModel {
                            id: model.into(),
                            reasoning_efforts: Some(vec!["medium".into(), "max".into()]),
                            default_reasoning: Some("medium".into()),
                            ..Default::default()
                        },
                        crate::wire::ConfiguredModel {
                            id: "custom-model".into(),
                            reasoning_efforts: Some(vec!["medium".into(), "max".into()]),
                            default_reasoning: Some("medium".into()),
                            ..Default::default()
                        },
                    ],
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
                    (if definition.models().is_empty() {
                        vec!["local".into()]
                    } else {
                        Vec::new()
                    })
                    .into_iter()
                    .map(|id| crate::wire::ConfiguredModel {
                        id,
                        reasoning_efforts: Some(Vec::new()),
                        default_reasoning: None,
                        ..Default::default()
                    })
                    .collect(),
                    image_ids,
                )
                .expect("register");
        }
        let media = media_routes(&config.configured_providers, None).expect("media");
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
                choice.provider.instance == instance
                    && choice.choice.reasoning_effort.as_deref() == Some(voice)
            })
        };
        assert!(voice("openai_socket", "cedar"));
        assert!(voice("openai_codex", "cove"));
        assert!(!voice("openai_codex", "cedar"));
        assert!(!voice("responses", "cedar"));
        assert_eq!(
            media
                .voices
                .first()
                .expect("native voice")
                .provider
                .instance,
            "openai_codex"
        );
        let choices = configured_model_catalog(&config).expect("configured choices");
        let voice_setting = |route: Option<&str>| {
            let mut config = crate::middleware_manifest::default_config(
                mobius::middleware::subagents::SubagentCeilings::default(),
            );
            config.set_setting(
                "voice",
                "model",
                route.map(|route| mobius::protocol::FrontendSettingValue::String(route.into())),
            );
            crate::middleware_manifest::validate_choices(&config, choices.catalogs())
        };
        voice_setting(Some("openai_socket::gpt-live-1::cedar")).expect("routed voice");
        for voice in [
            "cedar",
            "openai_socket::gpt-live-1::missing",
            "responses::gpt-live-1::cedar",
        ] {
            assert!(voice_setting(Some(voice)).is_err(), "{voice}");
        }
        voice_setting(None).expect("first available voice is optional");
        assert!(
            media
                .voices
                .iter()
                .all(|choice| choice.provider.instance != "openrouter")
        );
    }

    #[test]
    fn every_built_in_provider_advertises_a_discovery_mode() {
        let expected = [
            ("openai_socket", ToolDiscoveryMode::Native),
            ("openai_codex", ToolDiscoveryMode::Native),
            ("deepseek", ToolDiscoveryMode::Rebuild),
            ("kimi", ToolDiscoveryMode::Rebuild),
            ("mistral", ToolDiscoveryMode::Rebuild),
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

    #[test]
    fn configured_catalogs_preserve_per_model_reasoning_and_plain_routes() {
        use crate::config::ConfiguredModel;

        let mut selection = crate::wire::AgentComposition::default().provider;
        selection.instance = "custom".into();
        selection.provider = "responses".into();
        selection.model = "reasoner".into();
        selection.reasoning_effort = None;
        let mut configured = ConfiguredProvider {
            selection,
            label: "Custom".into(),
            tint: Default::default(),
            models: prepare_configured_models(
                provider("responses").expect("provider"),
                vec![
                    ConfiguredModel {
                        id: "reasoner".into(),
                        reasoning_efforts: Some(vec!["low".into(), "high".into()]),
                        default_reasoning: Some("high".into()),
                        ..Default::default()
                    },
                    ConfiguredModel {
                        id: "fast".into(),
                        reasoning_efforts: Some(vec!["brief".into()]),
                        default_reasoning: Some("brief".into()),
                        ..Default::default()
                    },
                    ConfiguredModel {
                        id: "plain".into(),
                        reasoning_efforts: Some(Vec::new()),
                        default_reasoning: None,
                        ..Default::default()
                    },
                ],
                Vec::new(),
            ),
            image_model_ids: Vec::new(),
            image_models: None,
            voice_models: None,
        };
        let definition = provider("responses").expect("custom provider");
        let models = configured_models(definition, &configured);
        assert_eq!(
            models
                .iter()
                .map(|model| (
                    model.id.as_str(),
                    model
                        .reasoning
                        .iter()
                        .map(|effort| effort.id.as_str())
                        .collect::<Vec<_>>(),
                    model.default_reasoning.as_deref(),
                ))
                .collect::<Vec<_>>(),
            vec![
                ("reasoner", vec!["low", "high"], Some("high")),
                ("fast", vec!["brief"], Some("brief")),
                ("plain", Vec::new(), None),
            ]
        );
        let routes = catalog_routes(definition, &configured, &configured.selection);
        assert_eq!(
            routes
                .iter()
                .map(|route| route.choice.route.as_str())
                .collect::<Vec<_>>(),
            [
                "custom::reasoner::low",
                "custom::reasoner::high",
                "custom::fast::brief",
                "custom::plain::default"
            ]
        );
        let builtin = provider("openai_socket").expect("builtin provider");
        configured.selection.provider = "openai_socket".into();
        configured.models.clear();
        assert!(configured_models(builtin, &configured).is_empty());
        assert!(!provider_status(builtin).models.is_empty());
    }

    #[test]
    fn editable_setups_keep_their_own_metadata_and_model_list() {
        for definition in providers() {
            assert_eq!(
                provider_status(definition).model_ids_configurable,
                !matches!(definition.id(), "openai_socket" | "openai_codex")
            );
        }
        let definition = provider("anthropic").expect("provider");
        let id = definition.default_model().expect("default");
        let mut selection = crate::wire::AgentComposition::default().provider;
        selection.instance = "editable".into();
        selection.provider = definition.id().into();
        selection.model = id.into();
        selection.base_url = definition.default_base_url().map(str::to_owned);
        selection.reasoning_effort = None;
        let mut gateway = GatewayConfig::new("127.0.0.1:8741".parse().expect("listen"), None)
            .expect("gateway")
            .registering_provider(
                selection.clone(),
                "Editable".into(),
                Default::default(),
                Vec::new(),
                Vec::new(),
            )
            .expect("seed catalog");
        assert_eq!(
            gateway.configured_providers["editable"].models,
            definition.models()
        );
        let stored = gateway
            .configured_providers
            .get_mut("editable")
            .expect("setup");
        let model = stored
            .models
            .iter_mut()
            .find(|model| model.id == id)
            .expect("model");
        model.context_window = 123456;
        model.label = "Operator label".into();
        model.description = "Operator description".into();
        stored.selection.tool_discovery = Some(ToolDiscoveryMode::Native);
        selection.tool_discovery = Some(ToolDiscoveryMode::Native);
        let gateway = gateway
            .registering_provider(
                selection,
                "Edited".into(),
                Default::default(),
                vec![ConfiguredModel {
                    id: id.into(),
                    reasoning_efforts: Some(Vec::new()),
                    ..Default::default()
                }],
                Vec::new(),
            )
            .expect("app edit");
        let stored = &gateway.configured_providers["editable"];
        assert_eq!(stored.models.len(), 1, "removed presets must not return");
        let model = &stored.models[0];
        assert_eq!(model.context_window, 123456);
        assert_eq!(model.label, "Operator label");
        assert_eq!(model.description, "Operator description");
        assert!(model.reasoning.is_empty());
        assert!(model.default_reasoning.is_none());
        let routes = catalog_routes(definition, stored, &stored.selection);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].choice.context_window, Some(123456));
        assert_eq!(routes[0].choice.tool_discovery, ToolDiscoveryMode::Native);
        assert!(routes[0].provider.reasoning_effort.is_none());
        let restored: GatewayConfig =
            toml::from_str(&toml::to_string(&gateway).expect("serialize")).expect("restore");
        assert_eq!(restored.configured_providers, gateway.configured_providers);
    }
}
