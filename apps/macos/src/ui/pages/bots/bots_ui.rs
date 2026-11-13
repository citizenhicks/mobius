//! Bots drafts, pending requests and list state.

use super::{AgentScope, routine_form, settings};
use crate::ui::Workspace;
use crate::ui::primitives::Row;
use gpui::{App, Context, SharedString, WeakEntity};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Package-owned UI state; new fields belong here rather than on `Workspace`.
#[derive(Default)]
pub(in crate::ui) struct BotsUi {
    pub(super) drafts: BTreeMap<AgentScope, settings::AgentDraft>,
    pub(super) pending: BTreeMap<Arc<str>, Pending>,
    /// How many rows a "Show more" list shows, by list key.
    pub(super) shown: BTreeMap<SharedString, usize>,
    /// Capabilities whose settings the user collapsed, by scope and feature.
    pub(super) collapsed: BTreeSet<SharedString>,
}

/// A gateway request whose answer changes what a Bots page shows.
#[derive(Clone, PartialEq)]
pub(in crate::ui) enum Pending {
    Save(AgentScope),
    /// A new Bot, with the Bots known when it was requested.
    Create(BTreeSet<Arc<str>>),
    Routine,
    SaveRoutine(WeakEntity<routine_form::RoutineForm>),
    RunRoutine(Arc<str>),
    /// An extension install from this source.
    Install(String),
}

impl BotsUi {
    pub(super) fn abandon(&mut self, pending: Pending, cx: &mut App) {
        match pending {
            Pending::Save(scope) => {
                if let Some(draft) = self.drafts.get_mut(&scope) {
                    draft.finish_save(None);
                }
            }
            Pending::SaveRoutine(form) => {
                if let Some(form) = form.upgrade() {
                    form.update(cx, |form, cx| {
                        form.save = routine_form::SaveState::Editing;
                        cx.notify();
                    });
                }
            }
            _ => {}
        }
    }

    pub(in crate::ui) fn disconnect(&mut self, cx: &mut App) {
        for pending in std::mem::take(&mut self.pending).into_values() {
            self.abandon(pending, cx);
        }
    }
}

impl Workspace {
    pub(super) fn is_pending(&self, pending: &Pending) -> bool {
        self.bots.pending.values().any(|value| value == pending)
    }

    pub(super) fn shown(&self, key: &str, page: usize) -> usize {
        self.bots.shown.get(key).copied().unwrap_or(page)
    }

    /// A muted "Show more" row that reveals `page` more rows of the list `key`.
    pub(super) fn show_more(key: String, page: usize, cx: &Context<Self>) -> Row {
        let key = SharedString::from(key);
        Row::empty(SharedString::from(format!("more:{key}")), "Show more").on_click(cx.listener(
            move |this, _, _, cx| {
                let shown = this.shown(&key, page) + page;
                this.bots.shown.insert(SharedString::clone(&key), shown);
                cx.notify();
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Entity, TestAppContext};
    use gpui_component::input::InputState;
    use gpui_component::input::TextareaState;
    use gpui_component::searchable_list::SearchableVec;
    use gpui_component::select::SelectState;
    use routine_form::{Inputs, RoutineForm, SaveState};

    #[gpui::test]
    fn disconnect_unlocks_a_routine_draft_and_forgets_its_old_reply(cx: &mut TestAppContext) {
        cx.add_empty_window().update(|window, cx| {
            gpui_component::init(cx);
            let input = cx.new(|cx| InputState::new(window, cx));
            let instructions = cx.new(|cx| TextareaState::new(window, cx));
            instructions.update(cx, |input, cx| {
                input.set_value("keep this draft", window, cx);
            });
            let zone = cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(vec!["Asia/Kathmandu".to_owned()]),
                    None,
                    window,
                    cx,
                )
            });
            let inputs = Inputs {
                instructions,
                zone,
                at: Entity::clone(&input),
                every: Entity::clone(&input),
                time: Entity::clone(&input),
                cron: Entity::clone(&input),
                duration: Entity::clone(&input),
                end_at: input,
            };
            let form = cx.new(|_| RoutineForm::new(None, &["/work".into()], inputs));
            form.update(cx, |form, _| form.save = SaveState::Pending);
            let mut bots = BotsUi::default();
            bots.pending
                .insert("lost-reply".into(), Pending::SaveRoutine(form.downgrade()));
            bots.disconnect(cx);
            assert!(!form.read(cx).busy());
            assert_eq!(form.read(cx).workspace, "/work");
            assert_eq!(
                form.read(cx).inputs.instructions.read(cx).value(),
                "keep this draft"
            );
            assert!(
                bots.pending.remove("lost-reply").is_none(),
                "a late ACK must not close the recovered editor"
            );
            bots.disconnect(cx);
            assert!(
                !form.read(cx).busy(),
                "repeated reconnects keep the editor usable"
            );
        });
    }
}
