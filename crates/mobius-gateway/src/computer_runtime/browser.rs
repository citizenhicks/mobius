//! The gateway's per-chat browser assignment rendered by the local Mac app.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use mobius::{Error, Result};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::wire::ServerMessage;

/// How long a call waits for the app's page before using the worker's own browser.
const PAGE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PENDING_PAGES: usize = 8;

#[derive(Default)]
pub(crate) struct BrowserHost {
    app: Mutex<Option<App>>,
}

struct App {
    id: Uuid,
    outgoing: mpsc::Sender<ServerMessage>,
    pending: HashMap<Uuid, oneshot::Sender<Option<String>>>,
}

/// The renderer connection. Dropping it withdraws the browser and answers waiting
/// calls with none.
pub(crate) struct BrowserConnection {
    host: Arc<BrowserHost>,
    id: Uuid,
    pub(crate) outgoing: mpsc::Receiver<ServerMessage>,
}

impl BrowserHost {
    fn app(&self) -> MutexGuard<'_, Option<App>> {
        mobius::sync::recover_lock(&self.app)
    }

    /// Registers this renderer; a newer connection replaces it.
    pub(crate) fn attach(self: &Arc<Self>) -> BrowserConnection {
        let id = Uuid::new_v4();
        let (outgoing, receiver) = mpsc::channel(MAX_PENDING_PAGES);
        *self.app() = Some(App {
            id,
            outgoing,
            pending: HashMap::new(),
        });
        BrowserConnection {
            host: Arc::clone(self),
            id,
            outgoing: receiver,
        }
    }

    /// Requests the renderer's assigned page for this chat.
    pub(crate) async fn page(&self, session_id: &str, foreground: bool) -> Option<String> {
        let request = Uuid::new_v4();
        let (reply, answer) = oneshot::channel();
        {
            let mut app = self.app();
            let app = app.as_mut()?;
            // Cancelled calls need no reply; keep their senders only until the next request.
            app.pending.retain(|_, reply| !reply.is_closed());
            if app.pending.len() >= MAX_PENDING_PAGES {
                return None;
            }
            app.outgoing
                .try_send(ServerMessage::BrowserPageRequested {
                    request_id: request.to_string(),
                    session_id: session_id.to_owned(),
                    foreground,
                })
                .ok()?;
            app.pending.insert(request, reply);
        }
        let endpoint = tokio::time::timeout(PAGE_TIMEOUT, answer)
            .await
            .ok()
            .and_then(|answer| answer.ok())
            .flatten();
        if let Some(app) = self.app().as_mut() {
            app.pending.remove(&request);
        }
        endpoint
    }
}

impl BrowserConnection {
    fn reply(&self, request_id: &str, endpoint: Option<String>) -> Result<()> {
        let stale = || Error::Sandbox("browser reply is stale or belongs to another app".into());
        let request: Uuid = request_id.parse().map_err(|_| stale())?;
        let reply = self
            .host
            .app()
            .as_mut()
            .filter(|app| app.id == self.id)
            .and_then(|app| app.pending.remove(&request))
            .ok_or_else(stale)?;
        let _ = reply.send(endpoint.filter(|endpoint| scoped_endpoint(endpoint)));
        Ok(())
    }
}

impl Drop for BrowserConnection {
    fn drop(&mut self) {
        let mut app = self.host.app();
        if app.as_ref().is_some_and(|app| app.id == self.id) {
            *app = None;
        }
    }
}

/// Only a scoped page on a local Unix socket is accepted; no debugging port is exposed.
fn scoped_endpoint(endpoint: &str) -> bool {
    let Some((socket, token)) = endpoint
        .strip_prefix("ws+unix://")
        .and_then(|value| value.rsplit_once(":/"))
    else {
        return false;
    };
    let path = std::path::Path::new(socket);
    endpoint.len() <= 4096
        && !endpoint.contains(char::is_whitespace)
        && !socket.contains([':', '?', '#', '%', '\\'])
        && !socket.split('/').any(|part| part == "." || part == "..")
        && path.is_absolute()
        && path.file_name().is_some_and(|name| name == "browser.sock")
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
        && token.len() == 32
        && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) async fn next_update(
    connection: &mut Option<BrowserConnection>,
) -> Option<ServerMessage> {
    match connection {
        Some(connection) => connection.outgoing.recv().await,
        None => std::future::pending().await,
    }
}

pub(crate) async fn write_update(
    connection: &mut Option<BrowserConnection>,
    message: Option<ServerMessage>,
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> crate::Result<()> {
    match message {
        Some(message) => {
            crate::wire::write_frame(writer, &crate::wire::ServerFrame::new(message)).await
        }
        None => {
            *connection = None;
            Ok(())
        }
    }
}

pub(crate) async fn handle_message(
    message: crate::wire::ClientMessage,
    host: &Arc<BrowserHost>,
    connection: &mut Option<BrowserConnection>,
    local: bool,
    client_kind: crate::wire::ClientKind,
    available: bool,
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> crate::Result<()> {
    use crate::wire::{ClientMessage, ServerFrame, write_frame};
    let (request_id, result) = match message {
        ClientMessage::SetBrowserRuntime {
            request_id,
            enabled,
        } => {
            let result = if !(available
                && cfg!(target_os = "macos")
                && local
                && client_kind == crate::wire::ClientKind::Macos)
            {
                Err(Error::Sandbox(
                    "only the local Mac app can register its browser renderer".into(),
                ))
            } else {
                *connection = enabled.then(|| host.attach());
                Ok(())
            };
            (request_id, result)
        }
        ClientMessage::BrowserPageReply {
            request_id,
            endpoint,
        } => {
            let result = connection
                .as_ref()
                .ok_or_else(|| {
                    Error::Sandbox("this connection has no registered browser renderer".into())
                })
                .and_then(|connection| connection.reply(&request_id, endpoint));
            // A reply answers its request; only a stray one is worth a response.
            if result.is_ok() {
                return Ok(());
            }
            (request_id, result)
        }
        _ => unreachable!("browser dispatch accepts only browser messages"),
    };
    let response = match result {
        Ok(()) => ServerMessage::Accepted { request_id },
        Err(error) => ServerMessage::Rejected {
            request_id,
            code: "browser".into(),
            message: error.to_string(),
            fatal: false,
        },
    };
    write_frame(writer, &ServerFrame::new(response)).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
