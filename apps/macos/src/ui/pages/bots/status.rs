//! The agent settings status: saved, unsaved, saving, conflicting or rejected; and Save.

use super::settings::{AgentDraft, AgentScope, Saved};
use super::*;
use mobius_gateway::wire::HarnessConfig;

/// What the header status says about a draft.
struct Status {
    label: &'static str,
    color: Hsla,
    conflict: bool,
    detail: Option<SharedString>,
}

impl Workspace {
    fn status(&self, scope: &AgentScope, draft: &AgentDraft, dirty: bool) -> Status {
        let p = self.palette;
        let status = |label, color| Status {
            label,
            color,
            conflict: false,
            detail: None,
        };
        let moved = self
            .saved_revision(scope)
            .is_some_and(|revision| revision != draft.base.revision);
        if self.scope_busy(scope) {
            Status {
                detail: Some("Bot settings can’t be changed while this Bot is running.".into()),
                ..status("Running", p.warning)
            }
        } else if draft.saving.is_some() {
            status("Saving…", p.accent)
        } else if let Some(message) = &draft.rejected {
            Status {
                detail: Some(message.into()),
                ..status("Rejected", p.danger)
            }
        } else if moved {
            Status {
                conflict: true,
                ..status("Changed elsewhere", p.warning)
            }
        } else if dirty {
            status("Unsaved changes", p.warning)
        } else {
            status("Up to date", p.signal)
        }
    }

    /// Status dot and caption, Reload after a conflict, then Save, for a header's
    /// trailing edge.
    pub(in crate::ui) fn agent_status_actions(
        &self,
        scope: &AgentScope,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(draft) = self.bots.drafts.get(scope) else {
            return Vec::new();
        };
        let composed = draft.composed(cx);
        let dirty = composed != draft.base;
        let status = self.status(scope, draft, dirty);
        let can_save = dirty
            && draft.saving.is_none()
            && self.client.link.is_ready()
            && self
                .scope_harness(scope)
                .is_some_and(crate::ui::pages::harness::available)
            && !self.scope_busy(scope);
        let label = div()
            .id("agent-status")
            .flex()
            .items_center()
            .flex_shrink_0()
            .gap(space::XS)
            .mr(space::XS)
            .typography(Type::Caption)
            .text_color(self.palette.muted)
            .whitespace_nowrap()
            .child(status_dot(status.color))
            .child(status.label)
            .when_some(status.detail, |label, detail| {
                label.tooltip(text_tooltip(detail))
            });
        let (reload, save) = (AgentScope::clone(scope), AgentScope::clone(scope));
        let mut actions = vec![label.into_any_element()];
        if status.conflict {
            actions.push(
                toolbar_button("agent-reload", "Reload", "hi.refresh03", cx)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.reload_draft(&reload, window, cx);
                    }))
                    .into_any_element(),
            );
        }
        actions.push(
            toolbar_button("agent-save", "Save", "hi.check", cx)
                .disabled(!can_save)
                .when(can_save, |button| button.text_color(self.palette.accent))
                .on_click(cx.listener(move |this, _, _, cx| this.save_agent(&save, cx)))
                .into_any_element(),
        );
        actions
    }

    fn save_agent(&mut self, scope: &AgentScope, cx: &mut Context<Self>) {
        let Some(draft) = self.bots.drafts.get_mut(scope) else {
            return;
        };
        let request_id = crate::gateway::request_id();
        let Some(message) = save_message(scope, draft.composed(cx), Arc::clone(&request_id)) else {
            return;
        };
        draft.saving = Some(Arc::clone(&request_id));
        draft.rejected = None;
        self.track_bots_request(
            request_id,
            Pending::Save(AgentScope::clone(scope)),
            message,
            cx,
        );
    }
}

/// The request that saves `saved` for `scope`: the Bot's update, or the agent defaults,
/// which are always a möbius composition.
fn save_message(scope: &AgentScope, saved: Saved, request_id: Arc<str>) -> Option<ClientMessage> {
    match (scope, saved.identity, saved.config) {
        (AgentScope::Bot(id), Some((name, description, tint)), config) => {
            Some(ClientMessage::UpdateBot {
                request_id,
                id: Arc::clone(id),
                expected_revision: saved.revision,
                name,
                description,
                tint,
                shape: saved.shape,
                config,
            })
        }
        (AgentScope::Defaults, _, HarnessConfig::Mobius(config)) => {
            Some(ClientMessage::ConfigureBotDefaults {
                request_id,
                expected_revision: saved.revision,
                config: Arc::unwrap_or_clone(config),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentScope, Saved, save_message};
    use mobius_gateway::wire::{
        AgentComposition, BotShape, ClientMessage, HarnessConfig, ProviderTint,
    };
    use serde_json::json;
    use std::sync::Arc;

    fn saved(model: &str, effort: &str) -> Saved {
        let config: AgentComposition = serde_json::from_value(json!({
            "provider": {
                "instance":"work", "provider":"openai", "model":model,
                "endpoint_auth":"provider_default", "reasoning_effort":effort,
                "web_search":"off"
            },
            "middleware": {"enabled": [], "settings": {}},
            "extensions": [],
            "system_prompt": "Stay focused.",
            "max_model_steps": 64
        }))
        .unwrap();
        Saved {
            revision: 7,
            config: HarnessConfig::Mobius(Arc::new(config)),
            identity: Some(("Reviewer".into(), "Reviews.".into(), ProviderTint::Blue)),
            shape: BotShape::Diamond,
        }
    }

    #[test]
    fn save_sends_the_draft_model_with_the_rest_of_the_bot() {
        let base = saved("sol", "high");
        let draft = saved("luna", "high");
        assert!(
            draft != base,
            "a model change alone marks the draft unsaved"
        );
        assert!(saved("sol", "low") != base, "so does a reasoning change");
        let Some(ClientMessage::UpdateBot {
            id,
            expected_revision,
            name,
            shape,
            config,
            ..
        }) = save_message(&AgentScope::Bot("bot-1".into()), draft, "save".into())
        else {
            panic!("a Bot saves as an update");
        };
        assert_eq!((id.as_ref(), expected_revision), ("bot-1", 7));
        assert_eq!(&*name, "Reviewer");
        assert_eq!(shape, BotShape::Diamond);
        let config = config.mobius().expect("a möbius Bot saves its composition");
        assert_eq!(config.provider.model, "luna");
        assert_eq!(config.system_prompt, "Stay focused.");
    }
}
