//! The Memory section: a möbius Bot switches its memory middleware.

use super::settings::AgentScope;
use super::*;
use crate::ui::pages::harness::MEMORY;
use mobius_gateway::wire::BotMemory;

/// "On · global", "On · this Bot only" or "Off", for the overview.
pub(super) const fn memory_summary(memory: BotMemory) -> &'static str {
    match (memory.enabled, memory.global) {
        (false, _) => "Off",
        (true, true) => "On · global",
        (true, false) => "On · this Bot only",
    }
}

impl Workspace {
    pub(super) fn memory_section(
        &self,
        surface: &'static str,
        scope: &Rc<AgentScope>,
        cx: &Context<Self>,
    ) -> Div {
        let section = div();
        match self.scope_harness(scope) {
            Some(HarnessKind::Mobius) => section.child(self.mobius_memory(surface, scope, cx)),
            Some(HarnessKind::Codex | HarnessKind::Claude) | None => section,
        }
    }

    /// The memory middleware's switch and settings.
    fn mobius_memory(
        &self,
        surface: &'static str,
        scope: &Rc<AgentScope>,
        cx: &Context<Self>,
    ) -> Group {
        let group = Group::new();
        let draft = self.bots.drafts.get(scope.as_ref());
        let feature = self.client.state.gateway.as_ref().and_then(|gateway| {
            gateway
                .agent_catalog
                .middleware_features
                .iter()
                .find(|feature| feature.id == MEMORY)
        });
        let (Some(draft), Some(feature)) = (draft, feature) else {
            return group.child(Row::empty(
                "memory-missing",
                "This gateway doesn’t offer memory for möbius Bots.",
            ));
        };
        let Some(config) = draft.mobius() else {
            return group;
        };
        let on = feature.required || config.middleware.enabled(MEMORY);
        let edit = Rc::clone(scope);
        let mut group = group
            .footer(
                "A memory stays with this Bot in the project it was written in, unless the \
                 Bot saves it as global for every Bot and project.",
            )
            .child(
                Row::new("memory-switch-row", feature.label.as_str())
                    .subtitle(feature.description.as_str())
                    .control(
                        toggle("memory-switch", feature.label.as_str(), on, cx)
                            .disabled(feature.required)
                            .on_click(cx.listener(move |this, on: &bool, _, cx| {
                                let on = *on;
                                this.update_draft(
                                    &edit,
                                    |draft| {
                                        if let Some(config) = draft.mobius_mut() {
                                            config.middleware.set_enabled(MEMORY, on);
                                        }
                                    },
                                    cx,
                                );
                            })),
                    ),
            );
        for setting in &feature.settings {
            group = group.child(self.setting_row(surface, scope, draft, feature, setting, cx));
        }
        group
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_overview_names_whether_memories_reach_other_bots() {
        let memory = |enabled, global| memory_summary(BotMemory { enabled, global });
        assert_eq!(memory(false, true), "Off");
        assert_eq!(memory(true, true), "On · global");
        assert_eq!(memory(true, false), "On · this Bot only");
    }
}
