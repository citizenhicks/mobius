//! How an agent reaches its chat's page without a debugging port: a Unix socket that only
//! this user can open, where each granted page answers to its own unguessable path. Every
//! connection speaks the DevTools protocol through a `Router` scoped to that one page.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{Sender, channel};
use tokio::sync::{Semaphore, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

pub(super) const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

use super::page::{PageEvent, PageEvents};
use super::router::{Action, Router};

/// What a connection asks of its page, in order, on the main thread.
pub enum Command {
    /// A client connected: page messages go to `messages` until it detaches.
    Attach {
        connection: u64,
        messages: Sender<String>,
    },
    Send {
        connection: u64,
        message: String,
        /// Dropped after the main thread consumes this command, bounding its input queue.
        processed: oneshot::Sender<()>,
    },
    Detach {
        connection: u64,
        message: Option<String>,
    },
}

type Pages = Arc<Mutex<HashMap<String, PageEvents>>>;

/// The socket agents connect to, and the pages granted on it.
struct Server {
    socket: PathBuf,
    pages: Pages,
}

static SERVER: OnceLock<Server> = OnceLock::new();

/// A page's endpoint; dropping it revokes the address.
pub struct Grant {
    endpoint: String,
    pages: Pages,
}

impl Grant {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        let token = self.endpoint.rsplit('/').next().unwrap_or_default();
        // A poisoned registry is never read; its grants are refused from then on.
        if let Ok(mut pages) = self.pages.lock() {
            pages.remove(token);
        }
    }
}

/// Makes an endpoint for the page reporting to `page`. Main thread only.
pub fn grant(page: PageEvents) -> Result<Grant> {
    let server = if let Some(server) = SERVER.get() {
        server
    } else {
        let home = std::env::var_os("HOME").context("no home folder")?;
        let server =
            Server::listen(&PathBuf::from(home).join("Library/Caches/app.mobius.desktop"))?;
        SERVER.get_or_init(|| server)
    };
    Ok(server.grant(page))
}

impl Server {
    /// Binds `browser.sock` in `folder`, which only this user may enter, and serves it from
    /// a thread of its own. The path must have no spaces: WebSocket clients take it from a
    /// URL without decoding it.
    fn listen(folder: &Path) -> Result<Self> {
        std::fs::create_dir_all(folder)?;
        std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700))?;
        let socket = folder.join("browser.sock");
        // A socket left by an earlier run refuses connections; this run takes its place.
        if let Err(error) = std::fs::remove_file(&socket)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error.into());
        }
        let listener = std::os::unix::net::UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let pages = Pages::default();
        let served = Arc::clone(&pages);
        std::thread::Builder::new()
            .name("browser-agents".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(accept(listener, served))
            })?;
        Ok(Self { socket, pages })
    }

    fn grant(&self, page: PageEvents) -> Grant {
        let token = uuid::Uuid::new_v4().simple().to_string();
        let endpoint = format!("ws+unix://{}:/{token}", self.socket.display());
        // A poisoned registry never records the grant, so the endpoint is refused.
        if let Ok(mut pages) = self.pages.lock() {
            pages.insert(token, page);
        }
        Grant {
            endpoint,
            pages: Arc::clone(&self.pages),
        }
    }
}

async fn accept(listener: std::os::unix::net::UnixListener, pages: Pages) -> std::io::Result<()> {
    let listener = tokio::net::UnixListener::from_std(listener)?;
    let mut connections = 0;
    let slots = Arc::new(Semaphore::new(16));
    loop {
        let (stream, _) = listener.accept().await?;
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            continue;
        };
        connections += 1;
        let pages = Arc::clone(&pages);
        tokio::spawn(async move {
            let _slot = slot;
            serve(stream, pages, connections).await;
        });
    }
}

/// Accepts a WebSocket whose path is a live token, and relays it to that token's page.
#[expect(
    clippy::result_large_err,
    reason = "tungstenite's handshake callback refuses with its own HTTP response type"
)]
async fn serve(stream: UnixStream, pages: Pages, connection: u64) {
    let mut page = None;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let handshake = tokio_tungstenite::accept_hdr_async_with_config(
        stream,
        |request: &Request, response| {
            let token = request.uri().path().trim_start_matches('/');
            page = pages
                .lock()
                .ok()
                .and_then(|pages| pages.get(token).cloned()); // rules-allow: the connection owns a sender to its page's channel, a refcounted handle
            if page.is_some() {
                Ok(response)
            } else {
                let mut refused = ErrorResponse::new(None);
                *refused.status_mut() = StatusCode::FORBIDDEN;
                Err(refused)
            }
        },
        Some(config),
    );
    if let (Ok(Ok(socket)), Some(page)) = (tokio::time::timeout(IO_TIMEOUT, handshake).await, page)
    {
        relay(socket, &page, connection).await;
    }
}

async fn relay(mut socket: WebSocketStream<UnixStream>, page: &PageEvents, connection: u64) {
    let send = |command: Command| page.send(PageEvent::DevTools(command)).is_ok();
    // Chromium emits a burst of initialization events before the relay can drain them.
    let (messages, mut on_page) = channel(32);
    if !send(Command::Attach {
        connection,
        messages,
    }) {
        return;
    }
    let mut router = Router::default();
    'connection: loop {
        let actions = tokio::select! {
            incoming = socket.next() => match incoming {
                Some(Ok(Message::Text(text))) => router.on_client(&text),
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => continue,
            },
            outgoing = on_page.recv() => match outgoing {
                Some(message) => router.on_page(&message),
                // The page closed.
                None => break,
            },
        };
        for action in actions {
            match action {
                Action::Client(text) => {
                    if !matches!(
                        tokio::time::timeout(IO_TIMEOUT, socket.send(Message::text(text))).await,
                        Ok(Ok(()))
                    ) {
                        break 'connection;
                    }
                }
                Action::Page(message) => {
                    let (processed, consumed) = oneshot::channel();
                    if !send(Command::Send {
                        connection,
                        message,
                        processed,
                    }) {
                        break 'connection;
                    }
                    if tokio::time::timeout(IO_TIMEOUT, consumed).await.is_err() {
                        break 'connection;
                    }
                }
            }
        }
    }
    send(Command::Detach {
        connection,
        message: router.detach(),
    });
    match tokio::time::timeout(IO_TIMEOUT, socket.close(None)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::debug!(%error, "Closing the agent's DevTools socket failed"),
        Err(error) => tracing::debug!(%error, "Closing the agent's DevTools socket timed out"),
    }
}

#[cfg(test)]
mod tests;
