use super::*;

/// Validates the complete frontend-writable agent composition.
pub fn validate_agent_composition(config: &AgentComposition) -> Result<()> {
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
    crate::extensions::validate_ids(&config.extensions)?;
    validate_provider_config(&config.provider)?;
    if let Some(voice) = config.realtime_voice.as_deref()
        && !provider(&config.provider.provider)?
            .realtime_voices(config.provider.base_url.as_deref())
            .contains(&voice)
    {
        return Err(Error::Config(
            "the selected voice is not supported by this provider".into(),
        ));
    }
    crate::middleware_manifest::validate(&config.middleware)
}

pub(super) fn validate_provider_config(config: &ProviderConfig) -> Result<()> {
    validate_instance_id(&config.instance)?;
    if config.provider.trim().is_empty() || config.provider.len() > 256 {
        return Err(Error::Config("provider ID must be 1–256 bytes".into()));
    }
    if config.model.trim().is_empty() || config.model.len() > 1024 {
        return Err(Error::Config("model must be 1–1024 bytes".into()));
    }
    let definition = provider(&config.provider)?;
    definition.build_config_is_valid(
        &config.model,
        config.base_url.as_deref(),
        config.reasoning_effort.as_deref(),
        config.web_search,
    )?;
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
        .validate_credentialless_endpoint(config.base_url.as_deref())
        .map_err(Error::from)
}

pub(super) fn validate_configured_provider(configured: &ConfiguredProvider) -> Result<()> {
    validate_provider_config(&configured.selection)?;
    validate_provider_label(&configured.label)?;
    let definition = provider(&configured.selection.provider)?;
    if definition.models().is_empty() {
        validate_model_ids(&configured.model_ids)?;
        validate_reasoning_efforts(&configured.reasoning_efforts)?;
    } else if !configured.model_ids.is_empty() || !configured.reasoning_efforts.is_empty() {
        return Err(Error::Config(format!(
            "provider `{}` uses its advertised model and reasoning catalogs",
            configured.selection.provider
        )));
    }
    validate_configured_provider_selection(configured, &configured.selection)
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
    if !definition.models().is_empty() {
        return Ok(());
    }
    if !configured.model_ids.contains(&selection.model) {
        return Err(Error::Config(format!(
            "provider `{}` selection model is not in its configured model catalog",
            selection.provider
        )));
    }
    let effort = effective_reasoning_effort(definition, configured, selection);
    if !effort.is_none_or(|effort| {
        configured
            .reasoning_efforts
            .iter()
            .any(|item| item == effort)
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
        if !provider(&configured.selection.provider)?
            .models()
            .is_empty()
        {
            continue;
        }
        for model in &configured.model_ids {
            if configured.reasoning_efforts.is_empty() {
                routes.insert(model_route_id(&configured.selection.instance, model, None));
                continue;
            }
            for effort in &configured.reasoning_efforts {
                if !routes.insert(model_route_id(
                    &configured.selection.instance,
                    model,
                    Some(effort),
                )) {
                    return Err(Error::Config(
                        "custom model and reasoning catalogs generate an ambiguous route".into(),
                    ));
                }
            }
        }
    }
    if routes.len() > MAX_CUSTOM_MODEL_ROUTES {
        return Err(Error::Config(format!(
            "custom provider catalogs may generate at most {MAX_CUSTOM_MODEL_ROUTES} model routes"
        )));
    }
    Ok(())
}

fn validate_model_ids(model_ids: &[String]) -> Result<()> {
    validate_catalog_entries(model_ids, "model IDs", "model ID")
}

fn validate_reasoning_efforts(reasoning_efforts: &[String]) -> Result<()> {
    if reasoning_efforts.is_empty() {
        return Ok(());
    }
    validate_catalog_entries(reasoning_efforts, "reasoning efforts", "reasoning effort")
}

fn validate_catalog_entries(
    entries: &[String],
    plural_name: &str,
    singular_name: &str,
) -> Result<()> {
    if entries.is_empty() || entries.len() > MAX_PROVIDER_CATALOG_ENTRIES {
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
        if !seen.insert(entry.as_str()) {
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
    if label.trim().is_empty() || label.len() > 128 {
        return Err(Error::Config("provider label must be 1–128 bytes".into()));
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
    selection
        .reasoning_effort
        .as_deref()
        .or_else(|| {
            definition
                .model(&selection.model)
                .and_then(|model| model.default_reasoning)
        })
        .or_else(|| configured.reasoning_efforts.first().map(String::as_str))
}

pub(super) fn valid_hostname_label(label: &str) -> bool {
    (1..=63).contains(&label.len())
        && label
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && label
            .bytes()
            .next_back()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
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

pub(super) fn invalid_cloudflare_token_file() -> Error {
    Error::Config("Cloudflare tunnel token must be stored in a regular file".into())
}
