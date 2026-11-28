//! Gateway-wide routing choices, saved as each control changes.

use crate::ui::Workspace;
use crate::ui::chat::ModelTarget;
use crate::ui::pages::bots::{SettingInputs, input_row, input_value, setting_inputs};
use crate::ui::pages::harness::{Choice, option_menu};
use crate::ui::primitives::{
    Group, Leading, MenuChoice, PageHeader, Row, Section, menu_select, page,
};
use gpui::Subscription;
use gpui::{
    AnyElement, ClickEvent, Context, IntoElement as _, ParentElement as _, SharedString, Window,
};
use gpui_component::input::InputEvent;
use mobius::protocol::{FrontendSettingKind, FrontendSettingValue};
use mobius_gateway::wire::{ClientMessage, ReadySection};
use std::sync::Arc;

/// Text controls for the gateway revision currently shown.
pub(in crate::ui::pages) struct RoutingInputs {
    revision: String,
    inputs: SettingInputs,
    _listeners: Vec<Subscription>,
}

impl Workspace {
    pub(in crate::ui) fn routing_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let page = page("gateway-routing", PageHeader::new("Auto routing"));
        let Some(gateway) = &self.client.state.gateway else {
            return page.into_any_element();
        };
        let catalog = &gateway.agent_catalog;
        let routing = &catalog.auto_routing;
        let revision = gateway
            .revisions
            .get(&ReadySection::Config)
            .map_or("", String::as_str);
        if self
            .config
            .routing
            .as_ref()
            .is_none_or(|draft| draft.revision != revision)
        {
            let inputs = setting_inputs(
                &routing.feature.settings,
                |id| routing.values.get(id),
                window,
                cx,
            );
            let listeners = inputs
                .values()
                .map(|(input, _)| {
                    cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                        if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                            this.save_auto_routing(None, cx);
                        }
                    })
                })
                .collect();
            self.config.routing = Some(RoutingInputs {
                revision: revision.into(),
                inputs,
                _listeners: listeners,
            });
        }
        let selected = &catalog.dictation.selection;
        let mut shown = SharedString::default();
        let items = catalog
            .dictation
            .choices
            .iter()
            .map(|choice| {
                let label = SharedString::from(choice.display());
                let checked = choice.target == *selected;
                if checked {
                    shown = SharedString::clone(&label);
                }
                let target = choice.target.clone(); // rules-allow: the menu owns the choice across gateway updates
                MenuChoice::new(
                    label,
                    checked,
                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.send(
                            ClientMessage::ConfigureDictation {
                                request_id: crate::gateway::request_id(),
                                target: target.clone(), // rules-allow: each selection sends an owned request
                            },
                            cx,
                        );
                    }),
                )
            })
            .collect();
        let picker = menu_select("dictation-route", shown, None, items, cx);
        page.child(
            Section::new("Agent routing")
                .child(Group::new().children(self.auto_routing_rows("gateway-routing", cx))),
        )
        .child(
            Section::new("Dictation").child(
                Group::new().child(
                    Row::new("default-dictation", "Dictation model")
                        .subtitle("Transcribes speech into your message draft.")
                        .control(picker),
                ),
            ),
        )
        .into_any_element()
    }

    /// One row per Auto routing setting, in the schema's order.
    fn auto_routing_rows(&self, surface: &'static str, cx: &Context<Self>) -> Vec<AnyElement> {
        let (Some(gateway), Some(draft)) = (
            self.client.state.gateway.as_ref(),
            self.config.routing.as_ref(),
        ) else {
            return Vec::new();
        };
        let routing = &gateway.agent_catalog.auto_routing;
        routing
            .feature
            .settings
            .iter()
            .map(|setting| {
                let row = Row::new(
                    SharedString::from(format!("auto-routing:{}", setting.id)),
                    setting.label.as_str(),
                )
                .leading(Leading::Gutter)
                .subtitle(setting.description.as_str());
                if self.setting_route_options(setting).is_some() {
                    let target = ModelTarget::AutoRouting {
                        setting: setting.id.clone(), // rules-allow: the picker target owns the ID it names
                    };
                    return row
                        .control(self.model_popover(surface, target, true, cx))
                        .into_any_element();
                }
                let FrontendSettingKind::Select {
                    options,
                    unset_label,
                } = &setting.kind
                else {
                    let input = draft.inputs.get(&setting.id).map(|(input, _)| input);
                    return input_row(row, setting, input, true).into_any_element();
                };
                let mut choices: Vec<Choice> = unset_label
                    .iter()
                    .map(|label| (None, SharedString::from(label)))
                    .collect();
                choices.extend(options.iter().map(|option| {
                    (
                        Some(Arc::clone(&option.value)),
                        SharedString::from(Arc::clone(&option.label)),
                    )
                }));
                let current = match routing.values.get(&setting.id) {
                    Some(FrontendSettingValue::String(value)) => Some(value.as_str()),
                    _ => None,
                };
                let id = setting.id.clone(); // rules-allow: the menu's 'static listener owns this copy
                row.control(option_menu(
                    format!("auto-routing-select:{}", setting.id),
                    choices,
                    current,
                    move |this, value, cx| {
                        let value =
                            value.map(|value| FrontendSettingValue::String(value.to_string()));
                        this.save_auto_routing(Some((&id, value)), cx);
                    },
                    cx,
                ))
                .into_any_element()
            })
            .collect()
    }

    /// Saves every Auto routing setting: the gateway's values, the defaults draft's typed
    /// fields, then `change`, where `None` resets a setting to its default.
    pub(in crate::ui) fn save_auto_routing(
        &mut self,
        change: Option<(&str, Option<FrontendSettingValue>)>,
        cx: &mut Context<Self>,
    ) {
        let Some(gateway) = &self.client.state.gateway else {
            return;
        };
        let mut values = gateway.agent_catalog.auto_routing.values.clone(); // rules-allow: the request owns the values it saves
        if let Some(draft) = self.config.routing.as_ref() {
            for (id, (input, initial)) in &draft.inputs {
                if let Some(value) = input_value(input, initial, cx) {
                    values.insert(id.clone(), value); // rules-allow: the request owns its keys
                }
            }
        }
        if let Some((id, value)) = change {
            match value {
                Some(value) => values.insert(id.to_owned(), value),
                None => values.remove(id),
            };
        }
        if values == gateway.agent_catalog.auto_routing.values {
            return;
        }
        let message = ClientMessage::ConfigureAutoRouting {
            request_id: crate::gateway::request_id(),
            values,
        };
        self.send(message, cx);
    }
}
