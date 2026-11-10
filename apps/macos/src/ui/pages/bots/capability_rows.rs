//! A capability's disclosed rows: its settings and the extensions it can load.

use super::capabilities::{extension_metadata, select_value};
use super::settings::{AgentDraft, AgentScope};
use super::*;
use crate::ui::chat::ModelTarget;
use crate::ui::pages::harness::{Choice, option_menu};
use gpui_component::input::NumberInput;
use mobius::protocol::{
    FrontendSetting, FrontendSettingKind, FrontendSettingValue, MiddlewareFeature,
};
use mobius_gateway::wire::{AgentComposition, ExtensionRecord};

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
            FrontendSettingKind::Integer { .. } => {
                match draft
                    .numbers
                    .get(&feature.id)
                    .and_then(|settings| settings.get(&setting.id))
                {
                    Some((input, _)) => {
                        row.control(NumberInput::new(input).w(width::STEPPER).disabled(!enabled))
                    }
                    None => row,
                }
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
