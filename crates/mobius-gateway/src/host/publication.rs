//! Shared gateway broadcasts and post-mutation publication results.

use super::rejection::{Rejection, internal};

use crate::wire::{ServerFrame, ServerMessage, SessionRecord};

use std::sync::Arc;
use tokio::sync::broadcast;
/// One gateway-wide broadcast. Catalog changes are built once and shared by every
/// connection, which writes each as the frame its client needs.
#[derive(Clone)]
pub(crate) enum GatewayUpdate {
    Frame(Arc<ServerFrame>),
    /// The whole session catalog.
    Sessions(SessionCatalogUpdate),
    /// The Bot catalog changed; each connection reads the current one when it writes.
    BotsChanged,
}

/// A broadcast session catalog with its content revision, computed once.
#[derive(Clone)]
pub(crate) struct SessionCatalogUpdate {
    pub(crate) sessions: Arc<[SessionRecord]>,
    pub(crate) revision: Arc<str>,
}

#[cfg(test)]
impl GatewayUpdate {
    /// The message a client without a held catalog would receive.
    pub(crate) fn message(self) -> ServerMessage {
        match self {
            Self::Frame(frame) => Arc::unwrap_or_clone(frame).message,
            Self::Sessions(catalog) => ServerMessage::Sessions {
                request_id: None,
                sessions: catalog.sessions.to_vec(),
            },
            Self::BotsChanged => ServerMessage::Bots {
                request_id: None,
                bots: Vec::new(),
            },
        }
    }
}

pub(super) fn broadcast(events: &broadcast::Sender<GatewayUpdate>, update: GatewayUpdate) {
    if let Err(error) = events.send(update) {
        tracing::debug!(%error, "gateway event had no subscribers");
    }
}

pub(super) fn send_event(events: &broadcast::Sender<GatewayUpdate>, message: ServerMessage) {
    broadcast(
        events,
        GatewayUpdate::Frame(Arc::new(ServerFrame::new(message))),
    );
}

/// Publishes the session list and the background approvals it carries.
pub(super) fn send_catalog(
    events: &broadcast::Sender<GatewayUpdate>,
    sessions: Vec<SessionRecord>,
    approvals: Vec<Arc<crate::wire::BackgroundApproval>>,
) {
    let revision = crate::wire::content_revision(&sessions).into();
    broadcast(
        events,
        GatewayUpdate::Sessions(SessionCatalogUpdate {
            sessions: sessions.into(),
            revision,
        }),
    );
    send_event(events, ServerMessage::BackgroundApprovals { approvals });
}

/// Tells clients about a failure that leaves the gateway usable.
pub(super) fn send_warning(
    events: &broadcast::Sender<GatewayUpdate>,
    code: &'static str,
    message: Arc<str>,
) {
    send_event(
        events,
        ServerMessage::Error {
            code: code.into(),
            message,
            fatal: false,
        },
    );
}

// Live mutations must complete follow-up effects without losing an earlier applied-write failure.
pub(crate) fn finish_publication<T>(
    publication: crate::Result<()>,
    follow_up: std::result::Result<T, Rejection>,
) -> std::result::Result<T, Rejection> {
    match (publication, follow_up) {
        (Ok(()), result) => result,
        (Err(error), Ok(_)) => Err(internal(error)),
        (Err(error), Err(follow_up)) => Err(internal(format!(
            "{error}; follow-up failed ({}): {}",
            follow_up.code, follow_up.message
        ))),
    }
}
