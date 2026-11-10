//! Native requests travel over the desktop app's existing authenticated connection.

use std::sync::Arc;

use mobius::{Error, Result, SessionId};
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, DuplexStream};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use super::app_bridge::{self, AppBridge, AppConnection, Occupied};
use crate::wire::ServerMessage;

/// The desktop request awaiting the app's reply.
type Pending = Option<(Uuid, oneshot::Sender<Value>)>;

pub(crate) type DesktopConnection = AppConnection<Pending>;

pub(crate) struct DesktopControl {
    app: Arc<AppBridge<Pending>>,
    control: Arc<tokio::sync::Mutex<()>>,
    /// Chats whose computer control found no attached desktop host.
    host_requests: tokio::sync::broadcast::Sender<SessionId>,
}

impl Default for DesktopControl {
    fn default() -> Self {
        Self {
            app: Arc::new(AppBridge::new("desktop control")),
            control: Arc::default(),
            host_requests: tokio::sync::broadcast::channel(4).0,
        }
    }
}

impl DesktopControl {
    pub(crate) fn attach(&self) -> Result<DesktopConnection> {
        self.app.attach(
            4,
            Occupied::Refuse("another Mac app already hosts desktop control"),
        )
    }

    /// Chats that asked for a desktop host while none was attached.
    pub(crate) fn host_requests(&self) -> tokio::sync::broadcast::Receiver<SessionId> {
        self.host_requests.subscribe()
    }

    pub(crate) fn connect(self: &Arc<Self>, session_id: &SessionId) -> Result<DuplexStream> {
        let lease = Arc::clone(&self.control).try_lock_owned().map_err(|busy| {
            Error::Sandbox(format!(
                "another Bot is controlling this Mac ({busy}); inspect again after it finishes"
            ))
        })?;
        let (runtime_id, outgoing) = {
            let guard = self.app.app()?;
            let Some(runtime) = guard.as_ref() else {
                drop(guard);
                if self.host_requests.send(session_id.share()).is_err() {
                    tracing::debug!("no desktop-host connection can attach for this chat");
                }
                return Err(unavailable());
            };
            let attached = (runtime.id, runtime.outgoing.clone()); // rules-allow: the relay task owns a sender
            drop(guard);
            attached
        };
        let session_id = session_id.share();
        let execution_id = Uuid::new_v4().to_string();
        let control = Arc::clone(self);
        let (worker, mut relay) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let _lease = lease;
            if let Err(error) = control
                .relay(
                    &mut relay,
                    runtime_id,
                    &outgoing,
                    &session_id,
                    &execution_id,
                )
                .await
            {
                tracing::debug!(%error, "desktop control relay ended");
            }
            control.clear_pending(runtime_id);
            if let Err(error) = outgoing
                .send(ServerMessage::DesktopControlEnded { execution_id })
                .await
            {
                tracing::debug!(%error, "desktop app left before the evaluation ended");
            }
        });
        Ok(worker)
    }

    async fn relay(
        &self,
        channel: &mut DuplexStream,
        runtime_id: Uuid,
        outgoing: &mpsc::Sender<ServerMessage>,
        session_id: &SessionId,
        execution_id: &str,
    ) -> Result<()> {
        loop {
            let request = app_bridge::read_frame(channel, 1024 * 1024)
                .await?
                .ok_or_else(unavailable)?;
            let request: Value = serde_json::from_slice(&request)?;
            let request_id = Uuid::new_v4();
            let (reply, response) = oneshot::channel();
            {
                let mut guard = self.app.app()?;
                let runtime = guard
                    .as_mut()
                    .filter(|runtime| runtime.id == runtime_id)
                    .ok_or_else(unavailable)?;
                runtime.pending = Some((request_id, reply));
                drop(guard);
            }
            outgoing
                .send(ServerMessage::DesktopControlRequested {
                    request_id: request_id.to_string().into(),
                    execution_id: execution_id.into(),
                    session_id: session_id.share(),
                    request,
                })
                .await
                .map_err(unavailable_after)?;
            let response = tokio::select! {
                response = response => response.map_err(unavailable_after)?,
                // A worker must wait for this reply. EOF or pipelining ends this evaluation.
                _ = channel.read_u8() => return Ok(()),
            };
            let response = serde_json::to_vec(&response)?;
            if response.len() > crate::wire::MAX_FRAME_BYTES {
                return Err(unavailable());
            }
            app_bridge::write_frame(channel, &response).await?;
        }
    }

    fn clear_pending(&self, id: Uuid) {
        if let Ok(mut runtime) = self.app.app()
            && let Some(runtime) = runtime.as_mut().filter(|runtime| runtime.id == id)
        {
            runtime.pending = None;
        }
    }
}

impl DesktopConnection {
    pub(crate) fn reply(&self, request_id: &str, response: Value) -> Result<()> {
        let mut buffer = Uuid::encode_buffer();
        let reply = self
            .with_app(|runtime| {
                runtime.ok_or_else(unavailable).map(|runtime| {
                    runtime.pending.take_if(|(id, _)| {
                        *id.hyphenated().encode_lower(&mut buffer) == *request_id
                    })
                })
            })??
            .ok_or_else(|| {
                Error::Sandbox("desktop reply is stale or belongs to another evaluation".into())
            })?;
        if reply.1.send(response).is_err() {
            tracing::debug!(
                request_id,
                "desktop evaluation ended before its reply arrived"
            );
        }
        Ok(())
    }
}

const UNAVAILABLE: &str =
    "Mac desktop control is unavailable; open the möbius desktop app and enable desktop control";

fn unavailable() -> Error {
    Error::Sandbox(UNAVAILABLE.into())
}

fn unavailable_after(error: impl std::fmt::Display) -> Error {
    Error::Sandbox(format!("{UNAVAILABLE} ({error})"))
}

#[cfg(test)]
mod tests;

/// The next chat asking a desktop-host connection to attach.
async fn next_host_request(
    requests: &mut Option<tokio::sync::broadcast::Receiver<SessionId>>,
) -> ServerMessage {
    match requests {
        Some(requests) => loop {
            match requests.recv().await {
                Ok(session_id) => return ServerMessage::DesktopHostRequested { session_id },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return std::future::pending().await;
                }
            }
        },
        None => std::future::pending().await,
    }
}

/// The attached runtime's next request, or, while none is attached, the next chat asking a
/// desktop host to attach.
pub(crate) async fn next_update(
    connection: &mut Option<DesktopConnection>,
    host_requests: &mut Option<tokio::sync::broadcast::Receiver<SessionId>>,
) -> Option<ServerMessage> {
    match connection {
        Some(connection) => connection.outgoing.recv().await,
        None => Some(next_host_request(host_requests).await),
    }
}

/// Registers or withdraws this connection as the gateway's local desktop host.
pub(crate) async fn set_runtime(
    request_id: Arc<str>,
    enabled: bool,
    control: &Arc<DesktopControl>,
    connection: &mut Option<DesktopConnection>,
    local: bool,
    client: crate::wire::ClientCapabilities,
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> crate::Result<()> {
    let result = if !(cfg!(target_os = "macos") && local && client.desktop_host) {
        Err(Error::Sandbox(
            "only a local desktop host can host desktop control on this gateway".into(),
        ))
    } else if !enabled {
        *connection = None;
        Ok(())
    } else if connection.is_none() {
        control
            .attach()
            .map(|attached| *connection = Some(attached))
    } else {
        Ok(())
    };
    app_bridge::write_outcome(writer, request_id, "desktop_control", result).await
}

/// Delivers the desktop host's answer to one control request.
pub(crate) async fn reply(
    request_id: Arc<str>,
    response: serde_json::Value,
    connection: Option<&DesktopConnection>,
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> crate::Result<()> {
    let result = connection
        .ok_or_else(unavailable)
        .and_then(|connection| connection.reply(&request_id, response));
    // Replies are already correlated to the request; do not acknowledge each image again.
    if result.is_ok() {
        return Ok(());
    }
    app_bridge::write_outcome(writer, request_id, "desktop_control", result).await
}
