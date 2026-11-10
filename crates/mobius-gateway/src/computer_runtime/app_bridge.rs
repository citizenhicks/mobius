//! The connection a local Mac app lends the gateway for requests only it can answer, and
//! the length-prefixed frames worker channels exchange.

use std::sync::{Arc, Mutex, MutexGuard};

use mobius::{Error, Result};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::wire::{ServerFrame, ServerMessage, write_frame as write_server_frame};

/// The attached app: where its requests go and the replies they await.
pub(crate) struct App<P> {
    pub(crate) id: Uuid,
    pub(crate) outgoing: mpsc::Sender<ServerMessage>,
    pub(crate) pending: P,
}

/// What attaching does while another connection already holds the app.
#[derive(Clone, Copy)]
pub(crate) enum Occupied {
    /// The newer connection takes over.
    Replace,
    /// Attaching fails with this message.
    Refuse(&'static str),
}

/// The gateway's single slot for one kind of lent app.
pub(crate) struct AppBridge<P> {
    app: Mutex<Option<App<P>>>,
    /// Names the slot when its lock is poisoned.
    name: &'static str,
}

impl<P> AppBridge<P> {
    pub(crate) const fn new(name: &'static str) -> Self {
        Self {
            app: Mutex::new(None),
            name,
        }
    }

    pub(crate) fn app(&self) -> Result<MutexGuard<'_, Option<App<P>>>> {
        self.app
            .lock()
            .map_err(|_poisoned| Error::Poisoned(self.name))
    }
}

impl<P: Default> AppBridge<P> {
    /// Registers a connection whose outgoing queue holds at most `capacity` requests.
    pub(crate) fn attach(
        self: &Arc<Self>,
        capacity: usize,
        occupied: Occupied,
    ) -> Result<AppConnection<P>> {
        let mut app = self.app()?;
        if app.is_some()
            && let Occupied::Refuse(message) = occupied
        {
            return Err(Error::Sandbox(message.into()));
        }
        let id = Uuid::new_v4();
        let (outgoing, receiver) = mpsc::channel(capacity);
        *app = Some(App {
            id,
            outgoing,
            pending: P::default(),
        });
        drop(app);
        Ok(AppConnection {
            bridge: Arc::clone(self),
            id,
            outgoing: receiver,
        })
    }
}

/// One app connection's registration. Dropping it withdraws the app, which answers
/// waiting requests with nothing.
pub(crate) struct AppConnection<P> {
    bridge: Arc<AppBridge<P>>,
    id: Uuid,
    pub(crate) outgoing: mpsc::Receiver<ServerMessage>,
}

impl<P> AppConnection<P> {
    /// Runs `action` on this connection's app, or on `None` once another replaced it.
    pub(crate) fn with_app<T>(&self, action: impl FnOnce(Option<&mut App<P>>) -> T) -> Result<T> {
        let mut app = self.bridge.app()?;
        Ok(action(app.as_mut().filter(|app| app.id == self.id)))
    }
}

impl<P> Drop for AppConnection<P> {
    fn drop(&mut self) {
        // A poisoned slot is never read; the next attach reports it.
        if let Ok(mut app) = self.bridge.app()
            && app.as_ref().is_some_and(|app| app.id == self.id)
        {
            *app = None;
        }
    }
}

/// The attached app's next request; never resolves while none is attached.
pub(crate) async fn next_update<P>(
    connection: &mut Option<AppConnection<P>>,
) -> Option<ServerMessage> {
    match connection {
        Some(connection) => connection.outgoing.recv().await,
        None => std::future::pending().await,
    }
}

/// Forwards one request to the app, or withdraws a connection whose queue closed.
pub(crate) async fn write_update<P>(
    connection: &mut Option<AppConnection<P>>,
    message: Option<ServerMessage>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> crate::Result<()> {
    if let Some(message) = message {
        write_server_frame(writer, &ServerFrame::new(message)).await
    } else {
        *connection = None;
        Ok(())
    }
}

/// Answers a registration or a stray reply, rejecting it under `code` on failure.
pub(crate) async fn write_outcome(
    writer: &mut (impl AsyncWrite + Unpin),
    request_id: Arc<str>,
    code: &str,
    result: Result<()>,
) -> crate::Result<()> {
    let response = match result {
        Ok(()) => ServerMessage::Accepted { request_id },
        Err(error) => ServerMessage::Rejected {
            request_id,
            code: code.into(),
            message: error.to_string().into(),
            fatal: false,
        },
    };
    write_server_frame(writer, &ServerFrame::new(response)).await
}

/// Reads one length-prefixed worker frame; `None` when its size is zero or above `max`.
pub(crate) async fn read_frame(
    channel: &mut (impl AsyncRead + Unpin),
    max: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let size = channel.read_u32().await? as usize;
    if size == 0 || size > max {
        return Ok(None);
    }
    let mut bytes = vec![0; size];
    channel.read_exact(&mut bytes).await?;
    Ok(Some(bytes))
}

/// Writes one length-prefixed worker frame.
pub(crate) async fn write_frame(
    channel: &mut (impl AsyncWrite + Unpin),
    bytes: &[u8],
) -> Result<()> {
    let size = u32::try_from(bytes.len())
        .map_err(|error| Error::Sandbox(format!("worker frame is too large: {error}")))?;
    channel.write_u32(size).await?;
    channel.write_all(bytes).await?;
    channel.flush().await?;
    Ok(())
}
