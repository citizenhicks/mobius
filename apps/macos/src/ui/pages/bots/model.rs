//! The Model section: the composer's model and reasoning picker for a Bot or Bot
//! defaults, reused for its voice.

use super::{
    Anchor, AnyElement, Context, Disableable, SharedString, Workspace, icon, select_button,
};
use crate::ui::chat::{ModelTarget, configured_model};
use mobius::protocol::ModelChoice;
use mobius_gateway::wire::ProviderConfig;
use std::borrow::Cow;

impl Workspace {
    /// "GPT-6 · High", or the configured model id when the gateway does not offer it.
    pub(super) fn model_summary<'a>(&self, provider: &'a ProviderConfig) -> Cow<'a, str> {
        let Some(gateway) = self.client.state.gateway.as_ref() else {
            return Cow::Borrowed(&provider.model);
        };
        configured_model(gateway, provider).map_or_else(
            || Cow::Borrowed(provider.model.as_str()),
            |choice| Cow::Owned(self.choice_summary(choice)),
        )
    }

    /// "GPT-6 · High", or just "GPT-6" when the route has no variant.
    pub(super) fn choice_summary(&self, choice: &ModelChoice) -> String {
        let model = self.model_identity(choice).0;
        match self.variant_label(choice) {
            Some(variant) => format!("{model} · {variant}"),
            None => model,
        }
    }

    /// The model of `target` as a select button that opens the composer's picker on
    /// `surface`; the choice goes into the draft, which Save sends.
    pub(super) fn model_popover(
        &self,
        surface: &'static str,
        target: ModelTarget,
        enabled: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let (label, glyph) = self.target_current(&target).map_or_else(
            || {
                let label = self.target_unset_label(&target);
                let label = label.unwrap_or_else(|| "Select model".to_owned());
                (label, icon("hi.aiScan", &self.palette))
            },
            |choice| {
                let (_, glyph) = self.model_identity(choice);
                (self.choice_summary(choice), glyph)
            },
        );
        let id = SharedString::from(format!("model-trigger:{target:?}"));
        let trigger = select_button(id, label, Some(glyph), cx)
            .disabled(!enabled || !self.client.link.is_ready());
        self.model_picker_popover(surface, target, trigger, Anchor::TopRight, cx)
    }
}
