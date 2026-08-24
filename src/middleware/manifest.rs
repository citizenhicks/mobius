//! Core-owned middleware configuration manifests.

use crate::protocol::ModelChoice;
use crate::protocol::{
    FrontendSetting, FrontendSettingKind, FrontendSettingOption, FrontendSettingValue,
    FrontendSymbol, FrontendTone, MiddlewareFeature,
};
use crate::{Error, Result};

/// Static metadata and configurable policy exported by one middleware module.
#[derive(Debug, Clone, Copy)]
pub struct MiddlewareManifest {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub required: bool,
    pub default_enabled: bool,
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
            settings: self
                .settings
                .iter()
                .map(|setting| setting.schema(models))
                .collect(),
        }
    }
}

/// One validated setting declared by its owning middleware module.
#[derive(Debug, Clone, Copy)]
pub enum MiddlewareSettingManifest {
    Integer {
        id: &'static str,
        label: &'static str,
        description: &'static str,
        min: i64,
        max: Option<i64>,
        step: i64,
        default: i64,
    },
    Select {
        id: &'static str,
        label: &'static str,
        description: &'static str,
        choices: MiddlewareSettingChoices,
        unset_label: Option<&'static str>,
        default: Option<&'static str>,
        max_bytes: usize,
        composer: bool,
    },
}

impl MiddlewareSettingManifest {
    /// Returns the stable setting identifier.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Integer { id, .. } | Self::Select { id, .. } => id,
        }
    }

    /// Returns whether this setting selects from the gateway's live model routes.
    #[must_use]
    pub const fn uses_model_routes(self) -> bool {
        matches!(
            self,
            Self::Select {
                choices: MiddlewareSettingChoices::ModelRoutes,
                ..
            }
        )
    }

    /// Returns the value used by new gateway configurations.
    #[must_use]
    pub fn default_value(self) -> Option<FrontendSettingValue> {
        match self {
            Self::Integer { default, .. } => Some(FrontendSettingValue::Integer(default)),
            Self::Select { default, .. } => {
                default.map(|value| FrontendSettingValue::String(value.into()))
            }
        }
    }

    /// Converts this declaration into the frontend-neutral setting schema.
    #[must_use]
    pub fn schema(self, models: &[ModelChoice]) -> FrontendSetting {
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
                kind: FrontendSettingKind::Integer { min, max, step },
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
                composer,
                kind: FrontendSettingKind::Select {
                    options: choices.options(models),
                    unset_label: unset_label.map(str::to_string),
                },
            },
        }
    }

    /// Validates one configured value against the owning module's declaration.
    pub fn validate(self, middleware: &str, value: Option<&FrontendSettingValue>) -> Result<()> {
        match (self, value) {
            (Self::Integer { min, max, .. }, Some(FrontendSettingValue::Integer(value)))
                if *value >= min && max.is_none_or(|max| *value <= max) =>
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
                && value.len() <= max_bytes
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
    pub fn validate_choice(
        self,
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
#[derive(Debug, Clone, Copy)]
pub enum MiddlewareSettingChoices {
    Static(&'static [MiddlewareSettingChoice]),
    ModelRoutes,
}

impl MiddlewareSettingChoices {
    fn options(self, models: &[ModelChoice]) -> Vec<FrontendSettingOption> {
        match self {
            Self::Static(choices) => choices
                .iter()
                .map(|choice| FrontendSettingOption {
                    value: choice.value.into(),
                    label: choice.label.into(),
                    description: choice.description.into(),
                    symbol: choice.symbol.map(FrontendSymbol::from_wire),
                    tone: choice.tone,
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
                })
                .collect(),
        }
    }

    fn contains(self, models: &[ModelChoice], value: &str) -> bool {
        match self {
            Self::Static(choices) => choices.iter().any(|choice| choice.value == value),
            Self::ModelRoutes => models.iter().any(|choice| choice.route == value),
        }
    }
}

/// One static select choice declared by a middleware module.
#[derive(Debug, Clone, Copy)]
pub struct MiddlewareSettingChoice {
    pub value: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub symbol: Option<&'static str>,
    pub tone: FrontendTone,
}

fn setting_type(middleware: &str, setting: &str, expected: &str) -> Error {
    Error::Config(format!(
        "middleware setting `{middleware}.{setting}` must be {expected}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHOICES: &[MiddlewareSettingChoice] = &[MiddlewareSettingChoice {
        value: "safe",
        label: "Safe",
        description: "Use the safe policy",
        symbol: None,
        tone: FrontendTone::Neutral,
    }];

    const SETTING: MiddlewareSettingManifest = MiddlewareSettingManifest::Select {
        id: "policy",
        label: "Policy",
        description: "Selection",
        choices: MiddlewareSettingChoices::Static(CHOICES),
        unset_label: None,
        default: Some("safe"),
        max_bytes: 16,
        composer: false,
    };

    #[test]
    fn select_rejects_values_not_declared_by_the_module() {
        let error = SETTING
            .validate(
                "example",
                Some(&FrontendSettingValue::String("unknown".into())),
            )
            .expect_err("unknown choice must fail");

        assert!(error.to_string().contains("advertised choice"));
    }
}
