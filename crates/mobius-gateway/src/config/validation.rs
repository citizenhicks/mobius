use super::*;

/// Validates one Bot against its exact selected model route and capabilities.
pub(crate) fn validate_bot_compatibility(
    gateway: &GatewayConfig,
    config: &AgentComposition,
    models: &[mobius::protocol::ModelChoice],
) -> Result<()> {
    validate_agent_composition_with_ceilings(config, gateway.execution.subagent_ceilings()?)?;
    validate_desktop_bot_policy(gateway, config)?;
    gateway.validate_provider_selection(&config.provider)?;
    let selection = &config.provider;
    let configured = gateway
        .configured_providers
        .get(&selection.instance)
        .ok_or_else(|| Error::Config("active provider is not in the configured catalog".into()))?;
    let definition = provider(&selection.provider)?;
    let effort = effective_reasoning_effort(definition, configured, selection);
    let route = model_route_id(&selection.instance, &selection.model, effort);
    let catalog = crate::provider_catalog::catalog_routes(definition, configured, selection);
    let selected = catalog
        .iter()
        .find(|candidate| candidate.choice.route == route)
        .ok_or_else(|| {
            Error::Config("active model route is not in the configured catalog".into())
        })?;
    crate::middleware_manifest::validate_choices(&config.middleware, models, &selected.choice)?;
    Ok(())
}

pub(crate) fn configured_approval_policy(
    settings: &crate::wire::MiddlewareConfig,
) -> Result<mobius::backend::sandbox::ApprovalPolicy> {
    crate::middleware_manifest::string_setting(settings, "sandbox", "approval_policy")?
        .ok_or_else(|| {
            Error::Config("missing middleware setting `sandbox.approval_policy`".into())
        })?
        .parse()
        .map_err(Error::from)
}

pub(crate) fn validate_desktop_bot_policy(
    gateway: &GatewayConfig,
    config: &AgentComposition,
) -> Result<()> {
    use mobius::backend::sandbox::ApprovalPolicy;

    if gateway.desktop_enabled
        && matches!(
            configured_approval_policy(&config.middleware)?,
            ApprovalPolicy::Ask | ApprovalPolicy::AllowNetwork
        )
    {
        return Err(Error::Config(
            "desktop gateways require Full access or no network; restricted execution with network access can reach the shared browser".into(),
        ));
    }
    Ok(())
}

/// Validates the structural composition without imposing a host's resource policy.
/// # Errors
///
/// Returns an error if the supplied value is invalid.
pub fn validate_agent_composition(config: &AgentComposition) -> Result<()> {
    let maximum = usize::try_from(i64::MAX).unwrap_or(usize::MAX);
    let ceilings = mobius::middleware::subagents::SubagentCeilings::new(u8::MAX, maximum, maximum)?;
    validate_agent_composition_with_ceilings(config, ceilings)
}

pub(crate) fn validate_agent_composition_with_ceilings(
    config: &AgentComposition,
    ceilings: mobius::middleware::subagents::SubagentCeilings,
) -> Result<()> {
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
    crate::middleware_manifest::validate_with_ceilings(&config.middleware, ceilings)
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
    if matches!(
        definition.auth(),
        mobius::backend::model::provider::ProviderAuth::Browser(_)
    ) && !mobius::backend::model::provider::uses_default_endpoint(
        crate::provider_catalog::selected_base_url(definition, &configured.selection),
        crate::provider_catalog::selected_base_url(definition, selection),
    ) {
        return Err(Error::Config(
            "browser-auth provider selection must use its operator-registered endpoint".into(),
        ));
    }
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
    selection
        .reasoning_effort
        .as_deref()
        .or_else(|| {
            definition
                .model(&selection.model)
                .and_then(|model| model.default_reasoning.as_deref())
        })
        .or_else(|| configured.reasoning_efforts.first().map(String::as_str))
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

pub(super) fn validate_telemetry(config: &crate::telemetry::TelemetryConfig) -> Result<()> {
    use crate::telemetry::SinkMethod;
    config.policy.validate()?;
    if config.sinks.len() > 16 {
        return Err(Error::Config(
            "telemetry.sinks accepts at most 16 destinations".into(),
        ));
    }
    let mut ids = std::collections::BTreeSet::new();
    if config
        .sinks
        .iter()
        .filter(|sink| sink.enabled && sink.upload_admission)
        .count()
        > 1
    {
        return Err(Error::Config(
            "telemetry accepts one enabled upload admission collector".into(),
        ));
    }
    for (index, sink) in config.sinks.iter().enumerate() {
        let invalid = |field: &str, requirement: &str| {
            Error::Config(format!("telemetry.sinks[{index}].{field} {requirement}"))
        };
        if sink.id.is_empty()
            || sink.id.len() > 64
            || !sink
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(invalid(
                "id",
                "must contain 1–64 lowercase letters, digits, underscores or hyphens",
            ));
        }
        if !ids.insert(&sink.id) {
            return Err(invalid("id", "must be unique"));
        }
        let url = url::Url::parse(&sink.url)
            .map_err(|_| invalid("url", "must be an absolute HTTP(S) URL"))?;
        let loopback = match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain("localhost")) => true,
            _ => false,
        };
        if url.host().is_none()
            || !(url.scheme() == "https"
                || url.scheme() == "http" && (loopback || config.policy.allow_insecure_http))
        {
            return Err(invalid(
                "url",
                "requires HTTPS or an explicit operator HTTP policy",
            ));
        }
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return Err(invalid("url", "must not contain credentials or a fragment"));
        }
        if !(15..=86_400).contains(&sink.every_seconds) || !sink.every_seconds.is_multiple_of(15) {
            return Err(invalid(
                "every_seconds",
                "must be a multiple of 15 between 15 and 86400",
            ));
        }
        if sink.method == SinkMethod::Get && !sink.events.is_empty() {
            return Err(invalid("events", "requires the POST method"));
        }
        if sink.method == SinkMethod::Get && sink.upload_admission {
            return Err(invalid("upload_admission", "requires the POST method"));
        }
        if sink.headers.len() > 16 {
            return Err(invalid("headers", "accepts at most 16 headers"));
        }
        if sink.fields.len() > 16 {
            return Err(invalid("fields", "accepts at most 16 labels"));
        }
        if sink.bearer_env.is_some() && sink.bearer_file.is_some() {
            return Err(invalid("bearer_env", "cannot be combined with bearer_file"));
        }
        for (name, value) in &sink.headers {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            {
                return Err(invalid("headers", "contains an invalid HTTP header name"));
            }
            if [
                "authorization",
                "proxy-authorization",
                "cookie",
                "host",
                "content-length",
                "transfer-encoding",
            ]
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
            {
                return Err(invalid(
                    "headers",
                    "contains a reserved credential or framing header",
                ));
            }
            if value.len() > 1024 || !value.bytes().all(|b| (32..=126).contains(&b)) {
                return Err(invalid(
                    "headers",
                    "values must be at most 1024 printable ASCII bytes",
                ));
            }
        }
        if sink
            .fields
            .iter()
            .any(|(key, value)| key.is_empty() || key.len() > 64 || value.len() > 1024)
        {
            return Err(invalid(
                "fields",
                "keys must be 1–64 bytes and values at most 1024 bytes",
            ));
        }
        if serde_json::to_vec(&sink.fields)?.len() > 8192 {
            return Err(invalid("fields", "encoded labels must fit within 8 KiB"));
        }
        if let Some(name) = &sink.bearer_env
            && (!name.starts_with("MOBIUS_")
                || name.len() == "MOBIUS_".len()
                || !mobius::identifier::valid_ascii_identifier(
                    name,
                    128,
                    mobius::identifier::AsciiCase::Any,
                    b"_",
                ))
        {
            return Err(invalid(
                "bearer_env",
                "must start with MOBIUS_ and contain at most 128 ASCII letters, digits or underscores",
            ));
        }
        if let Some(name) = &sink.bearer_env
            && crate::sandbox::reserved_credential_environment().any(|reserved| reserved == name)
        {
            return Err(invalid(
                "bearer_env",
                "cannot select a gateway or provider credential",
            ));
        }
        if let Some(path) = &sink.bearer_file
            && (path.is_empty()
                || !std::path::Path::new(path)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))))
        {
            return Err(invalid(
                "bearer_file",
                "must be a relative path without parent or current directory components",
            ));
        }
    }
    Ok(())
}
