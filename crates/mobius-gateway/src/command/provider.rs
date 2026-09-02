use std::io::{IsTerminal as _, Read};

use mobius::backend::model::provider::provider;

use super::*;
use crate::config::{ConfiguredProvider, MAX_API_KEY_BYTES};
use crate::wire::{ProviderConfig, ProviderEndpointAuth, ProviderTint};

pub(super) async fn register_provider_command(
    options: RegisterProviderOptions,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
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
    register_provider_with_credential(options, credential, load_local_client).await
}

pub(super) async fn register_provider_with_credential(
    options: RegisterProviderOptions,
    credential: Option<String>,
    load_local_client: fn(&Endpoint) -> Result<Option<String>>,
) -> Result<()> {
    let (_, config) = ConfigStore::open(options.state_dir)?;
    let endpoint = direct_loopback_endpoint(&config)?;
    let token = load_local_client(&endpoint)?
        .ok_or_else(|| Error::Config("gateway local control credential is unavailable".into()))?;
    let definition = provider(&options.provider)?;
    let base_url = options
        .base_url
        .or_else(|| definition.default_base_url().map(str::to_owned));
    let instance = options.instance.unwrap_or_else(|| options.provider.clone());
    let existing = config.configured_providers.get(&instance).cloned();
    if let Some(api_key) = credential {
        request_provider_credential(
            &endpoint,
            &token,
            &instance,
            &options.provider,
            base_url.as_deref(),
            api_key,
        )
        .await?;
    }
    let label = options
        .label
        .or_else(|| existing.as_ref().map(|configured| configured.label.clone()))
        .unwrap_or_else(|| definition.label().to_owned());
    let tint = existing
        .as_ref()
        .map_or_else(ProviderTint::default, |configured| configured.tint);
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
        web_search: options.web_search,
    };
    let model_ids = if definition.models().is_empty() {
        vec![selection.model.clone()]
    } else {
        Vec::new()
    };
    let registration = ConfiguredProvider {
        selection: selection.clone(),
        label,
        tint,
        model_ids,
        reasoning_efforts: options.reasoning_efforts,
    };
    request_provider_registration(&endpoint, &token, registration).await?;
    println!("{}", register_provider_json(&selection.provider)?);
    Ok(())
}

pub(super) fn read_provider_credential(mut input: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take((MAX_API_KEY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_API_KEY_BYTES {
        return Err(Error::Config(format!(
            "API key must be 1–{MAX_API_KEY_BYTES} bytes"
        )));
    }
    String::from_utf8(bytes)
        .map_err(|_| Error::Config("provider credential is not valid UTF-8".into()))
}

pub(super) fn register_provider_json(provider: &str) -> Result<String> {
    Ok(serde_json::to_string(&RegisterProviderOutput { provider })?)
}

async fn request_provider_credential(
    endpoint: &Endpoint,
    token: &str,
    instance: &str,
    provider: &str,
    base_url: Option<&str>,
    api_key: String,
) -> Result<()> {
    let client = GatewayClient::connect(endpoint, token, ClientKind::GatewayDashboard).await?;
    let (sender, mut events) = client.into_parts();
    let request_id = Uuid::new_v4().to_string();
    let message = match base_url {
        Some(base_url) => ClientMessage::SetProviderEndpointCredential {
            request_id: request_id.clone(),
            instance: instance.into(),
            provider: provider.into(),
            base_url: base_url.into(),
            api_key,
        },
        None => ClientMessage::SetProviderCredential {
            request_id: request_id.clone(),
            instance: instance.into(),
            provider: provider.into(),
            api_key,
        },
    };
    sender.send(message).await?;
    for _ in 0..MAX_PENDING_FRAMES {
        let frame = events.next().await?.ok_or_else(|| {
            Error::Protocol("gateway disconnected before saving the provider credential".into())
        })?;
        match frame.message {
            ServerMessage::ProviderCredentialSaved {
                request_id: actual,
                instance: actual_instance,
                provider: actual_provider,
            } if actual == request_id
                && actual_instance == instance
                && actual_provider == provider =>
            {
                return Ok(());
            }
            ServerMessage::Rejected {
                request_id: actual,
                message,
                ..
            } if actual == request_id => return Err(Error::Protocol(message)),
            ServerMessage::Error { message, .. } => return Err(Error::Protocol(message)),
            _ => {}
        }
    }
    Err(Error::Protocol(format!(
        "gateway sent {MAX_PENDING_FRAMES} unrelated frames before the provider credential response"
    )))
}

async fn request_provider_registration(
    endpoint: &Endpoint,
    token: &str,
    registration: ConfiguredProvider,
) -> Result<()> {
    let client = GatewayClient::connect(endpoint, token, ClientKind::GatewayDashboard).await?;
    let (sender, mut events) = client.into_parts();
    let request_id = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::RegisterProvider {
            request_id: request_id.clone(),
            config: registration.selection,
            label: registration.label,
            tint: registration.tint,
            model_ids: registration.model_ids,
            reasoning_efforts: registration.reasoning_efforts,
        })
        .await?;
    for _ in 0..MAX_PENDING_FRAMES {
        let frame = events.next().await?.ok_or_else(|| {
            Error::Protocol("gateway disconnected before registering the provider".into())
        })?;
        match frame.message {
            ServerMessage::GatewayConfigured {
                request_id: actual, ..
            } if actual == request_id => return Ok(()),
            ServerMessage::Rejected {
                request_id: actual,
                message,
                ..
            } if actual == request_id => return Err(Error::Protocol(message)),
            ServerMessage::Error { message, .. } => return Err(Error::Protocol(message)),
            _ => {}
        }
    }
    Err(Error::Protocol(format!(
        "gateway sent {MAX_PENDING_FRAMES} unrelated frames before the provider response"
    )))
}

#[derive(Serialize)]
struct RegisterProviderOutput<'a> {
    provider: &'a str,
}
