//! The model list the picker swaps in (ModelSelectionSheet).

use super::*;
use gpui_component::Icon;

impl Workspace {
    /// Label and provider glyph, in the provider setup's tint, for a chat, image, or voice
    /// model choice.
    pub(in crate::ui) fn model_identity(&self, choice: &ModelChoice) -> (String, Icon) {
        let instance = self.choice_instance(choice);
        let status = self.choice_status(choice);
        let label = status
            .and_then(|status| {
                let id = choice.model.as_ref();
                let chat = status.models.iter().find(|model| model.id == id);
                chat.map(|model| model.label.as_str()).or_else(|| {
                    status
                        .image_models
                        .iter()
                        .chain(status.voice_models.iter())
                        .find(|model| model.id == id)
                        .map(|model| model.label.as_str())
                })
            })
            .unwrap_or(&choice.model)
            .to_owned();
        let glyph = status
            .and_then(|status| known_symbol_icon(Some(&status.symbol)))
            .unwrap_or("hi.aiScan");
        let glyph = icon(glyph, &self.palette);
        let glyph = match instance {
            Some(instance) => glyph.text_color(Accent::from(instance.tint).color()),
            None => glyph,
        };
        (label, glyph)
    }

    /// The gateway's label for `choice`'s variant (a reasoning effort, image quality, or
    /// voice), else the advertised reasoning label, else its id; `None` without a variant.
    pub(in crate::ui) fn variant_label<'a>(&'a self, choice: &'a ModelChoice) -> Option<&'a str> {
        let variant = choice.reasoning_effort.as_deref()?;
        let advertised = || {
            self.choice_status(choice)?
                .models
                .iter()
                .filter(|preset| *preset.id == *choice.model)
                .flat_map(|preset| &preset.reasoning)
                .find(|reasoning| reasoning.id == variant)
                .map(|reasoning| reasoning.label.as_str())
        };
        Some(
            choice
                .variant_label
                .as_deref()
                .or_else(advertised)
                .unwrap_or(variant),
        )
    }

    /// The variant label the picker shows, "default" when the choice has none.
    pub(in crate::ui) fn effort_label<'a>(&'a self, choice: &'a ModelChoice) -> &'a str {
        self.variant_label(choice).unwrap_or("default")
    }

    fn choice_status(&self, choice: &ModelChoice) -> Option<&mobius_gateway::wire::ProviderStatus> {
        let provider = &self.choice_instance(choice)?.selection.provider;
        self.client
            .state
            .gateway
            .as_ref()?
            .providers
            .iter()
            .find(|status| status.provider == *provider)
    }

    fn choice_instance(
        &self,
        choice: &ModelChoice,
    ) -> Option<&mobius_gateway::wire::ProviderInstance> {
        let gateway = self.client.state.gateway.as_ref()?;
        let instance = gateway.agent_catalog.model_providers.get(&choice.route)?;
        gateway
            .provider_instances
            .iter()
            .find(|candidate| &candidate.selection.instance == instance)
    }

    pub(super) fn model_list(&self, target: &ModelTarget, cx: &Context<Self>) -> AnyElement {
        let Some(gateway) = &self.client.state.gateway else {
            return div().into_any_element();
        };
        let current = self.target_current(target);
        let choices = self.target_choices(target);
        let unset = self.target_unset_label(target).map(|label| {
            let target = target.clone(); // rules-allow: the 'static row listener owns its copy of the picker's target
            let glyph = icon("hi.aiScan", &self.palette);
            self.model_row("model:unset".into(), glyph, label, current.is_none())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.apply_model_route(&target, None, cx);
                    this.show_models(false, cx);
                }))
        });
        let rows = distinct_models(gateway, &choices)
            .into_iter()
            .map(|choice| {
                let selected = current.is_some_and(|model| same_model(gateway, model, choice));
                let route = preferred_route(gateway, &choices, choice, current);
                let (name, glyph) = self.model_identity(choice);
                let provider = self
                    .choice_instance(choice)
                    .map_or(&*choice.group, |instance| instance.label.as_str());
                let target = target.clone(); // rules-allow: the 'static row listener owns its copy of the picker's target
                let id = SharedString::from(format!("model:{}", choice.route));
                self.model_row(id, glyph, format!("{provider} · {name}"), selected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.apply_model_route(&target, Some(&route), cx);
                        this.show_models(false, cx);
                    }))
            });
        let back = current.is_some().then(|| {
            Button::new("models-back")
                .ghost()
                .h(height::REGULAR)
                .px(space::S)
                .accessibility_label("Back to model and reasoning")
                .justify_start()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::XS)
                        .typography(Type::Headline)
                        .child(
                            icon("hi.caretRight", &palette(cx))
                                .size(glyph::MARK)
                                .rotate(Radians(std::f32::consts::PI)),
                        )
                        .child("Models"),
                )
                .on_click(cx.listener(|this, _, _, cx| this.show_models(false, cx)))
        });
        div()
            .flex()
            .flex_col()
            .gap(space::XS)
            .children(back)
            .child(
                div()
                    .id("model-options")
                    .max_h(height::LIST_MAX)
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(unset)
                    .children(rows),
            )
            .into_any_element()
    }
}

impl Workspace {
    /// A model list row: its glyph and label, then a check when it is the current one.
    fn model_row(&self, id: SharedString, glyph: Icon, label: String, selected: bool) -> Button {
        let check = selected.then(|| icon("hi.check", &self.palette).size(glyph::INLINE));
        let label = SharedString::from(label);
        Button::new(id)
            .ghost()
            .w_full()
            .h(height::REGULAR)
            .px(space::S)
            .accessibility_label(SharedString::clone(&label))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(space::S)
                    .typography(Type::Body)
                    .child(glyph.size(glyph::INLINE))
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .children(check),
            )
    }
}

/// One entry per model of `choices`, whatever its reasoning levels, in their order.
pub(super) fn distinct_models<'a>(
    gateway: &ReadyPayload,
    choices: &[&'a ModelChoice],
) -> Vec<&'a ModelChoice> {
    let mut seen = std::collections::BTreeSet::new();
    choices
        .iter()
        .copied()
        .filter(|choice| {
            let instance = gateway.agent_catalog.model_providers.get(&choice.route);
            seen.insert((
                instance.map_or(&*choice.route, String::as_str),
                &choice.model,
            ))
        })
        .collect()
}

/// The route to select for `choice`, keeping the current reasoning effort when it exists.
fn preferred_route(
    gateway: &ReadyPayload,
    choices: &[&ModelChoice],
    choice: &ModelChoice,
    current: Option<&ModelChoice>,
) -> Arc<str> {
    let effort = current.and_then(|model| model.reasoning_effort.as_ref());
    let chosen = choices
        .iter()
        .copied()
        .find(|candidate| {
            same_model(gateway, candidate, choice) && candidate.reasoning_effort.as_ref() == effort
        })
        .or_else(|| {
            provider_for_route(gateway, &choice.route).and_then(|mut provider| {
                provider.reasoning_effort = None;
                configured_model(gateway, &provider)
            })
        })
        .unwrap_or(choice);
    Arc::clone(&chosen.route)
}
