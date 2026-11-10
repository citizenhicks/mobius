use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures_util::{StreamExt as _, stream};
use mobius::SessionId;
use mobius::protocol::SessionFileReference;
use mobius_gateway::client::{Endpoint, GatewayEvents, GatewaySender};
use mobius_gateway::wire::{ClientMessage, ServerMessage};
use mobius_gateway::{Error, Result};
use tokio::sync::{RwLock, RwLockReadGuard, oneshot};
use uuid::Uuid;

use super::connection::connect_once;
use super::{Update, recap};

#[cfg(test)]
#[path = "transfer_tests.rs"]
mod transfer_tests;

/// Requests one transfer keeps in flight on the shared connection.
pub(super) const WINDOW: usize = 4;
/// The gateway's largest session file read.
const READ_CHUNK_BYTES: usize = 256 * 1024;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
/// A transfer connection unused for a whole interval closes.
const IDLE_INTERVAL: Duration = Duration::from_secs(30);

pub(super) enum Transfer {
    Export {
        id: Arc<str>,
        session_id: SessionId,
        destination: PathBuf,
        uploads: bool,
        artifacts: bool,
    },
    Preview {
        id: Arc<str>,
        session_id: SessionId,
        request: super::PreviewRequest,
    },
    Download {
        session_id: SessionId,
        file: SessionFileReference,
        destination: PathBuf,
    },
    Thumbnail {
        session_id: SessionId,
        file: SessionFileReference,
        destination: PathBuf,
    },
    Upload {
        id: Arc<str>,
        session_id: SessionId,
        path: PathBuf,
        cancellation: Arc<AtomicBool>,
    },
    Delete {
        id: Arc<str>,
        session_id: SessionId,
        file_id: String,
    },
    /// What `session_id` said after `since`, for its recap.
    Recent {
        session_id: SessionId,
        since: Option<i64>,
    },
}

/// File transfers to one gateway. They share one connection, opened on first use and
/// closed once idle, so thumbnails and uploads never pay for a connection each.
pub(super) struct Transfers {
    endpoint: Endpoint,
    token: String,
    link: tokio::sync::Mutex<Option<Arc<TransferLink>>>,
}

impl Transfers {
    pub(super) fn new(endpoint: Endpoint, token: String) -> Arc<Self> {
        Arc::new(Self {
            endpoint,
            token,
            link: tokio::sync::Mutex::default(),
        })
    }

    pub(super) fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The shared connection, opened again when there is none or it closed.
    async fn link(self: &Arc<Self>) -> Result<Arc<TransferLink>> {
        let mut slot = self.link.lock().await;
        if let Some(link) = slot.as_ref().filter(|link| link.is_open()) {
            return Ok(Arc::clone(link));
        }
        let link = Arc::new(TransferLink::connect(&self.endpoint, &self.token).await?);
        tokio::spawn(close_when_idle(Arc::downgrade(self), Arc::downgrade(&link)));
        *slot = Some(Arc::clone(&link));
        drop(slot);
        Ok(link)
    }
}

/// Drops `link` once no transfer holds or used it for a whole interval.
async fn close_when_idle(transfers: Weak<Transfers>, link: Weak<TransferLink>) {
    loop {
        tokio::time::sleep(IDLE_INTERVAL).await;
        let Some(transfers) = transfers.upgrade() else {
            return;
        };
        let mut slot = transfers.link.lock().await;
        let Some(held) = slot
            .as_ref()
            .filter(|held| std::ptr::eq(Arc::as_ptr(held), link.as_ptr()))
        else {
            return;
        };
        // Each running transfer holds its own handle to the link.
        if !held.used.swap(false, Ordering::Relaxed) && Arc::strong_count(held) == 1 {
            *slot = None;
            return;
        }
        drop(slot);
    }
}

type Waiters = Mutex<Option<HashMap<Uuid, oneshot::Sender<ServerMessage>>>>;

/// One connection with at most one chat selected; transfers of that chat interleave on it.
pub(super) struct TransferLink {
    sender: GatewaySender,
    /// Responses by request, until the connection closes.
    waiters: Arc<Waiters>,
    selected: RwLock<Option<Selected>>,
    used: AtomicBool,
    reader: tokio::task::JoinHandle<()>,
}

struct Selected {
    session_id: SessionId,
    workspace: Option<PathBuf>,
}

impl Drop for TransferLink {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl TransferLink {
    async fn connect(endpoint: &Endpoint, token: &str) -> Result<Self> {
        let mut options = super::catalog_free();
        let (sender, events) = connect_once(endpoint, token, &mut options)
            .await?
            .into_parts();
        let waiters = Arc::new(Mutex::new(Some(HashMap::new())));
        let reader = tokio::spawn(route(events, Arc::clone(&waiters)));
        Ok(Self {
            sender,
            waiters,
            selected: RwLock::new(None),
            used: AtomicBool::new(true),
            reader,
        })
    }

    fn is_open(&self) -> bool {
        self.waiters.lock().is_ok_and(|waiters| waiters.is_some())
    }

    /// Sends the request `build` makes for a fresh request ID and waits for its answer.
    pub(super) async fn request(
        &self,
        build: impl FnOnce(Arc<str>) -> ClientMessage,
    ) -> Result<ServerMessage> {
        let id = Uuid::new_v4();
        let (reply, response) = oneshot::channel();
        self.used.store(true, Ordering::Relaxed);
        self.waiters
            .lock()
            .map_err(|_poisoned| disconnected())?
            .as_mut()
            .ok_or_else(disconnected)?
            .insert(id, reply);
        let _waiting = Waiting {
            waiters: &self.waiters,
            id,
        };
        let request_id: Arc<str> =
            Arc::from(&*id.hyphenated().encode_lower(&mut Uuid::encode_buffer()));
        // A connection that failed a write or stopped answering is not reused.
        if let Err(error) = self.sender.send(build(Arc::clone(&request_id))).await {
            close(&self.waiters);
            return Err(error);
        }
        let Ok(message) = tokio::time::timeout(RESPONSE_TIMEOUT, response).await else {
            close(&self.waiters);
            return Err(Error::Protocol("File transfer timed out.".into()));
        };
        let message = message.map_err(|error| {
            tracing::debug!(%error, "the transfer connection dropped its reply");
            disconnected()
        })?;
        if let Some(error) = message.response_error(Some(&request_id)) {
            if matches!(&message, ServerMessage::Rejected { code, .. } if code == "storage_full") {
                return Err(mobius::Error::StorageFull.into());
            }
            return Err(Error::Protocol(error.message.into()));
        }
        Ok(message)
    }

    /// Keeps `session_id` selected on this connection while the returned guard lives.
    async fn select(
        &self,
        session_id: &SessionId,
    ) -> Result<RwLockReadGuard<'_, Option<Selected>>> {
        {
            let selected = self.selected.read().await;
            if selected
                .as_ref()
                .is_some_and(|selected| selected.session_id == *session_id)
            {
                return Ok(selected);
            }
        }
        let mut selected = self.selected.write().await;
        if selected
            .as_ref()
            .is_none_or(|selected| selected.session_id != *session_id)
        {
            *selected = None;
            let response = self
                .request(|request_id| ClientMessage::SelectSession {
                    request_id,
                    session_id: session_id.share(),
                })
                .await?;
            let ServerMessage::SessionOpened { payload, .. } = response else {
                return Err(Error::Protocol(
                    "Gateway did not open the file-transfer session.".into(),
                ));
            };
            if payload.session.session_id != *session_id {
                return Err(Error::Protocol(
                    "Gateway opened a different file-transfer session.".into(),
                ));
            }
            *selected = Some(Selected {
                session_id: payload.session.session_id.share(),
                workspace: payload.workspace.map(|workspace| workspace.path),
            });
        }
        Ok(selected.downgrade())
    }
}

/// Forgets a request's waiter when its transfer stops waiting.
struct Waiting<'a> {
    waiters: &'a Waiters,
    id: Uuid,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Ok(mut waiters) = self.waiters.lock()
            && let Some(waiters) = waiters.as_mut()
        {
            waiters.remove(&self.id);
        }
    }
}

fn disconnected() -> Error {
    Error::Protocol("Gateway disconnected during file transfer.".into())
}

/// Hands each response to the transfer waiting for it; the rest, such as a replay or a
/// broadcast, has no reader on this connection.
async fn route(mut events: GatewayEvents, waiters: Arc<Waiters>) {
    while let Ok(Some(frame)) = events.next().await {
        if matches!(frame.message, ServerMessage::Error { fatal: true, .. }) {
            break;
        }
        let waiter = response_id(&frame.message)
            .and_then(|id| Uuid::parse_str(id).ok())
            .and_then(|id| waiters.lock().ok()?.as_mut()?.remove(&id));
        if let Some(waiter) = waiter
            && waiter.send(frame.message).is_err()
        {
            tracing::debug!("a file transfer stopped waiting before its response arrived");
        }
    }
    close(&waiters);
}

/// Fails every waiting transfer; the next one opens a new connection.
fn close(waiters: &Waiters) {
    if let Ok(mut waiters) = waiters.lock() {
        *waiters = None;
    }
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

impl Transfer {
    pub fn start(
        self,
        transfers: Option<&Arc<Transfers>>,
        tasks: &mut tokio::task::JoinSet<()>,
        slots: Arc<tokio::sync::Semaphore>,
        updates: tokio::sync::mpsc::Sender<Update>,
    ) -> Option<Update> {
        let Some(transfers) = transfers.map(Arc::clone) else {
            return Some(self.failed(
                String::new(),
                "Connect to a gateway before transferring files.".into(),
            ));
        };
        if tasks.len() >= 64 {
            return Some(self.failed(
                transfers.endpoint().to_string(),
                "Wait for the current file transfers to finish.".into(),
            ));
        }
        tasks.spawn(async move {
            let Ok(_permit) = slots.acquire_owned().await else {
                return;
            };
            let update = match self.run(&transfers).await {
                Ok(Some(update)) => update,
                Ok(None) => self.finished(transfers.endpoint().to_string()),
                Err(error) => {
                    self.failed(transfers.endpoint().to_string(), super::describe(&error))
                }
            };
            if let Err(error) = updates.send(update).await {
                tracing::debug!(%error, "the window was gone when a file transfer finished");
            }
        });
        None
    }

    /// The transfer's update, or `None` when [`Self::finished`] makes it from the transfer.
    async fn run(&self, transfers: &Arc<Transfers>) -> Result<Option<Update>> {
        let link = transfers.link().await?;
        let session_id = match self {
            Self::Export { session_id, .. }
            | Self::Download { session_id, .. }
            | Self::Thumbnail { session_id, .. }
            | Self::Upload { session_id, .. }
            | Self::Delete { session_id, .. }
            | Self::Preview { session_id, .. } => session_id,
            // Selecting would start a stopped chat; the gateway reads history without it,
            // and an older gateway's rejection leaves the chat without a recap.
            Self::Recent { .. } => return self.transfer(&link, transfers.endpoint(), None).await,
        };
        let selected = link.select(session_id).await?;
        let workspace = selected
            .as_ref()
            .ok_or_else(disconnected)?
            .workspace
            .as_deref();
        let update = self.transfer(&link, transfers.endpoint(), workspace).await;
        drop(selected);
        update
    }

    /// The transfer itself, on a connection whose session is selected unless it is a recap.
    async fn transfer(
        &self,
        link: &TransferLink,
        endpoint: &Endpoint,
        workspace: Option<&Path>,
    ) -> Result<Option<Update>> {
        match self {
            Self::Export {
                id,
                session_id,
                destination,
                uploads,
                artifacts,
            } => {
                export(link, session_id, destination, *uploads, *artifacts).await?;
                Ok(Some(Update::StorageExported {
                    id: Arc::clone(id),
                    endpoint: endpoint.to_string(),
                    result: Ok(()),
                }))
            }
            Self::Preview {
                id,
                session_id,
                request,
            } => Ok(Some(Update::PreviewLoaded {
                endpoint: endpoint.to_string(),
                id: Arc::clone(id),
                session_id: session_id.share(),
                result: super::preview::load(link, session_id, workspace, request)
                    .await
                    .map_err(|error| super::describe(&error)),
            })),
            Self::Download {
                session_id,
                file,
                destination,
            }
            | Self::Thumbnail {
                session_id,
                file,
                destination,
            } => {
                download(link, session_id, file, destination).await?;
                Ok(None)
            }
            Self::Upload {
                id,
                session_id,
                path,
                cancellation,
            } => {
                let result = super::upload::upload(link, session_id, path, cancellation).await;
                Ok(Some(Update::Uploaded {
                    endpoint: endpoint.to_string(),
                    session_id: session_id.share(),
                    id: Arc::clone(id),
                    result,
                }))
            }
            Self::Recent { .. } => recap::update(link, endpoint, self).await.map(Some),
            Self::Delete {
                id,
                session_id,
                file_id,
            } => {
                delete(link, session_id, file_id).await?;
                Ok(Some(Update::AttachmentDeleted {
                    endpoint: endpoint.to_string(),
                    session_id: session_id.share(),
                    id: Arc::clone(id),
                    result: Ok(()),
                }))
            }
        }
    }

    /// The update of a finished download, made from the transfer it consumes.
    fn finished(self, endpoint: String) -> Update {
        match self {
            Self::Thumbnail {
                session_id,
                file,
                destination,
            } => Update::ThumbnailSource {
                endpoint,
                session_id,
                file_id: file.id,
                result: Ok(destination),
            },
            Self::Download {
                session_id,
                file,
                destination,
            } => Update::Downloaded {
                endpoint,
                session_id,
                file,
                path: destination,
            },
            other => other.failed(endpoint, "The transfer finished without a result.".into()),
        }
    }

    fn failed(self, endpoint: String, message: String) -> Update {
        match self {
            Self::Export { id, .. } => Update::StorageExported {
                id,
                endpoint,
                result: Err(message),
            },
            Self::Preview { id, session_id, .. } => Update::PreviewLoaded {
                endpoint,
                id,
                session_id,
                result: Err(message),
            },
            Self::Download { .. } => Update::OperationError { endpoint, message },
            Self::Thumbnail {
                session_id, file, ..
            } => Update::ThumbnailSource {
                endpoint,
                session_id,
                file_id: file.id,
                result: Err(message),
            },
            Self::Recent { session_id, .. } => Update::Recent {
                endpoint,
                session_id,
                result: Err(message),
            },
            Self::Delete { id, session_id, .. } => Update::AttachmentDeleted {
                endpoint,
                session_id,
                id,
                result: Err(message),
            },
            Self::Upload { id, session_id, .. } => Update::Uploaded {
                endpoint,
                session_id,
                id,
                result: Err(super::UploadFailure {
                    storage_full: false,
                    message,
                    cleanup_file_id: None,
                }),
            },
        }
    }
}

/// Streams the selected file origins into separate, uniquely named files.
async fn export(
    link: &TransferLink,
    session_id: &SessionId,
    destination: &Path,
    uploads: bool,
    artifacts: bool,
) -> Result<()> {
    tokio::fs::create_dir_all(destination).await?;
    let response = link
        .request(|request_id| ClientMessage::ListSessionFiles {
            request_id,
            session_id: session_id.share(),
        })
        .await?;
    let ServerMessage::SessionFiles { files, .. } = response else {
        return Err(Error::Protocol("Gateway did not list export files".into()));
    };
    for record in files {
        let included = match record.origin {
            mobius::protocol::SessionFileOrigin::User => uploads,
            mobius::protocol::SessionFileOrigin::Agent => artifacts,
        };
        if !included {
            continue;
        }
        let name = Path::new(&record.file.name)
            .file_name()
            .ok_or_else(|| Error::Protocol("Invalid export file name".into()))?
            .to_string_lossy();
        let path = destination.join(format!("{}-{name}", Uuid::new_v4()));
        download(link, session_id, &record.file, &path).await?;
    }
    Ok(())
}

/// Cancels an active upload on this connection or removes a completed file.
pub(super) async fn delete(
    link: &TransferLink,
    session_id: &SessionId,
    file_id: &str,
) -> Result<()> {
    let response = link
        .request(|request_id| ClientMessage::DeleteSessions {
            request_id,
            session_ids: vec![session_id.share()],
            selection: mobius::backend::session_files::SessionFileSelection::Ids(vec![
                file_id.into(),
            ]),
        })
        .await?;
    if matches!(response, ServerMessage::Accepted { .. }) {
        Ok(())
    } else {
        Err(Error::Protocol(
            "Gateway did not confirm attachment removal.".into(),
        ))
    }
}

/// Reads the file with a few chunk requests in flight, since each costs a round trip.
async fn download(
    link: &TransferLink,
    session_id: &SessionId,
    file: &SessionFileReference,
    destination: &Path,
) -> Result<()> {
    let destination = destination.to_path_buf();
    let (mut output, destination) = tokio::task::spawn_blocking(move || {
        let parent = destination
            .parent()
            .filter(|path| path.is_dir())
            .ok_or_else(|| Error::Config("The download folder does not exist.".into()))?;
        Ok::<_, Error>((tempfile::NamedTempFile::new_in(parent)?, destination))
    })
    .await
    .map_err(std::io::Error::other)??;
    let mut chunks = stream::iter((0..file.size.max(1)).step_by(READ_CHUNK_BYTES))
        .map(|offset| {
            link.request(move |request_id| ClientMessage::ReadSessionFile {
                request_id,
                session_id: session_id.share(),
                file_id: file.id.as_str().into(),
                offset,
                max_bytes: READ_CHUNK_BYTES,
            })
        })
        .buffered(WINDOW);
    let mut complete = false;
    while let Some(response) = chunks.next().await {
        let ServerMessage::SessionFileChunk {
            data, next_offset, ..
        } = response?
        else {
            return Err(Error::Protocol(
                "Gateway returned an unexpected file response.".into(),
            ));
        };
        output = tokio::task::spawn_blocking(move || {
            output.write_all(&data)?;
            Ok::<_, std::io::Error>(output)
        })
        .await
        .map_err(std::io::Error::other)??;
        if next_offset.is_none() {
            complete = true;
            break;
        }
    }
    if !complete {
        return Err(Error::Protocol(
            "Gateway returned an incomplete or inconsistent file.".into(),
        ));
    }
    // Until this final step, cancellation only drops the temporary file. As before,
    // a completed download is synced before the atomic no-overwrite publication.
    tokio::task::spawn_blocking(move || {
        output.as_file().sync_all()?;
        output
            .persist_noclobber(destination)
            .map_err(|error| error.error)?;
        Ok(())
    })
    .await
    .map_err(std::io::Error::other)?
}
