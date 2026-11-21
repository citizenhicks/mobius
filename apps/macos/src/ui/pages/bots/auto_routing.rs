//! Auto routing's gateway-wide settings at the top of the agent defaults' Model section,
//! rendered from their schema like a capability's; each change saves at once.

use super::capability_rows::input_row;
use super::settings::{AgentScope, input_value};
use crate::ui::Workspace;
use crate::ui::chat::ModelTarget;
use crate::ui::pages::harness::{Choice, option_menu};
use crate::ui::primitives::{Leading, Row};
use gpui::{AnyElement, Context, IntoElement as _, SharedString};
use mobius::protocol::{FrontendSettingKind, FrontendSettingValue};
use mobius_gateway::wire::ClientMessage;
use std::sync::Arc;

impl Workspace {
    /// One row per Auto routing setting, in the schema's order.
    pub(super) fn auto_routing_rows(
        &self,
        surface: &'static str,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let (Some(gateway), Some(draft)) = (
            self.client.state.gateway.as_ref(),
            self.bots.drafts.get(&AgentScope::Defaults),
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
                    let input = draft.routing.get(&setting.id).map(|(input, _)| input);
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
        if let Some(draft) = self.bots.drafts.get(&AgentScope::Defaults) {
            for (id, (input, initial)) in &draft.routing {
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
