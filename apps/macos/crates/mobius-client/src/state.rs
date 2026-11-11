use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use mobius::SessionId;
use mobius::protocol::{EventMsg, FrontendEvent, FrontendPreviewUpdate, TokenUsageInfo};
use mobius_gateway::wire::{
    BotRecord, HistoryCursor, ReadyPayload, RecordedEvent, RenderedPreview, ServerMessage,
    SessionReadyPayload, SessionRecord, SessionWidget,
};

use super::TranscriptStore;

/// A capability preview, such as a subagent's or the voice call's transcript, with its
/// page moved into a store.
#[derive(Debug)]
pub struct Preview {
    pub source: RenderedPreview,
    pub transcript: TranscriptStore,
}

#[derive(Default, Debug)]
pub struct GatewayState {
    /// Gateway capabilities/configuration. Its sessions are extracted into `sessions`
    /// so the chat catalog has one owner and one update path.
    pub gateway: Option<ReadyPayload>,
    pub sessions: Vec<SessionRecord>,
    pub selected_session_id: Option<SessionId>,
    pub session: Option<SessionReadyPayload>,
    pub transcript: TranscriptStore,
    /// Where the selected chat's next older history page starts.
    pub history: Option<HistoryCursor>,
    pub previews: BTreeMap<Arc<str>, Rc<Preview>>,
    /// The preview the last applied event carried.
    pub arrived_preview: Option<Arc<str>>,
    /// The transcript items the last applied event changed.
    pub touched: Vec<Arc<str>>,
    pub picker: Option<(String, Vec<mobius::protocol::FrontendPickerOption>)>,
    pub error: Option<Arc<str>>,
    pub replaying: bool,
    pub token_usage: Option<Arc<TokenUsageInfo>>,
    pub routines: Vec<mobius_gateway::wire::Routine>,
    pub routine_history: Vec<mobius_gateway::wire::RoutineRun>,
    pub pairing_code: Option<(String, i64)>,
    pub routine_preview_request: Option<Arc<str>>,
    pub profile: Option<mobius_gateway::wire::ProfileSnapshot>,
    pub git_credential: Option<(bool, Option<String>)>,
    pub ssh_identities: Option<Vec<mobius_gateway::wire::SshIdentityRecord>>,
    pub ssh_public_key: Option<String>,
    pub clients: Option<(String, Vec<mobius_gateway::wire::ClientStatus>)>,
    /// Every harness's memories, as the last `memories` answer listed them.
    pub memories: Option<Vec<mobius_gateway::wire::MemoryRecord>>,
}

impl GatewayState {
    /// The items the last applied event changed.
    pub fn touched_entries(&self) -> impl Iterator<Item = &super::TranscriptEntry> {
        self.touched
            .iter()
            .filter_map(|id| self.transcript.item(id))
    }

    /// Workspace tools use only the selected session's actual project workspace.
    pub fn workspace_session_id(&self) -> Option<&SessionId> {
        let session = self.session.as_ref()?;
        (session.workspace.is_some()
            && self.selected_session_id.as_ref() == Some(&session.session.session_id))
        .then_some(&session.session.session_id)
    }

    /// The selected chat, as a handle of its own.
    pub fn selected(&self) -> Option<SessionId> {
        self.selected_session_id.as_ref().map(SessionId::share)
    }

    /// The chat catalog's record for `id`.
    pub fn session_record(&self, id: &str) -> Option<&SessionRecord> {
        self.sessions
            .iter()
            .find(|session| *session.session_id == *id)
    }

    /// The gateway's Bot with `id`.
    pub fn bot(&self, id: &str) -> Option<&BotRecord> {
        self.gateway
            .as_ref()?
            .bots
            .iter()
            .find(|bot| *bot.id == *id)
    }

    /// Leaves the selected chat while keeping catalogs, profile and credentials.
    pub fn reset_session(&mut self) {
        self.selected_session_id = None;
        self.session = None;
        self.transcript = TranscriptStore::default();
        self.history = None;
        self.previews.clear();
        self.picker = None;
        self.replaying = false;
        self.token_usage = None;
    }

    /// Folds `message` into the state, moving its transcript ops and pages into the store.
    pub fn apply(&mut self, message: &mut ServerMessage) {
        self.arrived_preview = None;
        self.touched.clear();
        if self.apply_catalog(message) {
            return;
        }
        let current = self.selected_session_id.as_ref().map(SessionId::share);
        let selected = |session: &str| current.as_deref() == Some(session);
        match message {
            ServerMessage::SessionOpened { payload, .. } => {
                let id = &payload.session.session_id;
                if self.selected_session_id.as_ref() != Some(id) {
                    self.transcript = TranscriptStore::default();
                    self.history = None;
                    self.previews.clear();
                    self.picker = None;
                }
                self.token_usage = payload.token_usage.as_ref().map(Arc::clone);
                self.selected_session_id = Some(id.share());
                self.session = Some(payload.clone()); // rules-allow: the runtime dispatches the borrowed message after this, so the state stores a copy
                self.replaying = true;
                self.error = None;
            }
            ServerMessage::SessionChanged { payload } if selected(&payload.session.session_id) => {
                self.session = Some(payload.clone()); // rules-allow: the runtime dispatches the borrowed message after this, so the state stores a copy
            }
            ServerMessage::SessionMetaChanged {
                session_id,
                contributions,
                context_limit_tokens,
            } if selected(session_id) => {
                if let Some(session) = &mut self.session {
                    session.contributions = std::mem::take(contributions);
                    session.context_limit_tokens = *context_limit_tokens;
                }
            }
            ServerMessage::MessagePinsChanged {
                session_id,
                pinned_messages,
            } if selected(session_id) => {
                if let Some(session) = &mut self.session {
                    session.pinned_messages = std::mem::take(pinned_messages);
                }
            }
            ServerMessage::SessionReplayComplete { session_id, .. } if selected(session_id) => {
                self.replaying = false;
            }
            ServerMessage::SessionTranscript {
                session_id,
                page,
                next,
                ..
            } if selected(session_id) => {
                self.transcript.replace(std::mem::take(page));
                self.history = next.take();
            }
            ServerMessage::SessionHistory {
                session_id,
                page,
                next,
                ..
            } if selected(session_id) => {
                self.transcript.prepend(std::mem::take(page));
                self.history = next.take();
            }
            ServerMessage::AgentEvent {
                session_id,
                record,
                transcript,
            } if selected(session_id) => {
                self.touched = self.transcript.apply(std::mem::take(transcript));
                if let EventMsg::TokenCount(usage) = &record.event.msg
                    && let Some(info) = &usage.info
                {
                    self.note_usage(info);
                }
                if !self.replaying {
                    self.apply_session_event(record);
                }
                if let Some(preview) = record.take_preview() {
                    self.apply_preview(preview);
                }
            }
            ServerMessage::BackgroundApprovals { approvals } => {
                if let Some(gateway) = &mut self.gateway {
                    gateway.background_approvals = std::mem::take(approvals);
                }
            }
            ServerMessage::Rejected { message, .. } | ServerMessage::Error { message, .. } => {
                self.error = Some(Arc::clone(message));
            }
            _ => {}
        }
    }

    fn apply_catalog(&mut self, message: &mut ServerMessage) -> bool {
        match message {
            ServerMessage::Clients {
                current_client_id,
                clients,
                ..
            } => {
                self.clients = Some((current_client_id.clone(), clients.clone())); // rules-allow: the runtime dispatches the borrowed message after this, so the state stores a copy
            }
            ServerMessage::Profile { profile, .. } => {
                self.profile = Some(profile.clone()); // rules-allow: the runtime dispatches the borrowed message after this, so the state stores a copy
            }
            ServerMessage::GitCredentialStatus {
                available,
                username,
                ..
            } => {
                self.git_credential = Some((*available, username.clone())); // rules-allow: the runtime dispatches the borrowed message after this, so the state stores a copy
            }
            ServerMessage::SshIdentities { identities, .. } => {
                self.ssh_identities = Some(identities.clone()); // rules-allow: the runtime dispatches the borrowed message after this, so the state stores a copy
            }
            ServerMessage::SshIdentityGenerated {
                identity,
                public_key,
                ..
            } => {
                let identities = self.ssh_identities.get_or_insert_default();
                if !identities
                    .iter()
                    .any(|current| current.fingerprint == identity.fingerprint)
                {
                    identities.push(identity.clone()); // rules-allow: the runtime dispatches the borrowed message after this, so the state stores a copy
                }
                self.ssh_public_key = Some(public_key.clone()); // rules-allow: the runtime dispatches the borrowed message after this, so the state stores a copy
            }
            ServerMessage::Memories { memories, .. } => {
                self.memories = Some(std::mem::take(memories));
            }
            ServerMessage::Routines { routines, .. } => self.routines.clone_from(routines),
            ServerMessage::RoutineHistory { runs, .. } => self.routine_history.clone_from(runs),
            ServerMessage::PairingCode {
                code, expires_at, ..
            } => {
                self.pairing_code = Some((code.clone(), *expires_at)); // rules-allow: the runtime dispatches the borrowed message after this, so the state stores a copy
            }
            ServerMessage::Ready { payload } | ServerMessage::GatewayConfigured { payload, .. } => {
                let mut catalog = payload.clone(); // rules-allow: the runtime dispatches the borrowed message after this, so the state stores a copy
                // The gateway leaves out the sections this window already holds.
                if let Some(mut held) = self.gateway.take() {
                    held.sessions = std::mem::take(&mut self.sessions);
                    catalog.restore_omitted(&mut held);
                }
                self.sessions = std::mem::take(&mut catalog.sessions);
                self.gateway = Some(catalog);
                self.error = None;
            }
            ServerMessage::Sessions { sessions, .. } => {
                self.sessions.clone_from(sessions);
            }
            ServerMessage::Bots { bots, .. } => {
                if let Some(gateway) = &mut self.gateway {
                    gateway.bots.clone_from(bots);
                }
            }
            ServerMessage::Contributions { contributions, .. } => {
                if let Some(gateway) = &mut self.gateway {
                    gateway.agent_catalog.contributions = std::mem::take(contributions);
                }
            }
            _ => return false,
        }
        true
    }

    /// Usage without a context breakdown (a compaction call's, say) keeps the last one.
    fn note_usage(&mut self, info: &TokenUsageInfo) {
        let mut info = info.clone(); // rules-allow: the runtime dispatches the borrowed message after this, so the state stores a copy
        if info.context.is_none() {
            info.context = self
                .token_usage
                .as_mut()
                .and_then(|usage| Arc::make_mut(usage).context.take());
        }
        self.token_usage = Some(Arc::new(info));
    }

    fn apply_session_event(&mut self, record: &RecordedEvent) {
        let Some(session) = &mut self.session else {
            return;
        };
        session.latest_sequence = record.sequence;
        match &record.event.msg {
            EventMsg::TurnStarted(turn) => {
                if !session.active_turn_ids.contains(&turn.turn_id) {
                    session.active_turn_ids.push(Arc::clone(&turn.turn_id));
                }
            }
            EventMsg::TurnComplete(turn) => finish_turn(session, &turn.turn_id),
            EventMsg::TurnAborted(turn) => finish_turn(session, &turn.turn_id),
            EventMsg::ToolCallBegin(call) => session.pending_approvals.retain(|approval| {
                !approval
                    .calls
                    .iter()
                    .any(|candidate| candidate.call_id == call.call_id)
            }),
            EventMsg::ModelChanged(model) => {
                Arc::make_mut(&mut session.session).model = model.clone(); // rules-allow: the runtime dispatches the borrowed message after this, so the state stores a copy
            }
            EventMsg::ExecApprovalDecided(decided) => session
                .pending_approvals
                .retain(|approval| approval.id != decided.id),
            EventMsg::ExecApprovalRequest(approval) => {
                if !session
                    .pending_approvals
                    .iter()
                    .any(|current| current.id == approval.id)
                {
                    session.pending_approvals.push(approval.clone()); // rules-allow: the runtime dispatches the borrowed message after this, so the state stores a copy
                }
            }
            EventMsg::Frontend(FrontendEvent::Widget { capability, item }) => {
                if let Some(widget) = session
                    .widgets
                    .iter_mut()
                    .find(|widget| widget.capability == *capability && widget.item.id == item.id)
                {
                    widget.item = Arc::clone(item);
                } else {
                    session.widgets.push(SessionWidget {
                        capability: Arc::clone(capability),
                        item: Arc::clone(item),
                    });
                }
            }
            EventMsg::Frontend(FrontendEvent::RemoveWidget { capability, id }) => session
                .widgets
                .retain(|widget| widget.capability != *capability || *widget.item.id != **id),
            EventMsg::Frontend(FrontendEvent::Picker { title, options }) => {
                self.picker = Some((title.clone(), options.clone())); // rules-allow: the runtime dispatches the borrowed message after this, so the state stores a copy
            }
            _ => {}
        }
    }

    /// A later page of a preview already shown prepends older history, or lays the live
    /// page over what was loaded, keeping how far back it reaches.
    fn apply_preview(&mut self, mut source: RenderedPreview) {
        let page = std::mem::take(&mut source.transcript);
        let id = Arc::clone(&source.id);
        let transcript = match self.previews.remove(&id) {
            Some(previous) => {
                let (mut transcript, next) = match Rc::try_unwrap(previous) {
                    Ok(previous) => (previous.transcript, previous.source.next),
                    Err(shared) => (
                        TranscriptStore::from(shared.transcript.page()),
                        shared.source.next.clone(), // rules-allow: an open tab still shows the previous snapshot
                    ),
                };
                match source.update {
                    FrontendPreviewUpdate::Prepend => transcript.prepend(page),
                    FrontendPreviewUpdate::Replace => {
                        transcript.merge(page);
                        source.next = next;
                    }
                }
                transcript
            }
            None => TranscriptStore::from(page),
        };
        self.previews
            .insert(Arc::clone(&id), Rc::new(Preview { source, transcript }));
        self.arrived_preview = Some(id);
    }
}

fn finish_turn(session: &mut SessionReadyPayload, turn_id: &str) {
    session.active_turn_ids.retain(|id| **id != *turn_id);
    session
        .pending_approvals
        .retain(|approval| *approval.turn_id != *turn_id);
}

#[cfg(test)]
#[path = "tests.rs"]
pub(super) mod tests;
