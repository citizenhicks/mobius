//! A capability's disclosed rows: its settings and the extensions it can load.

use super::capabilities::{extension_metadata, select_value};
use super::settings::{AgentDraft, AgentScope};
use crate::ui::Workspace;
use crate::ui::chat::ModelTarget;
use crate::ui::pages::harness::{Choice, option_menu};
use crate::ui::primitives::{Leading, Row, text_field, toggle, width};
use gpui::Entity;
use gpui::{AnyElement, Context, IntoElement as _, SharedString, Styled as _};
use gpui_component::Disableable as _;
use gpui_component::input::{InputState, NumberInput};
use mobius::protocol::{
    FrontendSetting, FrontendSettingKind, FrontendSettingValue, MiddlewareFeature,
};
use mobius_gateway::wire::{AgentComposition, ExtensionRecord};
use std::rc::Rc;
use std::sync::Arc;

impl Workspace {
    pub(super) fn setting_row(
        &self,
        surface: &'static str,
        scope: &Rc<AgentScope>,
        draft: &AgentDraft,
        feature: &MiddlewareFeature,
        setting: &FrontendSetting,
        cx: &Context<Self>,
    ) -> AnyElement {
        let row = Row::new(
            SharedString::from(format!("setting:{}:{}", feature.id, setting.id)),
            setting.label.as_str(),
        )
        .leading(Leading::Gutter)
        .subtitle(setting.description.as_str());
        let Some(config) = draft.mobius() else {
            return row.into_any_element();
        };
        let enabled = feature.required || config.middleware.enabled(&feature.id);
        if self.setting_route_options(setting).is_some() {
            let target = ModelTarget::Setting {
                scope: Rc::clone(scope),
                feature: feature.id.clone(), // rules-allow: the picker target owns the IDs it names
                setting: setting.id.clone(), // rules-allow: the picker target owns the IDs it names
            };
            return row
                .control(self.model_popover(surface, target, enabled, cx))
                .into_any_element();
        }
        match &setting.kind {
            FrontendSettingKind::Integer { .. } | FrontendSettingKind::Text { .. } => {
                let input = draft
                    .inputs
                    .get(&feature.id)
                    .and_then(|settings| settings.get(&setting.id));
                input_row(row, setting, input.map(|(input, _)| input), enabled)
            }
            FrontendSettingKind::Select {
                options,
                unset_label,
            } => {
                let current = select_value(config, &feature.id, &setting.id);
                let mut choices: Vec<Choice> = unset_label
                    .iter()
                    .map(|label| (None, SharedString::from(label)))
                    .collect();
                choices.extend(options.iter().map(|option| {
                    let label = self.option_label(
                        std::iter::once((&*option.value, &*option.label)),
                        &option.value,
                    );
                    (Some(Arc::clone(&option.value)), SharedString::from(label))
                }));
                // The menu's 'static listener owns one copy of the IDs.
                let ids = (
                    feature.id.clone(), // rules-allow: the menu's 'static listener owns this copy
                    setting.id.clone(), // rules-allow: the menu's 'static listener owns this copy
                );
                let scope = Rc::clone(scope);
                row.control(option_menu(
                    format!("select:{}:{}", feature.id, setting.id),
                    choices,
                    current,
                    move |this, value, cx| {
                        let value =
                            value.map(|value| FrontendSettingValue::String(value.to_string()));
                        let (feature, setting) = &ids;
                        this.update_draft(
                            &scope,
                            |draft| {
                                if let Some(config) = draft.mobius_mut() {
                                    config.middleware.set_setting(feature, setting, value);
                                }
                            },
                            cx,
                        );
                    },
                    cx,
                ))
            }
        }
        .into_any_element()
    }

    pub(super) fn extension_row(
        scope: &AgentScope,
        config: &AgentComposition,
        extension: &ExtensionRecord,
        enabled: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let active = config.extensions.contains(&*extension.id);
        let (scope, id) = (AgentScope::clone(scope), Arc::clone(&extension.id));
        Row::new(
            SharedString::from(format!("extension:{}", extension.id)),
            Arc::clone(&extension.name),
        )
        .leading(Leading::Gutter)
        .subtitle(extension_metadata(extension))
        .control(
            toggle(
                SharedString::from(format!("extension-switch:{}", extension.id)),
                &extension.name,
                active,
                cx,
            )
            .tooltip(Arc::clone(&extension.description))
            .disabled(!enabled && !active)
            .on_click(cx.listener(move |this, on: &bool, _, cx| {
                let (id, on) = (Arc::clone(&id), *on);
                this.update_draft(
                    &scope,
                    |draft| {
                        if let Some(config) = draft.mobius_mut() {
                            if on {
                                config.extensions.insert(id.to_string());
                            } else {
                                config.extensions.remove(&*id);
                            }
                        }
                    },
                    cx,
                );
            })),
        )
        .into_any_element()
    }
}

/// A typed setting's field: a stepper for a count, a text field for text.
pub(super) fn input_row(
    row: Row,
    setting: &FrontendSetting,
    input: Option<&Entity<InputState>>,
    enabled: bool,
) -> Row {
    match (input, &setting.kind) {
        (Some(input), FrontendSettingKind::Integer { .. }) => {
            row.control(NumberInput::new(input).w(width::STEPPER).disabled(!enabled))
        }
        (Some(input), FrontendSettingKind::Text { .. }) => {
            row.control(text_field(input).w(width::FIELD).disabled(!enabled))
        }
        _ => row,
    }
}
