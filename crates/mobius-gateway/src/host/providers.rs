use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex as StdMutex};

use mobius::backend::model::provider::{ProviderAuth, provider};
use uuid::Uuid;

use crate::Error;
use crate::config::GatewayConfig;
use crate::provider_catalog::{configured_model_choices, credential_is_configured};
use crate::wire::{
    AgentComposition, BotRecord, ProviderConfig, ProviderEndpointAuth, ProviderTint, ReadyPayload,
    ServerFrame, ServerMessage,
};

use super::session::ProviderRefresh;
use super::{GatewayHost, Rejection, gateway_ready, internal, invalid_config};

impl GatewayHost {
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

    pub(crate) async fn set_credential(
        &self,
        instance: String,
        provider_id: String,
        api_key: String,
        base_url: Option<String>,
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
                .set(&instance, &provider_id, &api_key, base_url.as_deref())
                .map_err(invalid_config)?;
            let configured = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?
                .configured_providers
                .get(&instance)
                .filter(|configured| {
                    let configured_base_url = if definition.configurable_base_url() {
                        configured
                            .selection
                            .base_url
                            .as_deref()
                            .or_else(|| definition.default_base_url())
                    } else {
                        None
                    };
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
    ) -> std::result::Result<(), Rejection> {
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
        let (login_guard, path) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.provider_login),
                state.store.provider_auth_path(),
            )
        };
        let login_id = Uuid::new_v4().to_string();
        reserve_provider_login(&login_guard, &login_id)?;
        let login = match auth.start_device().await {
            Ok(login) => login,
            Err(error) => {
                release_provider_login(&login_guard, &login_id)?;
                return Err(internal(error));
            }
        };
        self.broadcast(ServerMessage::ProviderLoginStarted {
            request_id: request_id.clone(),
            login_id: login_id.clone(),
            provider: provider_id.clone(),
            verification_url: login.verification_url().into(),
            user_code: login.user_code().into(),
        });
        let gateway = self.clone();
        tokio::spawn(async move {
            let result = login
                .complete(path)
                .await
                .map_err(|error| error.to_string());
            gateway
                .finish_provider_login(request_id, login_id, provider_id, result)
                .await;
        });
        Ok(())
    }

    async fn finish_provider_login(
        &self,
        request_id: String,
        login_id: String,
        provider: String,
        result: std::result::Result<(), String>,
    ) {
        let login_guard = Arc::clone(&self.state.lock().await.provider_login);
        match release_provider_login(&login_guard, &login_id) {
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
        if let Err(message) = result {
            self.broadcast(ServerMessage::Rejected {
                request_id,
                code: "provider_login_failed".into(),
                message,
                fatal: false,
            });
            return;
        }
        let refresh = self
            .refresh_provider_sessions(ProviderRefresh::Provider(provider.clone()))
            .await;
        self.broadcast(ServerMessage::ProviderLoginFinished {
            request_id,
            login_id,
            provider,
        });
        if let Err(rejection) = refresh {
            self.broadcast(ServerMessage::Error {
                code: rejection.code.into(),
                message: rejection.message,
                fatal: rejection.fatal,
            });
        }
    }

    async fn refresh_provider_sessions(
        &self,
        scope: ProviderRefresh,
    ) -> std::result::Result<(), Rejection> {
        let sessions = self
            .state
            .lock()
            .await
            .sessions
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut failure = None;
        for host in sessions {
            if let Err(rejection) = host.refresh_provider(scope.clone()).await {
                failure.get_or_insert(rejection);
            }
        }
        failure.map_or(Ok(()), Err)
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
        let mut state = self.state.lock().await;
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
        let catalog_changed = current.configured_providers != next.configured_providers;
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
        let residents = provider_cutover_residents(&mut state).await?;
        if residents.iter().any(|resident| !resident.status.idle) {
            return Err(Rejection {
                code: "agent_busy",
                message: "finish or interrupt active Bot turns before changing gateway providers"
                    .into(),
                fatal: false,
            });
        }
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
        let reload_failures = reload_provider_residents(&mut state, residents, &bots).await?;
        let payload = gateway_ready(&state).await?;
        let frame = ServerFrame::new(ServerMessage::Ready {
            payload: payload.clone(),
        });
        let _ = self.events.send(frame);
        broadcast_reload_failures(self, "provider changed", &reload_failures);
        Ok(payload)
    }

    pub(crate) async fn remove_provider(
        &self,
        instance: String,
    ) -> std::result::Result<ReadyPayload, Rejection> {
        let _mutation = self.begin_exclusive_mutation().await?;
        let mut state = self.state.lock().await;
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
        let residents = provider_cutover_residents(&mut state).await?;
        if residents.iter().any(|resident| !resident.status.idle) {
            return Err(Rejection {
                code: "agent_busy",
                message: "finish or interrupt active turns before removing a gateway provider"
                    .into(),
                fatal: false,
            });
        }
        commit_provider_removal(&state, &instance).map_err(internal)?;
        state.provider_epoch.store(target_epoch, Ordering::Release);
        let reload_failures = reload_provider_residents(&mut state, residents, &bots).await?;
        let payload = gateway_ready(&state).await?;
        let _ = self.events.send(ServerFrame::new(ServerMessage::Ready {
            payload: payload.clone(),
        }));
        broadcast_reload_failures(self, "provider removed", &reload_failures);
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

struct ProviderCutoverResident {
    session_id: String,
    host: super::HostHandle,
    status: super::ProviderCutoverStatus,
}

async fn provider_cutover_residents(
    state: &mut super::GatewayState,
) -> std::result::Result<Vec<ProviderCutoverResident>, Rejection> {
    let sessions = state
        .sessions
        .iter()
        .map(|(id, host)| (id.clone(), host.clone()))
        .collect::<Vec<_>>();
    let mut residents = Vec::new();
    let mut stopped = Vec::new();
    for (id, host) in sessions {
        if !host.is_alive() {
            stopped.push(id);
            continue;
        }
        match host.provider_cutover_status().await {
            Ok(status) => residents.push(ProviderCutoverResident {
                session_id: id,
                host,
                status,
            }),
            Err(rejection) if rejection.code == "gateway_stopped" => stopped.push(id),
            Err(rejection) => return Err(rejection),
        }
    }
    for id in stopped {
        state.sessions.remove(&id);
    }
    Ok(residents)
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

async fn reload_provider_residents(
    state: &mut super::GatewayState,
    residents: Vec<ProviderCutoverResident>,
    bots: &[BotRecord],
) -> std::result::Result<Vec<String>, Rejection> {
    let mut failures = Vec::new();
    for resident in residents {
        let bot = bots
            .iter()
            .find(|bot| bot.id == resident.host.bot_id())
            .cloned()
            .ok_or_else(|| internal("resident chat has no authoritative Bot profile"))?;
        let result = resident.host.reload_bot(bot).await;
        if let Err(error) = result {
            if !resident.host.stop_if_idle().await {
                return Err(internal(
                    "chat became busy after the provider catalog was committed",
                ));
            }
            state.sessions.remove(&resident.session_id);
            failures.push(format!("{}: {}", resident.session_id, error.message));
        }
    }
    Ok(failures)
}

fn broadcast_reload_failures(host: &GatewayHost, action: &str, failures: &[String]) {
    if failures.is_empty() {
        return;
    }
    let _ = host.events.send(ServerFrame::new(ServerMessage::Error {
        code: "provider_reload".into(),
        message: format!(
            "{action}; reopen chats that could not reload: {}",
            failures.join(", ")
        ),
        fatal: false,
    }));
}

fn ensure_provider_login_available(
    active_login: Option<&str>,
) -> std::result::Result<(), Rejection> {
    if active_login.is_some() {
        return Err(Rejection {
            code: "provider_login_in_progress",
            message: "finish the active provider login before starting another".into(),
            fatal: false,
        });
    }
    Ok(())
}

fn reserve_provider_login(
    active_login: &StdMutex<Option<String>>,
    login_id: &str,
) -> std::result::Result<(), Rejection> {
    let mut active_login = active_login
        .lock()
        .map_err(|_| internal("provider login lock is poisoned"))?;
    ensure_provider_login_available(active_login.as_deref())?;
    *active_login = Some(login_id.into());
    Ok(())
}

fn release_provider_login(
    active_login: &StdMutex<Option<String>>,
    login_id: &str,
) -> std::result::Result<bool, Rejection> {
    let mut active_login = active_login
        .lock()
        .map_err(|_| internal("provider login lock is poisoned"))?;
    if active_login.as_deref() != Some(login_id) {
        return Ok(false);
    }
    *active_login = None;
    Ok(true)
}

#[cfg(test)]
#[path = "tests/provider_bots.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/providers.rs"]
mod coverage_tests;
