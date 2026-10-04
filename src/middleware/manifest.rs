//! Core-owned middleware configuration manifests.

use crate::protocol::{
    FrontendSetting, FrontendSettingKind, FrontendSettingOption, FrontendSettingValue,
    FrontendSymbol, FrontendTone, MiddlewareFeature,
};
use crate::protocol::{ModelCapability, ModelChoice};
use crate::{Error, Result};

macro_rules! middleware_manifest {
    ($(#[$attr:meta])* $id:literal, $definition:expr, required: $required:expr,
     capability: $capability:expr, settings: $settings:expr) => {
        $(#[$attr])*
        pub static MANIFEST: std::sync::LazyLock<$crate::middleware::manifest::MiddlewareManifest> =
            std::sync::LazyLock::new(|| $crate::middleware::manifest::MiddlewareManifest {
                id: $id,
                label: &$definition.manifest_label,
                description: &$definition.manifest_description,
                required: $required,
                default_enabled: $definition.default_enabled,
                required_model_capability: $capability,
                settings: $settings,
            });
    };
}
pub(crate) use middleware_manifest;

/// Static metadata and configurable policy exported by one middleware module.
#[derive(Debug, Clone, Copy)]
pub struct MiddlewareManifest {
    /// The identifier.
    pub id: &'static str,
    /// The label.
    pub label: &'static str,
    /// The description.
    pub description: &'static str,
    /// The required.
    pub required: bool,
    /// The default enabled.
    pub default_enabled: bool,
    /// Neutral model capability required by this middleware, if any.
    pub required_model_capability: Option<ModelCapability>,
    /// The settings.
    pub settings: &'static [MiddlewareSettingManifest],
}

impl MiddlewareManifest {
    /// Materializes frontend-safe settings using the gateway's current model routes.
    #[must_use]
    pub fn feature(self, models: &[ModelChoice]) -> MiddlewareFeature {
        MiddlewareFeature {
            id: self.id.into(),
            label: self.label.into(),
            description: self.description.into(),
            required: self.required,
            required_model_capability: self.required_model_capability,
            settings: self
                .settings
                .iter()
                .map(|setting| setting.schema(models))
                .collect(),
        }
    }
}

/// One validated setting declared by its owning middleware module.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MiddlewareSettingManifest {
    /// Selects the integer case.
    Integer {
        /// The identifier.
        id: String,
        /// The label.
        label: String,
        /// The description.
        description: String,
        /// The min.
        min: i64,
        /// The max.
        max: Option<i64>,
        /// The step.
        step: i64,
        /// The default.
        default: i64,
    },
    /// Selects the select case.
    Select {
        /// The identifier.
        id: String,
        /// The label.
        label: String,
        /// The description.
        description: String,
        /// The choices.
        choices: MiddlewareSettingChoices,
        /// The unset label.
        unset_label: Option<String>,
        /// The default.
        default: Option<String>,
        /// The max bytes.
        max_bytes: usize,
        /// The composer.
        composer: bool,
    },
}

impl MiddlewareSettingManifest {
    /// Returns the stable setting identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Integer { id, .. } | Self::Select { id, .. } => id,
        }
    }

    /// Returns whether this setting selects from the gateway's live model routes.
    #[must_use]
    pub fn uses_model_routes(&self) -> bool {
        matches!(
            &self,
            Self::Select {
                choices: MiddlewareSettingChoices::ModelRoutes,
                ..
            }
        )
    }

    /// Returns the value used by new gateway configurations.
    #[must_use]
    pub fn default_value(&self) -> Option<FrontendSettingValue> {
        match self {
            Self::Integer { default, .. } => Some(FrontendSettingValue::Integer(*default)),
            Self::Select { default, .. } => default
                .as_ref()
                .map(|value| FrontendSettingValue::String(value.clone())),
        }
    }

    /// Converts this declaration into the frontend-neutral setting schema.
    #[must_use]
    pub fn schema(&self, models: &[ModelChoice]) -> FrontendSetting {
        match self {
            Self::Integer {
                id,
                label,
                description,
                min,
                max,
                step,
                ..
            } => FrontendSetting {
                id: id.into(),
                label: label.into(),
                description: description.into(),
                composer: false,
                kind: FrontendSettingKind::Integer {
                    min: *min,
                    max: *max,
                    step: *step,
                },
            },
            Self::Select {
                id,
                label,
                description,
                choices,
                unset_label,
                composer,
                ..
            } => FrontendSetting {
                id: id.into(),
                label: label.into(),
                description: description.into(),
                composer: *composer,
                kind: FrontendSettingKind::Select {
                    options: choices.options(models),
                    unset_label: unset_label.clone(),
                },
            },
        }
    }

    /// Validates one configured value against the owning module's declaration.
    /// # Errors
    ///
    /// Returns an error if the supplied value is invalid.
    pub fn validate(&self, middleware: &str, value: Option<&FrontendSettingValue>) -> Result<()> {
        match (self, value) {
            (Self::Integer { min, max, .. }, Some(FrontendSettingValue::Integer(value)))
                if *value >= *min && max.is_none_or(|max| *value <= max) =>
            {
                Ok(())
            }
            (Self::Integer { id, .. }, Some(FrontendSettingValue::Integer(_))) => {
                Err(Error::Config(format!(
                    "middleware setting `{middleware}.{id}` is out of range"
                )))
            }
            (Self::Integer { id, .. }, Some(FrontendSettingValue::String(_))) => {
                Err(setting_type(middleware, id, "integer"))
            }
            (Self::Integer { id, .. }, None) => Err(Error::Config(format!(
                "missing middleware setting `{middleware}.{id}`"
            ))),
            (
                Self::Select {
                    choices, max_bytes, ..
                },
                Some(FrontendSettingValue::String(value)),
            ) if !value.trim().is_empty()
                && value.len() <= *max_bytes
                && match choices {
                    MiddlewareSettingChoices::Static(_) => choices.contains(&[], value),
                    MiddlewareSettingChoices::ModelRoutes => true,
                } =>
            {
                Ok(())
            }
            (Self::Select { id, max_bytes, .. }, Some(FrontendSettingValue::String(_))) => {
                Err(Error::Config(format!(
                    "middleware setting `{middleware}.{id}` must be an advertised choice of 1–{max_bytes} bytes"
                )))
            }
            (Self::Select { id, .. }, Some(FrontendSettingValue::Integer(_))) => {
                Err(setting_type(middleware, id, "string"))
            }
            (
                Self::Select {
                    unset_label: Some(_),
                    ..
                },
                None,
            ) => Ok(()),
            (Self::Select { id, .. }, None) => Err(Error::Config(format!(
                "missing middleware setting `{middleware}.{id}`"
            ))),
        }
    }

    /// Validates a dynamic select value against the gateway's live model catalog.
    /// # Errors
    ///
    /// Returns an error if the supplied value is invalid.
    pub fn validate_choice(
        &self,
        middleware: &str,
        value: Option<&FrontendSettingValue>,
        models: &[ModelChoice],
    ) -> Result<()> {
        let Self::Select {
            id,
            choices: MiddlewareSettingChoices::ModelRoutes,
            ..
        } = self
        else {
            return Ok(());
        };
        let Some(FrontendSettingValue::String(value)) = value else {
            return Ok(());
        };
        if MiddlewareSettingChoices::ModelRoutes.contains(models, value) {
            Ok(())
        } else {
            Err(Error::Config(format!(
                "middleware setting `{middleware}.{id}` is not an advertised choice"
            )))
        }
    }
}

/// How a select setting obtains its finite choices.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "source",
    content = "options",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MiddlewareSettingChoices {
    /// Selects the static case.
    Static(Vec<MiddlewareSettingChoice>),
    /// Selects the model routes case.
    ModelRoutes,
}

impl MiddlewareSettingChoices {
    fn options(&self, models: &[ModelChoice]) -> Vec<FrontendSettingOption> {
        match self {
            Self::Static(choices) => choices
                .iter()
                .map(|choice| FrontendSettingOption {
                    value: choice.value.clone(),
                    label: choice.label.clone(),
                    description: choice.description.clone(),
                    symbol: choice.symbol.as_deref().map(FrontendSymbol::from_wire),
                    tone: choice.tone,
                    disables: choice.disables.clone(),
                })
                .collect(),
            Self::ModelRoutes => models
                .iter()
                .map(|choice| FrontendSettingOption {
                    value: choice.route.clone(),
                    label: choice.reasoning_effort.as_ref().map_or_else(
                        || choice.group.clone(),
                        |effort| format!("{} · {effort}", choice.group),
                    ),
                    description: format!("{} · {}", choice.model, choice.route),
                    symbol: None,
                    tone: FrontendTone::Neutral,
                    disables: Vec::new(),
                })
                .collect(),
        }
    }

    fn contains(&self, models: &[ModelChoice], value: &str) -> bool {
        match self {
            Self::Static(choices) => choices.iter().any(|choice| choice.value == value),
            Self::ModelRoutes => models.iter().any(|choice| choice.route == value),
        }
    }
}

/// One static select choice declared by a middleware module.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiddlewareSettingChoice {
    /// The value.
    pub value: String,
    /// The label.
    pub label: String,
    /// The description.
    pub description: String,
    /// The symbol.
    pub symbol: Option<String>,
    /// The tone.
    pub tone: FrontendTone,
    /// Optional middleware excluded by this policy choice.
    pub disables: Vec<String>,
}

pub(crate) fn deserialize_settings<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<MiddlewareSettingManifest>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let settings =
        <Vec<MiddlewareSettingManifest> as serde::Deserialize>::deserialize(deserializer)?;
    let mut ids = std::collections::BTreeSet::new();
    for setting in &settings {
        if setting.id().is_empty() || !ids.insert(setting.id()) {
            return Err(serde::de::Error::custom(
                "setting IDs must be nonempty and unique",
            ));
        }
        if let MiddlewareSettingManifest::Integer { min, max, step, .. } = setting
            && (*step <= 0 || max.is_some_and(|max| max < *min))
        {
            return Err(serde::de::Error::custom("invalid setting range or step"));
        }
        if let MiddlewareSettingManifest::Select {
            choices: MiddlewareSettingChoices::Static(choices),
            max_bytes,
            ..
        } = setting
        {
            let mut values = std::collections::BTreeSet::new();
            if choices.iter().any(|choice| {
                choice.value.is_empty()
                    || choice.value.len() > *max_bytes
                    || !values.insert(&choice.value)
            }) {
                return Err(serde::de::Error::custom(
                    "select choices must be nonempty, unique, and within the byte limit",
                ));
            }
        }
        setting
            .validate("embedded", setting.default_value().as_ref())
            .map_err(serde::de::Error::custom)?;
    }
    Ok(settings)
}

pub(crate) fn integer_default(settings: &[MiddlewareSettingManifest], id: &str) -> i64 {
    match settings.iter().find(|setting| setting.id() == id) {
        Some(MiddlewareSettingManifest::Integer { default, .. }) => *default,
        _ => panic!("missing embedded integer setting {id}"),
    }
}

pub(crate) fn string_default<'a>(settings: &'a [MiddlewareSettingManifest], id: &str) -> &'a str {
    match settings.iter().find(|setting| setting.id() == id) {
        Some(MiddlewareSettingManifest::Select {
            default: Some(default),
            ..
        }) => default,
        _ => panic!("missing embedded select setting {id}"),
    }
}

#[cfg(test)]
pub(crate) fn static_choices<'a>(
    settings: &'a [MiddlewareSettingManifest],
    id: &str,
) -> &'a [MiddlewareSettingChoice] {
    match settings.iter().find(|setting| setting.id() == id) {
        Some(MiddlewareSettingManifest::Select {
            choices: MiddlewareSettingChoices::Static(choices),
            ..
        }) => choices,
        _ => panic!("missing embedded static choices {id}"),
    }
}

fn setting_type(middleware: &str, setting: &str, expected: &str) -> Error {
    Error::Config(format!(
        "middleware setting `{middleware}.{setting}` must be {expected}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting() -> MiddlewareSettingManifest {
        let choices = vec![MiddlewareSettingChoice {
            disables: vec![],
            value: "safe".into(),
            label: "Safe".into(),
            description: "Use the safe policy".into(),
            symbol: None,
            tone: FrontendTone::Neutral,
        }];

        MiddlewareSettingManifest::Select {
            id: "policy".into(),
            label: "Policy".into(),
            description: "Selection".into(),
            choices: MiddlewareSettingChoices::Static(choices),
            unset_label: None,
            default: Some("safe".into()),
            max_bytes: 16,
            composer: false,
        }
    }

    #[test]
    fn select_rejects_values_not_declared_by_the_module() {
        let error = setting()
            .validate(
                "example",
                Some(&FrontendSettingValue::String("unknown".into())),
            )
            .expect_err("unknown choice must fail");

        assert!(error.to_string().contains("advertised choice"));
    }
    #[test]
    fn embedded_settings_reject_invalid_ranges_defaults_and_duplicate_ids() {
        let valid = serde_json::json!({"type":"integer", "id":"count", "label":"Count", "description":"Count", "min":1, "max":10, "step":1, "default":4});
        assert!(deserialize_settings(&serde_json::json!([valid.clone()])).is_ok());
        assert!(deserialize_settings(&serde_json::json!([valid.clone(), valid.clone()])).is_err());
        for (key, value) in [("step", 0), ("max", 0), ("default", 11)] {
            let mut invalid = valid.clone();
            invalid[key] = value.into();
            assert!(deserialize_settings(&serde_json::json!([invalid])).is_err());
        }
        let mut invalid = valid;
        invalid["typo"] = true.into();
        assert!(deserialize_settings(&serde_json::json!([invalid])).is_err());
    }

    #[test]
    fn embedded_settings_reject_duplicate_choices() {
        let mut select = setting();
        let MiddlewareSettingManifest::Select {
            choices: MiddlewareSettingChoices::Static(choices),
            ..
        } = &mut select
        else {
            unreachable!()
        };
        choices.push(choices[0].clone());
        assert!(deserialize_settings(&serde_json::to_value([select]).expect("settings")).is_err());
    }
}
