//! The gateway's per-chat browser assignment rendered by the local Mac app.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use mobius::{Error, Result, SessionId};
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
    fn app(&self) -> Result<MutexGuard<'_, Option<App>>> {
        self.app
            .lock()
            .map_err(|_poisoned| Error::Poisoned("browser renderer"))
    }

    /// Registers this renderer; a newer connection replaces it.
    pub(crate) fn attach(self: &Arc<Self>) -> Result<BrowserConnection> {
        let id = Uuid::new_v4();
        let (outgoing, receiver) = mpsc::channel(MAX_PENDING_PAGES);
        *self.app()? = Some(App {
            id,
            outgoing,
            pending: HashMap::new(),
        });
        Ok(BrowserConnection {
            host: Arc::clone(self),
            id,
            outgoing: receiver,
        })
    }

    /// Requests the renderer's assigned page for this chat.
    pub(crate) async fn page(&self, session_id: &SessionId, foreground: bool) -> Option<String> {
        let request = Uuid::new_v4();
        let (reply, answer) = oneshot::channel();
        {
            let mut guard = self.app().ok()?;
            let app = guard.as_mut()?;
            // Cancelled calls need no reply; keep their senders only until the next request.
            app.pending.retain(|_, reply| !reply.is_closed());
            if app.pending.len() >= MAX_PENDING_PAGES {
                return None;
            }
            app.outgoing
                .try_send(ServerMessage::BrowserPageRequested {
                    request_id: request.to_string().into(),
                    session_id: session_id.share(),
                    foreground,
                })
                .ok()?;
            app.pending.insert(request, reply);
            drop(guard);
        }
        let endpoint = tokio::time::timeout(PAGE_TIMEOUT, answer)
            .await
            .ok()
            .and_then(std::result::Result::ok)
            .flatten();
        if let Ok(mut app) = self.app()
            && let Some(app) = app.as_mut()
        {
            app.pending.remove(&request);
        }
        endpoint
    }
}

impl BrowserConnection {
    fn reply(&self, request_id: &str, endpoint: Option<String>) -> Result<()> {
        let stale = || Error::Sandbox("browser reply is stale or belongs to another app".into());
        let request: Uuid = request_id.parse().map_err(|error| {
            Error::Sandbox(format!(
                "browser reply is stale or belongs to another app: {error}"
            ))
        })?;
        let reply = self
            .host
            .app()?
            .as_mut()
            .filter(|app| app.id == self.id)
            .and_then(|app| app.pending.remove(&request))
            .ok_or_else(stale)?;
        if reply
            .send(endpoint.filter(|endpoint| scoped_endpoint(endpoint)))
            .is_err()
        {
            tracing::debug!(
                request_id,
                "browser page call ended before its reply arrived"
            );
        }
        Ok(())
    }
}

impl Drop for BrowserConnection {
    fn drop(&mut self) {
        // A poisoned renderer slot is never read; the next attach reports it.
        if let Ok(mut app) = self.host.app()
            && app.as_ref().is_some_and(|app| app.id == self.id)
        {
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
    if let Some(message) = message {
        crate::wire::write_frame(writer, &crate::wire::ServerFrame::new(message)).await
    } else {
        *connection = None;
        Ok(())
    }
}

/// A connection's request to lend, or stop lending, its browser renderer.
pub(crate) struct Registration {
    pub(crate) request_id: Arc<str>,
    pub(crate) enabled: bool,
    pub(crate) local: bool,
    pub(crate) client: crate::wire::ClientCapabilities,
    /// Whether this gateway can use a lent browser at all.
    pub(crate) available: bool,
}

/// Registers or withdraws this connection as the gateway's local browser renderer.
pub(crate) async fn set_runtime(
    registration: Registration,
    host: &Arc<BrowserHost>,
    connection: &mut Option<BrowserConnection>,
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> crate::Result<()> {
    let Registration {
        request_id,
        enabled,
        local,
        client,
        available,
    } = registration;
    let result = if available && cfg!(target_os = "macos") && local && client.browser_renderer {
        if enabled {
            host.attach().map(|attached| *connection = Some(attached))
        } else {
            *connection = None;
            Ok(())
        }
    } else {
        Err(Error::Sandbox(
            "only a local browser renderer can register on this gateway".into(),
        ))
    };
    write_outcome(writer, request_id, result).await
}

/// Delivers the renderer's answer to one page request.
pub(crate) async fn reply(
    request_id: Arc<str>,
    endpoint: Option<String>,
    connection: Option<&BrowserConnection>,
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> crate::Result<()> {
    let result = connection
        .ok_or_else(|| Error::Sandbox("this connection has no registered browser renderer".into()))
        .and_then(|connection| connection.reply(&request_id, endpoint));
    // A reply answers its request; only a stray one is worth a response.
    if result.is_ok() {
        return Ok(());
    }
    write_outcome(writer, request_id, result).await
}

async fn write_outcome(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    request_id: Arc<str>,
    result: Result<()>,
) -> crate::Result<()> {
    use crate::wire::{ServerFrame, write_frame};
    let response = match result {
        Ok(()) => ServerMessage::Accepted { request_id },
        Err(error) => ServerMessage::Rejected {
            request_id,
            code: "browser".into(),
            message: error.to_string().into(),
            fatal: false,
        },
    };
    write_frame(writer, &ServerFrame::new(response)).await
}

#[cfg(test)]
mod tests;
