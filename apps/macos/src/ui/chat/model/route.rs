use super::{
    AgentComposition, BotRecord, Context, FrontendSettingValue, ModelChoice, ModelTarget,
    ReadyPayload, Workspace,
};
use crate::gateway;
use mobius_gateway::wire::{ClientMessage, HarnessConfig, ProviderConfig};
use std::sync::Arc;

impl Workspace {
    /// Puts `route` on `target`: a Bot saves it at once; a draft keeps it until Save. Only
    /// a setting can be unset, with `None`.
    pub(in crate::ui) fn apply_model_route(
        &mut self,
        target: &ModelTarget,
        route: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        match target {
            ModelTarget::Setting {
                scope,
                feature,
                setting,
            } => {
                self.edit_draft_config(
                    scope,
                    |config| set_route_setting(config, feature, setting, route),
                    cx,
                );
                return;
            }
            ModelTarget::Bot(_) | ModelTarget::Draft(_) => {}
        }
        let Some(route) = route else {
            return;
        };
        let (Some(gateway), Some(config)) =
            (&self.client.state.gateway, self.target_config(target))
        else {
            return;
        };
        let Some(config) = with_route(gateway, config, route) else {
            return;
        };
        match target {
            ModelTarget::Bot(_) => {
                if let Some(message) = self
                    .target_bot(target)
                    .map(|bot| update_bot_message(bot, HarnessConfig::Mobius(Arc::new(config))))
                {
                    self.send(message, cx);
                }
            }
            ModelTarget::Draft(scope) | ModelTarget::Setting { scope, .. } => {
                self.edit_draft_config(scope, |draft| *draft = config, cx);
            }
        }
    }
}

/// Stores a model-route setting as the gateway validates it: the route itself, or nothing
/// to inherit.
pub(super) fn set_route_setting(
    config: &mut AgentComposition,
    feature: &str,
    setting: &str,
    route: Option<&str>,
) {
    let value = route.map(|route| FrontendSettingValue::String(route.to_owned()));
    config.middleware.set_setting(feature, setting, value);
}

/// `config` on the model of `route`, dropping capabilities its selected policies exclude;
/// the voice is independent of the chat model.
pub(super) fn with_route(
    gateway: &ReadyPayload,
    config: &AgentComposition,
    route: &str,
) -> Option<AgentComposition> {
    let mut config = config.clone(); // rules-allow: the new route goes on an edited copy while the source keeps its config
    config.provider = provider_for_route(gateway, route)?;
    config
        .middleware
        .reconcile(&gateway.agent_catalog.middleware_features);
    Some(config)
}

/// Saves `config` for `bot`, keeping its identity and guarding against a stale revision.
pub(in crate::ui) fn update_bot_message(bot: &BotRecord, config: HarnessConfig) -> ClientMessage {
    ClientMessage::UpdateBot {
        request_id: gateway::request_id(),
        id: Arc::clone(&bot.id),
        expected_revision: bot.config.revision,
        name: Arc::clone(&bot.name),
        description: Arc::clone(&bot.description),
        tint: bot.tint,
        shape: bot.shape,
        config,
    }
}

pub(in crate::ui) fn provider_for_route(
    gateway: &ReadyPayload,
    route: &str,
) -> Option<ProviderConfig> {
    let choice = gateway
        .agent_catalog
        .models
        .iter()
        .find(|choice| *choice.route == *route)?;
    let instance = gateway.agent_catalog.model_providers.get(&choice.route)?;
    let mut provider = gateway
        .provider_instances
        .iter()
        .find(|provider| &provider.selection.instance == instance)?
        .selection
        .clone(); // rules-allow: the Bot's provider is an edited copy of the gateway's instance
    provider.model = choice.model.to_string();
    provider.reasoning_effort = choice.reasoning_effort.as_deref().map(str::to_owned);
    Some(provider)
}

pub(in crate::ui) fn configured_model<'a>(
    gateway: &'a ReadyPayload,
    provider: &ProviderConfig,
) -> Option<&'a ModelChoice> {
    let default = gateway
        .providers
        .iter()
        .find(|status| status.provider == provider.provider)
        .and_then(|status| {
            status
                .models
                .iter()
                .find(|model| model.id == provider.model)
        })
        .and_then(|model| model.default_reasoning.as_ref());
    let candidates = || {
        gateway.agent_catalog.models.iter().filter(|model| {
            *model.model == *provider.model
                && gateway.agent_catalog.model_providers.get(&model.route)
                    == Some(&provider.instance)
        })
    };
    let effort = provider
        .reasoning_effort
        .as_ref()
        .or(default)
        .map(String::as_str);
    candidates()
        .find(|model| model.reasoning_effort.as_deref() == effort)
        .or_else(|| {
            provider
                .reasoning_effort
                .is_none()
                .then(|| candidates().next())
                .flatten()
        })
}
