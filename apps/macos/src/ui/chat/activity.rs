//! The workspace's code changes, for the changes pill (ComposerActivityView): the chat
//! workspace's unstaged Git line totals, as Git counts them, asked for when the chat opens,
//! a turn ends or a tool reports an edit. A folder outside Git answers zero, which hides it.

use super::{Arc, EventMsg, ServerMessage, Workspace};
use mobius::SessionId;
use mobius::protocol::FrontendBlockFormat;
use mobius_gateway::wire::{ClientMessage, DiffTotals, GitDiffScope};

/// The workspace diff the pill shows, and the one request that refreshes it.
#[derive(Default)]
pub(super) struct WorkspaceChanges {
    /// The chat the totals belong to, with its `+added −removed`.
    totals: Option<(SessionId, (u64, u64))>,
    /// The diff-totals request in flight, and whether another refresh was asked for meanwhile.
    request: Option<Arc<str>>,
    stale: bool,
}

impl WorkspaceChanges {
    /// The selected chat's totals, while it has any.
    pub(super) fn totals(&self, session: &str) -> Option<(u64, u64)> {
        self.totals
            .as_ref()
            .filter(|(owner, totals)| **owner == *session && totals.0 + totals.1 > 0)
            .map(|(_, totals)| *totals)
    }

    pub(super) fn owns(&self, request_id: &str) -> bool {
        self.request.as_deref() == Some(request_id)
    }

    /// Folds in a gateway message for the `selected` chat; true when the diff should be
    /// asked for again: the chat finished opening or changed, a live turn ended or a tool
    /// edited, or a refresh waited for the answer that just arrived.
    fn on_message(
        &mut self,
        message: &ServerMessage,
        selected: Option<&str>,
        live: bool,
        edited: bool,
    ) -> bool {
        match message {
            ServerMessage::Ready { .. } => {
                // A new connection answers nothing the old one asked.
                self.request = None;
                self.stale = false;
                false
            }
            ServerMessage::SessionReplayComplete { session_id, .. } => selected == Some(session_id),
            // A branch switch or a reconfigured session may have changed the checkout.
            ServerMessage::SessionChanged { payload } => {
                selected == Some(payload.session.session_id.as_ref())
            }
            ServerMessage::AgentEvent {
                session_id, record, ..
            } => {
                live && selected == Some(session_id)
                    && (edited
                        || matches!(
                            record.event.msg,
                            EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
                        ))
            }
            ServerMessage::Rejected { request_id, .. } if self.owns(request_id) => {
                self.request = None;
                std::mem::take(&mut self.stale)
            }
            _ => false,
        }
    }

    /// Takes the totals the pill asked for, moving their chat out of the reply; true when
    /// another refresh waited for this answer.
    fn answer(
        &mut self,
        request_id: &str,
        session_id: &SessionId,
        totals: DiffTotals,
        selected: Option<&SessionId>,
    ) -> bool {
        if !self.owns(request_id) {
            return false;
        }
        self.request = None;
        if selected == Some(session_id) {
            let totals = (totals.additions, totals.deletions);
            self.totals = Some((session_id.share(), totals));
        }
        std::mem::take(&mut self.stale)
    }

    /// Whether a request may start now; otherwise the one in flight is followed by another.
    fn begin(&mut self) -> bool {
        self.stale |= self.request.is_some();
        self.request.is_none()
    }
}

impl Workspace {
    /// The pill's own reply, which nothing else reads, so its chat moves into the pill.
    pub(in crate::ui) fn workspace_changes_answer(&mut self, message: &mut ServerMessage) {
        if let ServerMessage::GitDiffTotals {
            request_id,
            session_id,
            totals,
            ..
        } = message
            && self.chat_ui.changes.answer(
                request_id,
                session_id,
                *totals,
                self.client.state.selected_session_id.as_ref(),
            )
        {
            self.refresh_workspace_changes();
        }
    }

    /// Keeps the pill's totals current from the gateway's messages.
    pub(super) fn workspace_changes_on_message(&mut self, message: &ServerMessage) {
        let selected = self.client.state.selected_session_id.as_deref();
        // A finished unified diff is an edit the workspace now holds.
        let edited = self.client.state.touched_entries().any(|entry| {
            !entry.is_pending()
                && entry
                    .block()
                    .is_some_and(|block| block.format == FrontendBlockFormat::UnifiedDiff)
        });
        if self
            .chat_ui
            .changes
            .on_message(message, selected, !self.client.state.replaying, edited)
        {
            self.refresh_workspace_changes();
        }
    }

    /// Asks for the selected chat's unstaged diff totals, one request at a time.
    fn refresh_workspace_changes(&mut self) {
        let Some(session_id) = self
            .client
            .state
            .workspace_session_id()
            .map(SessionId::share)
        else {
            return;
        };
        if !self.client.link.is_ready() || !self.chat_ui.changes.begin() {
            return;
        }
        let request_id = crate::gateway::request_id();
        let message = ClientMessage::GetGitDiff {
            request_id: Arc::clone(&request_id),
            session_id,
            scope: GitDiffScope::Unstaged,
            totals: true,
        };
        if self
            .client
            .gateway
            .as_ref()
            .is_some_and(|gateway| gateway.send(message).is_ok())
        {
            self.chat_ui.changes.request = Some(request_id);
        }
    }

    /// Shows `totals` for the selected chat, as a snapshot fixture does.
    #[cfg(feature = "snapshot")]
    pub(super) fn fixture_changes(&mut self, totals: (u64, u64)) {
        let Some(session) = self
            .client
            .state
            .selected_session_id
            .as_ref()
            .map(SessionId::share)
        else {
            return;
        };
        self.chat_ui.changes = WorkspaceChanges {
            totals: Some((session, totals)),
            ..Default::default()
        };
    }
}
