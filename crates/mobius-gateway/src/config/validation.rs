use super::{
    AgentComposition, BTreeMap, BTreeSet, ConfiguredProvider, Error, GatewayConfig,
    MAX_CLOUDFLARE_TOKEN_BYTES, MAX_CUSTOM_MODEL_ROUTES, MAX_PROVIDER_CATALOG_BYTES,
    MAX_PROVIDER_CATALOG_ENTRIES, MAX_PROVIDER_CATALOG_ENTRY_BYTES, MAX_PROVIDER_LABEL_BYTES,
    MAX_SYSTEM_PROMPT_BYTES, ProviderConfig, ProviderDefinition, ProviderEndpointAuth, Result,
    provider,
};
use crate::harness::mobius::middleware_manifest::MobiusSettings;

/// Validates one Bot against its exact selected model route and capabilities, returning its
/// parsed middleware settings.
pub(crate) fn validate_bot_compatibility(
    gateway: &GatewayConfig,
    config: &AgentComposition,
    catalogs: mobius::middleware::manifest::ModelCatalogs<'_>,
) -> Result<MobiusSettings> {
    let settings =
        validate_agent_composition_with_ceilings(config, gateway.execution.subagent_ceilings)?;
    let selection = &config.provider;
    let configured = gateway
        .configured_providers
        .get(&selection.instance)
        .ok_or_else(|| Error::Config("active provider is not in the configured catalog".into()))?;
    validate_configured_provider_selection(configured, selection)?;
    let definition = provider(&selection.provider)?;
    let route = crate::harness::mobius::provider_catalog::selected_route(
        definition,
        Some(configured),
        selection,
    )
    .route;
    let catalog =
        crate::harness::mobius::provider_catalog::catalog_routes(definition, configured, selection);
    if !catalog
        .iter()
        .any(|candidate| *candidate.choice.route == *route)
    {
        return Err(Error::Config(
            "active model route is not in the configured catalog".into(),
        ));
    }
    crate::harness::mobius::middleware_manifest::validate_choices(&config.middleware, catalogs)?;
    Ok(settings)
}

fn validate_computer_control_policy(
    config: &AgentComposition,
    approval_policy: mobius::backend::sandbox::ApprovalPolicy,
) -> Result<()> {
    use mobius::middleware::computer_control;

    if config.middleware.enabled(computer_control::MANIFEST.id)
        && !computer_control::supports_approval_policy(approval_policy)
    {
        return Err(Error::Config(
            "computer control middleware requires Full access or Allow · no network".into(),
        ));
    }
    Ok(())
}

/// Validates the structural composition without imposing a host's resource policy, returning
/// its approval policy.
/// # Errors
///
/// Returns an error if the supplied value is invalid.
pub fn validate_agent_composition(
    config: &AgentComposition,
) -> Result<mobius::backend::sandbox::ApprovalPolicy> {
    validate_mobius_settings(config).map(|settings| settings.approval_policy)
}

/// Validates a composition's structure, returning its parsed middleware settings.
pub(crate) fn validate_mobius_settings(config: &AgentComposition) -> Result<MobiusSettings> {
    let maximum = usize::try_from(i64::MAX).unwrap_or(usize::MAX);
    let ceilings = mobius::middleware::subagents::SubagentCeilings::new(u8::MAX, maximum, maximum)?;
    validate_agent_composition_with_ceilings(config, ceilings)
}

pub(crate) fn validate_agent_composition_with_ceilings(
    config: &AgentComposition,
    ceilings: mobius::middleware::subagents::SubagentCeilings,
) -> Result<MobiusSettings> {
    if config.max_model_steps == 0 {
        return Err(Error::Config("maximum model steps must be positive".into()));
    }
    if config.system_prompt.trim().is_empty()
        || config.system_prompt.len() > MAX_SYSTEM_PROMPT_BYTES
    {
        return Err(Error::Config(format!(
            "system prompt must be 1–{MAX_SYSTEM_PROMPT_BYTES} bytes"
        )));
    }
    crate::harness::mobius::extensions::validate_ids(&config.extensions)?;
    validate_provider_config(&config.provider)?;
    crate::harness::mobius::middleware_manifest::validate(&config.middleware, ceilings)?;
    let settings = MobiusSettings::parse(&config.middleware)?;
    validate_computer_control_policy(config, settings.approval_policy)?;
    Ok(settings)
}

pub(super) fn validate_provider_config(config: &ProviderConfig) -> Result<()> {
    validate_provider_transport(config)?;
    if config.model.trim().is_empty() || config.model.len() > 1024 {
        return Err(Error::Config("model must be 1–1024 bytes".into()));
    }
    provider(&config.provider)?
        .model_config_is_valid(&config.model, config.reasoning_effort.as_deref())?;
    Ok(())
}

fn validate_provider_transport(config: &ProviderConfig) -> Result<()> {
    validate_instance_id(&config.instance)?;
    if config.provider.trim().is_empty() || config.provider.len() > 256 {
        return Err(Error::Config("provider ID must be 1–256 bytes".into()));
    }
    let definition = provider(&config.provider)?;
    if let Some(mode) = config.tool_discovery {
        definition.validate_tool_discovery(mode)?;
    }
    validate_provider_endpoint_auth(definition, config)?;
    Ok(())
}

fn validate_provider_endpoint_auth(
    definition: &ProviderDefinition,
    config: &ProviderConfig,
) -> Result<()> {
    if config.endpoint_auth == ProviderEndpointAuth::ProviderDefault {
        return Ok(());
    }
    definition
        .validate_credentialless_endpoint(config.base_url.as_ref())
        .map_err(Error::from)
}

pub(super) fn validate_configured_provider(configured: &ConfiguredProvider) -> Result<()> {
    if configured.selection.model.is_empty() {
        validate_provider_transport(&configured.selection)?;
        provider(&configured.selection.provider)?;
        if !configured.models.is_empty() || configured.selection.reasoning_effort.is_some() {
            return Err(Error::Config(
                "a chat catalog requires a selected chat model".into(),
            ));
        }
    } else {
        validate_provider_config(&configured.selection)?;
    }
    validate_provider_label(&configured.label)?;
    let definition = provider(&configured.selection.provider)?;
    if definition.locked_models() && !configured.models.is_empty() {
        return Err(Error::Config(format!(
            "provider `{}` uses its advertised model and reasoning catalogs",
            configured.selection.provider
        )));
    }
    if !definition.locked_models() && !configured.selection.model.is_empty() {
        validate_catalog_entries(
            configured.models.iter().map(|model| model.id.as_str()),
            "model IDs",
            "model ID",
        )?;
    }
    for model in &configured.models {
        if !model.reasoning.is_empty() {
            validate_catalog_entries(
                model.reasoning.iter().map(|effort| effort.id.as_str()),
                "reasoning efforts",
                "reasoning effort",
            )?;
        }
        definition.validate_tool_discovery(
            crate::harness::mobius::provider_catalog::effective_tool_discovery(
                definition,
                &configured.selection,
                model,
            ),
        )?;
        if model.context_window <= 0 {
            return Err(Error::Config(format!(
                "model `{}` requires a positive context window",
                model.id
            )));
        }
        validate_display(&model.label, &model.description)?;
        for effort in &model.reasoning {
            validate_display(&effort.label, &effort.description)?;
        }
        match (&model.default_reasoning, model.reasoning.is_empty()) {
            (None, true) => {}
            (Some(default), false)
                if model.reasoning.iter().any(|effort| &effort.id == default) => {}
            _ => {
                return Err(Error::Config(format!(
                    "model `{}` must have an explicit default reasoning effort from its nonempty catalog, and no default when its catalog is empty",
                    model.id
                )));
            }
        }
    }
    if !configured.image_model_ids.is_empty() {
        if !definition.supports(mobius::protocol::ModelCapability::ImageGeneration) {
            return Err(Error::Config(format!(
                "provider `{}` does not accept listed image model IDs",
                configured.selection.provider
            )));
        }
        validate_catalog_entries(
            configured.image_model_ids.iter().map(String::as_str),
            "image model IDs",
            "image model ID",
        )?;
        validate_image_ids(configured.image_model_ids.iter().map(String::as_str))?;
    }
    validate_media_catalog(
        definition,
        configured.image_models.as_deref(),
        mobius::protocol::ModelCapability::ImageGeneration,
    )?;
    validate_media_catalog(
        definition,
        configured.voice_models.as_deref(),
        mobius::protocol::ModelCapability::RealtimeVoice,
    )?;
    if configured.image_models.is_some() && !configured.image_model_ids.is_empty() {
        return Err(Error::Config(
            "configure either image_models or image_model_ids, not both".into(),
        ));
    }
    if configured.selection.model.is_empty() {
        Ok(())
    } else {
        validate_configured_provider_selection(configured, &configured.selection)
    }
}

fn validate_media_catalog(
    definition: &ProviderDefinition,
    models: Option<&[mobius::backend::model::provider::MediaModelPreset]>,
    capability: mobius::protocol::ModelCapability,
) -> Result<()> {
    let Some(models) = models else {
        return Ok(());
    };
    if models.is_empty() {
        return Ok(());
    }
    if !definition.supports(capability) {
        return Err(Error::Config(format!(
            "provider `{}` does not support {capability:?}",
            definition.id()
        )));
    }
    validate_catalog_entries(
        models.iter().map(|model| model.id.as_str()),
        "media models",
        "media model",
    )?;
    if models
        .iter()
        .map(|model| model.variants.len().max(1))
        .sum::<usize>()
        > MAX_CUSTOM_MODEL_ROUTES
    {
        return Err(Error::Config(format!(
            "a configured media catalog may generate at most {MAX_CUSTOM_MODEL_ROUTES} routes"
        )));
    }
    let mut routes = BTreeSet::new();
    if capability == mobius::protocol::ModelCapability::ImageGeneration {
        validate_image_ids(models.iter().map(|model| model.id.as_str()))?;
    }
    for model in models {
        for variant in model
            .variants
            .iter()
            .map(|variant| Some(variant.id.as_str()))
            .chain(model.variants.is_empty().then_some(None))
        {
            if !routes.insert(model_route_id(definition.id(), &model.id, variant)) {
                return Err(Error::Config(
                    "media catalog generates an ambiguous route".into(),
                ));
            }
        }
        validate_display(&model.label, &model.description)?;
        if model.variants.is_empty()
            && capability == mobius::protocol::ModelCapability::RealtimeVoice
        {
            return Err(Error::Config(
                "a live model requires at least one voice".into(),
            ));
        }
        if !model.variants.is_empty() {
            validate_catalog_entries(
                model.variants.iter().map(|variant| variant.id.as_str()),
                "media variants",
                "media variant",
            )?;
        }
        for variant in &model.variants {
            if capability == mobius::protocol::ModelCapability::ImageGeneration
                && variant.id.len() > 64
            {
                return Err(Error::Config(
                    "image quality IDs must be at most 64 bytes".into(),
                ));
            }
            if capability == mobius::protocol::ModelCapability::RealtimeVoice
                && variant.id.len() > 256
            {
                return Err(Error::Config("voice IDs must be at most 256 bytes".into()));
            }
            validate_display(&variant.label, &variant.description)?;
        }
    }
    Ok(())
}

fn validate_image_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Result<()> {
    if ids.into_iter().any(|id| id.len() > 256) {
        return Err(Error::Config(
            "image model IDs must be at most 256 bytes".into(),
        ));
    }
    Ok(())
}

fn validate_display(label: &str, description: &str) -> Result<()> {
    if label.trim().is_empty() || label.len() > 1024 || description.len() > 16 * 1024 {
        return Err(Error::Config(
            "catalog labels must be 1–1024 bytes and descriptions at most 16 KiB".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_configured_provider_selection(
    configured: &ConfiguredProvider,
    selection: &ProviderConfig,
) -> Result<()> {
    if selection.instance != configured.selection.instance
        || selection.provider != configured.selection.provider
    {
        return Err(Error::Config(
            "provider selection does not match its configured provider entry".into(),
        ));
    }
    let definition = provider(&selection.provider)?;
    if matches!(
        definition.auth(),
        mobius::backend::model::provider::ProviderAuth::Browser(_)
    ) && !mobius::backend::model::provider::uses_default_endpoint(
        Some(
            crate::harness::mobius::provider_catalog::selected_base_url(
                definition,
                &configured.selection,
            )
            .as_str(),
        ),
        Some(
            crate::harness::mobius::provider_catalog::selected_base_url(definition, selection)
                .as_str(),
        ),
    ) {
        return Err(Error::Config(
            "browser-auth provider selection must use its operator-registered endpoint".into(),
        ));
    }
    let models = crate::harness::mobius::provider_catalog::models(definition, configured);
    if !models.iter().any(|model| model.id == selection.model) {
        return Err(Error::Config(format!(
            "provider `{}` selection model is not in its configured model catalog",
            selection.provider
        )));
    }
    let effort = effective_reasoning_effort(definition, configured, selection);
    if !effort.is_none_or(|effort| {
        models
            .iter()
            .find(|model| model.id == selection.model)
            .is_some_and(|model| model.reasoning.iter().any(|item| item.id == effort))
    }) {
        return Err(Error::Config(format!(
            "provider `{}` selection reasoning effort is not in its configured reasoning catalog",
            selection.provider
        )));
    }
    Ok(())
}

pub(super) fn validate_custom_model_route_count(
    configured_providers: &BTreeMap<String, ConfiguredProvider>,
) -> Result<()> {
    let mut routes = BTreeSet::new();
    for configured in configured_providers.values() {
        let definition = provider(&configured.selection.provider)?;
        if definition.locked_models() {
            continue;
        }
        for model in &configured.models {
            let efforts = model
                .reasoning
                .iter()
                .map(|effort| Some(effort.id.as_str()))
                .chain(model.reasoning.is_empty().then_some(None));
            for effort in efforts {
                if !routes.insert(model_route_id(
                    &configured.selection.instance,
                    &model.id,
                    effort,
                )) {
                    return Err(Error::Config(
                        "custom model and reasoning catalogs generate an ambiguous route".into(),
                    ));
                }
                if routes.len() > MAX_CUSTOM_MODEL_ROUTES {
                    return Err(Error::Config(format!(
                        "custom provider catalogs may generate at most {MAX_CUSTOM_MODEL_ROUTES} model routes"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_catalog_entries<'a>(
    entries: impl ExactSizeIterator<Item = &'a str>,
    plural_name: &str,
    singular_name: &str,
) -> Result<()> {
    if entries.len() == 0 || entries.len() > MAX_PROVIDER_CATALOG_ENTRIES {
        return Err(Error::Config(format!(
            "{plural_name} must contain 1–{MAX_PROVIDER_CATALOG_ENTRIES} entries"
        )));
    }
    let mut seen = BTreeSet::new();
    let mut bytes = 0_usize;
    for entry in entries {
        if entry.is_empty()
            || entry.len() > MAX_PROVIDER_CATALOG_ENTRY_BYTES
            || entry != entry.trim()
        {
            return Err(Error::Config(format!(
                "each {singular_name} must be canonical and 1–{MAX_PROVIDER_CATALOG_ENTRY_BYTES} bytes"
            )));
        }
        if entry.chars().any(char::is_control) {
            return Err(Error::Config(format!(
                "each {singular_name} must not contain control characters"
            )));
        }
        if !seen.insert(entry) {
            return Err(Error::Config(format!(
                "duplicate {singular_name} `{entry}`"
            )));
        }
        bytes = bytes
            .checked_add(entry.len())
            .ok_or_else(|| Error::Config(format!("{singular_name} catalog is too large")))?;
    }
    if bytes > MAX_PROVIDER_CATALOG_BYTES {
        return Err(Error::Config(format!(
            "{plural_name} are limited to {MAX_PROVIDER_CATALOG_BYTES} bytes in total"
        )));
    }
    Ok(())
}

pub(crate) fn model_route_id(instance: &str, model: &str, effort: Option<&str>) -> String {
    format!("{instance}::{model}::{}", effort.unwrap_or("default"))
}

/// An instance ID names one durable provider setup and appears in model route IDs.
pub(super) fn validate_instance_id(instance: &str) -> Result<()> {
    if instance.is_empty() || instance.len() > 256 {
        return Err(Error::Config(
            "provider instance ID must be 1–256 bytes".into(),
        ));
    }
    if !instance
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(Error::Config(
            "provider instance ID accepts only letters, digits, `_`, `-`, and `.`".into(),
        ));
    }
    Ok(())
}

/// A label is the user-facing name of one provider instance.
pub(super) fn validate_provider_label(label: &str) -> Result<()> {
    if label.trim().is_empty() || label.len() > MAX_PROVIDER_LABEL_BYTES {
        return Err(Error::Config(format!(
            "provider label must be 1–{MAX_PROVIDER_LABEL_BYTES} bytes"
        )));
    }
    if label.chars().any(char::is_control) {
        return Err(Error::Config(
            "provider label must not contain control characters".into(),
        ));
    }
    Ok(())
}

pub(crate) fn effective_reasoning_effort<'a>(
    definition: &ProviderDefinition,
    configured: &'a ConfiguredProvider,
    selection: &'a ProviderConfig,
) -> Option<&'a str> {
    selection.reasoning_effort.as_deref().or_else(|| {
        let model = crate::harness::mobius::provider_catalog::models(definition, configured)
            .iter()
            .find(|model| model.id == selection.model);
        model.and_then(|model| model.default_reasoning.as_deref())
    })
}

pub(super) fn invalid_cloudflare_hostname() -> Error {
    Error::Config(
        "Cloudflare hostname must be a DNS name such as mobius.example.com, without a scheme, path, or port"
            .into(),
    )
}

pub(super) fn validate_cloudflare_token(token: &str) -> Result<&str> {
    let token = token.trim();
    if token.is_empty()
        || token.len() > MAX_CLOUDFLARE_TOKEN_BYTES
        || !token.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(invalid_cloudflare_token());
    }
    Ok(token)
}

pub(super) fn invalid_cloudflare_token() -> Error {
    Error::Config(format!(
        "Cloudflare tunnel token must be 1–{MAX_CLOUDFLARE_TOKEN_BYTES} visible ASCII bytes"
    ))
}
