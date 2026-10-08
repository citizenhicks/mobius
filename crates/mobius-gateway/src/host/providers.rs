use crate::config::ProviderRegistration;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures_util::future::BoxFuture;
use mobius::backend::model::provider::{
    DeviceLogin, ProviderAuth, provider, uses_default_endpoint,
};
use uuid::Uuid;

use crate::Error;
use crate::config::GatewayConfig;
use crate::provider_catalog::{
    configured_model_catalog, configured_model_choices, credential_is_configured, selected_base_url,
};
use crate::wire::{
    AgentComposition, BotRecord, ProviderConfig, ProviderEndpointAuth, ReadyPayload, ServerFrame,
    ServerMessage,
};

use super::session::ProviderRefresh;
use super::{
    GatewayHost, Rejection, finish_publication, gateway_ready_after_unlock, internal,
    invalid_config,
};

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
        let publication = {
            let mut current = state.config()?;
            let models = configured_model_choices(&current, &state.store, &state.credentials)
                .map_err(internal)?;
            crate::config::validate_bot_compatibility(&current, &config, models.catalogs())
                .map_err(invalid_config)?;
            if current
                .bot_defaults
                .as_ref()
                .is_none_or(|previous| previous.config.realtime_voice != config.realtime_voice)
            {
                models
                    .validate_voice(config.realtime_voice.as_deref())
                    .map_err(invalid_config)?;
            }
            crate::extensions::ExtensionStore::new(&state.store)
                .resolve(&current, &config.extensions)
                .map_err(invalid_config)?;
            let next = current
                .replacing_bot_defaults(expected_revision, config)
                .map_err(invalid_config)?;
            let publication =
                crate::publication::Outcome::applied(state.store.save(&next)).map_err(internal)?;
            *current = next;
            publication
        };
        let follow_up = async {
            let payload = gateway_ready_after_unlock(state).await?;
            let _ = self.events.send(ServerFrame::new(ServerMessage::Ready {
                payload: payload.clone(),
            }));
            Ok(payload)
        }
        .await;
        finish_publication(publication.confirm(), follow_up)
    }

    pub(crate) async fn clear_credential(
        &self,
        instance: String,
    ) -> std::result::Result<(), Rejection> {
        let credential_mutation = self.begin_credential_mutation().await;
        let (base_url, publication) = {
            let state = self.state.lock().await;
            let config = state.config()?;
            let base_url = config
                .configured_providers
                .get(&instance)
                .map(|configured| {
                    provider(&configured.selection.provider).map(|definition| {
                        selected_base_url(definition, &configured.selection).map(str::to_owned)
                    })
                })
                .transpose()
                .map_err(invalid_config)?
                .flatten();
            let publication = crate::publication::Outcome::applied(
                state.credentials.remove(&instance).map(|_| ()),
            )
            .map_err(invalid_config)?;
            (base_url, publication)
        };
        drop(credential_mutation);
        finish_publication(
            publication.confirm(),
            self.refresh_provider_sessions(ProviderRefresh::Instance {
                instance: &instance,
                base_url: base_url.as_deref(),
            })
            .await,
        )
    }

    pub(crate) async fn set_credential(
        &self,
        instance: String,
        provider_id: String,
        api_key: String,
        base_url: Option<String>,
        expires_at: Option<u64>,
    ) -> std::result::Result<(), Rejection> {
        let credential_mutation = self.begin_credential_mutation().await;
        let (base_url, configured, publication) = {
            let state = self.state.lock().await;
            let definition = provider(&provider_id).map_err(invalid_config)?;
            let base_url = if definition.configurable_base_url() {
                base_url
                    .as_deref()
                    .or_else(|| definition.default_base_url())
            } else {
                base_url.as_deref()
            };
            let configured = state
                .config()?
                .configured_providers
                .get(&instance)
                .filter(|configured| {
                    let configured_base_url = selected_base_url(definition, &configured.selection);
                    configured.selection.provider == provider_id.as_str()
                        && configured.selection.endpoint_auth
                            == ProviderEndpointAuth::Credentialless
                        && configured_base_url == base_url
                })
                .map(|configured| ProviderRegistration {
                    selection: configured.selection.clone(),
                    label: None,
                    tint: None,
                    models: Vec::new(),
                    image_model_ids: None,
                });
            let publication = crate::publication::Outcome::applied(state.credentials.set(
                &instance,
                &provider_id,
                &api_key,
                base_url,
                expires_at,
            ))
            .map_err(invalid_config)?;
            (base_url, configured, publication)
        };
        drop(credential_mutation);
        if let Some(configured) = configured {
            let mut configured = configured;
            configured.selection.endpoint_auth = ProviderEndpointAuth::ProviderDefault;
            return finish_publication(
                publication.confirm(),
                self.register_provider(false, configured, true, true)
                    .await
                    .map(|_| ()),
            );
        }
        finish_publication(
            publication.confirm(),
            self.refresh_provider_sessions(ProviderRefresh::Instance {
                instance: &instance,
                base_url,
            })
            .await,
        )
    }

    pub(crate) async fn start_provider_login(
        &self,
        request_id: String,
        provider_id: String,
        client_id: &str,
    ) -> std::result::Result<Option<ServerMessage>, Rejection> {
        let definition = provider(&provider_id).map_err(invalid_config)?;
        let ProviderAuth::Browser(auth) = definition.auth() else {
            return Err(Rejection::new(
                "invalid_provider_auth",
                "the selected provider uses an API key",
            ));
        };
        if !auth.supports_device_login() {
            return Err(Rejection::new(
                "device_login_unavailable",
                "the selected provider does not support device-code login",
            ));
        }
        let state = self.state.lock().await;
        let transport = state.config()?.model_transport;
        let login_guard = Arc::clone(&state.provider_login);
        drop(state);
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
        self.spawn_provider_login(
            request_id,
            login_id,
            provider_id,
            auth.start_device_with_transport(transport),
        );
        Ok(None)
    }

    fn spawn_provider_login(
        &self,
        request_id: String,
        login_id: String,
        provider: String,
        start: BoxFuture<'static, mobius::Result<DeviceLogin>>,
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
                let completion = login.complete(path).await;
                if matches!(&completion, Err(mobius::Error::PublicationDurability(_))) {
                    let refresh = gateway
                        .refresh_provider_sessions(ProviderRefresh::Provider(&provider))
                        .await;
                    return finish_publication(completion.map_err(Error::from), refresh)
                        .map_err(|rejection| rejection.message);
                }
                completion.map_err(|error| error.to_string())
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
                .refresh_provider_sessions(ProviderRefresh::Provider(&provider))
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
        scope: ProviderRefresh<'_>,
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
        operator: bool,
        registration: ProviderRegistration,
        preserve_selection: bool,
        if_configured: bool,
    ) -> std::result::Result<ReadyPayload, Rejection> {
        if !operator
            && registration.models.iter().any(|model| {
                model.label.is_some()
                    || model.description.is_some()
                    || model.context_window.is_some()
            })
        {
            return Err(Rejection::new(
                "operator_required",
                "model metadata is configured locally by the gateway operator",
            ));
        }
        let selection = &registration.selection;
        let _mutation = self.begin_exclusive_mutation().await?;
        let state = self.state.lock().await;
        if if_configured
            && !state
                .config()?
                .configured_providers
                .contains_key(&registration.selection.instance)
        {
            return gateway_ready_after_unlock(state).await;
        }
        let mut bots = state.bots.bots().map_err(internal)?;
        let mut publication = None;
        let (changed, target_epoch) = {
            let current = state.config()?;
            validate_browser_endpoint_registration(&current, selection, operator)?;
            if !credential_is_configured(selection, &state.store, &state.credentials)
                .map_err(invalid_config)?
            {
                return Err(invalid_config(Error::Config(format!(
                    "provider `{}` is not configured on this gateway",
                    selection.provider
                ))));
            }
            let next = current
                .registering_configured(registration, preserve_selection)
                .map_err(invalid_config)?;
            validate_bot_catalog(&state, &current, &next, &bots)?;
            let catalog_changed = current.configured_providers.len()
                != next.configured_providers.len()
                || current.configured_providers.iter().any(|(id, previous)| {
                    next.configured_providers.get(id).is_none_or(|next| {
                        previous.selection != next.selection
                            || previous.models != next.models
                            || previous.image_model_ids != next.image_model_ids
                    })
                });
            if *current == next {
                (false, None)
            } else {
                let target_epoch = catalog_changed
                    .then(|| {
                        state
                            .provider_epoch
                            .load(Ordering::Acquire)
                            .checked_add(1)
                            .ok_or_else(|| internal("provider catalog epoch overflow"))
                    })
                    .transpose()?;
                publication = Some(
                    crate::publication::Outcome::applied(commit_provider_registration(
                        &state, current, next,
                    ))
                    .map_err(internal)?,
                );
                (true, target_epoch)
            }
        };
        if !changed {
            return gateway_ready_after_unlock(state).await;
        }
        if let Some(target_epoch) = target_epoch {
            state.provider_epoch.store(target_epoch, Ordering::Release);
        }
        let follow_up = async {
            let seeded = {
                let config = state.config()?;
                let defaults = config.bot_defaults.as_ref().ok_or_else(|| {
                    internal("registered provider did not establish Bot defaults")
                })?;
                state
                    .bots
                    .seed_default(defaults)
                    .map_err(internal)?
                    .is_some()
            };
            if seeded {
                bots = state.bots.bots().map_err(internal)?;
                self.broadcast_bots(&bots);
            }
            let payload = gateway_ready_after_unlock(state).await?;
            let frame = ServerFrame::new(ServerMessage::Ready {
                payload: payload.clone(),
            });
            let _ = self.events.send(frame);
            Ok(payload)
        }
        .await;
        finish_publication(
            publication.map_or(Ok(()), crate::publication::Outcome::confirm),
            follow_up,
        )
    }

    pub(crate) async fn remove_provider(
        &self,
        instance: String,
    ) -> std::result::Result<ReadyPayload, Rejection> {
        let _mutation = self.begin_exclusive_mutation().await?;
        let state = self.state.lock().await;
        let next = {
            let current = state.config()?;
            let next = current
                .removing_provider(&instance)
                .map_err(invalid_config)?;
            let bots = state.bots.bots().map_err(internal)?;
            for bot in &bots {
                if bot_references_removed_provider(bot, &instance, &next).map_err(invalid_config)? {
                    return Err(Rejection::new(
                        "provider_in_use",
                        format!(
                            "provider `{instance}` is selected by Bot @{}; update that Bot first",
                            bot.handle
                        ),
                    ));
                }
            }
            validate_bot_catalog(&state, &current, &next, &bots)?;
            next
        };
        let target_epoch = state
            .provider_epoch
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or_else(|| internal("provider catalog epoch overflow"))?;
        let publication =
            crate::publication::Outcome::applied(commit_provider_removal(&state, &instance, next))
                .map_err(internal)?;
        state.provider_epoch.store(target_epoch, Ordering::Release);
        let follow_up = async {
            let payload = gateway_ready_after_unlock(state).await?;
            let _ = self.events.send(ServerFrame::new(ServerMessage::Ready {
                payload: payload.clone(),
            }));
            Ok(payload)
        }
        .await;
        finish_publication(publication.confirm(), follow_up)
    }
}

fn validate_browser_endpoint_registration(
    gateway: &GatewayConfig,
    selection: &ProviderConfig,
    operator: bool,
) -> std::result::Result<(), Rejection> {
    let definition = provider(&selection.provider).map_err(invalid_config)?;
    let endpoint = selected_base_url(definition, selection);
    if operator
        || !matches!(definition.auth(), ProviderAuth::Browser(_))
        || definition.uses_default_endpoint(endpoint)
    {
        return Ok(());
    }
    let trusted = gateway.configured_providers.values().any(|configured| {
        configured.selection.provider == selection.provider
            && uses_default_endpoint(
                selected_base_url(definition, &configured.selection),
                endpoint,
            )
    });
    if trusted {
        return Ok(());
    }
    Err(Rejection::new(
        "operator_required",
        "browser-authenticated proxy destinations are configured locally by the gateway operator",
    ))
}

fn commit_provider_registration(
    state: &super::GatewayState,
    mut current: std::sync::MutexGuard<'_, GatewayConfig>,
    next: GatewayConfig,
) -> crate::Result<()> {
    let publication = crate::publication::Outcome::applied(state.store.save(&next))?;
    *current = next;
    publication.confirm()
}

fn commit_provider_removal(
    state: &super::GatewayState,
    instance: &str,
    next: GatewayConfig,
) -> crate::Result<()> {
    let mut current = state
        .config
        .lock()
        .map_err(|_| Error::Config("gateway configuration lock is poisoned".into()))?;
    let publication = crate::publication::Outcome::applied(state.store.save(&next))?;
    match state.credentials.remove(instance) {
        Ok(_) => {
            *current = next;
            publication.confirm()
        }
        Err(error @ Error::PublicationApplied { .. }) => {
            *current = next;
            Err(error)
        }
        Err(error) => {
            // Restore memory only if the rollback bytes actually became visible.
            match state.store.save(&current) {
                Ok(()) => Err(error),
                Err(rollback @ Error::PublicationApplied { .. }) => {
                    Err(crate::publication::applied_error(Error::Config(format!(
                        "{error}; rollback was applied but completion failed: {rollback}"
                    ))))
                }
                Err(rollback) => {
                    *current = next;
                    Err(crate::publication::applied_error(Error::Config(format!(
                        "{error}; provider configuration remains changed because rollback failed: {rollback}"
                    ))))
                }
            }
        }
    }
}

fn validate_bot_catalog(
    state: &super::GatewayState,
    previous: &GatewayConfig,
    gateway: &GatewayConfig,
    bots: &[BotRecord],
) -> std::result::Result<(), Rejection> {
    let models = configured_model_choices(gateway, &state.store, &state.credentials)
        .map_err(invalid_config)?;
    let previous_models = configured_model_catalog(previous).map_err(invalid_config)?;
    let next_models = configured_model_catalog(gateway).map_err(invalid_config)?;
    let validate_voice = |config: &AgentComposition| {
        let route = config.realtime_voice.as_deref();
        if previous_models.validate_voice(route).is_ok() {
            next_models.validate_voice(route)?;
        }
        Ok::<_, Error>(())
    };
    if let Some(defaults) = &gateway.bot_defaults {
        validate_voice(&defaults.config).map_err(invalid_config)?;
    }
    for bot in bots {
        if let Err(error) = crate::config::validate_bot_compatibility(
            gateway,
            &bot.config.config,
            models.catalogs(),
        ) {
            return Err(bot_catalog_rejection(bot, error));
        }
        validate_voice(&bot.config.config).map_err(|error| bot_catalog_rejection(bot, error))?;
    }
    Ok(())
}

fn bot_catalog_rejection(bot: &BotRecord, error: impl std::fmt::Display) -> Rejection {
    Rejection::new(
        "provider_in_use",
        format!(
            "provider catalog change would invalidate Bot @{}: {error}; register the replacement under a new instance, move affected Bots and Bot defaults, then remove the old instance",
            bot.handle
        ),
    )
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
        return Err(Rejection::new(
            "invalid_provider_login",
            "provider login request IDs must contain 1–128 bytes",
        ));
    }
    if let Some(attempt) = logins.attempts.get(client_id)
        && attempt.request_id == request_id
    {
        if attempt.provider != provider_id {
            return Err(Rejection::new(
                "invalid_provider_login",
                "the provider login request belongs to a different provider",
            ));
        }
        return Ok(false);
    }
    if logins.active_id.is_some() {
        return Err(Rejection::new(
            "provider_login_in_progress",
            "finish the active provider login before starting another",
        ));
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
