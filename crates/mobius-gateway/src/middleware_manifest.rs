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
    IntelligentUi,
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

pub(crate) static MIDDLEWARE: std::sync::LazyLock<[MiddlewareRegistration; 18]> =
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
                kind: BuiltinMiddleware::IntelligentUi,
                manifest: &mobius::middleware::intelligent_ui::MANIFEST,
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

/// Gives every declared setting the config leaves unset its declared default.
pub(crate) fn fill_defaults(config: &mut MiddlewareConfig) {
    for entry in MIDDLEWARE.iter() {
        for setting in entry.settings(SubagentCeilings::default()).iter() {
            if config.setting(entry.manifest.id, setting.id()).is_none() {
                config.set_setting(entry.manifest.id, setting.id(), setting.default_value());
            }
        }
    }
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
    let max_depth = u8::try_from(value("max_depth")?).map_err(|error| {
        Error::Config(format!(
            "subagent max depth must fit an unsigned byte: {error}"
        ))
    })?;
    let max_concurrency = usize::try_from(value("max_concurrency")?).map_err(|error| {
        Error::Config(format!(
            "subagent max concurrency must fit an unsigned integer: {error}"
        ))
    })?;
    let max_agents = usize::try_from(value("max_agents")?).map_err(|error| {
        Error::Config(format!(
            "subagent max agents must fit an unsigned integer: {error}"
        ))
    })?;
    Ok(ceilings.limits(max_depth, max_concurrency, max_agents)?)
}

pub(crate) fn count_setting(
    config: &MiddlewareConfig,
    middleware: &str,
    setting: &str,
) -> Result<std::num::NonZeroUsize> {
    usize::try_from(integer_setting(config, middleware, setting)?)
        .ok()
        .and_then(std::num::NonZeroUsize::new)
        .ok_or_else(|| {
            Error::Config(format!(
                "middleware setting `{middleware}.{setting}` must be a positive count"
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
    use super::*;

    #[test]
    fn loading_fills_a_newly_declared_setting_with_its_default() {
        let mut stored =
            serde_json::to_value(default_config(SubagentCeilings::default())).expect("config JSON");
        stored["settings"]["compaction"]
            .as_object_mut()
            .expect("compaction settings")
            .remove("checkpoint_percent");
        let config: MiddlewareConfig = serde_json::from_value(stored).expect("stored config");
        assert_eq!(
            config.setting("compaction", "checkpoint_percent"),
            Some(&FrontendSettingValue::Integer(20))
        );
        assert!(validate(&config, SubagentCeilings::default()).is_ok());
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
}
