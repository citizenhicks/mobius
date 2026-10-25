//! Native requests travel over the desktop app's existing authenticated connection.

use std::sync::{Arc, Mutex};

use mobius::{Error, Result, SessionId};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::wire::ServerMessage;

pub(crate) struct DesktopControl {
    runtime: Mutex<Option<Runtime>>,
    control: Arc<tokio::sync::Mutex<()>>,
    /// Chats whose computer control found no attached desktop host.
    host_requests: tokio::sync::broadcast::Sender<SessionId>,
}

impl Default for DesktopControl {
    fn default() -> Self {
        Self {
            runtime: Mutex::default(),
            control: Arc::default(),
            host_requests: tokio::sync::broadcast::channel(4).0,
        }
    }
}

struct Runtime {
    id: Uuid,
    outgoing: mpsc::Sender<ServerMessage>,
    pending: Option<(Uuid, oneshot::Sender<Value>)>,
}

pub(crate) struct DesktopConnection {
    control: Arc<DesktopControl>,
    id: Uuid,
    pub(crate) outgoing: mpsc::Receiver<ServerMessage>,
}

impl DesktopControl {
    pub(crate) fn attach(self: &Arc<Self>) -> Result<DesktopConnection> {
        let mut runtime = self.runtime.lock().map_err(unavailable_after)?;
        if runtime.is_some() {
            return Err(Error::Sandbox(
                "another Mac app already hosts desktop control".into(),
            ));
        }
        let id = Uuid::new_v4();
        let (outgoing, receiver) = mpsc::channel(4);
        *runtime = Some(Runtime {
            id,
            outgoing,
            pending: None,
        });
        drop(runtime);
        Ok(DesktopConnection {
            control: Arc::clone(self),
            id,
            outgoing: receiver,
        })
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
            let guard = self.runtime.lock().map_err(unavailable_after)?;
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
            let size = channel.read_u32().await? as usize;
            if size == 0 || size > 1024 * 1024 {
                return Err(unavailable());
            }
            let mut request = vec![0; size];
            channel.read_exact(&mut request).await?;
            let request: Value = serde_json::from_slice(&request)?;
            let request_id = Uuid::new_v4();
            let (reply, response) = oneshot::channel();
            {
                let mut guard = self.runtime.lock().map_err(unavailable_after)?;
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
            channel
                .write_u32(u32::try_from(response.len()).map_err(unavailable_after)?)
                .await?;
            channel.write_all(&response).await?;
        }
    }

    fn clear_pending(&self, id: Uuid) {
        if let Ok(mut runtime) = self.runtime.lock()
            && let Some(runtime) = runtime.as_mut().filter(|runtime| runtime.id == id)
        {
            runtime.pending = None;
        }
    }
}

impl DesktopConnection {
    pub(crate) fn reply(&self, request_id: &str, response: Value) -> Result<()> {
        let mut state = self.control.runtime.lock().map_err(unavailable_after)?;
        let runtime = state
            .as_mut()
            .filter(|runtime| runtime.id == self.id)
            .ok_or_else(unavailable)?;
        let mut buffer = Uuid::encode_buffer();
        let (_, reply) = runtime
            .pending
            .take_if(|(id, _)| *id.hyphenated().encode_lower(&mut buffer) == *request_id)
            .ok_or_else(|| {
                Error::Sandbox("desktop reply is stale or belongs to another evaluation".into())
            })?;
        drop(state);
        if reply.send(response).is_err() {
            tracing::debug!(
                request_id,
                "desktop evaluation ended before its reply arrived"
            );
        }
        Ok(())
    }
}

impl Drop for DesktopConnection {
    fn drop(&mut self) {
        if let Ok(mut runtime) = self.control.runtime.lock()
            && runtime
                .as_ref()
                .is_some_and(|runtime| runtime.id == self.id)
        {
            *runtime = None;
        }
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

pub(crate) async fn write_update(
    connection: &mut Option<DesktopConnection>,
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

pub(crate) async fn handle_message(
    message: crate::wire::ClientMessage,
    control: &Arc<DesktopControl>,
    connection: &mut Option<DesktopConnection>,
    local: bool,
    client: crate::wire::ClientCapabilities,
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> crate::Result<()> {
    use crate::wire::{ClientMessage, ServerFrame, write_frame};
    let (request_id, result) = match message {
        ClientMessage::SetDesktopRuntime {
            request_id,
            enabled,
        } => {
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
            (request_id, result)
        }
        ClientMessage::DesktopControlReply {
            request_id,
            response,
        } => {
            let result = connection
                .as_ref()
                .ok_or_else(unavailable)
                .and_then(|connection| connection.reply(&request_id, response));
            // Replies are already correlated to the request; do not acknowledge each image again.
            if result.is_ok() {
                return Ok(());
            }
            (request_id, result)
        }
        _ => unreachable!("desktop dispatch accepts only desktop messages"),
    };
    let response = match result {
        Ok(()) => ServerMessage::Accepted { request_id },
        Err(error) => ServerMessage::Rejected {
            request_id,
            code: "desktop_control".into(),
            message: error.to_string().into(),
            fatal: false,
        },
    };
    write_frame(writer, &ServerFrame::new(response)).await?;
    Ok(())
}
