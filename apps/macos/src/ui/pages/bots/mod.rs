//! Bots: the list, each Bot's overview and settings, routines and agent defaults (WP4).

mod capabilities;
mod capability_rows;
mod create;
mod menu;
mod model;
mod overview;
mod pin;
mod routine_dialog;
mod routine_form;
mod routines;
mod runs;
mod schedule;
mod sections;
mod settings;
mod sidebar;
mod status;

pub(in crate::ui) use capabilities::extension_metadata;
pub(in crate::ui) use create::dialog_heading;
pub(in crate::ui) use settings::AgentScope;

use super::*;
use menu::{MenuAction, more_menu};
use mobius_gateway::wire::{BotRecord, BotShape, HarnessKind, ServerMessage};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// Package-owned UI state; new fields belong here rather than on `Workspace`.
#[derive(Default)]
pub(in crate::ui) struct BotsUi {
    drafts: BTreeMap<AgentScope, settings::AgentDraft>,
    pending: BTreeMap<Arc<str>, Pending>,
    /// How many rows a "Show more" list shows, by list key.
    shown: BTreeMap<SharedString, usize>,
    /// Capabilities whose settings the user collapsed, by scope and feature.
    collapsed: BTreeSet<SharedString>,
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
    fn abandon(&mut self, pending: Pending, cx: &mut App) {
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
    pub(in crate::ui) fn bots_on_message(
        &mut self,
        message: &ServerMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            message,
            ServerMessage::Ready { .. } | ServerMessage::Bots { .. }
        ) {
            self.prune_bot_drafts(cx);
        }
        match message {
            ServerMessage::Bots {
                request_id: Some(id),
                ..
            }
            | ServerMessage::GatewayConfigured { request_id: id, .. }
            | ServerMessage::Accepted { request_id: id } => {
                self.finish_request(id, None, window, cx);
            }
            ServerMessage::Rejected {
                request_id,
                message,
                ..
            } => self.finish_request(request_id, Some(Arc::clone(message)), window, cx),
            ServerMessage::BackgroundApprovals { .. }
                if matches!(self.primary_page(), Some(PageTab::Bot(_))) =>
            {
                self.refresh_routines(cx);
            }
            ServerMessage::HookEvent { event }
                if matches!(
                    event.data,
                    mobius_gateway::wire::HookData::RoutineCreated { .. }
                        | mobius_gateway::wire::HookData::RoutineUpdated { .. }
                        | mobius_gateway::wire::HookData::RoutinePaused { .. }
                        | mobius_gateway::wire::HookData::RoutineResumed { .. }
                        | mobius_gateway::wire::HookData::RoutineDeleted { .. }
                        | mobius_gateway::wire::HookData::RunStarted { .. }
                        | mobius_gateway::wire::HookData::RunFinished { .. }
                        | mobius_gateway::wire::HookData::RunSkipped { .. }
                        | mobius_gateway::wire::HookData::ScheduleDue { .. }
                ) =>
            {
                self.refresh_routines(cx);
            }
            _ => {}
        }
    }

    fn finish_request(
        &mut self,
        id: &str,
        rejection: Option<Arc<str>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.bots.pending.remove(id) else {
            return;
        };
        match pending {
            Pending::Save(scope) => {
                if let Some(draft) = self.bots.drafts.get_mut(&scope) {
                    draft.finish_save(rejection);
                }
            }
            Pending::Create(known) if rejection.is_none() => {
                let created = self.client.state.gateway.as_ref().and_then(|gateway| {
                    gateway
                        .bots
                        .iter()
                        .find(|bot| !known.contains(&bot.id))
                        .map(|bot| Arc::clone(&bot.id))
                });
                if let Some(id) = created {
                    self.open_page(PageTab::Bot(id), window, cx);
                }
            }
            Pending::SaveRoutine(form) => {
                let accepted = rejection.is_none();
                if let Some(form) = form.upgrade() {
                    form.update(cx, |form, _| {
                        form.save = if accepted {
                            routine_form::SaveState::Saved
                        } else {
                            routine_form::SaveState::Editing
                        };
                    });
                    window.refresh();
                }
                if accepted {
                    self.refresh_routines(cx);
                }
            }
            Pending::Routine | Pending::RunRoutine(_) => self.refresh_routines(cx),
            Pending::Install(source) => {
                if rejection.is_some() {
                    self.install_dialog(&source, window, cx);
                }
            }
            Pending::Create(_) => {}
        }
        cx.notify();
    }

    /// Sends `message` and remembers what its answer should update.
    pub(in crate::ui) fn track_bots_request(
        &mut self,
        request_id: Arc<str>,
        pending: Pending,
        message: ClientMessage,
        cx: &mut Context<Self>,
    ) -> bool {
        let sent = match self.enqueue(message) {
            Ok(()) => {
                if let Pending::SaveRoutine(form) = &pending
                    && let Some(form) = form.upgrade()
                {
                    form.update(cx, |form, cx| {
                        form.save = routine_form::SaveState::Pending;
                        cx.notify();
                    });
                }
                self.bots.pending.insert(request_id, pending);
                true
            }
            Err(error) => {
                self.bots.abandon(pending, cx);
                self.report(error);
                false
            }
        };
        cx.notify();
        sent
    }

    fn is_pending(&self, pending: &Pending) -> bool {
        self.bots.pending.values().any(|value| value == pending)
    }

    fn shown(&self, key: &str, page: usize) -> usize {
        self.bots.shown.get(key).copied().unwrap_or(page)
    }

    /// A muted "Show more" row that reveals `page` more rows of the list `key`.
    fn show_more(key: String, page: usize, cx: &Context<Self>) -> Row {
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
    use gpui::TestAppContext;
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
