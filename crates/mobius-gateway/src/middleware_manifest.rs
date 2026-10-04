//! Gateway composition registry for core-owned middleware manifests.

use std::collections::{BTreeMap, BTreeSet};

use mobius::middleware::manifest::{MiddlewareManifest, MiddlewareSettingManifest};
use mobius::middleware::subagents::SubagentCeilings;
use mobius::protocol::{FrontendSettingValue, MiddlewareFeature, ModelChoice};

use crate::wire::MiddlewareConfig;
use crate::{Error, Result};

#[derive(Clone, Copy)]
pub(crate) enum BuiltinMiddleware {
    Sandbox,
    Attachments,
    Artifacts,
    ImageGeneration,
    Tools,
    Instructions,
    Extensions,
    Tasks,
    Subagents,
    Messages,
    ContextOffloading,
    Compaction,
    ComputerControl,
    Scratchpad,
    Sessions,
    PersistentChat,
}

pub(crate) struct MiddlewareRegistration {
    pub(crate) kind: BuiltinMiddleware,
    pub(crate) manifest: &'static MiddlewareManifest,
}

pub(crate) static MIDDLEWARE: std::sync::LazyLock<[MiddlewareRegistration; 16]> =
    std::sync::LazyLock::new(|| {
        [
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
                kind: BuiltinMiddleware::ImageGeneration,
                manifest: &mobius::middleware::image_generation::MANIFEST,
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
                kind: BuiltinMiddleware::ComputerControl,
                manifest: &mobius::middleware::computer_control::MANIFEST,
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
                kind: BuiltinMiddleware::PersistentChat,
                manifest: &crate::persistent_chat::MANIFEST,
            },
            MiddlewareRegistration {
                kind: BuiltinMiddleware::Sessions,
                manifest: &mobius::middleware::sessions::MANIFEST,
            },
        ]
    });

pub(crate) fn features(models: &[ModelChoice]) -> Vec<MiddlewareFeature> {
    features_with_ceilings(models, SubagentCeilings::default())
}

pub(crate) fn features_with_ceilings(
    models: &[ModelChoice],
    ceilings: SubagentCeilings,
) -> Vec<MiddlewareFeature> {
    MIDDLEWARE
        .iter()
        .map(|entry| {
            let mut feature = entry.manifest.feature(models);
            if matches!(entry.kind, BuiltinMiddleware::Subagents) {
                feature.settings = ceilings
                    .settings()
                    .iter()
                    .map(|setting| setting.schema(models))
                    .collect();
            }
            feature
        })
        .collect()
}

pub(crate) fn default_config() -> MiddlewareConfig {
    let mut config = MiddlewareConfig {
        enabled: BTreeSet::new(),
        settings: BTreeMap::new(),
    };
    for entry in MIDDLEWARE.iter() {
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

pub(crate) fn materialize_integer_defaults(
    config: &mut MiddlewareConfig,
    ceilings: Option<SubagentCeilings>,
) {
    let subagent_settings = ceilings.map(SubagentCeilings::settings);
    for entry in MIDDLEWARE.iter() {
        let settings = if matches!(entry.kind, BuiltinMiddleware::Subagents) {
            let Some(settings) = subagent_settings.as_deref() else {
                continue;
            };
            settings
        } else {
            entry.manifest.settings
        };
        for setting in settings {
            if let MiddlewareSettingManifest::Integer { id, default, .. } = *setting
                && config.setting(entry.manifest.id, id).is_none()
            {
                config.set_setting(
                    entry.manifest.id,
                    id,
                    Some(FrontendSettingValue::Integer(default)),
                );
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn validate(config: &MiddlewareConfig) -> Result<()> {
    validate_with_ceilings(config, SubagentCeilings::default())
}

pub(crate) fn validate_with_ceilings(
    config: &MiddlewareConfig,
    ceilings: SubagentCeilings,
) -> Result<()> {
    let features = features_with_ceilings(&[], ceilings);
    for id in config.entries() {
        if let Some(policy) = config.disabled_by(&features, id, None) {
            return Err(Error::Config(format!(
                "middleware `{id}` is incompatible with the selected `{policy}` policy"
            )));
        }
    }
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
    for entry in MIDDLEWARE.iter() {
        let subagent_settings;
        let settings = if matches!(entry.kind, BuiltinMiddleware::Subagents) {
            subagent_settings = ceilings.settings();
            &subagent_settings
        } else {
            entry.manifest.settings
        };
        for setting in settings {
            let default = match setting {
                MiddlewareSettingManifest::Integer { .. } => setting.default_value(),
                MiddlewareSettingManifest::Select { .. } => None,
            };
            setting.validate(
                entry.manifest.id,
                config
                    .setting(entry.manifest.id, setting.id())
                    .or(default.as_ref()),
            )?;
        }
    }
    subagent_limits(config, ceilings)?;
    crate::assembly::configured_compaction(config)?;
    Ok(())
}

pub(crate) fn validate_choices(
    config: &MiddlewareConfig,
    models: &[ModelChoice],
    selected_model: &ModelChoice,
) -> Result<()> {
    let features = features(models);
    for id in config.entries() {
        if let Some(policy) = config.disabled_by(&features, id, Some(selected_model)) {
            return Err(Error::Config(format!(
                "middleware `{id}` is incompatible with {policy}"
            )));
        }
    }
    for entry in MIDDLEWARE.iter() {
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
    integer_setting_from_manifest(
        config,
        middleware,
        setting,
        definition(middleware)?.manifest.settings,
    )
}

fn integer_setting_from_manifest(
    config: &MiddlewareConfig,
    middleware: &str,
    setting: &str,
    manifest: &[MiddlewareSettingManifest],
) -> Result<i64> {
    match config.setting(middleware, setting) {
        Some(FrontendSettingValue::Integer(value)) => Ok(*value),
        Some(FrontendSettingValue::String(_)) => Err(setting_type(middleware, setting, "integer")),
        None => {
            let declared = manifest.iter().find(|entry| entry.id() == setting);
            match declared.and_then(|entry| entry.default_value()) {
                Some(FrontendSettingValue::Integer(value)) => Ok(value),
                _ => Err(Error::Config(format!(
                    "missing integer middleware setting `{middleware}.{setting}`"
                ))),
            }
        }
    }
}

pub(crate) fn subagent_limits(
    config: &MiddlewareConfig,
    ceilings: SubagentCeilings,
) -> Result<(u8, usize, usize)> {
    let settings = ceilings.settings();
    let value = |id| integer_setting_from_manifest(config, "subagents", id, &settings);
    let max_depth = u8::try_from(value("max_depth")?)
        .map_err(|_| Error::Config("subagent max depth must fit an unsigned byte".into()))?;
    let max_concurrency = usize::try_from(value("max_concurrency")?).map_err(|_| {
        Error::Config("subagent max concurrency must fit an unsigned integer".into())
    })?;
    let max_agents = usize::try_from(value("max_agents")?)
        .map_err(|_| Error::Config("subagent max agents must fit an unsigned integer".into()))?;
    ceilings.validate(max_depth, max_concurrency, max_agents)?;
    Ok((max_depth, max_concurrency, max_agents))
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
    use mobius::middleware::context_offloading::default_stale_after_tokens;
    use mobius::protocol::FrontendSettingKind;

    use super::*;

    #[test]
    fn defaults_and_required_features_come_from_core_manifests() {
        let config = default_config();
        let features = features(&[]);

        assert!(validate(&config).is_ok());
        assert_eq!(config.setting("bots", "collaboration"), None);
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
            default_stale_after_tokens(),
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
            BTreeSet::from([
                "messages",
                "persistent_chat",
                "sandbox",
                "sessions",
                "tools"
            ])
        );

        let mut invalid = config;
        invalid.set_enabled("tools", true);
        assert!(validate(&invalid).is_err());
    }

    #[test]
    fn handoff_excludes_offloading_but_keeps_tasks_independent() {
        let mut config = default_config();
        config.set_setting(
            "compaction",
            "mode",
            Some(FrontendSettingValue::String("handoff".into())),
        );
        config.set_enabled("tasks", true);
        assert!(
            validate(&config)
                .expect_err("conflicting policies")
                .to_string()
                .contains("incompatible")
        );
        config.reconcile(&features(&[]), None);
        assert!(!config.enabled("context_offloading"));
        assert!(config.enabled("tasks"));
        assert!(validate(&config).is_ok());
        config.set_enabled("compaction", false);
        config.set_enabled("context_offloading", true);
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn image_generation_is_disabled_for_an_unsupported_model() {
        let mut config = default_config();
        config.set_enabled("image_generation", true);
        let models = [ModelChoice {
            route: "provider::model::default".into(),
            group: "Provider".into(),
            model: "model".into(),
            reasoning_effort: None,
            context_window: None,
            supports_image_input: true,
            supports_image_generation: false,
            supports_realtime_voice: false,
            tool_discovery: mobius::protocol::ToolDiscoveryMode::Native,
        }];
        assert!(validate_choices(&config, &models, &models[0]).is_err());
        config.reconcile(&features(&models), Some(&models[0]));
        assert!(!config.enabled("image_generation"));
        let mut supported = models;
        supported[0].supports_image_generation = true;
        config.set_enabled("image_generation", true);
        assert!(validate_choices(&config, &supported, &supported[0]).is_ok());

        let mut sibling = supported[0].clone();
        sibling.route = "provider::other::default".into();
        sibling.supports_image_generation = false;
        let models = [supported[0].clone(), sibling];
        assert!(validate_choices(&config, &models, &models[1]).is_err());
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
            supports_image_generation: false,
            supports_realtime_voice: false,
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
        assert!(validate_choices(&config, &models, &models[0]).is_ok());
        assert!(validate_choices(&config, &[], &models[0]).is_err());
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
            [
                "approval_policy",
                "tool_output_bytes",
                "background_commands"
            ]
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
            "compaction",
            "handoff_warning_reserves",
            Some(FrontendSettingValue::Integer(1)),
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

    #[test]
    fn operator_subagent_ceilings_control_the_catalog_and_client_settings() {
        let ceilings = SubagentCeilings::new(32, 128, 512).expect("operator policy");
        let mut config = default_config();
        for (id, value) in [
            ("max_depth", 24),
            ("max_concurrency", 96),
            ("max_agents", 300),
        ] {
            config.set_setting("subagents", id, Some(FrontendSettingValue::Integer(value)));
        }
        assert!(validate(&config).is_err());
        validate_with_ceilings(&config, ceilings).expect("operator permits larger agent tree");
        let feature = features_with_ceilings(&[], ceilings)
            .into_iter()
            .find(|feature| feature.id == "subagents")
            .expect("subagents");
        for (id, expected) in [
            ("max_depth", 32),
            ("max_concurrency", 128),
            ("max_agents", 512),
        ] {
            let setting = feature
                .settings
                .iter()
                .find(|setting| setting.id == id)
                .expect("setting");
            assert!(
                matches!(setting.kind, FrontendSettingKind::Integer { max: Some(max), .. } if max == expected)
            );
        }
        config.set_setting(
            "subagents",
            "max_depth",
            Some(FrontendSettingValue::Integer(33)),
        );
        assert!(validate_with_ceilings(&config, ceilings).is_err());
    }

    #[test]
    fn lower_subagent_ceilings_resolve_absent_values_and_reject_saved_overrides() {
        let ceilings = SubagentCeilings::new(1, 2, 3).expect("small operator policy");
        let mut config = default_config();
        config.settings.remove("subagents");
        validate_with_ceilings(&config, ceilings).expect("absent values inherit operator bounds");
        assert_eq!(
            subagent_limits(&config, ceilings).expect("runtime limits"),
            (1, 2, 3)
        );
        for (id, expected) in [("max_depth", 1), ("max_concurrency", 2), ("max_agents", 3)] {
            let settings = ceilings.settings();
            let setting = settings
                .iter()
                .find(|setting| setting.id() == id)
                .expect("control");
            assert_eq!(
                setting.default_value(),
                Some(FrontendSettingValue::Integer(expected))
            );
        }
        materialize_integer_defaults(&mut config, None);
        assert!(config.setting("subagents", "max_agents").is_none());
        materialize_integer_defaults(&mut config, Some(ceilings));
        assert_eq!(
            subagent_limits(&config, ceilings).expect("projected limits"),
            (1, 2, 3)
        );
        config.set_setting(
            "subagents",
            "max_depth",
            Some(FrontendSettingValue::Integer(2)),
        );
        assert!(validate_with_ceilings(&config, ceilings).is_err());
        assert!(subagent_limits(&config, ceilings).is_err());
    }

    #[test]
    fn absent_integer_settings_resolve_to_the_owning_manifest_defaults() {
        let mut config = default_config();
        let defaults = config.clone();
        config
            .settings
            .get_mut("sandbox")
            .expect("sandbox")
            .retain(|id, _| id == "approval_policy");
        config
            .settings
            .get_mut("compaction")
            .expect("compaction")
            .retain(|id, _| id == "mode" || id == "at_tokens");
        let encoded = serde_json::to_string(&config).expect("old persisted settings");
        let restored = serde_json::from_str::<MiddlewareConfig>(&encoded)
            .expect("deserialize persisted settings");
        validate(&restored).expect("new scalar settings inherit owner defaults");
        for middleware in ["sandbox", "compaction"] {
            for (setting, value) in &defaults.settings[middleware] {
                if let FrontendSettingValue::Integer(value) = value {
                    assert_eq!(
                        integer_setting(&restored, middleware, setting).expect("effective value"),
                        *value
                    );
                }
            }
        }
        let mut unknown = restored;
        unknown.set_setting("sandbox", "unknown", Some(FrontendSettingValue::Integer(1)));
        assert!(validate(&unknown).is_err());
    }
}
