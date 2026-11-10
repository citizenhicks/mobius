//! One window's side of its gateway: the connection, how it stands, what the gateway has
//! told it and the requests it is waiting on. The interface reads this and reacts.

use std::collections::BTreeMap;
use std::sync::Arc;

use mobius::SessionId;
use mobius::protocol::{EventMsg, FrontendEvent, MessageReply, Op, SessionFileReference};
use mobius_gateway::wire::{ClientMessage, ServerMessage};

use super::storage::Storage;
use super::{Connection, Gateway, GatewayState};

/// The connection as the window presents it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Link {
    Connecting(String),
    Ready,
    /// The transport retries the same gateway; pages, drafts and the catalog stay.
    Reconnecting {
        endpoint: Option<String>,
        attempt: u32,
        message: String,
    },
    Failed(Arc<str>),
}

impl Link {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Connecting(_) | Self::Reconnecting { .. })
    }
}

/// The open or create request whose answer becomes the selected chat.
#[derive(Debug)]
struct PendingSelection {
    request_id: Arc<str>,
    session_id: Option<SessionId>,
}

/// A message sent from a chat's composer that the gateway has not taken up yet.
#[derive(Debug)]
pub struct PendingMessage {
    pub draft_key: String,
    pub text: String,
    /// The composer attachments it carries, by draft id.
    pub attachments: Vec<Arc<str>>,
    pub files: Vec<SessionFileReference>,
    /// Kept until delivery is confirmed so a refused send can restore its quote.
    pub reply: Option<(String, MessageReply, String)>,
}

/// What a server message means before the shared state applies it.
#[derive(Debug, PartialEq, Eq)]
pub enum Arrival {
    Apply,
    /// The answer to an open this window no longer waits for.
    Ignore,
    /// The gateway no longer holds the events since the last one this window has, so the
    /// chat must open in full.
    ResumeRejected,
    /// The chat this window asked for opened; it becomes the selection once applied.
    Opened,
}

#[derive(Debug)]
pub struct Client {
    pub gateway: Option<Gateway>,
    pub link: Link,
    pub state: GatewayState,
    pub storage: Storage,
    /// The endpoint of the gateway this window is connected to, or connecting to.
    pub endpoint: String,
    /// Counts connections; work started for an earlier one compares it before landing.
    pub epoch: u64,
    pub local_gateway: bool,
    /// Re-open the chat when the interrupted connection is Ready again.
    pub reopen: bool,
    /// The pending selection resumes the open chat after its last applied event.
    pub resuming: bool,
    /// The connection in progress pairs a new gateway.
    pub pairing: bool,
    selection: Option<PendingSelection>,
    messages: BTreeMap<Arc<str>, PendingMessage>,
}

impl Default for Client {
    fn default() -> Self {
        Self {
            gateway: None,
            link: Link::Connecting(String::new()),
            state: GatewayState::default(),
            storage: Storage::default(),
            endpoint: String::new(),
            epoch: 0,
            local_gateway: true,
            reopen: false,
            resuming: false,
            pairing: false,
            selection: None,
            messages: BTreeMap::new(),
        }
    }
}

impl Client {
    /// Starts the window's gateway transport; a failure leaves the link failed.
    pub fn start() -> Self {
        let (gateway, link) = match Gateway::start() {
            Ok(gateway) => (Some(gateway), Link::Connecting(String::new())),
            Err(error) => (None, Link::Failed(super::describe(&error).into())),
        };
        Self {
            gateway,
            link,
            ..Self::default()
        }
    }

    /// Connects to `connection`; `false` when it is the gateway already open and ready.
    ///
    /// # Errors
    ///
    /// A message for people when the transport is missing or refuses the connection.
    pub fn connect(&mut self, connection: Connection) -> Result<bool, String> {
        let current = match &connection {
            Connection::Local => self.local_gateway,
            Connection::Saved(endpoint) => !self.local_gateway && *endpoint == self.endpoint,
            Connection::Pair { .. } => false,
        };
        if current && self.link.is_ready() {
            return Ok(false);
        }
        let pairing = matches!(connection, Connection::Pair { .. });
        let local = matches!(connection, Connection::Local);
        self.gateway
            .as_ref()
            .ok_or_else(|| "Gateway is unavailable".to_owned())?
            .connect(connection)
            .map_err(|error| super::describe(&error))?;
        self.epoch += 1;
        self.local_gateway = local;
        self.pairing = pairing;
        Ok(true)
    }

    /// The transport reached `endpoint`; returns whether it resumes the interrupted one.
    pub fn connected(&mut self, endpoint: &str) -> bool {
        let resumed = self.reopen && endpoint == self.endpoint;
        endpoint.clone_into(&mut self.endpoint);
        resumed
    }

    /// After Ready: drops the chat a reconnect could not resume; returns whether this
    /// connection paired a new gateway.
    pub fn settle(&mut self) -> bool {
        if std::mem::take(&mut self.reopen) && !self.resuming {
            self.state.reset_session();
        }
        std::mem::take(&mut self.pairing)
    }

    /// Changes the link; leaving Ready abandons storage work in flight.
    pub fn set_link(&mut self, link: Link) {
        if !link.is_ready() {
            self.storage.disconnected();
        }
        self.link = link;
    }

    /// Where a chat's unsent text is kept: per gateway, so chats on two gateways never share.
    pub fn draft_key(&self, session_id: &str) -> String {
        format!("{}:{}{}", self.endpoint.len(), self.endpoint, session_id)
    }

    /// Sends `message`; `pending` is the composer message a `Submit` carries. Returns
    /// whether the message selects a chat. Before Ready an open waits for
    /// `resume_after_ready` and anything else is refused.
    ///
    /// # Errors
    ///
    /// A message for people before Ready, or when the transport is missing or refuses it.
    pub fn send(
        &mut self,
        message: ClientMessage,
        pending: Option<PendingMessage>,
    ) -> Result<bool, String> {
        if !self.link.is_ready() {
            if matches!(message, ClientMessage::OpenSession { .. }) {
                return Ok(false);
            }
            return Err("Wait for the gateway connection before sending.".into());
        }
        let selection = match &message {
            ClientMessage::OpenSession {
                request_id,
                session_id,
                ..
            } => Some(PendingSelection {
                request_id: Arc::clone(request_id),
                session_id: Some(session_id.share()),
            }),
            ClientMessage::CreateSession { request_id, .. } => Some(PendingSelection {
                request_id: Arc::clone(request_id),
                session_id: None,
            }),
            _ => None,
        };
        let submission = match &message {
            ClientMessage::Submit { submission, .. }
                if matches!(submission.op, Op::Message { .. }) =>
            {
                Some(Arc::clone(&submission.id))
            }
            _ => None,
        };
        self.gateway
            .as_ref()
            .ok_or_else(|| "Gateway is unavailable".to_owned())?
            .send(message)
            .map_err(|error| super::describe(&error))?;
        if let Some((id, pending)) = submission.zip(pending) {
            self.messages.insert(id, pending);
        }
        let selecting = selection.is_some();
        if selecting {
            self.selection = selection;
            self.resuming = false;
        }
        Ok(selecting)
    }

    /// Decides what `message` means for the selection before anything else sees it.
    pub fn arrive(&mut self, message: &mut ServerMessage) -> Arrival {
        if let ServerMessage::SessionsChanged { sessions, .. } = message {
            mobius_gateway::wire::apply_session_changes(
                &mut self.state.sessions,
                std::mem::take(sessions),
            );
        }
        match message {
            ServerMessage::Rejected { request_id, .. }
                if self.resuming && self.failed_request(request_id, true) =>
            {
                self.resuming = false;
                self.selection = None;
                self.state.reset_session();
                Arrival::ResumeRejected
            }
            ServerMessage::SessionOpened {
                request_id,
                payload,
            } if !self.accepts_session(request_id, &payload.session.session_id) => Arrival::Ignore,
            ServerMessage::SessionOpened { .. } => {
                self.selection = None;
                self.resuming = false;
                Arrival::Opened
            }
            _ => Arrival::Apply,
        }
    }

    /// Applies `message` to the shared state; returns the error it reported.
    pub fn apply(&mut self, message: &mut ServerMessage) -> Option<Arc<str>> {
        self.state.apply(message);
        self.state.error.take()
    }

    /// The gateway refused `request_id`; returns whether it was the selection's request.
    pub fn rejected(&mut self, request_id: &str) -> bool {
        self.messages.remove(request_id);
        let selection = self
            .selection
            .as_ref()
            .is_some_and(|selection| *selection.request_id == *request_id);
        if selection {
            self.selection = None;
        }
        selection
    }

    fn accepts_session(&self, request_id: &str, session_id: &SessionId) -> bool {
        self.selection.as_ref().is_some_and(|pending| {
            *pending.request_id == *request_id
                && pending
                    .session_id
                    .as_ref()
                    .is_none_or(|id| id == session_id)
        })
    }

    /// An open or create is waiting for its answer.
    pub fn selecting(&self) -> bool {
        self.selection.is_some()
    }

    /// The chat the pending open names; `Some(None)` while a new chat is being created.
    pub fn selection_target(&self) -> Option<Option<&SessionId>> {
        self.selection
            .as_ref()
            .map(|pending| pending.session_id.as_ref())
    }

    /// For the pending open or create answered by `request_id`: the chat it names, if any.
    pub fn selection_for(&self, request_id: &str) -> Option<Option<SessionId>> {
        self.selection
            .as_ref()
            .filter(|pending| *pending.request_id == *request_id)
            .map(|pending| pending.session_id.as_ref().map(SessionId::share))
    }

    pub fn cancel_selection(&mut self) {
        self.resuming = false;
        if self.selection.take().is_some() {
            // An ignored OpenSession may already have changed the gateway's selection.
            self.state.session = None;
        }
    }

    /// The connection reopens `session_id` right behind authentication; when this window
    /// holds that chat's events, it keeps them and takes only the newer ones.
    pub fn resume(&mut self, request_id: Arc<str>, session_id: SessionId) {
        if self.selection.is_none()
            && self.state.selected_session_id.as_ref() == Some(&session_id)
            && self.state.session.is_some()
            && !self.state.replaying
        {
            self.selection = Some(PendingSelection {
                request_id,
                session_id: Some(session_id),
            });
            self.resuming = true;
        }
    }

    /// A send that failed; a certain failure releases its message. Returns whether it was
    /// the selection's request.
    pub fn failed_request(&mut self, id: &str, delivery_uncertain: bool) -> bool {
        if !delivery_uncertain {
            self.messages.remove(id);
        }
        self.selection
            .as_ref()
            .is_some_and(|pending| *pending.request_id == *id)
    }

    /// The gateway took up a sent message: it recorded it, or holds it as a queued or
    /// steering widget until the turn reaches it (SwiftUI drops its pending draft on either).
    pub fn confirmed_message(&mut self, event: &mobius::protocol::Event) -> Option<PendingMessage> {
        let id = event.submission_id.as_deref()?;
        match event.msg {
            EventMsg::Message(_) | EventMsg::Frontend(FrontendEvent::Widget { .. }) => {
                self.messages.remove(id)
            }
            EventMsg::SubmissionRejected(_) => {
                self.messages.remove(id);
                None
            }
            _ => None,
        }
    }

    /// Messages sent from the draft at `key` that the gateway has not taken up yet.
    pub fn pending_messages(&self, key: &str) -> impl Iterator<Item = (&str, &PendingMessage)> {
        self.messages
            .iter()
            .filter(move |(_, message)| message.draft_key == key)
            .map(|(id, message)| (&**id, message))
    }

    pub fn pending_submission(&self, id: &str) -> bool {
        self.messages.contains_key(id)
    }

    /// Takes back a message the gateway refused.
    pub fn take_pending(&mut self, id: &str) -> Option<PendingMessage> {
        self.messages.remove(id)
    }

    /// Shows a message as sent and not yet taken up.
    #[cfg(feature = "snapshot")]
    pub fn insert_pending(&mut self, id: Arc<str>, message: PendingMessage) {
        self.messages.insert(id, message);
    }

    #[cfg(feature = "snapshot")]
    pub fn clear_pending(&mut self) {
        self.messages.clear();
    }

    /// An attachment still travels with an unconfirmed message from the draft at `key`.
    pub fn attachment_pending(&self, key: &str, id: &str) -> bool {
        self.pending_messages(key).any(|(_, message)| {
            message
                .attachments
                .iter()
                .any(|attachment| **attachment == *id)
        })
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
