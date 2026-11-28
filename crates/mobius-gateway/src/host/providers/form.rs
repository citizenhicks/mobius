//! Decode native settings drafts against the same schema that was advertised for this provider.
//! A test receives a validated in-memory snapshot. Saving uses normal provider publication.
use super::registration::validate_browser_endpoint_registration;
use crate::auth::ProviderAccess;
use crate::config::GatewayConfig;
use crate::config::{ConfiguredProvider, ProviderRegistration};
use crate::harness::mobius::provider_catalog::{model_groups, selected_base_url};
use crate::host::rejection::invalid_config;
use crate::host::{GatewayHost, Rejection};
use crate::wire::{
    ConfiguredModel, GroupEndpointOverride, ProviderConfig, ProviderField, ProviderForm,
    ProviderGroupDraft, ProviderModelDraft, ProviderModelGroup, ProviderModelKind,
};
use crate::wire::{ModelEndpointApi, ReadyPayload};
use mobius::backend::model::provider::{
    MediaModelPreset, ProviderDefinition, ReasoningPreset, provider,
};
use mobius::backend::model::{ImageAspect, ImageDefaults};
use mobius::protocol::{FrontendSettingKind, FrontendSettingValue};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

type Values = BTreeMap<String, FrontendSettingValue>;

impl GatewayHost {
    pub(crate) fn save_provider_form(
        &self,
        access: ProviderAccess,
        form: ProviderForm,
    ) -> impl Future<Output = Result<Arc<ReadyPayload>, Rejection>> + '_ {
        self.publish_provider_registration(access, false, false, move |current| {
            registration(current, form)
        })
    }

    pub(crate) async fn prepare_provider_form(
        &self,
        access: ProviderAccess,
        form: ProviderForm,
    ) -> Result<GatewayConfig, Rejection> {
        let state = self.state.lock().await;
        let current = state.config()?;
        validate_browser_endpoint_registration(&current, &form.config, access)?;
        let draft = registration(&current, form)?;
        // An existing instance borrows its key from the saved catalog. A new instance is
        // the only addition to the validated snapshot.
        let existing = current
            .configured_providers
            .get_key_value(&draft.selection.instance)
            .map(|(id, _)| id);
        let mut next = current
            .registering_configured(draft, false)
            .map_err(invalid_config)?
            .into_inner();
        next.configured_providers.retain(|id, _| {
            existing.map_or_else(
                || !current.configured_providers.contains_key(id),
                |selected| selected == id,
            )
        });
        drop(current);
        drop(state);
        Ok(next)
    }
}

fn registration(
    current: &GatewayConfig,
    form: ProviderForm,
) -> Result<ProviderRegistration, Rejection> {
    let definition = provider(&form.config.provider).map_err(invalid_config)?;
    let existing = current.configured_providers.get(&form.config.instance);
    if existing.is_some_and(|p| p.selection.provider != form.config.provider) {
        return Err(invalid_config(
            "a configured provider cannot change its service",
        ));
    }
    if existing.is_some_and(|p| {
        p.operator_managed
            && (form.config.base_url != p.selection.base_url
                || form.config.endpoint_auth != p.selection.endpoint_auth)
    }) {
        return Err(Rejection::new(
            "operator_required",
            "this provider connection is managed by the gateway operator",
        ));
    }
    let groups = model_groups(
        definition,
        selected_base_url(definition, &form.config),
        existing,
    );
    // A paired form owns the connection draft. Existing catalog policy stays authoritative
    // when an unrelated group is saved concurrently.
    let selection = if let Some(p) = existing {
        ProviderConfig {
            model: p.selection.model.clone(), // rules-allow: the staged candidate owns unchanged selection fields
            reasoning_effort: p.selection.reasoning_effort.as_deref().map(str::to_owned),
            service_tier: p.selection.service_tier.as_deref().map(str::to_owned),
            tool_discovery: p.selection.tool_discovery,
            web_search: p.selection.web_search,
            ..form.config
        }
    } else {
        form.config
    };
    let mut registration = ProviderRegistration {
        selection,
        label: form.label,
        tint: form.tint,
        models: vec![],
        group_endpoints: None,
        image_model_ids: None,
        image_defaults: None,
        voice_models: None,
        dictation_model_ids: None,
        decision_model_ids: None,
        operator_managed: false,
    };
    let mut endpoints = existing.map_or_else(Vec::new, |p| p.group_endpoints.clone()); // rules-allow: the candidate owns unchanged endpoint overrides
    for (kind, draft) in form.groups {
        let group = groups
            .iter()
            .find(|g| g.kind == kind)
            .ok_or_else(|| invalid_config("unknown model group"))?;
        if !group.supported {
            if !draft.models.is_empty() || !draft.values.is_empty() {
                return Err(invalid_config(format!("{} is unavailable", group.label)));
            }
            continue;
        }
        validate_fields(group, &draft, &group.fields, &draft.values)?;
        validate_models(group, &draft)?;
        // Managed settings were checked against their read-only schema above.
        // Keep inherited defaults in their original stored representation.
        if existing.is_some_and(|p| p.operator_managed) {
            continue;
        }
        if draft.values.contains_key("endpoint") || draft.values.contains_key("api") {
            let base = selected_base_url(definition, &registration.selection);
            let options = definition.group_endpoint_options(kind, base);
            update_endpoint(group, &draft, &options, &mut endpoints)?;
        }
        apply_group(definition, existing, group, draft, &mut registration)?;
    }
    registration.group_endpoints = Some(endpoints);
    Ok(registration)
}

fn validate_fields(
    group: &ProviderModelGroup,
    draft: &ProviderGroupDraft,
    fields: &[ProviderField],
    values: &Values,
) -> Result<(), Rejection> {
    for (id, value) in values {
        let field = fields
            .iter()
            .find(|f| f.setting.id == *id)
            .ok_or_else(|| invalid_config(format!("Unknown provider setting: {id}")))?;
        if !field.editable
            && field.value != *value
            && draft.display_override(group, field) != Some(value)
            && !fields.iter().any(|source| {
                string(values, &source.setting.id)
                    .and_then(|choice| source.on_change.get(choice))
                    .and_then(|updates| updates.get(id))
                    == Some(value)
            })
        {
            return Err(invalid_config(format!(
                "{} is read-only",
                field.setting.label
            )));
        }
        let valid = match (&field.setting.kind, value) {
            (FrontendSettingKind::Integer { min, max, step }, FrontendSettingValue::Integer(i)) => {
                i >= min
                    && max.is_none_or(|m| *i <= m)
                    && *step > 0
                    && (i128::from(*i) - i128::from(*min)) % i128::from(*step) == 0
            }
            (FrontendSettingKind::Select { options, .. }, FrontendSettingValue::String(s)) => {
                options.iter().any(|o| o.value.as_ref() == s)
            }
            (FrontendSettingKind::Text { max_bytes }, FrontendSettingValue::String(s)) => {
                s.len() <= *max_bytes && !s.chars().any(char::is_control)
            }
            _ => false,
        };
        if !valid {
            return Err(invalid_config(format!(
                "Invalid value for {}",
                field.setting.label
            )));
        }
    }
    Ok(())
}
fn validate_models(
    group: &ProviderModelGroup,
    draft: &ProviderGroupDraft,
) -> Result<(), Rejection> {
    if !group.editable
        && !group
            .models
            .iter()
            .map(|m| &m.id)
            .eq(draft.models.iter().map(|m| &m.id))
    {
        return Err(invalid_config(format!(
            "The {} catalog is read-only",
            group.label
        )));
    }
    for model in &draft.models {
        let fields = group
            .models
            .iter()
            .chain(&group.model_presets)
            .find(|m| m.id == model.id)
            .map_or(&group.new_model_fields, |m| &m.fields);
        validate_fields(group, draft, fields, &model.values)?;
    }
    Ok(())
}
fn string<'a>(values: &'a Values, id: &str) -> Option<&'a str> {
    match values.get(id) {
        Some(FrontendSettingValue::String(s)) => Some(s),
        _ => None,
    }
}
fn list(value: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    value
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty() && seen.insert(*id))
        .map(str::to_owned)
        .collect()
}
fn update_endpoint(
    group: &ProviderModelGroup,
    draft: &ProviderGroupDraft,
    options: &[ModelEndpointApi],
    endpoints: &mut Vec<GroupEndpointOverride>,
) -> Result<(), Rejection> {
    let current = endpoints.iter().find(|e| e.kind == group.kind);
    let Some(first) = options.first() else {
        return Ok(());
    };
    let id = string(&draft.values, "api")
        .or_else(|| current.map(|e| e.api.as_str()))
        .unwrap_or(&first.id);
    let api = options
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| invalid_config("Incompatible endpoint API"))?;
    let path = string(&draft.values, "endpoint")
        .or_else(|| current.filter(|e| e.api == id).map(|e| e.path.as_str()))
        .filter(|p| !p.is_empty())
        .unwrap_or(&api.path);
    let override_ =
        (api != first || path.strip_prefix('/').unwrap_or(path) != first.path).then(|| {
            GroupEndpointOverride {
                kind: group.kind,
                api: id.to_owned(),
                path: path.to_owned(),
            }
        });
    match (
        endpoints.iter().position(|e| e.kind == group.kind),
        override_,
    ) {
        (Some(i), Some(value)) => {
            if let Some(previous) = endpoints.get_mut(i) {
                *previous = value;
            }
        }
        (Some(i), None) => {
            endpoints.remove(i);
        }
        (None, value) => endpoints.extend(value),
    }
    Ok(())
}

fn language_model(
    draft: ProviderModelDraft,
    definition: &ProviderDefinition,
    existing: Option<&ConfiguredProvider>,
) -> ConfiguredModel {
    let original = existing
        .and_then(|p| p.models.iter().find(|m| m.id == draft.id))
        .or_else(|| definition.model(&draft.id));
    let reasoning = string(&draft.values, "reasoning").map(list).filter(|ids| {
        original.map_or(!ids.is_empty(), |m| {
            !m.reasoning.iter().map(|r| &r.id).eq(ids)
        })
    });
    let default_reasoning = reasoning.as_ref().and_then(|opts| {
        original
            .and_then(|m| m.default_reasoning.as_deref())
            .and_then(|id| opts.iter().find(|v| *v == id))
            .or_else(|| opts.first())
            .cloned() // rules-allow: the configured default owns the selected effort ID
    });
    let context_window = match draft.values.get("context_window") {
        Some(FrontendSettingValue::Integer(v)) => Some(*v),
        _ => None,
    };
    ConfiguredModel {
        id: draft.id,
        reasoning_efforts: reasoning,
        default_reasoning,
        context_window,
        ..Default::default()
    }
}

fn voice_model(draft: ProviderModelDraft, catalog: &[MediaModelPreset]) -> MediaModelPreset {
    let mut preset = catalog
        .iter()
        .find(|model| model.id == draft.id)
        .cloned() // rules-allow: the draft owns its copy of the saved preset
        .unwrap_or_else(|| MediaModelPreset {
            label: draft.id.clone(), // rules-allow: a new model uses its ID as its initial display label
            id: draft.id,
            description: String::new(),
            variants: Vec::new(),
        });
    if let Some(ids) = string(&draft.values, "voice_ids") {
        let mut previous = std::mem::take(&mut preset.variants);
        preset.variants = list(ids)
            .into_iter()
            .map(|id| {
                if let Some(index) = previous.iter().position(|v| v.id == id) {
                    previous.swap_remove(index)
                } else {
                    ReasoningPreset {
                        label: id.clone(), // rules-allow: a new voice uses its ID as its initial display label
                        id,
                        description: String::new(),
                    }
                }
            })
            .collect();
    }
    preset
}

fn apply_group(
    definition: &ProviderDefinition,
    existing: Option<&ConfiguredProvider>,
    group: &ProviderModelGroup,
    draft: ProviderGroupDraft,
    registration: &mut ProviderRegistration,
) -> Result<(), Rejection> {
    match group.kind {
        ProviderModelKind::Language => {
            if let Some(mode) = string(&draft.values, "web_search") {
                registration.selection.web_search = definition
                    .web_search()
                    .iter()
                    .copied()
                    .find(|s| s.id() == mode)
                    .ok_or_else(|| invalid_config("Unsupported hosted web search mode"))?;
            }
            registration.models = draft
                .models
                .into_iter()
                .map(|m| language_model(m, definition, existing))
                .collect();
            if !registration
                .models
                .iter()
                .any(|m| m.id == registration.selection.model)
            {
                registration.selection.model = registration
                    .models
                    .first()
                    .map_or_else(String::new, |m| m.id.clone()); // rules-allow: the selection and catalog own their IDs
                registration.selection.reasoning_effort = None;
            }
            if let Some(model) = registration
                .models
                .iter()
                .find(|m| m.id == registration.selection.model)
                && let Some(efforts) = &model.reasoning_efforts
                && !registration
                    .selection
                    .reasoning_effort
                    .as_ref()
                    .is_some_and(|v| efforts.contains(v))
            {
                registration.selection.reasoning_effort =
                    model.default_reasoning.as_deref().map(str::to_owned);
            }
            if !group.editable
                && existing.is_none_or(|p| p.models.is_empty())
                && registration.models.iter().all(|m| {
                    m.context_window.is_none_or(|w| {
                        definition
                            .model(&m.id)
                            .is_some_and(|p| p.context_window == w)
                    })
                })
            {
                registration.models.clear();
            }
        }
        ProviderModelKind::Dictation => {
            registration.dictation_model_ids =
                Some(draft.models.into_iter().map(|m| m.id).collect());
        }
        ProviderModelKind::Decision => {
            registration.decision_model_ids =
                Some(draft.models.into_iter().map(|m| m.id).collect());
        }
        ProviderModelKind::Voice => {
            if group.editable {
                let old = existing
                    .and_then(|v| v.voice_models.as_deref())
                    .unwrap_or_else(|| {
                        definition.media_presets_at(
                            mobius::protocol::ModelCapability::RealtimeVoice,
                            registration.selection.base_url.as_deref(),
                        )
                    });
                registration.voice_models = Some(
                    draft
                        .models
                        .into_iter()
                        .map(|model| voice_model(model, old))
                        .collect(),
                );
            }
        }
        ProviderModelKind::Image => {
            let mut defaults = BTreeMap::new();
            for m in &draft.models {
                let mut value = existing
                    .and_then(|p| p.image_defaults.get(&m.id))
                    .cloned() // rules-allow: the draft starts with stored defaults before applying changes
                    .unwrap_or_default();
                if let Some(size) = string(&m.values, "image_size") {
                    value.size = (!size.is_empty()).then(|| size.to_owned());
                }
                if let Some(quality) = string(&m.values, "image_quality") {
                    value.quality = (!quality.is_empty()).then(|| quality.to_owned());
                }
                if let Some(aspect) = string(&m.values, "image_aspect") {
                    value.aspect = match aspect {
                        "square" => ImageAspect::Square,
                        "landscape" => ImageAspect::Landscape,
                        "portrait" => ImageAspect::Portrait,
                        _ => return Err(invalid_config("Invalid image aspect ratio")),
                    };
                }
                if value != ImageDefaults::default() {
                    defaults.insert(m.id.clone(), value); // rules-allow: stored settings own the model ID
                }
            }
            registration.image_defaults = Some(defaults);
            if group.editable {
                registration.image_model_ids =
                    Some(draft.models.into_iter().map(|m| m.id).collect());
            }
        }
        _ => return Err(invalid_config("Unsupported model group")),
    }
    Ok(())
}
