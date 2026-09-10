//! Native requests travel over the menu bar app's existing authenticated connection.

use std::sync::{Arc, Mutex};

use mobius::{Error, Result};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::wire::ServerMessage;

#[derive(Default)]
pub(crate) struct DesktopControl {
    runtime: Mutex<Option<Runtime>>,
    control: Arc<tokio::sync::Mutex<()>>,
}

struct Runtime {
    id: Uuid,
    outgoing: mpsc::Sender<ServerMessage>,
    pending: Option<(String, oneshot::Sender<Value>)>,
}

pub(crate) struct DesktopConnection {
    control: Arc<DesktopControl>,
    id: Uuid,
    pub(crate) outgoing: mpsc::Receiver<ServerMessage>,
}

impl DesktopControl {
    pub(crate) fn attach(self: &Arc<Self>) -> Result<DesktopConnection> {
        let mut runtime = self.runtime.lock().map_err(|_| unavailable())?;
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
        Ok(DesktopConnection {
            control: Arc::clone(self),
            id,
            outgoing: receiver,
        })
    }

    pub(crate) fn connect(self: &Arc<Self>, session_id: &str) -> Result<DuplexStream> {
        let lease = Arc::clone(&self.control).try_lock_owned().map_err(|_| {
            Error::Sandbox(
                "another Bot is controlling this Mac; inspect again after it finishes".into(),
            )
        })?;
        let (runtime_id, outgoing) = {
            let runtime = self.runtime.lock().map_err(|_| unavailable())?;
            let runtime = runtime.as_ref().ok_or_else(unavailable)?;
            (runtime.id, runtime.outgoing.clone())
        };
        let session_id = session_id.to_owned();
        let execution_id = Uuid::new_v4().to_string();
        let control = Arc::clone(self);
        let (worker, mut relay) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let _lease = lease;
            let _ = control
                .relay(
                    &mut relay,
                    runtime_id,
                    &outgoing,
                    &session_id,
                    &execution_id,
                )
                .await;
            control.clear_pending(runtime_id);
            let _ = outgoing
                .send(ServerMessage::DesktopControlEnded { execution_id })
                .await;
        });
        Ok(worker)
    }

    async fn relay(
        &self,
        channel: &mut DuplexStream,
        runtime_id: Uuid,
        outgoing: &mpsc::Sender<ServerMessage>,
        session_id: &str,
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
            let request_id = Uuid::new_v4().to_string();
            let (reply, response) = oneshot::channel();
            {
                let mut runtime = self.runtime.lock().map_err(|_| unavailable())?;
                let runtime = runtime
                    .as_mut()
                    .filter(|runtime| runtime.id == runtime_id)
                    .ok_or_else(unavailable)?;
                runtime.pending = Some((request_id.clone(), reply));
            }
            outgoing
                .send(ServerMessage::DesktopControlRequested {
                    request_id,
                    execution_id: execution_id.into(),
                    session_id: session_id.into(),
                    request,
                })
                .await
                .map_err(|_| unavailable())?;
            let response = tokio::select! {
                response = response => response.map_err(|_| unavailable())?,
                // A worker must wait for this reply. EOF or pipelining ends this evaluation.
                _ = channel.read_u8() => return Ok(()),
            };
            let response = serde_json::to_vec(&response)?;
            if response.len() > crate::wire::MAX_FRAME_BYTES {
                return Err(unavailable());
            }
            channel
                .write_u32(u32::try_from(response.len()).map_err(|_| unavailable())?)
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
        let mut state = self.control.runtime.lock().map_err(|_| unavailable())?;
        let runtime = state
            .as_mut()
            .filter(|runtime| runtime.id == self.id)
            .ok_or_else(unavailable)?;
        let (_, reply) = runtime
            .pending
            .take_if(|(id, _)| id == request_id)
            .ok_or_else(|| {
                Error::Sandbox("desktop reply is stale or belongs to another evaluation".into())
            })?;
        let _ = reply.send(response);
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

fn unavailable() -> Error {
    Error::Sandbox("Mac desktop control is unavailable; open the möbius-app menu bar app and enable desktop control".into())
}

#[cfg(test)]
mod tests;

pub(crate) async fn next_update(
    connection: &mut Option<DesktopConnection>,
) -> Option<ServerMessage> {
    match connection {
        Some(connection) => connection.outgoing.recv().await,
        None => std::future::pending().await,
    }
}

pub(crate) async fn write_update(
    connection: &mut Option<DesktopConnection>,
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
    control: &Arc<DesktopControl>,
    connection: &mut Option<DesktopConnection>,
    local: bool,
    client_kind: crate::wire::ClientKind,
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> crate::Result<()> {
    use crate::wire::{ClientMessage, ServerFrame, write_frame};
    let (request_id, result) = match message {
        ClientMessage::SetDesktopRuntime {
            request_id,
            enabled,
        } => {
            let result = if !(cfg!(target_os = "macos")
                && local
                && client_kind == crate::wire::ClientKind::Macos)
            {
                Err(Error::Sandbox(
                    "only the local Mac app can host desktop control".into(),
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
            message: error.to_string(),
            fatal: false,
        },
    };
    write_frame(writer, &ServerFrame::new(response)).await?;
    Ok(())
}
