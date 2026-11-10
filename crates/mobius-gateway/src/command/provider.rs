use std::io::{IsTerminal as _, Read};

use mobius::backend::model::provider::{BaseUrl, provider};

use super::{
    ClientCapabilities, ClientMessage, ConfigStore, Endpoint, Error, GatewayClient, PathBuf,
    RegisterProviderArgs, Result, Serialize, ServerMessage, Uuid, direct_loopback_endpoint,
    print_line,
};
use crate::config::{MAX_PROVIDER_API_KEY_BYTES, ProviderRegistration};
use crate::wire::{ConfiguredModel, ProviderConfig, ProviderEndpointAuth};

pub(super) async fn register_provider_command(
    state_dir: PathBuf,
    options: RegisterProviderArgs,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    let models: Option<Vec<ConfiguredModel>> = options
        .models_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?;
    let credential = if options.credential_stdin {
        if std::io::stdin().is_terminal() {
            return Err(Error::Config(
                "--credential-stdin requires piped standard input".into(),
            ));
        }
        Some(read_provider_credential(std::io::stdin().lock())?)
    } else {
        None
    };
    let endpoint = {
        let (_, config) = ConfigStore::open(state_dir)?;
        direct_loopback_endpoint(&config)?
    };
    let token = load_local_client(&endpoint)?
        .ok_or_else(|| Error::Config("gateway local control credential is unavailable".into()))?;
    let definition = provider(&options.provider)?;
    let base_url = Some(definition.resolve_base_url(options.base_url.as_ref()));
    let instance = options.instance.unwrap_or_else(|| definition.id().into());
    if let Some(api_key) = credential {
        request_provider_credential(
            &endpoint,
            &token,
            &instance,
            &options.provider,
            base_url.as_ref(),
            api_key,
            options.credential_expires_at,
        )
        .await?;
    }
    let selection = ProviderConfig {
        instance,
        provider: options.provider,
        model: options.model,
        base_url,
        endpoint_auth: if options.credentialless {
            ProviderEndpointAuth::Credentialless
        } else {
            ProviderEndpointAuth::ProviderDefault
        },
        reasoning_effort: None,
        service_tier: options.service_tier,
        web_search: options.web_search,
        tool_discovery: options.tool_discovery.map(|mode| match mode.as_str() {
            "native" => mobius::protocol::ToolDiscoveryMode::Native,
            _ => mobius::protocol::ToolDiscoveryMode::Rebuild,
        }),
    };
    let models = models
        .or_else(|| {
            options.model_ids.map(|ids| {
                ids.into_iter()
                    .map(|id| ConfiguredModel {
                        id,
                        ..Default::default()
                    })
                    .collect()
            })
        })
        .unwrap_or_default();
    let registration = ProviderRegistration {
        selection,
        label: options.label,
        tint: None,
        models,
        image_model_ids: options.image_model_ids,
        operator_managed: options.operator_managed,
    };
    request_provider_registration(
        &endpoint,
        &token,
        registration,
        options.preserve_selection,
        options.if_configured,
    )
    .await?;
    Ok(print_line(format_args!(
        "{}",
        register_provider_json(definition.id())?
    ))?)
}

pub(super) fn read_provider_credential(mut input: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take((MAX_PROVIDER_API_KEY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PROVIDER_API_KEY_BYTES {
        return Err(Error::Config(format!(
            "API key must be 1–{MAX_PROVIDER_API_KEY_BYTES} bytes"
        )));
    }
    String::from_utf8(bytes)
        .map_err(|error| Error::Config(format!("provider credential is not valid UTF-8: {error}")))
}

pub(super) fn register_provider_json(provider: &str) -> Result<String> {
    Ok(serde_json::to_string(&RegisterProviderOutput { provider })?)
}

async fn provider_command_connection(
    endpoint: &Endpoint,
    token: &str,
) -> Result<(crate::client::GatewaySender, crate::client::GatewayEvents)> {
    let client = GatewayClient::connect(endpoint, token, ClientCapabilities::default()).await?;
    let (sender, events) = client.into_parts();
    sender
        .send(ClientMessage::SetNotifications {
            request_id: Uuid::new_v4().to_string().into(),
            disabled: [
                crate::wire::GatewayNotification::Sessions,
                crate::wire::GatewayNotification::Bots,
            ]
            .into(),
        })
        .await?;
    Ok((sender, events))
}

async fn request_provider_credential(
    endpoint: &Endpoint,
    token: &str,
    instance: &str,
    provider: &str,
    base_url: Option<&BaseUrl>,
    api_key: String,
    expires_at: Option<u64>,
) -> Result<()> {
    let (sender, mut events) = provider_command_connection(endpoint, token).await?;
    crate::client::request(
        &sender,
        &mut events,
        "provider credential",
        |request_id| ClientMessage::SetProviderCredential {
            request_id,
            instance: instance.into(),
            provider: provider.into(),
            base_url: base_url.map(BaseUrl::share),
            api_key,
            expires_at,
        },
        |request_id, message| match message {
            ServerMessage::ProviderCredentialSaved {
                request_id: actual,
                instance: actual_instance,
                provider: actual_provider,
            } if *actual == *request_id
                && actual_instance == instance
                && actual_provider == provider =>
            {
                Some(())
            }
            _ => None,
        },
    )
    .await
}

async fn request_provider_registration(
    endpoint: &Endpoint,
    token: &str,
    registration: ProviderRegistration,
    preserve_selection: bool,
    if_configured: bool,
) -> Result<()> {
    let (sender, mut events) = provider_command_connection(endpoint, token).await?;
    crate::client::request(
        &sender,
        &mut events,
        "provider",
        |request_id| ClientMessage::RegisterProvider {
            request_id,
            config: registration.selection,
            preserve_selection,
            if_configured,
            label: registration.label,
            tint: registration.tint,
            models: registration.models,
            image_model_ids: registration.image_model_ids,
            operator_managed: registration.operator_managed,
        },
        |request_id, message| {
            matches!(message, ServerMessage::GatewayConfigured { request_id: actual, .. } if *actual == *request_id)
                .then_some(())
        },
    )
    .await
}

#[derive(Serialize)]
struct RegisterProviderOutput<'a> {
    provider: &'a str,
}

pub(super) async fn clear_provider_credential(
    state_dir: std::path::PathBuf,
    instance: String,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    let (_, config) = ConfigStore::open(state_dir)?;
    let endpoint = direct_loopback_endpoint(&config)?;
    let token = load_local_client(&endpoint)?
        .ok_or_else(|| Error::Config("gateway local control credential is unavailable".into()))?;
    let (sender, mut events) = provider_command_connection(&endpoint, &token).await?;
    crate::client::request(
        &sender,
        &mut events,
        "provider credential",
        |request_id| ClientMessage::ClearProviderCredential {
            request_id,
            instance: instance.clone(), // rules-allow: the message owns the instance the response match and output still use
        },
        |request_id, message| {
            matches!(message, ServerMessage::ProviderCredentialCleared { request_id: actual, instance: actual_instance }
                if *actual == *request_id && actual_instance == instance)
            .then_some(())
        },
    )
    .await?;
    Ok(print_line(format_args!(
        "{}",
        serde_json::json!({"instance": instance})
    ))?)
}
