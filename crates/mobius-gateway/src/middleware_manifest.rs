//! Gateway composition registry for core-owned middleware manifests.

use std::collections::{BTreeMap, BTreeSet};

use mobius::middleware::manifest::MiddlewareManifest;
use mobius::protocol::{FrontendSettingValue, MiddlewareFeature, ModelChoice};

use crate::wire::MiddlewareConfig;
use crate::{Error, Result};

#[derive(Clone, Copy)]
pub(crate) enum BuiltinMiddleware {
    Sandbox,
    Attachments,
    Artifacts,
    Tools,
    Instructions,
    Extensions,
    Tasks,
    Subagents,
    Messages,
    ContextOffloading,
    Compaction,
    Scratchpad,
    Sessions,
    Bots,
}

pub(crate) struct MiddlewareRegistration {
    pub(crate) kind: BuiltinMiddleware,
    pub(crate) manifest: &'static MiddlewareManifest,
}

pub(crate) const MIDDLEWARE: [MiddlewareRegistration; 14] = [
    MiddlewareRegistration {
        kind: BuiltinMiddleware::Sandbox,
        manifest: &mobius::backend::sandbox::MANIFEST,
    },
    MiddlewareRegistration {
        kind: BuiltinMiddleware::Attachments,
        manifest: &mobius::middleware::attachments::MANIFEST,
    },
    MiddlewareRegistration {
        kind: BuiltinMiddleware::Artifacts,
        manifest: &mobius::middleware::artifacts::MANIFEST,
    },
    MiddlewareRegistration {
        kind: BuiltinMiddleware::Tools,
        manifest: &mobius::middleware::tools::MANIFEST,
    },
    MiddlewareRegistration {
        kind: BuiltinMiddleware::Instructions,
        manifest: &mobius::middleware::instructions::MANIFEST,
    },
    MiddlewareRegistration {
        kind: BuiltinMiddleware::Extensions,
        manifest: &mobius::middleware::extensions::MANIFEST,
    },
    MiddlewareRegistration {
        kind: BuiltinMiddleware::Tasks,
        manifest: &mobius::middleware::tasks::MANIFEST,
    },
    MiddlewareRegistration {
        kind: BuiltinMiddleware::Subagents,
        manifest: &mobius::middleware::subagents::MANIFEST,
    },
    MiddlewareRegistration {
        kind: BuiltinMiddleware::Messages,
        manifest: &mobius::middleware::messages::MANIFEST,
    },
    MiddlewareRegistration {
        kind: BuiltinMiddleware::ContextOffloading,
        manifest: &mobius::middleware::context_offloading::MANIFEST,
    },
    MiddlewareRegistration {
        kind: BuiltinMiddleware::Compaction,
        manifest: &mobius::middleware::compaction::MANIFEST,
    },
    MiddlewareRegistration {
        kind: BuiltinMiddleware::Scratchpad,
        manifest: &mobius::middleware::scratchpad::MANIFEST,
    },
    MiddlewareRegistration {
        kind: BuiltinMiddleware::Sessions,
        manifest: &mobius::middleware::sessions::MANIFEST,
    },
    MiddlewareRegistration {
        kind: BuiltinMiddleware::Bots,
        manifest: &mobius::middleware::bots::MANIFEST,
    },
];

pub(crate) fn features(models: &[ModelChoice]) -> Vec<MiddlewareFeature> {
    MIDDLEWARE
        .iter()
        .map(|entry| entry.manifest.feature(models))
        .collect()
}

pub(crate) fn default_config() -> MiddlewareConfig {
    let mut config = MiddlewareConfig {
        enabled: BTreeSet::new(),
        settings: BTreeMap::new(),
    };
    for entry in &MIDDLEWARE {
        let manifest = entry.manifest;
        if !manifest.required {
            config.set_enabled(manifest.id, manifest.default_enabled);
        }
        for setting in manifest.settings {
            config.set_setting(manifest.id, setting.id(), setting.default_value());
        }
    }
    config
}

pub(crate) fn validate(config: &MiddlewareConfig) -> Result<()> {
    for id in config.entries() {
        let manifest = definition(id)?.manifest;
        if manifest.required {
            return Err(Error::Config(format!(
                "required middleware `{id}` cannot be configured"
            )));
        }
    }
    for (middleware_id, settings) in &config.settings {
        let manifest = definition(middleware_id)?.manifest;
        if manifest.settings.is_empty() {
            return Err(Error::Config(format!(
                "middleware `{middleware_id}` has no settings"
            )));
        }
        for setting_id in settings.keys() {
            if !manifest
                .settings
                .iter()
                .any(|setting| setting.id() == setting_id)
            {
                return Err(Error::Config(format!(
                    "unknown setting `{middleware_id}.{setting_id}`"
                )));
            }
        }
    }
    for entry in &MIDDLEWARE {
        for setting in entry.manifest.settings {
            setting.validate(
                entry.manifest.id,
                config.setting(entry.manifest.id, setting.id()),
            )?;
        }
    }
    if integer_setting(config, "subagents", "max_agents")?
        < integer_setting(config, "subagents", "max_concurrency")?
    {
        return Err(Error::Config(
            "middleware setting `subagents.max_agents` must be at least `subagents.max_concurrency`"
                .into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_choices(config: &MiddlewareConfig, models: &[ModelChoice]) -> Result<()> {
    for entry in &MIDDLEWARE {
        for setting in entry.manifest.settings {
            setting.validate_choice(
                entry.manifest.id,
                config.setting(entry.manifest.id, setting.id()),
                models,
            )?;
        }
    }
    Ok(())
}

pub(crate) fn configured_model_routes(
    config: &MiddlewareConfig,
) -> Vec<(&'static str, &'static str, &str)> {
    MIDDLEWARE
        .iter()
        .flat_map(|entry| {
            entry.manifest.settings.iter().filter_map(|setting| {
                if !setting.uses_model_routes() {
                    return None;
                }
                let FrontendSettingValue::String(route) =
                    config.setting(entry.manifest.id, setting.id())?
                else {
                    return None;
                };
                Some((entry.manifest.id, setting.id(), route.as_str()))
            })
        })
        .collect()
}

pub(crate) fn integer_setting(
    config: &MiddlewareConfig,
    middleware: &str,
    setting: &str,
) -> Result<i64> {
    match config.setting(middleware, setting) {
        Some(FrontendSettingValue::Integer(value)) => Ok(*value),
        Some(FrontendSettingValue::String(_)) => Err(setting_type(middleware, setting, "integer")),
        None => Err(Error::Config(format!(
            "missing middleware setting `{middleware}.{setting}`"
        ))),
    }
}

pub(crate) fn usize_setting(
    config: &MiddlewareConfig,
    middleware: &str,
    setting: &str,
) -> Result<usize> {
    usize::try_from(integer_setting(config, middleware, setting)?).map_err(|_| {
        Error::Config(format!(
            "middleware setting `{middleware}.{setting}` must fit an unsigned integer"
        ))
    })
}

pub(crate) fn string_setting<'a>(
    config: &'a MiddlewareConfig,
    middleware: &str,
    setting: &str,
) -> Result<Option<&'a str>> {
    match config.setting(middleware, setting) {
        Some(FrontendSettingValue::String(value)) => Ok(Some(value)),
        Some(FrontendSettingValue::Integer(_)) => Err(setting_type(middleware, setting, "string")),
        None => Ok(None),
    }
}

fn definition(id: &str) -> Result<&'static MiddlewareRegistration> {
    MIDDLEWARE
        .iter()
        .find(|entry| entry.manifest.id == id)
        .ok_or_else(|| Error::Config(format!("unknown middleware `{id}`")))
}

fn setting_type(middleware: &str, setting: &str, expected: &str) -> Error {
    Error::Config(format!(
        "middleware setting `{middleware}.{setting}` must be {expected}"
    ))
}

#[cfg(test)]
mod tests {
    use mobius::middleware::context_offloading::DEFAULT_STALE_AFTER_TOKENS;
    use mobius::protocol::FrontendSettingKind;

    use super::*;

    #[test]
    fn defaults_and_required_features_come_from_core_manifests() {
        let config = default_config();
        let features = features(&[]);

        assert!(validate(&config).is_ok());
        assert_eq!(
            config.entries().collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "artifacts",
                "attachments",
                "compaction",
                "context_offloading",
                "scratchpad",
                "subagents",
            ])
        );
        assert_eq!(
            integer_setting(&config, "context_offloading", "stale_after_tokens")
                .expect("context setting"),
            DEFAULT_STALE_AFTER_TOKENS,
        );
        assert_eq!(
            config.setting("messages", "delivery"),
            Some(&FrontendSettingValue::String("steer".into()))
        );
        assert_eq!(
            features
                .iter()
                .filter(|feature| feature.required)
                .map(|feature| feature.id.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["bots", "messages", "sandbox", "sessions", "tools"])
        );

        let mut invalid = config;
        invalid.set_enabled("tools", true);
        assert!(validate(&invalid).is_err());
    }

    #[test]
    fn scratchpad_projects_after_compaction() {
        let position = |id| {
            MIDDLEWARE
                .iter()
                .position(|entry| entry.manifest.id == id)
                .expect("registered middleware")
        };

        assert!(position("compaction") < position("scratchpad"));
    }

    #[test]
    fn dynamic_choices_use_the_live_model_catalog() {
        let models = [ModelChoice {
            route: "provider::model::high".into(),
            group: "Provider · Model".into(),
            model: "model".into(),
            reasoning_effort: Some("high".into()),
            context_window: Some(200_000),
            supports_image_input: true,
            tool_discovery: mobius::protocol::ToolDiscoveryMode::Native,
        }];
        let subagents = features(&models)
            .into_iter()
            .find(|feature| feature.id == "subagents")
            .expect("subagent feature");
        let route = subagents
            .settings
            .iter()
            .find(|setting| setting.id == "model_route")
            .expect("model route setting");
        let FrontendSettingKind::Select {
            options,
            unset_label,
        } = &route.kind
        else {
            panic!("subagent route must be a select setting")
        };
        assert_eq!(unset_label.as_deref(), Some("Inherit parent"));
        assert_eq!(options[0].value, models[0].route);

        let mut config = default_config();
        config.set_setting(
            "subagents",
            "model_route",
            Some(FrontendSettingValue::String(models[0].route.clone())),
        );
        assert!(validate(&config).is_ok());
        assert!(validate_choices(&config, &models).is_ok());
        assert!(validate_choices(&config, &[]).is_err());
    }

    #[test]
    fn sandbox_manifest_drives_generic_approval_settings() {
        let config = default_config();
        let sandbox = features(&[])
            .into_iter()
            .find(|feature| feature.id == "sandbox")
            .expect("sandbox feature");

        assert!(sandbox.required);
        assert_eq!(
            config.setting("sandbox", "approval_policy"),
            Some(&FrontendSettingValue::String("ask".into()))
        );
        assert_eq!(
            sandbox
                .settings
                .iter()
                .map(|setting| setting.id.as_str())
                .collect::<Vec<_>>(),
            ["approval_policy"]
        );
    }

    #[test]
    fn config_rejects_unknown_mistyped_and_inconsistent_settings() {
        let mut config = default_config();
        config.set_setting("tools", "extra", Some(FrontendSettingValue::Integer(1)));
        assert!(validate(&config).is_err());

        config.set_setting("tools", "extra", None);
        config.set_setting(
            "context_offloading",
            "stale_after_tokens",
            Some(FrontendSettingValue::String("50000".into())),
        );
        assert!(validate(&config).is_err());

        let mut config = default_config();
        config.set_setting(
            "subagents",
            "max_agents",
            Some(FrontendSettingValue::Integer(2)),
        );
        assert!(validate(&config).is_err());
    }
}
