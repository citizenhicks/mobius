//! Auto routing's settings: declared in `auto_routing.toml` with the middleware settings
//! schema, stored as the values a person changed, and read through their defaults.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use mobius::middleware::manifest::{MiddlewareSettingManifest, ModelCatalogs};
use mobius::protocol::{FrontendSettingValue, MiddlewareFeature, ModelChoice};
use serde::Deserialize;

use crate::wire::RouteTarget;
use crate::{Error, Result};

/// The settings ID that names a feature in error messages and catalogs.
pub(crate) const ID: &str = "auto_routing";

/// Stored values, keyed by setting ID; settings left at their default are absent.
pub(crate) type Values = BTreeMap<String, FrontendSettingValue>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    label: String,
    description: String,
    #[serde(deserialize_with = "mobius::middleware::manifest::deserialize_settings")]
    settings: Vec<MiddlewareSettingManifest>,
}

static MANIFEST: LazyLock<Manifest> =
    LazyLock::new(|| mobius::config::embedded(include_str!("auto_routing.toml")));

/// The settings schema, with select choices from the gateway's live catalogs.
pub(crate) fn feature(catalogs: ModelCatalogs<'_>) -> MiddlewareFeature {
    MiddlewareFeature {
        id: ID.into(),
        label: MANIFEST.label.clone(), // rules-allow: the catalog record owns its text
        description: MANIFEST.description.clone(), // rules-allow: the catalog record owns its text
        required: false,
        settings: MANIFEST
            .settings
            .iter()
            .map(|setting| setting.schema(catalogs))
            .collect(),
        requires: Vec::new(),
    }
}

/// Every setting's value: the stored one, else its default.
pub(crate) fn effective(stored: &Values) -> Values {
    MANIFEST
        .settings
        .iter()
        .filter_map(|setting| {
            let value = stored
                .get(setting.id())
                .cloned() // rules-allow: the effective map owns its values
                .or_else(|| setting.default_value())?;
            Some((setting.id().to_owned(), value))
        })
        .collect()
}

/// The configured model route, if the owner has turned Auto routing on.
pub(crate) fn model(stored: &Values) -> Option<&str> {
    match stored.get("model")? {
        FrontendSettingValue::String(value) => Some(value),
        FrontendSettingValue::Integer(_) => None,
    }
}

/// Only advertise Auto while its selected decision model is available.
pub(crate) fn targets(values: &Values, decisions: &[ModelChoice]) -> BTreeSet<RouteTarget> {
    if !model(values).is_some_and(|model| decisions.iter().any(|choice| *choice.route == *model)) {
        return BTreeSet::new();
    }
    RouteTarget::ALL
        .into_iter()
        .filter(|&target| values.contains_key(&question_id(target)))
        .collect()
}

/// Checks stored values against the schema; with `catalogs`, also that a chosen decision
/// model is still offered.
pub(crate) fn validate(stored: &Values, catalogs: Option<ModelCatalogs<'_>>) -> Result<()> {
    if let Some(unknown) = stored.keys().find(|id| {
        !MANIFEST
            .settings
            .iter()
            .any(|setting| setting.id() == id.as_str())
    }) {
        return Err(Error::Invalid(format!("unknown {ID} setting `{unknown}`")));
    }
    let values = effective(stored);
    for setting in &MANIFEST.settings {
        let value = values.get(setting.id());
        setting.validate(ID, value)?;
        if let Some(catalogs) = catalogs {
            setting.validate_choice(ID, value, catalogs)?;
        }
    }
    Ok(())
}

/// The typed reading of effective values that routing runs on.
pub(crate) struct Settings {
    pub(crate) questions: BTreeMap<RouteTarget, String>,
    pub(crate) min_confidence: f64,
    pub(crate) timeout: std::time::Duration,
    pub(crate) max_options: usize,
    pub(crate) recent_titles: usize,
}

impl Settings {
    pub(crate) fn read(stored: &Values) -> Result<Self> {
        let mut values = effective(stored);
        let questions = RouteTarget::ALL
            .into_iter()
            .filter_map(|target| {
                let FrontendSettingValue::String(question) = values.remove(&question_id(target))?
                else {
                    return None;
                };
                Some((target, question))
            })
            .collect();
        let integer = |id: &str| match values.get(id) {
            Some(FrontendSettingValue::Integer(value)) => u64::try_from(*value)
                .map_err(|error| Error::Internal(format!("{ID} setting `{id}`: {error}"))),
            _ => Err(Error::Internal(format!("{ID} setting `{id}` is missing"))),
        };
        let count = |id: &str| {
            integer(id).and_then(|value| {
                usize::try_from(value)
                    .map_err(|error| Error::Internal(format!("{ID} setting `{id}`: {error}")))
            })
        };
        Ok(Self {
            questions,
            #[expect(
                clippy::cast_precision_loss,
                reason = "the schema bounds the percentage to 0–100"
            )]
            min_confidence: integer("min_confidence_percent")? as f64 / 100.0,
            timeout: std::time::Duration::from_millis(integer("timeout_ms")?),
            max_options: count("max_options")?,
            recent_titles: count("recent_titles")?,
        })
    }
}

/// The setting that holds a target's question.
pub(crate) fn question_id(target: RouteTarget) -> String {
    format!("{}_question", target.key())
}

impl Default for crate::wire::AutoRoutingCatalog {
    /// The schema with no live catalogs and every setting at its default.
    fn default() -> Self {
        Self {
            feature: feature(ModelCatalogs::default()),
            values: effective(&Values::new()),
            targets: BTreeSet::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_has_a_question_per_target_and_its_defaults_validate() {
        validate(&Values::new(), None).expect("defaults validate");
        let settings = Settings::read(&Values::new()).expect("defaults read");
        assert!(model(&Values::new()).is_none());
        assert!(
            targets(&Values::new(), &[]).is_empty(),
            "no decision model, nothing on Auto"
        );
        assert_eq!(settings.questions.len(), RouteTarget::ALL.len());
    }

    #[test]
    fn unknown_and_out_of_range_values_are_rejected() {
        let unknown = Values::from([("nope".into(), FrontendSettingValue::Integer(1))]);
        assert!(validate(&unknown, None).is_err());
        let range = Values::from([(
            "min_confidence_percent".into(),
            FrontendSettingValue::Integer(101),
        )]);
        assert!(validate(&range, None).is_err());
        let model = Values::from([("model".into(), FrontendSettingValue::String("r".into()))]);
        assert!(
            targets(&effective(&model), &[]).is_empty(),
            "a removed route cannot advertise Auto"
        );
    }
}
