//! Moves every Bot and the Bot defaults off retired models of one provider setup in one
//! gateway operation, so an operator names the change instead of rewriting each Bot.

use std::collections::BTreeSet;

use mobius::protocol::FrontendSettingValue;

use super::gateway_host::GatewayHost;
use super::rejection::{Rejection, internal, invalid_config};
use crate::wire::{AgentComposition, BotModelSwitch, HarnessConfig};

impl GatewayHost {
    /// Bots and defaults on `instance` running a model in `from` run `to` at its default
    /// effort, and middleware settings naming a retired route name `to`'s route.
    pub(crate) async fn switch_bot_models(
        &self,
        instance: &str,
        from: &[String],
        to: &str,
    ) -> std::result::Result<BotModelSwitch, Rejection> {
        let from: BTreeSet<&str> = from.iter().map(String::as_str).collect();
        if from.contains(to) {
            return Err(invalid_config("the new model is also a retired one"));
        }
        let (bots, next_defaults, route) = {
            let state = self.state.lock().await;
            let gateway = state.config()?;
            let configured = gateway
                .configured_providers
                .get(instance)
                .ok_or_else(|| invalid_config("unknown provider setup"))?;
            let definition =
                mobius::backend::model::provider::provider(&configured.selection.provider)
                    .map_err(invalid_config)?;
            let mut selection = configured.selection.clone(); // rules-allow: the target selection is resolved on a copy
            selection.model = to.into();
            selection.reasoning_effort = None;
            let effort =
                crate::config::effective_reasoning_effort(definition, configured, &selection);
            let route = crate::config::model_route_id(instance, to, effort);
            let next_defaults = gateway.bot_defaults.as_ref().and_then(|defaults| {
                let next = switched(&defaults.config, instance, &from, to, &route)?;
                Some((defaults.revision, next))
            });
            drop(gateway);
            (state.bots.bots().map_err(internal)?, next_defaults, route)
        };
        let switch = |config: &AgentComposition| switched(config, instance, &from, to, &route);
        let mut changed = 0;
        for bot in &bots {
            let HarnessConfig::Mobius(composition) = &bot.config.config else {
                continue;
            };
            let Some(next) = switch(composition) else {
                continue;
            };
            self.update_bot(
                &bot.id,
                bot.config.revision,
                crate::bots::BotIdentity {
                    name: &bot.name,
                    description: &bot.description,
                    tint: bot.tint,
                    shape: bot.shape,
                },
                HarnessConfig::Mobius(next.into()),
            )
            .await
            .map_err(|rejection| switch_failure(rejection, changed))?;
            changed += 1;
        }
        let defaults_changed = next_defaults.is_some();
        if let Some((revision, config)) = next_defaults {
            self.configure_bot_defaults(revision, config)
                .await
                .map_err(|rejection| switch_failure(rejection, changed))?;
        }
        Ok(BotModelSwitch {
            bots: bots.len(),
            bots_changed: changed,
            defaults_changed,
        })
    }
}

fn switch_failure(mut rejection: Rejection, changed: usize) -> Rejection {
    if changed > 0 {
        rejection.message = format!(
            "Model switch stopped after updating {changed} Bots; those changes remain saved. Retry to move the remaining retired models. {}",
            rejection.message
        )
        .into();
    }
    rejection
}

fn switched(
    config: &AgentComposition,
    instance: &str,
    from: &BTreeSet<&str>,
    to: &str,
    route: &str,
) -> Option<AgentComposition> {
    let retired = |value: &str| {
        let mut parts = value.split("::");
        parts.next() == Some(instance) && parts.next().is_some_and(|model| from.contains(model))
    };
    let primary =
        config.provider.instance == instance && from.contains(config.provider.model.as_str());
    let middleware = config
        .middleware
        .settings
        .values()
        .flat_map(|settings| settings.values())
        .any(|value| matches!(value, FrontendSettingValue::String(text) if retired(text)));
    if !primary && !middleware {
        return None;
    }
    let mut next = config.clone(); // rules-allow: the switched copy is saved in place of the original
    if primary {
        next.provider.model = to.into();
        next.provider.reasoning_effort = None;
    }
    for value in next
        .middleware
        .settings
        .values_mut()
        .flat_map(|settings| settings.values_mut())
    {
        if let FrontendSettingValue::String(text) = value
            && retired(text)
        {
            *text = route.into();
        }
    }
    Some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_models_and_their_routes_move_and_others_stay() {
        let mut config = AgentComposition::default();
        config.provider.instance = "cloud".into();
        config.provider.model = "old".into();
        config.provider.reasoning_effort = Some("high".into());
        let settings = config
            .middleware
            .settings
            .entry("images".into())
            .or_default();
        settings.insert(
            "text".into(),
            FrontendSettingValue::String("cloud::old::low".into()),
        );
        settings.insert(
            "kept".into(),
            FrontendSettingValue::String("own::old::low".into()),
        );
        let from = BTreeSet::from(["old"]);
        let next = switched(&config, "cloud", &from, "new", "cloud::new::default").unwrap();
        assert_eq!(
            (
                next.provider.model.as_str(),
                next.provider.reasoning_effort.as_deref()
            ),
            ("new", None)
        );
        let settings = &next.middleware.settings["images"];
        assert_eq!(
            settings["text"],
            FrontendSettingValue::String("cloud::new::default".into())
        );
        assert_eq!(
            settings["kept"],
            FrontendSettingValue::String("own::old::low".into())
        );
        assert!(switched(&next, "cloud", &from, "new", "cloud::new::default").is_none());
    }
}
