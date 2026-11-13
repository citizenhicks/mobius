//! The Capabilities section: one switch per capability, with its settings and extensions
//! disclosed beneath it.

use super::settings::{AgentDraft, AgentScope};
use crate::ui::Workspace;
use crate::ui::primitives::{Group, Row, chevron, inline_button, palette, toggle};
use gpui::{
    AnyElement, App, Context, Div, IntoElement as _, ParentElement as _, SharedString, div,
};
use gpui_component::Disableable as _;
use mobius::protocol::{
    FrontendSetting, FrontendSettingKind, FrontendSettingRequirement, FrontendSettingValue,
    MiddlewareFeature,
};
use mobius_gateway::wire::{AgentComposition, ExtensionKind, ExtensionRecord, ReadyPayload};
use std::borrow::Cow;
use std::rc::Rc;

/// Why a capability cannot be switched on, if it cannot.
fn unavailable(
    gateway: &ReadyPayload,
    config: &AgentComposition,
    feature: &MiddlewareFeature,
) -> Option<String> {
    config
        .middleware
        .disabled_by(&gateway.agent_catalog.middleware_features, &feature.id)
        .map(|label| format!("Unavailable while {label} is selected."))
}

/// The first setting another capability must have for `feature` to work, when `config`
/// does not have it; advisory only.
fn unmet_requirement<'a>(
    config: &AgentComposition,
    feature: &'a MiddlewareFeature,
) -> Option<&'a FrontendSettingRequirement> {
    feature.requires.iter().find(|requirement| {
        !matches!(
            config.middleware.setting(&requirement.middleware, &requirement.setting),
            Some(FrontendSettingValue::String(value)) if **value == *requirement.value
        )
    })
}

/// The settings a capability discloses.
fn settings(feature: &MiddlewareFeature) -> impl Iterator<Item = &FrontendSetting> {
    feature.settings.iter()
}

/// Whether a capability has rows to disclose.
fn has_details(gateway: &ReadyPayload, feature: &MiddlewareFeature) -> bool {
    settings(feature).next().is_some() || !extensions(gateway, feature).is_empty()
}

fn extensions<'a>(
    gateway: &'a ReadyPayload,
    feature: &MiddlewareFeature,
) -> Vec<&'a ExtensionRecord> {
    gateway
        .agent_catalog
        .extensions
        .iter()
        .filter(|extension| extension.capability == feature.id)
        .collect()
}

/// "Plugin · v1.2 · Hooks disabled until trusted".
pub(in crate::ui) fn extension_metadata(extension: &ExtensionRecord) -> String {
    let mut parts = vec![match extension.kind {
        ExtensionKind::Plugin => "Plugin".to_owned(),
        ExtensionKind::Skill => "Skill".to_owned(),
    }];
    parts.extend(extension.version.as_deref().map(str::to_owned));
    if !extension.hooks.is_empty() && !extension.hooks_trusted {
        parts.push("Hooks disabled until trusted".into());
    }
    parts.join(" · ")
}

pub(super) fn select_value<'a>(
    config: &'a AgentComposition,
    feature: &str,
    setting: &str,
) -> Option<&'a str> {
    match config.middleware.setting(feature, setting) {
        Some(FrontendSettingValue::String(value)) => Some(value),
        _ => None,
    }
}

impl Workspace {
    pub(super) fn capabilities_section(
        &self,
        surface: &'static str,
        scope: &Rc<AgentScope>,
        cx: &Context<Self>,
    ) -> Div {
        let section = div();
        let (Some(draft), Some(gateway)) = (
            self.bots.drafts.get(scope.as_ref()),
            self.client.state.gateway.as_ref(),
        ) else {
            return section;
        };
        let Some(config) = draft.mobius() else {
            return section;
        };
        let mut group = Group::new();
        for feature in &gateway.agent_catalog.middleware_features {
            group = group.child(self.capability_row(scope, draft, config, gateway, feature, cx));
            if !self
                .bots
                .collapsed
                .contains(format!("{}:{}", scope.key(), feature.id).as_str())
            {
                let enabled = feature.required || config.middleware.enabled(&feature.id);
                for setting in settings(feature) {
                    group =
                        group.child(self.setting_row(surface, scope, draft, feature, setting, cx));
                }
                for extension in extensions(gateway, feature) {
                    group = group.child(Self::extension_row(scope, config, extension, enabled, cx));
                }
            }
        }
        section.child(group)
    }

    /// The feature's settings and active extensions, joined for its row.
    fn capability_summary(
        &self,
        draft: &AgentDraft,
        config: &AgentComposition,
        gateway: &ReadyPayload,
        feature: &MiddlewareFeature,
        cx: &App,
    ) -> String {
        let mut parts: Vec<Cow<'_, str>> = settings(feature)
            .map(|setting| match &setting.kind {
                FrontendSettingKind::Integer { .. } => {
                    let value = draft
                        .numbers
                        .get(&feature.id)
                        .and_then(|settings| settings.get(&setting.id))
                        .map(|(input, _)| input.read(cx).value().to_string());
                    Cow::Owned(format!("{}: {}", setting.label, value.unwrap_or_default()))
                }
                FrontendSettingKind::Select {
                    options,
                    unset_label,
                } => {
                    let value =
                        match select_value(config, &feature.id, &setting.id) {
                            Some(value) => Cow::Owned(self.option_label(
                                options.iter().map(|o| (&*o.value, &*o.label)),
                                value,
                            )),
                            None => Cow::Borrowed(unset_label.as_deref().unwrap_or("Not set")),
                        };
                    Cow::Owned(format!("{}: {value}", setting.label))
                }
            })
            .collect();
        let available = extensions(gateway, feature);
        if !available.is_empty() {
            let active: Vec<_> = available
                .iter()
                .filter(|extension| config.extensions.contains(&*extension.id))
                .map(|extension| extension.name.as_ref())
                .collect();
            parts.push(if active.is_empty() {
                Cow::Borrowed("No extensions active")
            } else {
                Cow::Owned(active.join(", "))
            });
        }
        parts.join(" · ")
    }

    /// A select option's label; model routes read as their model and reasoning.
    pub(super) fn option_label<'a>(
        &self,
        mut options: impl Iterator<Item = (&'a str, &'a str)>,
        value: &str,
    ) -> String {
        let models = self
            .client
            .state
            .gateway
            .iter()
            .flat_map(|gateway| &gateway.agent_catalog.models);
        if let Some(choice) = models.into_iter().find(|choice| *choice.route == *value) {
            return self.choice_summary(choice);
        }
        options
            .find(|(option, _)| *option == value)
            .map_or_else(|| value.to_owned(), |(_, label)| label.to_owned())
    }

    fn capability_row(
        &self,
        scope: &AgentScope,
        draft: &AgentDraft,
        config: &AgentComposition,
        gateway: &ReadyPayload,
        feature: &MiddlewareFeature,
        cx: &Context<Self>,
    ) -> AnyElement {
        let enabled = feature.required || config.middleware.enabled(&feature.id);
        let blocked = unavailable(gateway, config, feature);
        let unmet = enabled
            .then(|| unmet_requirement(config, feature))
            .flatten();
        let details = has_details(gateway, feature);
        let switchable = blocked.is_none() || enabled;
        let subtitle = blocked.map_or_else(
            || {
                if let Some(requirement) = unmet {
                    requirement.reason.as_str().into()
                } else if details {
                    self.capability_summary(draft, config, gateway, feature, cx)
                        .into()
                } else {
                    feature.description.as_str().into()
                }
            },
            SharedString::from,
        );
        let key = SharedString::from(format!("{}:{}", scope.key(), feature.id));
        let open = !self.bots.collapsed.contains(&key);
        let mut row = Row::new(
            SharedString::from(format!("capability:{}", feature.id)),
            feature.label.as_str(),
        )
        .subtitle(subtitle);
        if details {
            let caret = chevron(open, &palette(cx));
            let label = format!(
                "{} {} settings",
                if open { "Hide" } else { "Show" },
                feature.label
            );
            row = row.control(
                inline_button(
                    SharedString::from(format!("disclose:{}", feature.id)),
                    label,
                    caret,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.bots.collapsed.remove(&key) {
                        this.bots.collapsed.insert(SharedString::clone(&key));
                    }
                    cx.notify();
                })),
            );
        }
        let scope = AgentScope::clone(scope);
        let id = feature.id.clone(); // rules-allow: the 'static switch listener owns its copy of the feature ID
        row.control(
            toggle(
                SharedString::from(format!("switch:{}", feature.id)),
                &feature.label,
                enabled,
                cx,
            )
            .tooltip(feature.description.as_str())
            .disabled(feature.required || !switchable)
            .on_click(cx.listener(move |this, on: &bool, _, cx| {
                let on = *on;
                this.update_draft(
                    &scope,
                    |draft| {
                        if let Some(config) = draft.mobius_mut() {
                            config.middleware.set_enabled(id.as_str(), on);
                        }
                    },
                    cx,
                );
            })),
        )
        .into_any_element()
    }
}

#[cfg(feature = "snapshot")]
impl Workspace {
    /// Discloses the first capability that has settings or extensions.
    pub(in crate::ui) fn disclose_first_capability(&mut self, scope: &AgentScope) {
        let Some(gateway) = self.client.state.gateway.as_ref() else {
            return;
        };
        if let Some(feature) = gateway
            .agent_catalog
            .middleware_features
            .iter()
            .find(|feature| has_details(gateway, feature))
        {
            self.bots
                .collapsed
                .remove(format!("{}:{}", scope.key(), feature.id).as_str());
        }
    }
}
