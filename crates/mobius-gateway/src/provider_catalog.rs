//! Gateway provider catalog, route policy, and credential availability.

use std::collections::BTreeMap;

use mobius::backend::model::provider::{ProviderAuth, ProviderDefinition, provider, providers};
use mobius::protocol::{FrontendSettingOption, FrontendTone, ModelChoice};

use crate::config::{
    ConfigStore, ConfiguredProvider, CredentialStore, DEFAULT_CONTEXT_WINDOW, GatewayConfig,
    effective_reasoning_effort, model_route_id,
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
    gateway: &GatewayConfig,
    store: &ConfigStore,
    credentials: &CredentialStore,
) -> Result<Vec<ProviderInstance>> {
    gateway
        .configured_providers
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
            })
        })
        .collect()
}

pub(crate) fn configured_model_choices(
    gateway: &GatewayConfig,
    store: &ConfigStore,
    credentials: &CredentialStore,
) -> Result<Vec<ModelChoice>> {
    Ok(configured_model_routes(gateway, store, credentials)?
        .into_iter()
        .map(|route| route.choice)
        .collect())
}

pub(crate) fn configured_model_providers(
    gateway: &GatewayConfig,
    store: &ConfigStore,
    credentials: &CredentialStore,
) -> Result<BTreeMap<String, String>> {
    Ok(configured_model_routes(gateway, store, credentials)?
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
    gateway: &GatewayConfig,
    store: &ConfigStore,
    credentials: &CredentialStore,
) -> Result<Vec<CatalogRoute>> {
    let mut routes = Vec::new();
    let default_instance = gateway
        .bot_defaults
        .as_ref()
        .map(|default| default.config.provider.instance.as_str());
    let mut configured = gateway.configured_providers.values().collect::<Vec<_>>();
    configured
        .sort_by_key(|configured| Some(configured.selection.instance.as_str()) != default_instance);
    for configured in configured {
        let definition = provider(&configured.selection.provider)?;
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
        .map(|preset| (preset.id, Some(preset)))
        .collect::<Vec<_>>();
    for model in &configured.model_ids {
        if models.iter().all(|(candidate, _)| *candidate != model) {
            models.push((model, None));
        }
    }
    models.sort_by_key(|(model, _)| *model != selection.model);

    let mut routes = Vec::new();
    for (model, preset) in models {
        let catalog_default = preset
            .and_then(|preset| preset.default_reasoning)
            .or_else(|| configured.reasoning_efforts.first().map(String::as_str));
        let preferred = if model == selection.model {
            effective_reasoning_effort(definition, configured, selection)
        } else {
            catalog_default
        };
        let mut efforts = vec![preferred];
        for reasoning in preset.into_iter().flat_map(|preset| preset.reasoning) {
            let effort = Some(reasoning.id);
            if !efforts.contains(&effort) {
                efforts.push(effort);
            }
        }
        if preset.is_none() {
            for reasoning in &configured.reasoning_efforts {
                let effort = Some(reasoning.as_str());
                if !efforts.contains(&effort) {
                    efforts.push(effort);
                }
            }
        }
        for effort in efforts {
            let mut provider = selection.clone();
            provider.model = model.into();
            provider.reasoning_effort = effort.map(str::to_string);
            let route = model_route_id(&selection.instance, model, effort);
            routes.push(CatalogRoute {
                choice: ModelChoice {
                    route,
                    group: format!(
                        "{} · {}",
                        configured.label,
                        preset.map_or(model, |preset| preset.label)
                    ),
                    model: model.into(),
                    reasoning_effort: effort.map(str::to_string),
                    context_window: Some(
                        preset.map_or(DEFAULT_CONTEXT_WINDOW, |preset| preset.context_window),
                    ),
                    supports_image_input: definition.supports_image_input(),
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
        auth,
        default_base_url: definition.default_base_url().map(str::to_string),
        default_api_key_env,
        models: definition
            .models()
            .iter()
            .map(|model| ProviderModel {
                id: model.id.into(),
                label: model.label.into(),
                description: model.description.into(),
                context_window: model.context_window,
                reasoning: model
                    .reasoning
                    .iter()
                    .map(|reasoning| ReasoningChoice {
                        id: reasoning.id.into(),
                        label: reasoning.label.into(),
                        description: reasoning.description.into(),
                    })
                    .collect(),
                default_reasoning: model.default_reasoning.map(str::to_string),
                tool_discovery: model.tool_discovery,
            })
            .collect(),
        web_search: definition
            .web_search()
            .iter()
            .map(|search| FrontendSettingOption {
                value: search.id().into(),
                label: search.label().into(),
                description: search.description().into(),
                symbol: None,
                tone: FrontendTone::Neutral,
            })
            .collect(),
        tool_discovery: definition.default_tool_discovery(),
        custom_endpoint_tool_discovery: definition.custom_endpoint_tool_discovery(),
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
            ("openai_codex", None, None),
            ("openai_codex", Some("https://custom.example/v1"), None),
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
    fn provider_status_uses_manifest_defaults() {
        let status = provider_status(provider("openai_socket").expect("provider"));

        assert_eq!(status.provider, "openai_socket");
        assert_eq!(status.label, "OpenAI");
        assert_eq!(status.symbol, FrontendSymbol::Custom("chat_gpt".into()));
        assert_eq!(status.models[0].id, "gpt-5.6-sol");
        assert_eq!(status.tool_discovery, ToolDiscoveryMode::Native);
        assert_eq!(status.models[0].tool_discovery, ToolDiscoveryMode::Native);
        assert_eq!(
            status.default_api_key_env.as_deref(),
            Some("OPENAI_API_KEY")
        );
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

        let custom = provider_status(provider("responses").expect("provider"));
        assert!(custom.models.is_empty());
        assert!(custom.model_ids_configurable);
        assert_eq!(
            custom.default_base_url.as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(custom.default_api_key_env, None);
        assert_eq!(custom.tool_discovery, ToolDiscoveryMode::Rebuild);

        let openrouter = provider_status(provider("openrouter").expect("provider"));
        assert!(openrouter.models.is_empty());
        assert!(openrouter.model_ids_configurable);
        assert_eq!(
            openrouter.default_base_url.as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
        assert_eq!(openrouter.tool_discovery, ToolDiscoveryMode::Native);
        assert_eq!(
            openrouter.custom_endpoint_tool_discovery,
            Some(ToolDiscoveryMode::Rebuild)
        );
    }

    #[test]
    fn catalog_routes_resolve_model_and_endpoint_tool_discovery() {
        let anthropic = provider("anthropic").expect("anthropic");
        assert_eq!(
            anthropic.tool_discovery("claude-sonnet-5", anthropic.default_base_url()),
            ToolDiscoveryMode::Rebuild
        );
        assert_eq!(
            anthropic.tool_discovery("claude-opus-4-8", anthropic.default_base_url()),
            ToolDiscoveryMode::Native
        );
        assert_eq!(
            anthropic.tool_discovery("claude-opus-4-8", Some("https://proxy.example/v1")),
            ToolDiscoveryMode::Rebuild
        );

        let openrouter = provider("openrouter").expect("openrouter");
        assert_eq!(
            openrouter.tool_discovery("openai/gpt-5.6-luna", openrouter.default_base_url()),
            ToolDiscoveryMode::Native
        );
        assert_eq!(
            openrouter.tool_discovery("openai/gpt-5.6-luna", Some("https://proxy.example/v1")),
            ToolDiscoveryMode::Rebuild
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
            selection.reasoning_effort = Some("medium".into());
            let config = GatewayConfig::new("127.0.0.1:8741".parse().expect("listen"), None)
                .expect("config")
                .registering_provider(
                    selection,
                    id.into(),
                    Default::default(),
                    vec![model.into(), "custom-model".into()],
                    vec!["medium".into(), "max".into()],
                )
                .expect("register custom catalog");
            let configured = &config.configured_providers[id];
            let routes = catalog_routes(definition, configured, &configured.selection);
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
    fn every_built_in_provider_advertises_a_discovery_mode() {
        let expected = [
            ("openai_socket", ToolDiscoveryMode::Native),
            ("openai_codex", ToolDiscoveryMode::Native),
            ("deepseek", ToolDiscoveryMode::Rebuild),
            ("kimi", ToolDiscoveryMode::Rebuild),
            ("openrouter", ToolDiscoveryMode::Native),
            ("anthropic", ToolDiscoveryMode::Rebuild),
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
