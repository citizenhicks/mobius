//! Gateway provider catalog, route policy, and credential availability.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use mobius::backend::model::provider::{
    BaseUrl, ModelPreset, ProviderAuth, ProviderDefinition, ReasoningPreset, provider, providers,
};
use mobius::middleware::manifest::ModelCatalogs;
use mobius::protocol::{FrontendSettingOption, FrontendTone, ModelCapability, ModelChoice};

use crate::config::{
    ConfigStore, ConfiguredModel, ConfiguredProvider, CredentialStore, DEFAULT_CONTEXT_WINDOW,
    GatewayConfig,
};
use crate::wire::{
    ProviderAuthKind, ProviderConfig, ProviderEndpointAuth, ProviderInstance, ProviderModel,
    ProviderStatus,
};
use crate::{Error, Result};

use super::{endpoints, form};

pub(crate) fn provider_statuses() -> Vec<ProviderStatus> {
    providers().iter().map(provider_status).collect()
}

pub(crate) fn selected_base_url<'a>(
    definition: &'a ProviderDefinition,
    selection: &'a ProviderConfig,
) -> &'a BaseUrl {
    selection
        .base_url
        .as_ref()
        .unwrap_or_else(|| definition.default_base_url())
}

pub(crate) struct CredentialedInstances<'config>(BTreeSet<&'config str>);

impl<'config> CredentialedInstances<'config> {
    pub(crate) fn resolve(
        configured_providers: &'config BTreeMap<String, ConfiguredProvider>,
        store: &ConfigStore,
        credentials: &CredentialStore,
    ) -> Result<Self> {
        let mut available = BTreeSet::new();
        for configured in configured_providers.values() {
            if credential_is_configured(&configured.selection, store, credentials)? {
                available.insert(configured.selection.instance.as_str());
            }
        }
        Ok(Self(available))
    }

    pub(crate) fn contains(&self, selection: &ProviderConfig) -> bool {
        self.0.contains(selection.instance.as_str())
    }
}

pub(crate) fn provider_instances(
    configured_providers: &BTreeMap<String, ConfiguredProvider>,
    credentialed: &CredentialedInstances<'_>,
    credentials: &CredentialStore,
) -> Result<Vec<ProviderInstance>> {
    configured_providers
        .values()
        .map(|configured| {
            let definition = provider(&configured.selection.provider)?;
            let base_url = Some(selected_base_url(definition, &configured.selection));
            Ok(ProviderInstance {
                label: configured.label.clone(), // rules-allow: provider records are sent to clients
                tint: configured.tint,
                configured: credentialed.contains(&configured.selection),
                credential_hint: credentials.hint(
                    &configured.selection.instance,
                    definition.id(),
                    base_url,
                )?,
                selection: configured.selection.clone(), // rules-allow: provider records are sent to clients
                models: configured_models(definition, configured),
                model_groups: model_groups(
                    definition,
                    selected_base_url(definition, &configured.selection),
                    Some(configured),
                ),
                operator_managed: configured.operator_managed,
                tool_discovery: language_discovery(
                    definition,
                    Some(configured),
                    configured
                        .group_endpoints
                        .iter()
                        .find(|e| e.kind == crate::wire::ProviderModelKind::Language)
                        .map(|e| e.api.as_str()),
                ),
            })
        })
        .collect()
}

/// The configured voice routes in selection order, and whether each call's audio waits for
/// its provider's session-started event.
#[derive(Debug, Default)]
pub(crate) struct VoiceRoutes(Vec<(Arc<str>, bool)>);

impl VoiceRoutes {
    pub(crate) fn new(
        configured_providers: &BTreeMap<String, ConfiguredProvider>,
        store: &ConfigStore,
        credentials: &CredentialStore,
    ) -> Result<Self> {
        let credentialed =
            CredentialedInstances::resolve(configured_providers, store, credentials)?;
        Self::from_routes(&media_routes(configured_providers, Some(&credentialed))?.voices)
    }

    pub(crate) fn from_routes(voices: &[CatalogRoute]) -> Result<Self> {
        voices
            .iter()
            .map(|route| {
                Ok((
                    Arc::clone(&route.choice.route),
                    provider(&route.provider.provider)?.voice_waits_for_session_started(),
                ))
            })
            .collect::<Result<_>>()
            .map(Self)
    }

    /// The route a call uses, by the router's rule: the saved route, or the first voice.
    pub(crate) fn select(
        &self,
        route: Option<&str>,
    ) -> std::result::Result<(&Arc<str>, bool), &'static str> {
        match route {
            Some(route) => self
                .0
                .iter()
                .find(|(candidate, _)| **candidate == *route)
                .ok_or("the saved voice is no longer offered by a configured provider"),
            None => self.0.first().ok_or("no voice model is configured"),
        }
        .map(|(route, waits)| (route, *waits))
    }
}

/// Independent chat, image, voice and decision catalogs.
pub(crate) struct ConfiguredChoices {
    pub(crate) models: Vec<ModelChoice>,
    pub(crate) images: Vec<ModelChoice>,
    voices: Vec<ModelChoice>,
    decisions: Vec<ModelChoice>,
}

impl ConfiguredChoices {
    fn from_routes(routes: Vec<CatalogRoute>, media: MediaRoutes) -> Self {
        Self {
            images: media.images.into_iter().map(|media| media.choice).collect(),
            voices: media.voices.into_iter().map(|media| media.choice).collect(),
            decisions: media
                .decisions
                .into_iter()
                .map(|media| media.choice)
                .collect(),
            models: routes.into_iter().map(|route| route.choice).collect(),
        }
    }

    /// Borrows the lists dynamic middleware settings select from.
    pub(crate) fn catalogs(&self) -> ModelCatalogs<'_> {
        ModelCatalogs {
            models: &self.models,
            images: &self.images,
            voices: &self.voices,
            decisions: &self.decisions,
        }
    }
}

pub(crate) fn configured_model_choices(
    gateway: &GatewayConfig,
    store: &ConfigStore,
    credentials: &CredentialStore,
) -> Result<ConfiguredChoices> {
    let credentialed =
        CredentialedInstances::resolve(&gateway.configured_providers, store, credentials)?;
    Ok(ConfiguredChoices::from_routes(
        configured_model_routes(
            &gateway.configured_providers,
            default_instance(gateway),
            &credentialed,
        )?,
        media_routes(&gateway.configured_providers, Some(&credentialed))?,
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
    credentialed: &CredentialedInstances<'_>,
) -> Result<BTreeMap<Arc<str>, String>> {
    let mut providers = BTreeMap::new();
    for configured in gateway.configured_providers.values() {
        if let Some(definition) = available_chat_provider(configured, credentialed)? {
            let instance = configured.selection.instance.as_str();
            for (route, _, _) in chat_variants(definition, configured, &configured.selection) {
                providers.insert(route.encode().into(), instance.to_owned());
            }
        }
    }
    let media = media_routes(&gateway.configured_providers, Some(credentialed))?;
    for route in media.images.into_iter().chain(media.voices) {
        providers.insert(route.choice.route, route.provider.instance);
    }
    Ok(providers)
}

/// The routes' choices, recording the provider instance that serves each route.
pub(crate) fn route_choices(
    routes: Vec<CatalogRoute>,
    providers: &mut BTreeMap<Arc<str>, String>,
) -> Vec<ModelChoice> {
    routes
        .into_iter()
        .map(|route| {
            providers.insert(Arc::clone(&route.choice.route), route.provider.instance);
            route.choice
        })
        .collect()
}

pub(crate) fn configured_route_exists(
    providers: &BTreeMap<String, ConfiguredProvider>,
    route: &str,
) -> Result<bool> {
    for configured in providers.values() {
        let definition = provider(&configured.selection.provider)?;
        if has_chat_route(definition, configured, &configured.selection, route) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn has_chat_route(
    definition: &ProviderDefinition,
    configured: &ConfiguredProvider,
    selection: &ProviderConfig,
    route: &str,
) -> bool {
    chat_variants(definition, configured, selection).any(|(id, _, _)| id.matches(route))
}

pub(crate) fn default_instance(gateway: &GatewayConfig) -> Option<&str> {
    gateway
        .bot_defaults
        .as_ref()
        .map(|defaults| defaults.config.provider.instance.as_str())
}

pub(crate) fn configured_model_routes(
    configured_providers: &BTreeMap<String, ConfiguredProvider>,
    default_instance: Option<&str>,
    credentialed: &CredentialedInstances<'_>,
) -> Result<Vec<CatalogRoute>> {
    let mut routes = Vec::new();
    let mut configured = configured_providers.values().collect::<Vec<_>>();
    configured
        .sort_by_key(|configured| Some(configured.selection.instance.as_str()) != default_instance);
    for configured in configured {
        if let Some(definition) = available_chat_provider(configured, credentialed)? {
            routes.extend(catalog_routes(
                definition,
                configured,
                &configured.selection,
            ));
        }
    }
    Ok(routes)
}

fn available_chat_provider(
    configured: &ConfiguredProvider,
    credentialed: &CredentialedInstances<'_>,
) -> Result<Option<&'static ProviderDefinition>> {
    let definition = provider(&configured.selection.provider)?;
    Ok((credentialed.contains(&configured.selection)
        && definition
            .web_search()
            .contains(&configured.selection.web_search))
    .then_some(definition))
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
                    preset.clone() // rules-allow: the configured catalog owns its copy of the built-in preset
                } else {
                    ModelPreset {
                        context_window: model_context_window(definition, &input.id),
                        tool_discovery: definition.tool_discovery(&input.id, None),
                        label: input.id.clone(), // rules-allow: the label starts as the model ID
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
                                    label: id.clone(), // rules-allow: the label starts as the effort ID
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

/// Parts of a wire route ID. Models and efforts may themselves contain the separator.
pub(crate) struct RouteId<'a>([&'a str; 5]);

impl<'a> RouteId<'a> {
    pub(crate) fn new(instance: &'a str, model: &'a str, effort: Option<&'a str>) -> Self {
        Self([instance, "::", model, "::", effort.unwrap_or("default")])
    }

    pub(crate) fn encode(&self) -> String {
        self.0.concat()
    }

    fn starts_with(&self, prefix: &str) -> bool {
        self.0
            .into_iter()
            .flat_map(str::bytes)
            .take(prefix.len())
            .eq(prefix.bytes())
    }

    fn matches(&self, route: &str) -> bool {
        self.0
            .into_iter()
            .try_fold(route, |tail, part| tail.strip_prefix(part))
            == Some("")
    }
}

pub(crate) fn configured_model<'a>(
    definition: &'a ProviderDefinition,
    configured: &'a ConfiguredProvider,
    model: &str,
) -> Option<&'a ModelPreset> {
    models(definition, configured)
        .iter()
        .find(|preset| preset.id == model)
}

pub(crate) fn default_model_route(
    definition: &ProviderDefinition,
    configured: &ConfiguredProvider,
    model: &str,
) -> String {
    let effort = configured_model(definition, configured, model)
        .and_then(|model| model.default_reasoning.as_deref());
    RouteId::new(&configured.selection.instance, model, effort).encode()
}

/// Recognizes also retired IDs that are no longer advertised. A still-advertised
/// route belongs to its exact model, even when another model's ID is its prefix.
pub(crate) fn retired_model_routes(
    definition: &ProviderDefinition,
    configured: &ConfiguredProvider,
    retired: &BTreeSet<&str>,
) -> impl Fn(&str) -> bool + use<> {
    let instance = &configured.selection.instance;
    // An empty effort encodes exactly the instance, model and trailing separator.
    let prefixes: Vec<_> = retired
        .iter()
        .map(|model| RouteId::new(instance, model, Some("")).encode())
        .collect();
    let retained: BTreeSet<_> = chat_variants(definition, configured, &configured.selection)
        .filter(|(route, model, _)| {
            !retired.contains(model.id.as_str())
                && prefixes.iter().any(|prefix| route.starts_with(prefix))
        })
        .map(|(route, _, _)| route.encode())
        .collect();
    move |route| {
        prefixes.iter().any(|prefix| route.starts_with(prefix)) && !retained.contains(route)
    }
}

/// The chat route one provider selection resolves to.
pub(crate) struct SelectedRoute<'a> {
    pub(crate) route: String,
    pub(crate) effort: Option<&'a str>,
    /// The selected model's preset, when the configured catalog or provider knows it.
    pub(crate) preset: Option<&'a ModelPreset>,
    pub(crate) context_window: i64,
}

/// Resolves a selection against its configured instance, or against the provider's
/// built-in presets when the instance is no longer configured.
pub(crate) fn selected_route<'a>(
    definition: &'a ProviderDefinition,
    configured: Option<&'a ConfiguredProvider>,
    selection: &'a ProviderConfig,
) -> SelectedRoute<'a> {
    let selected = match configured {
        Some(configured) => configured_model(definition, configured, &selection.model),
        None => definition.model(&selection.model),
    };
    let effort = selection
        .reasoning_effort
        .as_deref()
        .or_else(|| selected?.default_reasoning.as_deref());
    let preset = selected.or_else(|| definition.model(&selection.model));
    SelectedRoute {
        route: RouteId::new(&selection.instance, &selection.model, effort).encode(),
        effort,
        preset,
        context_window: preset.map_or_else(
            || model_context_window(definition, &selection.model),
            |model| model.context_window,
        ),
    }
}

pub(crate) fn models<'a>(
    definition: &ProviderDefinition,
    configured: &'a ConfiguredProvider,
) -> &'a [ModelPreset] {
    if definition.locked_models() && configured.models.is_empty() {
        definition.models()
    } else {
        &configured.models
    }
}

fn chat_variants<'a>(
    definition: &'a ProviderDefinition,
    configured: &'a ConfiguredProvider,
    selection: &'a ProviderConfig,
) -> impl Iterator<Item = (RouteId<'a>, &'a ModelPreset, Option<&'a ReasoningPreset>)> {
    let models = if selection.model.is_empty() {
        &[]
    } else {
        models(definition, configured)
    };
    models
        .iter()
        .filter(|model| model.id == selection.model)
        .chain(models.iter().filter(|model| model.id != selection.model))
        .flat_map(move |model| {
            variant_routes(&selection.instance, &model.id, &model.reasoning)
                .map(move |(route, variant)| (route, model, variant))
        })
}

pub(crate) fn catalog_routes(
    definition: &ProviderDefinition,
    configured: &ConfiguredProvider,
    selection: &ProviderConfig,
) -> Vec<CatalogRoute> {
    chat_variants(definition, configured, selection)
        .map(|(route, model, variant)| {
            let id = model.id.as_str();
            let effort = variant.map(|variant| variant.id.as_str());
            let variant_label = variant.map(|variant| variant.label.as_str());
            let provider = ProviderConfig {
                instance: selection.instance.clone(), // rules-allow: each catalog route owns its provider selection
                provider: selection.provider.clone(), // rules-allow: each catalog route owns its provider selection
                base_url: configured.selection.base_url.as_ref().map(BaseUrl::share),
                endpoint_auth: configured.selection.endpoint_auth,
                model: id.into(),
                reasoning_effort: effort.map(str::to_owned),
                service_tier: selection
                    .service_tier
                    .as_ref()
                    .or(configured.selection.service_tier.as_ref())
                    .cloned(), // rules-allow: each catalog route owns its provider selection
                web_search: configured.selection.web_search,
                tool_discovery: configured.selection.tool_discovery,
            };
            CatalogRoute {
                choice: ModelChoice {
                    route: route.encode().into(),
                    group: format!("{} · {}", configured.label, model.label).into(),
                    model: id.into(),
                    reasoning_effort: effort.map(Arc::from),
                    variant_label: variant_label.map(str::to_owned),
                    context_window: Some(model.context_window),
                    supports_image_input: definition.supports_image_input(),
                    supports_image_generation: definition.supports_at(
                        ModelCapability::ImageGeneration,
                        configured.selection.base_url.as_deref(),
                    ) || !configured.image_model_ids.is_empty()
                        || configured
                            .image_models
                            .as_ref()
                            .is_some_and(|m| !m.is_empty()),
                    supports_realtime_voice: configured.voice_models.as_ref().map_or_else(
                        || {
                            definition.supports_at(
                                ModelCapability::RealtimeVoice,
                                configured.selection.base_url.as_deref(),
                            )
                        },
                        |models| !models.is_empty(),
                    ),
                    tool_discovery: catalog_tool_discovery(
                        definition, configured, &provider, model,
                    ),
                },
                endpoint: endpoints::endpoint(configured, crate::wire::ProviderModelKind::Language),
                provider,
                capability: None,
            }
        })
        .collect()
}

pub(crate) struct CatalogRoute {
    pub(crate) endpoint: Option<crate::wire::GroupEndpointOverride>,
    pub(crate) choice: ModelChoice,
    pub(crate) provider: ProviderConfig,
    pub(crate) capability: Option<ModelCapability>,
}

impl CatalogRoute {
    pub(crate) fn operation_base(
        &self,
        definition: &ProviderDefinition,
        base: BaseUrl,
    ) -> Result<BaseUrl> {
        if let Some(endpoint) = &self.endpoint {
            Ok(definition.with_group_endpoint(
                endpoint.kind,
                &base,
                &endpoint.api,
                &endpoint.path,
            )?)
        } else {
            Ok(base)
        }
    }
}

/// Image, voice and decision choices, independent of the chat catalog.
#[derive(Default)]
pub(crate) struct MediaRoutes {
    pub(crate) images: Vec<CatalogRoute>,
    pub(crate) voices: Vec<CatalogRoute>,
    pub(crate) decisions: Vec<CatalogRoute>,
    pub(crate) dictations: Vec<CatalogRoute>,
}

/// Effective setup defaults. An explicitly selected quality route still takes precedence.
pub(crate) fn image_defaults(
    d: &ProviderDefinition,
    configured: &ConfiguredProvider,
    id: &str,
) -> mobius::backend::model::ImageDefaults {
    let mut defaults = configured
        .image_defaults
        .get(id)
        .cloned() // rules-allow: effective defaults overlay stored values without editing configuration
        .unwrap_or_default();
    if defaults.quality.is_none() && configured.image_model_ids.is_empty() {
        defaults.quality = configured
            .image_models
            .as_deref()
            .unwrap_or_else(|| {
                d.media_presets_at(
                    ModelCapability::ImageGeneration,
                    configured.selection.base_url.as_deref(),
                )
            })
            .iter()
            .find(|m| m.id == id)
            .and_then(|m| m.variants.first())
            .map(|v| v.id.clone()); // rules-allow: effective default owns preset quality
    }
    defaults
}

/// Derives media choices directly from configured providers, optionally checking credentials.
pub(crate) fn media_routes(
    configured_providers: &BTreeMap<String, ConfiguredProvider>,
    credentialed: Option<&CredentialedInstances<'_>>,
) -> Result<MediaRoutes> {
    let mut media = MediaRoutes::default();
    for configured in configured_providers.values() {
        if credentialed.is_some_and(|credentialed| !credentialed.contains(&configured.selection)) {
            continue;
        }
        let definition = provider(&configured.selection.provider)?;
        let at = configured.selection.base_url.as_deref();
        if configured.image_model_ids.is_empty() {
            for preset in configured.image_models.as_deref().unwrap_or_else(|| {
                definition.media_presets_at(ModelCapability::ImageGeneration, at)
            }) {
                for quality in media_variants(ModelCapability::ImageGeneration, &preset.variants) {
                    media.images.push(media_route(
                        configured,
                        definition,
                        &preset.id,
                        &preset.label,
                        quality,
                        Some(ModelCapability::ImageGeneration),
                    ));
                }
            }
        }
        // Explicit image IDs opt a compatible endpoint into its image API.
        for id in &configured.image_model_ids {
            media.images.push(media_route(
                configured,
                definition,
                id,
                id,
                None,
                Some(ModelCapability::ImageGeneration),
            ));
        }
        for id in &configured.dictation_model_ids {
            media
                .dictations
                .push(media_route(configured, definition, id, id, None, None));
        }
        for id in &configured.decision_model_ids {
            media.decisions.push(media_route(
                configured,
                definition,
                id,
                id,
                None,
                Some(ModelCapability::Decisions),
            ));
        }
        for preset in configured
            .voice_models
            .as_deref()
            .unwrap_or_else(|| definition.media_presets_at(ModelCapability::RealtimeVoice, at))
        {
            for voice in media_variants(ModelCapability::RealtimeVoice, &preset.variants) {
                media.voices.push(media_route(
                    configured,
                    definition,
                    &preset.id,
                    &preset.label,
                    voice,
                    Some(ModelCapability::RealtimeVoice),
                ));
            }
        }
    }
    Ok(media)
}

/// Resolves only the configured decision choice, without assembling unrelated media routes.
pub(crate) fn decision_route(
    configured_providers: &BTreeMap<String, ConfiguredProvider>,
    selected: &str,
) -> Result<Option<CatalogRoute>> {
    for configured in configured_providers.values() {
        if let Some(model) = configured
            .decision_model_ids
            .iter()
            .find(|id| RouteId::new(&configured.selection.instance, id, None).matches(selected))
        {
            let definition = provider(&configured.selection.provider)?;
            return Ok(Some(media_route(
                configured,
                definition,
                model,
                model,
                None,
                Some(ModelCapability::Decisions),
            )));
        }
    }
    Ok(None)
}

fn media_route(
    configured: &ConfiguredProvider,
    definition: &ProviderDefinition,
    model: &str,
    label: &str,
    variant: Option<&ReasoningPreset>,
    capability: Option<ModelCapability>,
) -> CatalogRoute {
    let instance = configured.selection.instance.as_str();
    CatalogRoute {
        endpoint: endpoints::endpoint(
            configured,
            match capability {
                Some(ModelCapability::ImageGeneration) => crate::wire::ProviderModelKind::Image,
                Some(ModelCapability::RealtimeVoice) => crate::wire::ProviderModelKind::Voice,
                Some(ModelCapability::Decisions) => crate::wire::ProviderModelKind::Decision,
                _ => crate::wire::ProviderModelKind::Dictation,
            },
        ),
        choice: ModelChoice {
            route: RouteId::new(instance, model, variant.map(|variant| variant.id.as_str()))
                .encode()
                .into(),
            group: format!("{} · {label}", configured.label).into(),
            model: model.into(),
            reasoning_effort: variant.map(|variant| variant.id.as_str().into()),
            variant_label: variant.map(|variant| variant.label.as_str().into()),
            context_window: None,
            supports_image_input: false,
            supports_image_generation: capability == Some(ModelCapability::ImageGeneration),
            supports_realtime_voice: capability == Some(ModelCapability::RealtimeVoice),
            tool_discovery: definition
                .tool_discovery(model, configured.selection.base_url.as_deref()),
        },
        provider: ProviderConfig {
            instance: instance.into(),
            provider: configured.selection.provider.as_str().into(),
            model: model.into(),
            base_url: configured.selection.base_url.as_ref().map(BaseUrl::share),
            endpoint_auth: configured.selection.endpoint_auth,
            reasoning_effort: None,
            service_tier: None,
            web_search: Default::default(),
            tool_discovery: None,
        },
        capability,
    }
}

pub(crate) fn variant_routes<'a>(
    instance: &'a str,
    model: &'a str,
    variants: &'a [ReasoningPreset],
) -> impl Iterator<Item = (RouteId<'a>, Option<&'a ReasoningPreset>)> {
    with_default(variants).map(move |variant| {
        (
            RouteId::new(instance, model, variant.map(|preset| preset.id.as_str())),
            variant,
        )
    })
}

/// Images also have a stable route for the configured default quality.
pub(crate) fn media_variants(
    capability: ModelCapability,
    variants: &[ReasoningPreset],
) -> impl Iterator<Item = Option<&ReasoningPreset>> {
    let default = variants.is_empty()
        || (capability == ModelCapability::ImageGeneration
            && !variants.iter().any(|v| v.id == "default"));
    default
        .then_some(None)
        .into_iter()
        .chain(variants.iter().map(Some))
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
        return Ok(true);
    }
    let base_url = Some(selected_base_url(definition, selection));
    match definition.auth() {
        ProviderAuth::ApiKey(default_env) => {
            if credentials
                .get(&selection.instance, definition.id(), base_url)?
                .is_some()
            {
                return Ok(true);
            }
            if !definition.uses_default_endpoint(base_url.map(BaseUrl::as_str)) {
                return Ok(false);
            }
            Ok(std::env::var(default_env).is_ok_and(|value| !value.trim().is_empty()))
        }
        ProviderAuth::Browser(auth) => auth
            .configured(&store.provider_auth_path())
            .map_err(Error::from),
    }
}

pub(crate) fn catalog_tool_discovery(
    definition: &ProviderDefinition,
    configured: &ConfiguredProvider,
    selection: &ProviderConfig,
    model: &ModelPreset,
) -> mobius::protocol::ToolDiscoveryMode {
    configured
        .group_endpoints
        .iter()
        .find_map(|endpoint| definition.group_api_tool_discovery(endpoint.kind, &endpoint.api))
        .unwrap_or_else(|| effective_tool_discovery(definition, selection, model))
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
    models(definition, configured)
        .iter()
        .map(|model| {
            let mut value = model.clone(); // rules-allow: provider records are sent to clients
            value.tool_discovery =
                catalog_tool_discovery(definition, configured, &configured.selection, model);
            value
        })
        .collect()
}

fn provider_models(definition: &ProviderDefinition) -> Vec<ProviderModel> {
    definition.models().to_vec()
}

pub(super) fn language_discovery(
    definition: &ProviderDefinition,
    configured: Option<&ConfiguredProvider>,
    api: Option<&str>,
) -> mobius::protocol::ToolDiscoveryMode {
    if let Some(mode) = api.and_then(|a| {
        definition.group_api_tool_discovery(crate::wire::ProviderModelKind::Language, a)
    }) {
        return mode;
    }
    let Some(instance) = configured else {
        return definition.default_tool_discovery();
    };
    let selection = &instance.selection;
    configured_model(definition, instance, &selection.model).map_or_else(
        || {
            selection.tool_discovery.unwrap_or_else(|| {
                definition.tool_discovery(&selection.model, selection.base_url.as_deref())
            })
        },
        |preset| effective_tool_discovery(definition, selection, preset),
    )
}

pub(crate) fn model_groups(
    definition: &ProviderDefinition,
    base: &BaseUrl,
    configured: Option<&ConfiguredProvider>,
) -> Vec<crate::wire::ProviderModelGroup> {
    use crate::wire::{ProviderModelGroup, ProviderModelKind};
    let at = Some(base.as_str());
    mobius::backend::model::endpoints::model_group_defaults()
        .iter()
        .map(|group| {
            let apis = definition.group_endpoint_options(group.kind, base);
            let supported = !apis.is_empty();
            let editable = supported
                && match group.kind {
                    ProviderModelKind::Image => definition
                        .media_presets_at(ModelCapability::ImageGeneration, at)
                        .is_empty(),
                    ProviderModelKind::Language | ProviderModelKind::Voice => {
                        !definition.locked_models()
                    }
                    ProviderModelKind::Decision | ProviderModelKind::Dictation => true,
                    _ => false,
                };
            let mut record = ProviderModelGroup {
                test_description: None,
                symbol: group.symbol.clone(), // rules-allow: wire record owns its icon
                fields: Vec::new(),
                models: Vec::new(),
                model_presets: Vec::new(),
                new_model_fields: Vec::new(),
                kind: group.kind,
                label: group.label.clone(), // rules-allow: each wire catalog owns the group label
                supported,
                editable,
            };
            form::populate(&mut record, definition, base, configured, &apis);
            record
        })
        .collect()
}

fn provider_status(definition: &ProviderDefinition) -> ProviderStatus {
    let auth = match definition.auth() {
        ProviderAuth::ApiKey(_) => ProviderAuthKind::ApiKey,
        ProviderAuth::Browser(_) => ProviderAuthKind::DeviceCode,
    };
    ProviderStatus {
        general_section: form::header(),
        provider: definition.id().into(),
        label: definition.label().into(),
        symbol: definition.symbol(),
        description: definition.description().into(),
        model_groups: model_groups(definition, definition.default_base_url(), None),
        auth,
        default_base_url: definition.default_base_url().share(),
        models: provider_models(definition),
        web_search: definition
            .web_search()
            .iter()
            .map(|search| FrontendSettingOption {
                disables: Arc::default(),
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
                Some(
                    provider("openai_codex")
                        .expect("Codex")
                        .default_base_url()
                        .as_str(),
                ),
            ),
            (
                "openai_codex",
                Some("https://custom.example/v1"),
                Some("https://custom.example/v1"),
            ),
        ] {
            let mut selection = crate::wire::AgentComposition::default().provider;
            selection.provider = provider_id.into();
            selection.base_url = explicit.map(|url| BaseUrl::new(url).unwrap());
            assert_eq!(
                Some(
                    selected_base_url(provider(provider_id).expect("provider"), &selection)
                        .as_str()
                ),
                expected
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
            image_defaults: Default::default(),
            operator_managed: false,
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
                        id: "fast::v2".into(),
                        reasoning_efforts: Some(vec!["brief::soft".into()]),
                        default_reasoning: Some("brief::soft".into()),
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
            group_endpoints: Vec::new(),
            dictation_model_ids: Vec::new(),
            decision_model_ids: Vec::new(),
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
                ("fast::v2", vec!["brief::soft"], Some("brief::soft")),
                ("plain", Vec::new(), None),
            ]
        );
        let routes = catalog_routes(definition, &configured, &configured.selection);
        assert_eq!(
            routes
                .iter()
                .map(|route| &*route.choice.route)
                .collect::<Vec<_>>(),
            [
                "custom::reasoner::low",
                "custom::reasoner::high",
                "custom::fast::v2::brief::soft",
                "custom::plain::default"
            ]
        );
        let target = default_model_route(definition, &configured, "fast::v2");
        assert_eq!(target, "custom::fast::v2::brief::soft");
        assert!(
            has_chat_route(definition, &configured, &configured.selection, &target),
            "membership accepts separators inside model and effort IDs"
        );
        assert!(
            !has_chat_route(
                definition,
                &configured,
                &configured.selection,
                "custom::fast::v2::brief"
            ),
            "a prefix is not a catalog route"
        );
        let retired = retired_model_routes(
            definition,
            &configured,
            &BTreeSet::from(["fast", "gone::v1"]),
        );
        assert!(
            !retired(&target),
            "retiring a shorter ID keeps the advertised model"
        );
        assert!(
            retired("custom::gone::v1::high"),
            "a saved route can outlive its catalog entry"
        );
        let builtin = provider("openai_socket").expect("builtin provider");
        configured.selection.provider = "openai_socket".into();
        configured.models.clear();
        let inherited = configured_models(builtin, &configured);
        assert!(!inherited.is_empty());
        assert_eq!(inherited, provider_status(builtin).models);
    }
}
