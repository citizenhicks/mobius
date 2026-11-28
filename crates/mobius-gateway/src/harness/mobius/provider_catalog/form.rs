//! Settings rows and defaults visible to paired clients.
use super::catalog::language_discovery;
use crate::config::ConfiguredProvider;
use crate::wire::{
    ProviderDisplayCase, ProviderField, ProviderModelFields, ProviderModelGroup, ProviderModelKind,
};
use mobius::backend::model::dictation::DictationMode;
use mobius::backend::model::provider::{BaseUrl, MediaModelPreset, ProviderDefinition};
use mobius::protocol::ModelCapability;
use mobius::protocol::{
    FrontendSetting, FrontendSettingKind, FrontendSettingOption, FrontendSettingValue,
    FrontendTone, ToolDiscoveryMode,
};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

#[derive(serde::Deserialize)]
struct Description {
    label: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    choices: BTreeMap<String, String>,
}
#[derive(serde::Deserialize)]
struct Presentation {
    test_description: Arc<str>,
    general: crate::wire::ProviderSectionHeader,
    fields: BTreeMap<String, Description>,
}
static FORM: LazyLock<Presentation> =
    LazyLock::new(|| mobius::config::embedded(include_str!("form.toml")));

pub(super) fn header() -> crate::wire::ProviderSectionHeader {
    FORM.general.clone() // rules-allow: each wire provider owns its header
}
pub(crate) fn dictation_mode_label(mode: Option<DictationMode>) -> &'static str {
    let key = if mode == Some(DictationMode::Live) {
        "live"
    } else {
        "after_stop"
    };
    choice_label("dictation_mode", key)
}

fn choice_label<'a>(id: &str, option: &'a str) -> &'a str {
    FORM.fields
        .get(id)
        .and_then(|field| field.choices.get(option))
        .map_or(option, String::as_str)
}

fn field(
    id: &str,
    kind: FrontendSettingKind,
    value: FrontendSettingValue,
    editable: bool,
) -> ProviderField {
    ProviderField {
        display_cases: Vec::new(),
        on_change: BTreeMap::new(),
        setting: FrontendSetting {
            id: id.into(),
            label: FORM.fields.get(id).map_or(id, |f| f.label.as_str()).into(),
            description: FORM.fields.get(id).map_or("", |f| &f.description).into(),
            composer: false,
            kind,
        },
        value,
        editable,
    }
}
fn text(id: &str, value: &str, editable: bool) -> ProviderField {
    field(
        id,
        FrontendSettingKind::Text { max_bytes: 4096 },
        FrontendSettingValue::String(value.into()),
        editable,
    )
}
fn pick<'a>(
    id: &str,
    value: &str,
    choices: impl IntoIterator<Item = (&'a str, &'a str)>,
    editable: bool,
) -> ProviderField {
    let options: Vec<_> = choices
        .into_iter()
        .map(|(v, l)| FrontendSettingOption {
            value: v.into(),
            label: l.into(),
            description: "".into(),
            symbol: None,
            tone: FrontendTone::Neutral,
            disables: Default::default(),
        })
        .collect();
    let editable = editable && options.len() > 1;
    field(
        id,
        FrontendSettingKind::Select {
            options,
            unset_label: None,
        },
        FrontendSettingValue::String(value.into()),
        editable,
    )
}
fn row(id: &str, label: &str, fields: Vec<ProviderField>) -> ProviderModelFields {
    ProviderModelFields {
        label: (label != id).then(|| label.to_owned()),
        id: id.into(),
        fields,
    }
}
fn window(value: i64, editable: bool) -> ProviderField {
    field(
        "context_window",
        FrontendSettingKind::Integer {
            min: 1,
            max: None,
            step: 1,
        },
        FrontendSettingValue::Integer(value),
        editable,
    )
}
fn language(
    m: &crate::wire::ProviderModel,
    editable: bool,
    context_editable: bool,
) -> ProviderModelFields {
    let efforts = m
        .reasoning
        .iter()
        .map(|r| r.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    row(
        &m.id,
        &m.label,
        vec![
            window(m.context_window, context_editable),
            text("reasoning", &efforts, editable),
        ],
    )
}
fn voice(m: &MediaModelPreset, editable: bool) -> ProviderModelFields {
    row(
        &m.id,
        &m.label,
        vec![text(
            "voice_ids",
            &m.variants
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            editable,
        )],
    )
}

fn image_fields(
    definition: &ProviderDefinition,
    defaults: &mobius::backend::model::ImageDefaults,
    variants: &[mobius::backend::model::provider::ReasoningPreset],
    editable: bool,
) -> Vec<ProviderField> {
    use mobius::backend::model::{ImageAspect, ImageDefaults};
    let default_size = definition.default_image_resolution().map_or_else(
        || choice_label("image_size", "").to_owned(),
        |size| choice_label("image_size", "pixels").replace("{size}", size),
    );
    let sizes =
        std::iter::once(("", default_size.as_str())).chain(ImageDefaults::sizes().map(|v| (v, v)));
    let ratio = match defaults.aspect {
        ImageAspect::Square => "square",
        ImageAspect::Landscape => "landscape",
        ImageAspect::Portrait => "portrait",
    };
    let quality = if variants.is_empty() {
        field(
            "image_quality",
            FrontendSettingKind::Text {
                max_bytes: ImageDefaults::MAX_QUALITY_BYTES,
            },
            FrontendSettingValue::String(defaults.quality.as_deref().unwrap_or("").into()),
            editable,
        )
    } else {
        let mut quality = pick(
            "image_quality",
            defaults
                .quality
                .as_deref()
                .or_else(|| variants.first().map(|p| p.id.as_str()))
                .unwrap_or(""),
            variants.iter().map(|v| (v.id.as_str(), v.label.as_str())),
            editable,
        );
        quality.setting.description.clear();
        quality
    };
    vec![
        pick(
            "image_size",
            defaults.size.as_deref().unwrap_or(""),
            sizes,
            editable,
        ),
        pick(
            "image_aspect",
            ratio,
            ["square", "landscape", "portrait"].map(|v| (v, choice_label("image_aspect", v))),
            editable,
        ),
        quality,
    ]
}

fn discovery_value(mode: ToolDiscoveryMode) -> &'static str {
    let key = match mode {
        ToolDiscoveryMode::Native => "native",
        ToolDiscoveryMode::Rebuild => "rebuild",
    };
    choice_label("tool_discovery", key)
}

pub(super) fn populate(
    group: &mut ProviderModelGroup,
    d: &ProviderDefinition,
    base: &BaseUrl,
    c: Option<&ConfiguredProvider>,
    apis: &[crate::wire::ModelEndpointApi],
) {
    if !group.supported {
        return;
    }
    group.test_description = Some(Arc::clone(&FORM.test_description));
    let can_edit = !c.is_some_and(|v| v.operator_managed);
    let models_editable = can_edit && group.editable;
    group.editable = models_editable;
    let saved = c.and_then(|v| v.group_endpoints.iter().find(|e| e.kind == group.kind));
    if let Some(api) = saved
        .and_then(|e| apis.iter().find(|a| a.id == e.api))
        .or_else(|| apis.first())
    {
        if apis.len() > 1 {
            let mut format = pick(
                "api",
                &api.id,
                apis.iter().map(|a| (a.id.as_str(), a.label.as_str())),
                can_edit,
            );
            format.on_change = apis
                .iter()
                .map(|a| {
                    let mut updates = BTreeMap::from([(
                        "endpoint".into(),
                        FrontendSettingValue::String(a.path.clone()), // rules-allow: wire form owns dependent endpoint path
                    )]);
                    if group.kind == ProviderModelKind::Language {
                        let mode = language_discovery(d, c, Some(&a.id));
                        updates.insert(
                            "tool_discovery".into(),
                            FrontendSettingValue::String(discovery_value(mode).into()),
                        );
                    }
                    (a.id.clone(), updates) // rules-allow: wire form owns option ID
                })
                .collect();
            group.fields.push(format);
        }
        group.fields.push(text(
            "endpoint",
            saved.map_or(&api.path, |e| &e.path),
            can_edit,
        ));
    }
    match group.kind {
        ProviderModelKind::Language => {
            let mode = language_discovery(d, c, saved.map(|e| e.api.as_str()));
            group
                .fields
                .push(text("tool_discovery", discovery_value(mode), false));
            let search = c.map_or_else(Default::default, |v| v.selection.web_search);
            group.fields.push(pick(
                "web_search",
                search.id(),
                d.web_search().iter().map(|s| (s.id(), s.label())),
                can_edit,
            ));
            let models = c.map_or_else(
                || d.models(),
                |provider| {
                    if provider.selection.model.is_empty() {
                        &[]
                    } else {
                        super::models(d, provider)
                    }
                },
            );
            group.models = models
                .iter()
                .map(|m| language(m, models_editable, can_edit))
                .collect();
            group.new_model_fields = vec![
                window(super::model_context_window(d, ""), models_editable),
                text("reasoning", "", models_editable),
            ];
        }
        ProviderModelKind::Dictation => {
            let operation = saved.and_then(|e| {
                d.with_group_endpoint(group.kind, base, &e.api, &e.path)
                    .ok()
            });
            let mode_field = |id: &str| {
                let label = |origin: &BaseUrl| dictation_mode_label(d.dictation_mode(id, origin));
                let mut field = text(
                    "dictation_mode",
                    label(operation.as_ref().unwrap_or(base)),
                    false,
                );
                if apis.len() > 1
                    && let Some(first) = apis.first()
                {
                    // The first adapter at its bundled suffix inherits each model's manifest mode.
                    // An explicitly bound operation chooses the mode for the whole group.
                    for path in [first.path.as_str(), &format!("/{}", first.path), ""] {
                        field.display_cases.push(ProviderDisplayCase {
                            when: BTreeMap::from([
                                ("api".into(), first.id.clone()), // rules-allow: the condition owns the advertised choice ID
                                ("endpoint".into(), path.to_owned()),
                            ]),
                            value: FrontendSettingValue::String(label(base).into()),
                        });
                    }
                    for api in apis {
                        field.display_cases.push(ProviderDisplayCase {
                            when: BTreeMap::from([("api".into(), api.id.clone())]), // rules-allow: the condition owns the advertised choice ID
                            value: FrontendSettingValue::String(
                                dictation_mode_label(DictationMode::for_endpoint_api(&api.id))
                                    .into(),
                            ),
                        });
                    }
                }
                vec![field]
            };
            group.new_model_fields = mode_field("");
            group.model_presets = d
                .live_dictation_models()
                .iter()
                .map(|id| row(id, id, mode_field(id)))
                .collect();
            if let Some(c) = c {
                group.models = c
                    .dictation_model_ids
                    .iter()
                    .map(|id| row(id, id, mode_field(id)))
                    .collect();
            }
        }
        ProviderModelKind::Decision => {
            if let Some(c) = c {
                group.models = c
                    .decision_model_ids
                    .iter()
                    .map(|id| row(id, id, vec![]))
                    .collect();
            }
        }
        ProviderModelKind::Voice => {
            group.models = c
                .and_then(|v| v.voice_models.as_deref())
                .unwrap_or_else(|| {
                    d.media_presets_at(ModelCapability::RealtimeVoice, Some(base.as_str()))
                })
                .iter()
                .map(|m| voice(m, models_editable))
                .collect();
            group.new_model_fields = vec![text("voice_ids", "", models_editable)];
        }
        ProviderModelKind::Image => {
            let presets = c
                .and_then(|v| v.image_models.as_deref())
                .unwrap_or_else(|| {
                    d.media_presets_at(ModelCapability::ImageGeneration, Some(base.as_str()))
                });
            let fallback = Default::default();
            let image = |id: &str, label: &str, variants: &[_]| {
                let defaults = c
                    .and_then(|v| v.image_defaults.get(id))
                    .unwrap_or(&fallback);
                row(id, label, image_fields(d, defaults, variants, can_edit))
            };
            group.models = if let Some(c) = c.filter(|v| !v.image_model_ids.is_empty()) {
                c.image_model_ids
                    .iter()
                    .map(|id| image(id, id, &[]))
                    .collect()
            } else {
                presets
                    .iter()
                    .map(|m| image(&m.id, &m.label, &m.variants))
                    .collect()
            };
            group.new_model_fields = image_fields(d, &fallback, &[], can_edit);
        }
        _ => {}
    }
}
