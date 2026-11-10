//! The model and reasoning picker (ModelRoutePicker.swift): a gauge that opens a popover
//! with the model and a stepped reasoning slider, or the list of models.

mod choices;
mod route;
mod slider;
#[cfg(test)]
mod tests;
pub(in crate::ui) use route::{configured_model, provider_for_route, update_bot_message};

use super::*;
use crate::numeric::{Saturating, ToFloat};
use crate::ui::pages::bots::AgentScope;
use gpui_component::popover::Popover;
use mobius::protocol::{FrontendSetting, FrontendSettingKind, FrontendSettingValue, ModelChoice};
use mobius_gateway::wire::{AgentComposition, BotRecord, ReadyPayload};
use std::rc::Rc;

/// The composer's picker surface.
pub(in crate::ui) const COMPOSER: &str = "composer";

/// Whose model the picker edits.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::ui) enum ModelTarget {
    /// A Bot's saved model; picking saves it at once (the composer).
    Bot(Arc<str>),
    /// An agent settings draft's model; picking leaves it unsaved until Save. Draft targets
    /// share their settings page's scope.
    Draft(Rc<AgentScope>),
    /// A draft's model-route capability setting, such as the subagents' default model;
    /// unset inherits.
    Setting {
        scope: Rc<AgentScope>,
        feature: String,
        setting: String,
    },
}

/// The routes a select setting offers, in its order, when every option is a route of one
/// gateway list, chat, image or voice models (SwiftUI then edits it with ModelRoutePicker).
pub(super) fn route_options<'a>(
    gateway: &'a ReadyPayload,
    setting: &FrontendSetting,
) -> Option<Vec<&'a ModelChoice>> {
    let FrontendSettingKind::Select { options, .. } = &setting.kind else {
        return None;
    };
    if options.is_empty() {
        return None;
    }
    [
        &gateway.agent_catalog.models,
        &gateway.agent_catalog.image_models,
        &gateway.agent_catalog.voice_models,
    ]
    .into_iter()
    .find_map(|catalog| {
        options
            .iter()
            .map(|option| catalog.iter().find(|model| model.route == option.value))
            .collect()
    })
}

/// Open picker state; the popover is open while this exists.
pub(in crate::ui) struct ModelPicker {
    target: ModelTarget,
    /// The trigger that opened the picker; only that surface shows it.
    surface: SharedString,
    models: bool,
    /// The reasoning step under the pointer while the slider is dragged.
    preview: Option<usize>,
}

impl ModelPicker {
    pub(in crate::ui) fn new(surface: impl Into<SharedString>, target: ModelTarget) -> Self {
        Self {
            target,
            surface: surface.into(),
            models: false,
            preview: None,
        }
    }
}

fn same_model(gateway: &ReadyPayload, lhs: &ModelChoice, rhs: &ModelChoice) -> bool {
    match (
        gateway.agent_catalog.model_providers.get(&lhs.route),
        gateway.agent_catalog.model_providers.get(&rhs.route),
    ) {
        (Some(lhs_instance), Some(rhs_instance)) => {
            lhs.model == rhs.model && lhs_instance == rhs_instance
        }
        _ => lhs.route == rhs.route,
    }
}

/// The reasoning gauge level, 0 (no effort) through 5.
fn reasoning_level(index: usize, count: usize, has_effort: bool) -> usize {
    if !has_effort {
        0
    } else if count < 2 {
        5
    } else {
        (index.to_f32() * 5. / (count - 1).to_f32())
            .round()
            .saturating_usize()
    }
}

/// The current choice and every reasoning level of the same model, in gateway order.
pub(super) struct Levels<'a> {
    pub(super) current: &'a ModelChoice,
    pub(super) choices: Vec<&'a ModelChoice>,
    pub(super) index: usize,
}

impl Levels<'_> {
    fn gauge(&self) -> usize {
        reasoning_level(
            self.index,
            self.choices.len(),
            self.current.reasoning_effort.is_some(),
        )
    }
}

impl Workspace {
    pub(in crate::ui) fn composer_bot(&self) -> Option<&BotRecord> {
        if self.draft_selected() {
            self.draft_bot()
        } else if self.active_session_ready() {
            let owner = &self.client.state.session.as_ref()?.session.context.owner_id;
            self.target_bot(&ModelTarget::Bot(Arc::clone(owner)))
        } else {
            None
        }
    }

    /// Why the composer cannot send: its Bot's harness is not set up on this gateway.
    pub(in crate::ui) fn composer_unavailable(&self) -> Option<String> {
        crate::ui::pages::harness::unavailable(self.composer_bot()?.harness)
    }

    pub(super) fn target_bot(&self, target: &ModelTarget) -> Option<&BotRecord> {
        let ModelTarget::Bot(id) = target else {
            return None;
        };
        self.client.state.bot(id)
    }

    /// The composition the picker reads and writes: a Bot's saved one, or a draft's.
    fn target_config(&self, target: &ModelTarget) -> Option<&AgentComposition> {
        match target {
            ModelTarget::Bot(_) => self
                .target_bot(target)
                .and_then(|bot| bot.config.config.mobius())
                .map(Arc::as_ref),
            ModelTarget::Draft(scope) | ModelTarget::Setting { scope, .. } => {
                self.draft_config(scope)
            }
        }
    }

    fn target_setting(&self, target: &ModelTarget) -> Option<&FrontendSetting> {
        let ModelTarget::Setting {
            feature, setting, ..
        } = target
        else {
            return None;
        };
        self.client
            .state
            .gateway
            .as_ref()?
            .agent_catalog
            .middleware_features
            .iter()
            .find(|candidate| candidate.id == *feature)?
            .settings
            .iter()
            .find(|candidate| candidate.id == *setting)
    }

    /// The models of a select setting, when it chooses a model route.
    pub(in crate::ui) fn setting_route_options(
        &self,
        setting: &FrontendSetting,
    ) -> Option<Vec<&ModelChoice>> {
        route_options(self.client.state.gateway.as_ref()?, setting)
    }

    /// The models `target` can take, in the order it offers them.
    fn target_choices(&self, target: &ModelTarget) -> Vec<&ModelChoice> {
        let Some(gateway) = &self.client.state.gateway else {
            return Vec::new();
        };
        match target {
            ModelTarget::Bot(_) | ModelTarget::Draft(_) => {
                gateway.agent_catalog.models.iter().collect()
            }
            ModelTarget::Setting { .. } => self
                .target_setting(target)
                .and_then(|setting| route_options(gateway, setting))
                .unwrap_or_default(),
        }
    }

    /// What a model-route setting reads while unset, such as "Same as Bot".
    pub(in crate::ui) fn target_unset_label(&self, target: &ModelTarget) -> Option<String> {
        match &self.target_setting(target)?.kind {
            FrontendSettingKind::Select { unset_label, .. } => unset_label.clone(), // rules-allow: callers here and in pages/bots own the label as row text
            FrontendSettingKind::Integer { .. } => None,
        }
    }

    /// The model `target` has now, including a draft's unsaved choice.
    pub(in crate::ui) fn target_current(&self, target: &ModelTarget) -> Option<&ModelChoice> {
        let gateway = self.client.state.gateway.as_ref()?;
        let config = self.target_config(target)?;
        match target {
            ModelTarget::Setting {
                feature, setting, ..
            } => {
                let Some(FrontendSettingValue::String(route)) =
                    config.middleware.setting(feature, setting)
                else {
                    return None;
                };
                self.target_choices(target)
                    .into_iter()
                    .find(|choice| *choice.route == **route)
            }
            ModelTarget::Bot(_) | ModelTarget::Draft(_) => {
                configured_model(gateway, &config.provider)
            }
        }
    }

    pub(super) fn target_levels(&self, target: &ModelTarget) -> Option<Levels<'_>> {
        let gateway = self.client.state.gateway.as_ref()?;
        let current = self.target_current(target)?;
        let choices: Vec<_> = self
            .target_choices(target)
            .into_iter()
            .filter(|choice| same_model(gateway, choice, current))
            .collect();
        let index = choices
            .iter()
            .position(|choice| choice.route == current.route)
            .unwrap_or(0);
        Some(Levels {
            current,
            choices,
            index,
        })
    }

    /// The gauge trigger and its popover.
    pub(super) fn model_selector(&self, cx: &Context<Self>) -> AnyElement {
        let Some(bot) = self.composer_bot() else {
            return gauge_button("model-reasoning", "Model and reasoning", 0, cx)
                .disabled(true)
                .into_any_element();
        };
        let target = ModelTarget::Bot(Arc::clone(&bot.id));
        let level = self
            .target_levels(&target)
            .map_or(0, |levels| levels.gauge());
        let label = self.target_levels(&target).map_or_else(
            || "Model".to_owned(),
            |levels| {
                let (name, _) = self.model_identity(levels.current);
                format!("{name} · {}", self.effort_label(levels.current))
            },
        );
        let trigger = gauge_button("model-reasoning", label, level, cx)
            .disabled(!self.client.link.is_ready() || self.queued_draft.is_some());
        self.model_picker_popover(COMPOSER, target, trigger, Anchor::BottomRight, cx)
    }

    /// `trigger` and the picker popover for `target`, open only on `surface`.
    pub(in crate::ui) fn model_picker_popover(
        &self,
        surface: &'static str,
        target: ModelTarget,
        trigger: Button,
        anchor: Anchor,
        cx: &Context<Self>,
    ) -> AnyElement {
        let open = self
            .model_picker
            .as_ref()
            .is_some_and(|picker| picker.surface == surface && picker.target == target);
        let body = open.then(|| self.model_picker_body(&target, cx));
        Popover::new(SharedString::from(format!(
            "model-picker:{surface}:{target:?}"
        )))
        .anchor(anchor)
        .trigger(trigger)
        .open(open)
        .on_open_change(cx.listener(move |this, open: &bool, _, cx| {
            if *open {
                this.model_picker = Some(ModelPicker::new(surface, target.clone())); // rules-allow: every open makes a picker that keeps its own target
            } else if this
                .model_picker
                .as_ref()
                .is_some_and(|picker| picker.surface == surface && picker.target == target)
            {
                this.model_picker = None;
            }
            this.overlay_changed(this.model_picker.is_some(), cx);
        }))
        .w(width::POPOVER)
        .p(space::L)
        .tile_corners()
        .bg(self.palette.control)
        .children(body)
        .into_any_element()
    }

    /// The picker body for `target`: the model and reasoning, or the model list.
    fn model_picker_body(&self, target: &ModelTarget, cx: &Context<Self>) -> AnyElement {
        let listing = self
            .model_picker
            .as_ref()
            .is_some_and(|picker| picker.target == *target && picker.models);
        match self.target_levels(target) {
            Some(levels) if !listing => self.reasoning_view(target, &levels, cx),
            _ => self.model_list(target, cx),
        }
    }

    pub(in crate::ui) fn show_models(&mut self, models: bool, cx: &mut Context<Self>) {
        if let Some(picker) = &mut self.model_picker {
            picker.models = models;
            picker.preview = None;
        }
        cx.notify();
    }

    fn reasoning_view(
        &self,
        target: &ModelTarget,
        levels: &Levels,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let preview = self
            .model_picker
            .as_ref()
            .and_then(|picker| picker.preview)
            .unwrap_or(levels.index)
            .min(levels.choices.len().saturating_sub(1));
        let shown = levels
            .choices
            .get(preview)
            .copied()
            .unwrap_or(levels.current);
        let (name, glyph) = self.model_identity(levels.current);
        let effort = self.effort_label(shown).to_owned();
        let setting = self.target_setting(target);
        let model = Button::new("choose-model")
            .ghost()
            .flex_1()
            .min_w_0()
            .h(height::REGULAR)
            .px(space::S)
            .tooltip(format!("{name} · {effort}"))
            .accessibility_label(format!("Model: {name}, {effort}"))
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(space::S)
                    .typography(Type::Body)
                    .child(glyph.size(glyph::INLINE).flex_shrink_0())
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(p.muted)
                            .child(format!("· {effort}")),
                    )
                    .child(chevron(false, &palette(cx)).flex_shrink_0()),
            )
            .on_click(cx.listener(|this, _, _, cx| this.show_models(true, cx)));
        div()
            .flex()
            .flex_col()
            .gap(space::S)
            // As on iOS: the label and its info on the left, the model and effort on the right.
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(space::S)
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap(space::XS)
                            .child(
                                div().typography(Type::Body).child(
                                    setting
                                        .map_or("Model", |setting| setting.label.as_str())
                                        .to_owned(),
                                ),
                            )
                            .child(inline_button(
                                "model-info",
                                setting.map_or(
                                    "Choose the model and reasoning effort used by this Bot.",
                                    |setting| setting.description.as_str(),
                                ),
                                "hi.info",
                                cx,
                            )),
                    )
                    .child(model),
            )
            .when(levels.choices.len() > 1, |body| {
                body.child(self.reasoning_slider(levels, preview, cx))
            })
            .into_any_element()
    }

    fn reasoning_preview(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(picker) = &mut self.model_picker
            && picker.preview != Some(index)
        {
            picker.preview = Some(index);
            cx.notify();
        }
    }

    /// Applies the dragged reasoning step when the pointer is released.
    fn commit_reasoning(&mut self, cx: &mut Context<Self>) {
        let Some(mut picker) = self.model_picker.take() else {
            return;
        };
        let Some(index) = picker.preview.take() else {
            self.model_picker = Some(picker);
            return;
        };
        let target = &picker.target;
        let route = self.target_levels(target).and_then(|levels| {
            (index != levels.index)
                .then(|| {
                    levels
                        .choices
                        .get(index)
                        .map(|choice| Arc::clone(&choice.route))
                })
                .flatten()
        });
        if let Some(route) = route {
            self.apply_model_route(target, Some(&route), cx);
        }
        self.model_picker = Some(picker);
        cx.notify();
    }
}

#[cfg(feature = "snapshot")]
impl Workspace {
    /// Opens the picker for `target` on `surface`, as its trigger would.
    pub(in crate::ui) fn snapshot_model_picker(
        &mut self,
        surface: &'static str,
        target: ModelTarget,
        cx: &mut Context<Self>,
    ) {
        self.model_picker = Some(ModelPicker::new(surface, target));
        cx.notify();
    }
}
