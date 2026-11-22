//! Request/reply routing on one authenticated connection. The foreground reader still
//! receives unsolicited events and its own replies.
use mobius_gateway::client::{GatewayClient, GatewayEvents, GatewaySender};
use mobius_gateway::wire::{ClientMessage, ServerFrame, ServerMessage};
use mobius_gateway::{Error, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use uuid::Uuid;

pub(super) enum Usage {
    Shared,
    Dedicated,
}

/// Dropping the reader wakes all jobs, even when a job still holds a sender.
pub(super) struct Link {
    pub(super) requests: Arc<Requests>,
    events: GatewayEvents,
}

impl Link {
    pub(super) fn new(client: GatewayClient, usage: Usage) -> Self {
        let (sender, events) = client.into_parts();
        Self {
            requests: Arc::new(Requests {
                sender,
                usage,
                waiting: Mutex::new(Some(HashMap::new())),
            }),
            events,
        }
    }
    pub(super) async fn next(&mut self) -> Result<Option<ServerFrame>> {
        while let Some(frame) = self.events.next().await? {
            if matches!(
                &frame.message,
                ServerMessage::Error { fatal: true, .. }
                    | ServerMessage::Rejected { fatal: true, .. }
            ) {
                self.requests.close();
                return Ok(Some(frame));
            }
            // The prefix also keeps a late reply to a cancelled job out of the foreground.
            let Some(id) = response_id(&frame.message).and_then(|s| s.strip_prefix('~')) else {
                return Ok(Some(frame));
            };
            let reply = Uuid::parse_str(id)
                .ok()
                .and_then(|id| self.requests.waiting.lock().ok()?.as_mut()?.remove(&id));
            if let Some(reply) = reply
                && reply.send(frame.message).is_err()
            {
                tracing::trace!("background request cancelled before delivery");
            }
        }
        Ok(None)
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.requests.close();
    }
}

pub(super) struct Requests {
    pub(super) sender: GatewaySender,
    waiting: Mutex<Option<HashMap<Uuid, oneshot::Sender<ServerMessage>>>>,
    usage: Usage,
}

impl Requests {
    pub(super) fn is_open(&self) -> bool {
        self.waiting.lock().is_ok_and(|waiting| waiting.is_some())
    }
    fn close(&self) {
        if let Ok(mut waiting) = self.waiting.lock() {
            *waiting = None;
        }
    }
    pub(super) async fn request(
        &self,
        build: impl FnOnce(Arc<str>) -> ClientMessage,
    ) -> Result<ServerMessage> {
        let id = Uuid::new_v4();
        let (reply, response) = oneshot::channel();
        self.waiting
            .lock()
            .map_err(|_poisoned| disconnected())?
            .as_mut()
            .ok_or_else(disconnected)?
            .insert(id, reply);
        let _waiting = Waiting { requests: self, id };
        // 33 ASCII bytes, including the background marker. No second identifier on the wire.
        let mut bytes = [b'~'; 33];
        id.simple().encode_lower(&mut bytes[1..]);
        let request_id =
            std::str::from_utf8(&bytes).map_err(|error| Error::Internal(error.to_string()))?;
        if let Err(error) = self.sender.send(build(request_id.into())).await {
            self.close();
            return Err(error);
        }
        let Ok(result) = tokio::time::timeout(Duration::from_secs(30), response).await else {
            if matches!(self.usage, Usage::Dedicated) {
                self.close();
            }
            return Err(Error::Protocol("Gateway request timed out.".into()));
        };
        let message = result.map_err(|_closed| disconnected())?;
        if let Some(error) = message.response_error(Some(request_id)) {
            if matches!(&message, ServerMessage::Rejected { code, .. } if code == "storage_full") {
                return Err(mobius::Error::StorageFull.into());
            }
            return Err(Error::Protocol(error.message.into()));
        }
        Ok(message)
    }
}

struct Waiting<'a> {
    requests: &'a Requests,
    id: Uuid,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Ok(mut waiting) = self.requests.waiting.lock()
            && let Some(waiting) = waiting.as_mut()
        {
            waiting.remove(&self.id);
        }
    }
}

pub(super) fn disconnected() -> Error {
    Error::Protocol("Gateway disconnected during the request.".into())
}

fn response_id(message: &ServerMessage) -> Option<&str> {
    match message {
        ServerMessage::SessionOpened { request_id, .. }
        | ServerMessage::SessionFileChunk { request_id, .. }
        | ServerMessage::SessionFileUploadReady { request_id, .. }
        | ServerMessage::SessionFileUploadChunkAccepted { request_id, .. }
        | ServerMessage::SessionFileUploadCompleted { request_id, .. }
        | ServerMessage::GitDiff { request_id, .. }
        | ServerMessage::GitDiffTotals { request_id, .. }
        | ServerMessage::WorkspaceFiles { request_id, .. }
        | ServerMessage::SessionFiles { request_id, .. }
        | ServerMessage::WorkspaceFileChunk { request_id, .. }
        | ServerMessage::SessionHistory { request_id, .. }
        | ServerMessage::Accepted { request_id }
        | ServerMessage::Rejected { request_id, .. } => Some(request_id),
        _ => None,
    }
}
