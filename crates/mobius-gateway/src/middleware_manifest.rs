//! Gateway composition registry for core-owned middleware manifests.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use mobius::middleware::manifest::{MiddlewareManifest, MiddlewareSettingManifest, ModelCatalogs};
use mobius::middleware::subagents::{SubagentCeilings, SubagentLimits};
use mobius::protocol::{FrontendSettingValue, MiddlewareFeature};

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
    Questions,
    Subagents,
    Messages,
    Voice,
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

impl MiddlewareRegistration {
    /// Returns settings bounded by the operator's ceilings.
    fn settings(&self, ceilings: SubagentCeilings) -> Cow<'static, [MiddlewareSettingManifest]> {
        match self.kind {
            BuiltinMiddleware::Subagents => Cow::Owned(ceilings.settings()),
            _ => Cow::Borrowed(self.manifest.settings),
        }
    }
}

pub(crate) static MIDDLEWARE: std::sync::LazyLock<[MiddlewareRegistration; 17]> =
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
                kind: BuiltinMiddleware::Questions,
                manifest: &mobius::middleware::questions::MANIFEST,
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
                kind: BuiltinMiddleware::Voice,
                manifest: &mobius::middleware::voice::MANIFEST,
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

pub(crate) fn features(
    catalogs: ModelCatalogs<'_>,
    ceilings: SubagentCeilings,
) -> Vec<MiddlewareFeature> {
    MIDDLEWARE
        .iter()
        .map(|entry| {
            let settings = entry.settings(ceilings);
            let mut feature = MiddlewareManifest {
                settings: &[],
                ..*entry.manifest
            }
            .feature(catalogs);
            feature.settings = settings
                .iter()
                .map(|setting| setting.schema(catalogs))
                .collect();
            feature
        })
        .collect()
}

pub(crate) fn default_config(ceilings: SubagentCeilings) -> MiddlewareConfig {
    let mut config = MiddlewareConfig {
        enabled: BTreeSet::new(),
        settings: BTreeMap::new(),
    };
    for entry in MIDDLEWARE.iter() {
        let manifest = entry.manifest;
        if !manifest.required {
            config.set_enabled(manifest.id, manifest.default_enabled);
        }
        for setting in entry.settings(ceilings).iter() {
            config.set_setting(manifest.id, setting.id(), setting.default_value());
        }
    }
    config
}

pub(crate) fn validate(config: &MiddlewareConfig, ceilings: SubagentCeilings) -> Result<()> {
    let features = features(ModelCatalogs::default(), ceilings);
    for id in config.entries() {
        if let Some(policy) = config.disabled_by(&features, id) {
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
        let settings = entry.settings(ceilings);
        for setting in settings.iter() {
            setting.validate(
                entry.manifest.id,
                config.setting(entry.manifest.id, setting.id()),
            )?;
        }
    }
    subagent_limits(config, ceilings)?;
    Ok(())
}

pub(crate) fn validate_choices(
    config: &MiddlewareConfig,
    catalogs: ModelCatalogs<'_>,
) -> Result<()> {
    for entry in MIDDLEWARE.iter() {
        for setting in entry.manifest.settings {
            setting.validate_choice(
                entry.manifest.id,
                config.setting(entry.manifest.id, setting.id()),
                catalogs,
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
            "missing integer middleware setting `{middleware}.{setting}`"
        ))),
    }
}

pub(crate) fn subagent_limits(
    config: &MiddlewareConfig,
    ceilings: SubagentCeilings,
) -> Result<SubagentLimits> {
    let value = |id| integer_setting(config, mobius::middleware::subagents::MANIFEST.id, id);
    let max_depth = u8::try_from(value("max_depth")?)
        .map_err(|_| Error::Config("subagent max depth must fit an unsigned byte".into()))?;
    let max_concurrency = usize::try_from(value("max_concurrency")?).map_err(|_| {
        Error::Config("subagent max concurrency must fit an unsigned integer".into())
    })?;
    let max_agents = usize::try_from(value("max_agents")?)
        .map_err(|_| Error::Config("subagent max agents must fit an unsigned integer".into()))?;
    Ok(ceilings.limits(max_depth, max_concurrency, max_agents)?)
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
    use mobius::protocol::{FrontendSettingKind, ModelChoice};

    use super::*;

    #[test]
    fn defaults_and_required_features_come_from_core_manifests() {
        let config = default_config(SubagentCeilings::default());
        let features = features(ModelCatalogs::default(), SubagentCeilings::default());

        assert!(validate(&config, SubagentCeilings::default()).is_ok());
        assert_eq!(config.setting("bots", "collaboration"), None);
        assert_eq!(
            config.setting("compaction", "allow_model_compaction"),
            Some(&FrontendSettingValue::String("off".into()))
        );
        assert_eq!(
            config.entries().collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "artifacts",
                "attachments",
                "compaction",
                "questions",
                "scratchpad",
                "subagents",
                "voice",
            ])
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
        assert!(validate(&invalid, SubagentCeilings::default()).is_err());
    }

    #[test]
    fn model_requested_compaction_keeps_tasks_independent() {
        let mut config = default_config(SubagentCeilings::default());
        config.set_setting(
            "compaction",
            "allow_model_compaction",
            Some(FrontendSettingValue::String("on".into())),
        );
        config.set_enabled("tasks", true);
        validate(&config, SubagentCeilings::default()).expect("independent settings");
        config.reconcile(&features(
            ModelCatalogs::default(),
            SubagentCeilings::default(),
        ));
        assert!(config.enabled("tasks"));
        assert!(validate(&config, SubagentCeilings::default()).is_ok());
        config.set_enabled("compaction", false);
        assert!(validate(&config, SubagentCeilings::default()).is_ok());
    }

    #[test]
    fn image_generation_follows_image_models_not_the_chat_model() {
        let mut config = default_config(SubagentCeilings::default());
        config.set_enabled("image_generation", true);
        let chat = ModelChoice {
            route: "provider::model::default".into(),
            group: "Provider".into(),
            model: "model".into(),
            reasoning_effort: None,
            variant_label: None,
            context_window: None,
            supports_image_input: true,
            supports_image_generation: false,
            supports_realtime_voice: false,
            tool_discovery: mobius::protocol::ToolDiscoveryMode::Native,
        };
        let image = ModelChoice {
            route: "openai::gpt-image-2.5-sunburst::default".into(),
            model: "gpt-image-2.5-sunburst".into(),
            supports_image_generation: true,
            ..chat.clone()
        };
        let models = [chat];
        let images = [image];
        let catalogs = ModelCatalogs {
            models: &models,
            images: &images,
            voices: &[],
        };
        assert!(validate_choices(&config, catalogs).is_ok());

        config.set_setting(
            "image_generation",
            "model",
            Some(FrontendSettingValue::String(
                images[0].route.as_str().into(),
            )),
        );
        assert!(validate_choices(&config, catalogs).is_ok());
        let options = &features(catalogs, SubagentCeilings::default())
            .into_iter()
            .find(|feature| feature.id == "image_generation")
            .expect("image feature")
            .settings[0];
        assert!(matches!(
            &options.kind,
            mobius::protocol::FrontendSettingKind::Select { options, .. }
                if options.len() == 1 && options[0].value == images[0].route
        ));

        config.set_setting(
            "image_generation",
            "model",
            Some(FrontendSettingValue::String(
                "openai::missing::default".into(),
            )),
        );
        assert!(validate_choices(&config, catalogs).is_err());
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
            variant_label: None,
            context_window: Some(200_000),
            supports_image_input: true,
            supports_image_generation: false,
            supports_realtime_voice: false,
            tool_discovery: mobius::protocol::ToolDiscoveryMode::Native,
        }];
        let subagents = features(
            ModelCatalogs {
                models: &models,
                images: &[],
                voices: &[],
            },
            SubagentCeilings::default(),
        )
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

        let mut config = default_config(SubagentCeilings::default());
        config.set_setting(
            "subagents",
            "model_route",
            Some(FrontendSettingValue::String(models[0].route.clone())),
        );
        assert!(validate(&config, SubagentCeilings::default()).is_ok());
        assert!(
            validate_choices(
                &config,
                ModelCatalogs {
                    models: &models,
                    images: &[],
                    voices: &[],
                },
            )
            .is_ok()
        );
        assert!(validate_choices(&config, ModelCatalogs::default(),).is_err());
    }

    #[test]
    fn sandbox_manifest_drives_generic_approval_settings() {
        let config = default_config(SubagentCeilings::default());
        let sandbox = features(ModelCatalogs::default(), SubagentCeilings::default())
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
        let mut config = default_config(SubagentCeilings::default());
        config.set_setting("tools", "extra", Some(FrontendSettingValue::Integer(1)));
        assert!(validate(&config, SubagentCeilings::default()).is_err());

        config.set_setting("tools", "extra", None);
        config.set_setting(
            "compaction",
            "at_tokens",
            Some(FrontendSettingValue::String("50000".into())),
        );
        assert!(validate(&config, SubagentCeilings::default()).is_err());
        let mut config = default_config(SubagentCeilings::default());
        config.set_setting(
            "compaction",
            "allow_model_compaction",
            Some(FrontendSettingValue::String("invalid".into())),
        );
        assert!(validate(&config, SubagentCeilings::default()).is_err());

        let mut config = default_config(SubagentCeilings::default());
        config.set_setting(
            "subagents",
            "max_agents",
            Some(FrontendSettingValue::Integer(2)),
        );
        assert!(validate(&config, SubagentCeilings::default()).is_err());
    }

    #[test]
    fn operator_subagent_ceilings_control_the_catalog_and_client_settings() {
        let ceilings = SubagentCeilings::new(32, 128, 512).expect("operator policy");
        let mut config = default_config(SubagentCeilings::default());
        for (id, value) in [
            ("max_depth", 24),
            ("max_concurrency", 96),
            ("max_agents", 300),
        ] {
            config.set_setting("subagents", id, Some(FrontendSettingValue::Integer(value)));
        }
        assert!(validate(&config, SubagentCeilings::default()).is_err());
        validate(&config, ceilings).expect("operator permits larger agent tree");
        let feature = features(ModelCatalogs::default(), ceilings)
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
        assert!(validate(&config, ceilings).is_err());
    }

    #[test]
    fn lower_subagent_ceilings_bound_explicit_values_and_reject_missing_ones() {
        let ceilings = SubagentCeilings::new(1, 2, 3).expect("small operator policy");
        let mut config = default_config(SubagentCeilings::default());
        config.settings.remove("subagents");
        assert!(validate(&config, ceilings).is_err());
        assert!(subagent_limits(&config, ceilings).is_err());
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
        for (id, value) in [("max_depth", 1), ("max_concurrency", 2), ("max_agents", 3)] {
            config.set_setting("subagents", id, Some(FrontendSettingValue::Integer(value)));
        }
        validate(&config, ceilings).expect("explicit values within operator bounds");
        assert_eq!(
            subagent_limits(&config, ceilings).expect("projected limits"),
            ceilings.limits(1, 2, 3).expect("limits")
        );
        config.set_setting(
            "subagents",
            "max_depth",
            Some(FrontendSettingValue::Integer(2)),
        );
        assert!(validate(&config, ceilings).is_err());
        assert!(subagent_limits(&config, ceilings).is_err());
    }

    #[test]
    fn persisted_configs_must_carry_every_integer_setting() {
        let mut config = default_config(SubagentCeilings::default());
        validate(&config, SubagentCeilings::default()).expect("complete defaults");
        config
            .settings
            .get_mut("compaction")
            .expect("compaction")
            .remove("reserve_tokens");
        assert!(
            validate(&config, SubagentCeilings::default())
                .expect_err("missing integer setting")
                .to_string()
                .contains("compaction.reserve_tokens")
        );
        assert!(integer_setting(&config, "compaction", "reserve_tokens").is_err());
    }
}
