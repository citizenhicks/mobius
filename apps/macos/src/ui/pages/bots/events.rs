//! Bots request publication and reply settlement.

use super::{Pending, routine_form};
use crate::ui::Workspace;
use crate::ui::router::PageTab;
use gpui::{Context, Window};
use mobius_gateway::wire::{ClientMessage, ServerMessage};
use std::sync::Arc;

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
}
