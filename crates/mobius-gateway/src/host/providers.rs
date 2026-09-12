use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures_util::future::BoxFuture;
use mobius::backend::model::provider::{DeviceLogin, ProviderAuth, provider};
use uuid::Uuid;

use crate::Error;
use crate::config::GatewayConfig;
use crate::provider_catalog::{
    configured_model_choices, credential_is_configured, selected_base_url,
};
use crate::wire::{
    AgentComposition, BotRecord, ProviderConfig, ProviderEndpointAuth, ProviderTint, ReadyPayload,
    ServerFrame, ServerMessage,
};

use super::session::ProviderRefresh;
use super::{GatewayHost, Rejection, gateway_ready, internal, invalid_config};

const MAX_LOGIN_REQUEST_ID_BYTES: usize = 128;

#[derive(Default)]
pub(super) struct ProviderLogins {
    active_id: Option<String>,
    // One latest attempt per paired client survives another client's subsequent login.
    // Unpairing removes its entry; credentials remain in their existing protected store.
    attempts: HashMap<String, ProviderLoginAttempt>,
}

struct ProviderLoginAttempt {
    login_id: String,
    request_id: String,
    provider: String,
    response: Option<ServerMessage>,
}

impl GatewayHost {
    pub(crate) async fn forget_provider_login(
        &self,
        client_id: &str,
    ) -> std::result::Result<(), Rejection> {
        self.state
            .lock()
            .await
            .provider_login
            .lock()
            .map_err(|_| internal("provider login lock is poisoned"))?
            .attempts
            .remove(client_id);
        Ok(())
    }

    pub(crate) async fn configure_bot_defaults(
        &self,
        expected_revision: u64,
        config: AgentComposition,
    ) -> std::result::Result<ReadyPayload, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        {
            let mut current = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?;
            let models = configured_model_choices(&current, &state.store, &state.credentials)
                .map_err(internal)?;
            crate::middleware_manifest::validate_choices(&config.middleware, &models)
                .map_err(invalid_config)?;
            crate::extensions::ExtensionStore::new(&state.store)
                .resolve(&current, &config.extensions)
                .map_err(invalid_config)?;
            let next = current
                .replacing_bot_defaults(expected_revision, config)
                .map_err(invalid_config)?;
            state.store.save(&next).map_err(internal)?;
            *current = next;
        }
        let payload = gateway_ready(&state).await?;
        let _ = self.events.send(ServerFrame::new(ServerMessage::Ready {
            payload: payload.clone(),
        }));
        Ok(payload)
    }

    pub(crate) async fn clear_credential(
        &self,
        instance: String,
    ) -> std::result::Result<(), Rejection> {
        let base_url = {
            let state = self.state.lock().await;
            state
                .credentials
                .remove(&instance)
                .map_err(invalid_config)?;
            let config = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?;
            config
                .configured_providers
                .get(&instance)
                .map(|configured| {
                    provider(&configured.selection.provider).map(|definition| {
                        selected_base_url(definition, &configured.selection).map(str::to_owned)
                    })
                })
                .transpose()
                .map_err(invalid_config)?
                .flatten()
        };
        self.refresh_provider_sessions(ProviderRefresh::Instance { instance, base_url })
            .await
    }

    pub(crate) async fn set_credential(
        &self,
        instance: String,
        provider_id: String,
        api_key: String,
        base_url: Option<String>,
        expires_at: Option<u64>,
    ) -> std::result::Result<(), Rejection> {
        let (base_url, configured) = {
            let state = self.state.lock().await;
            let definition = provider(&provider_id).map_err(invalid_config)?;
            let base_url = if definition.configurable_base_url() {
                base_url.or_else(|| definition.default_base_url().map(str::to_owned))
            } else {
                base_url
            };
            definition
                .validate_base_url(base_url.as_deref())
                .map_err(invalid_config)?;
            state
                .credentials
                .set(
                    &instance,
                    &provider_id,
                    &api_key,
                    base_url.as_deref(),
                    expires_at,
                )
                .map_err(invalid_config)?;
            let configured = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?
                .configured_providers
                .get(&instance)
                .filter(|configured| {
                    let configured_base_url = selected_base_url(definition, &configured.selection);
                    configured.selection.provider == provider_id.as_str()
                        && configured.selection.endpoint_auth
                            == ProviderEndpointAuth::Credentialless
                        && configured_base_url == base_url.as_deref()
                })
                .cloned();
            (base_url, configured)
        };
        if let Some(configured) = configured {
            let mut selection = configured.selection;
            selection.endpoint_auth = ProviderEndpointAuth::ProviderDefault;
            self.register_provider(
                selection,
                configured.label,
                configured.tint,
                configured.model_ids,
                configured.reasoning_efforts,
            )
            .await?;
            return Ok(());
        }
        self.refresh_provider_sessions(ProviderRefresh::Instance { instance, base_url })
            .await
    }

    pub(crate) async fn start_provider_login(
        &self,
        request_id: String,
        provider_id: String,
        client_id: &str,
    ) -> std::result::Result<Option<ServerMessage>, Rejection> {
        let definition = provider(&provider_id).map_err(invalid_config)?;
        let ProviderAuth::Browser(auth) = definition.auth() else {
            return Err(Rejection {
                code: "invalid_provider_auth",
                message: "the selected provider uses an API key".into(),
                fatal: false,
            });
        };
        if !auth.supports_device_login() {
            return Err(Rejection {
                code: "device_login_unavailable",
                message: "the selected provider does not support device-code login".into(),
                fatal: false,
            });
        }
        let login_guard = Arc::clone(&self.state.lock().await.provider_login);
        let login_id = Uuid::new_v4().to_string();
        {
            let mut logins = login_guard
                .lock()
                .map_err(|_| internal("provider login lock is poisoned"))?;
            if !reserve_provider_login(
                &mut logins,
                &login_id,
                client_id,
                &request_id,
                &provider_id,
            )? {
                // Reply only to the retrying connection. Its dispatch writes this
                // response before processing any subsequently queued completion.
                return Ok(logins
                    .attempts
                    .get(client_id)
                    .and_then(|attempt| attempt.response.clone()));
            }
        }
        self.spawn_provider_login(request_id, login_id, provider_id, auth.start_device());
        Ok(None)
    }

    fn spawn_provider_login(
        &self,
        request_id: String,
        login_id: String,
        provider: String,
        start: BoxFuture<'static, mobius::Result<Box<dyn DeviceLogin>>>,
    ) {
        let gateway = self.clone();
        // Code acquisition and polling both belong to the gateway. No connection
        // future can be cancelled after reserving the slot but before launching it.
        tokio::spawn(async move {
            let result = async {
                let login = start.await.map_err(|error| error.to_string())?;
                gateway
                    .publish_provider_login(
                        &login_id,
                        ServerMessage::ProviderLoginStarted {
                            request_id: request_id.clone(),
                            login_id: login_id.clone(),
                            provider: provider.clone(),
                            verification_url: login.verification_url().into(),
                            user_code: login.user_code().into(),
                        },
                    )
                    .await
                    .map_err(|rejection| rejection.message)?;
                let path = gateway.state.lock().await.store.provider_auth_path();
                login
                    .complete(path)
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            gateway
                .finish_provider_login(request_id, login_id, provider, result)
                .await;
        });
    }

    async fn finish_provider_login(
        &self,
        request_id: String,
        login_id: String,
        provider: String,
        result: std::result::Result<(), String>,
    ) {
        let refresh = result.is_ok();
        let message = match result {
            Ok(()) => ServerMessage::ProviderLoginFinished {
                request_id,
                login_id: login_id.clone(),
                provider: provider.clone(),
            },
            Err(message) => ServerMessage::Rejected {
                request_id,
                code: "provider_login_failed".into(),
                message,
                fatal: false,
            },
        };
        match self.publish_provider_login(&login_id, message).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(rejection) => {
                self.broadcast(ServerMessage::Error {
                    code: rejection.code.into(),
                    message: rejection.message,
                    fatal: rejection.fatal,
                });
                return;
            }
        }
        if refresh
            && let Err(rejection) = self
                .refresh_provider_sessions(ProviderRefresh::Provider(provider))
                .await
        {
            self.broadcast(ServerMessage::Error {
                code: rejection.code.into(),
                message: rejection.message,
                fatal: rejection.fatal,
            });
        }
    }

    async fn publish_provider_login(
        &self,
        login_id: &str,
        message: ServerMessage,
    ) -> std::result::Result<bool, Rejection> {
        let state = self.state.lock().await;
        let mut logins = state
            .provider_login
            .lock()
            .map_err(|_| internal("provider login lock is poisoned"))?;
        if logins.active_id.as_deref() != Some(login_id) {
            return Ok(false);
        }
        if !matches!(message, ServerMessage::ProviderLoginStarted { .. }) {
            logins.active_id = None;
        }
        if let Some(attempt) = logins
            .attempts
            .values_mut()
            .find(|attempt| attempt.login_id == login_id)
        {
            attempt.response = Some(message.clone());
        }
        self.broadcast(message);
        Ok(true)
    }

    async fn refresh_provider_sessions(
        &self,
        scope: ProviderRefresh,
    ) -> std::result::Result<(), Rejection> {
        let state = self.state.lock().await;
        let cache = state.bots.prepared.lock().await;
        state
            .bots
            .preparation_generation
            .fetch_add(1, Ordering::Release);
        for prepared in cache.values() {
            for selection in &prepared.providers {
                if super::session::provider_refresh_matches(selection, &scope)
                    .map_err(invalid_config)?
                {
                    prepared.invalidate();
                    break;
                }
            }
        }
        Ok(())
    }

    fn broadcast(&self, message: ServerMessage) {
        let _ = self.events.send(ServerFrame::new(message));
    }

    pub(crate) async fn register_provider(
        &self,
        selection: ProviderConfig,
        label: String,
        tint: ProviderTint,
        model_ids: Vec<String>,
        reasoning_efforts: Vec<String>,
    ) -> std::result::Result<ReadyPayload, Rejection> {
        let _mutation = self.begin_exclusive_mutation().await?;
        let state = self.state.lock().await;
        if !credential_is_configured(&selection, &state.store, &state.credentials)
            .map_err(invalid_config)?
        {
            return Err(invalid_config(Error::Config(format!(
                "provider `{}` is not configured on this gateway",
                selection.provider
            ))));
        }
        let current = state
            .config
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .clone();
        let next = current
            .registering_provider(
                selection.clone(),
                label.clone(),
                tint,
                model_ids.clone(),
                reasoning_efforts.clone(),
            )
            .map_err(invalid_config)?;
        let mut bots = state.bots.bots().map_err(internal)?;
        validate_bot_catalog(&state, &next, &bots)?;
        let catalog_changed = current.configured_providers.len() != next.configured_providers.len()
            || current.configured_providers.iter().any(|(id, previous)| {
                next.configured_providers.get(id).is_none_or(|next| {
                    previous.selection != next.selection
                        || previous.model_ids != next.model_ids
                        || previous.reasoning_efforts != next.reasoning_efforts
                })
            });
        if current == next {
            return gateway_ready(&state).await;
        }
        let target_epoch = catalog_changed
            .then(|| {
                state
                    .provider_epoch
                    .load(Ordering::Acquire)
                    .checked_add(1)
                    .ok_or_else(|| internal("provider catalog epoch overflow"))
            })
            .transpose()?;
        commit_provider_registration(
            &state,
            &selection,
            &label,
            &tint,
            &model_ids,
            &reasoning_efforts,
        )
        .map_err(internal)?;
        let defaults = state
            .config
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .bot_defaults
            .clone()
            .ok_or_else(|| internal("registered provider did not establish Bot defaults"))?;
        if state
            .bots
            .seed_default(&defaults)
            .map_err(internal)?
            .is_some()
        {
            bots = state.bots.bots().map_err(internal)?;
            self.broadcast_bots(&bots);
        }
        if let Some(target_epoch) = target_epoch {
            state.provider_epoch.store(target_epoch, Ordering::Release);
        }
        let payload = gateway_ready(&state).await?;
        let frame = ServerFrame::new(ServerMessage::Ready {
            payload: payload.clone(),
        });
        let _ = self.events.send(frame);
        Ok(payload)
    }

    pub(crate) async fn remove_provider(
        &self,
        instance: String,
    ) -> std::result::Result<ReadyPayload, Rejection> {
        let _mutation = self.begin_exclusive_mutation().await?;
        let state = self.state.lock().await;
        let current = state
            .config
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .clone();
        let next = current
            .removing_provider(&instance)
            .map_err(invalid_config)?;
        let bots = state.bots.bots().map_err(internal)?;
        for bot in &bots {
            if bot_references_removed_provider(bot, &instance, &next).map_err(invalid_config)? {
                return Err(Rejection {
                    code: "provider_in_use",
                    message: format!(
                        "provider `{instance}` is selected by Bot @{}; update that Bot first",
                        bot.handle
                    ),
                    fatal: false,
                });
            }
        }
        validate_bot_catalog(&state, &next, &bots)?;
        let target_epoch = state
            .provider_epoch
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or_else(|| internal("provider catalog epoch overflow"))?;
        commit_provider_removal(&state, &instance).map_err(internal)?;
        state.provider_epoch.store(target_epoch, Ordering::Release);
        let payload = gateway_ready(&state).await?;
        let _ = self.events.send(ServerFrame::new(ServerMessage::Ready {
            payload: payload.clone(),
        }));
        Ok(payload)
    }
}

fn commit_provider_registration(
    state: &super::GatewayState,
    selection: &ProviderConfig,
    label: &str,
    tint: &ProviderTint,
    model_ids: &[String],
    reasoning_efforts: &[String],
) -> crate::Result<()> {
    let mut current = state
        .config
        .lock()
        .map_err(|_| Error::Config("gateway configuration lock is poisoned".into()))?;
    let next = current.registering_provider(
        selection.clone(),
        label.into(),
        *tint,
        model_ids.to_vec(),
        reasoning_efforts.to_vec(),
    )?;
    state.store.save(&next)?;
    *current = next;
    Ok(())
}

fn commit_provider_removal(state: &super::GatewayState, instance: &str) -> crate::Result<()> {
    let mut current = state
        .config
        .lock()
        .map_err(|_| Error::Config("gateway configuration lock is poisoned".into()))?;
    let next = current.removing_provider(instance)?;
    state.store.save(&next)?;
    if let Err(error) = state.credentials.remove(instance) {
        if let Err(rollback) = state.store.save(&current) {
            return Err(Error::Config(format!(
                "{error}; failed to roll back provider configuration: {rollback}"
            )));
        }
        return Err(error);
    }
    *current = next;
    Ok(())
}

fn validate_bot_catalog(
    state: &super::GatewayState,
    gateway: &GatewayConfig,
    bots: &[BotRecord],
) -> std::result::Result<(), Rejection> {
    let models = configured_model_choices(gateway, &state.store, &state.credentials)
        .map_err(invalid_config)?;
    for bot in bots {
        if let Err(error) = gateway.validate_provider_selection(&bot.config.config.provider) {
            return Err(bot_catalog_rejection(bot, error));
        }
        if let Err(error) =
            crate::middleware_manifest::validate_choices(&bot.config.config.middleware, &models)
        {
            return Err(bot_catalog_rejection(bot, error));
        }
    }
    Ok(())
}

fn bot_catalog_rejection(bot: &BotRecord, error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "provider_in_use",
        message: format!(
            "provider catalog change would invalidate Bot @{}: {error}; register the replacement under a new instance, move affected Bots and Bot defaults, then remove the old instance",
            bot.handle
        ),
        fatal: false,
    }
}

fn bot_references_removed_provider(
    bot: &BotRecord,
    instance: &str,
    next: &GatewayConfig,
) -> crate::Result<bool> {
    if bot.config.config.provider.instance == instance {
        return Ok(true);
    }
    for (_, _, route) in
        crate::middleware_manifest::configured_model_routes(&bot.config.config.middleware)
    {
        if !crate::provider_catalog::configured_route_exists(next, route)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn reserve_provider_login(
    logins: &mut ProviderLogins,
    login_id: &str,
    client_id: &str,
    request_id: &str,
    provider_id: &str,
) -> std::result::Result<bool, Rejection> {
    if request_id.is_empty() || request_id.len() > MAX_LOGIN_REQUEST_ID_BYTES {
        return Err(Rejection {
            code: "invalid_provider_login",
            message: "provider login request IDs must contain 1–128 bytes".into(),
            fatal: false,
        });
    }
    if let Some(attempt) = logins.attempts.get(client_id)
        && attempt.request_id == request_id
    {
        if attempt.provider != provider_id {
            return Err(Rejection {
                code: "invalid_provider_login",
                message: "the provider login request belongs to a different provider".into(),
                fatal: false,
            });
        }
        return Ok(false);
    }
    if logins.active_id.is_some() {
        return Err(Rejection {
            code: "provider_login_in_progress",
            message: "finish the active provider login before starting another".into(),
            fatal: false,
        });
    }
    logins.active_id = Some(login_id.into());
    logins.attempts.insert(
        client_id.into(),
        ProviderLoginAttempt {
            login_id: login_id.into(),
            request_id: request_id.into(),
            provider: provider_id.into(),
            response: None,
        },
    );
    Ok(true)
}

#[cfg(test)]
#[path = "tests/provider_bots.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/providers.rs"]
mod coverage_tests;
