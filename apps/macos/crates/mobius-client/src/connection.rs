use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use mobius::SessionId;
use mobius::protocol::SessionFileReference;

use mobius_gateway::client::{
    ConnectOptions, Endpoint, GatewayClient, GatewayEvents, GatewaySender,
};
use mobius_gateway::config::state_dir;
use mobius_gateway::gateway_accounts::{
    GatewayAccounts, configured_token, dashboard_gateway_endpoint, missing_local_token,
};
use mobius_gateway::wire::{ClientCapabilities, ClientMessage, ReadySection, ServerMessage};
use mobius_gateway::{Error, Result};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use crate::numeric::ToFloat;

#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub enum Connection {
    #[default]
    Local,
    Saved(String),
    Pair {
        endpoint: String,
        code: String,
    },
}

#[derive(Debug)]
pub enum Update {
    StorageExported {
        id: Arc<str>,
        endpoint: String,
        result: std::result::Result<(), String>,
    },
    /// A user-requested connection; `same_endpoint` keeps pages and drafts.
    Connecting {
        label: String,
        same_endpoint: bool,
    },
    /// The connection dropped; attempt `attempt` follows after a backoff.
    Reconnecting {
        /// The current dial target; None means the owned local gateway.
        endpoint: Option<String>,
        attempt: u32,
        message: String,
    },
    Connected(String),
    /// The connection reopens a chat from the last event the window applied:
    /// `(request_id, session_id)`.
    Resuming(Arc<str>, SessionId),
    Message(Box<ServerMessage>),
    Error(String),
    SendFailed {
        request_id: Option<Arc<str>>,
        delivery_uncertain: bool,
        message: String,
    },
    Disconnected,
    OperationError {
        endpoint: String,
        message: String,
    },
    Downloaded {
        endpoint: String,
        session_id: SessionId,
        file: SessionFileReference,
        path: PathBuf,
    },
    /// An image's bytes for its thumbnail; failures stay quiet, unlike a download.
    ThumbnailSource {
        endpoint: String,
        session_id: SessionId,
        file_id: String,
        result: std::result::Result<PathBuf, String>,
    },
    PreviewLoaded {
        endpoint: String,
        id: Arc<str>,
        session_id: SessionId,
        result: std::result::Result<super::PreviewData, String>,
    },
    Uploaded {
        endpoint: String,
        session_id: SessionId,
        id: Arc<str>,
        result: std::result::Result<Option<SessionFileReference>, super::UploadFailure>,
    },
    AttachmentDeleted {
        endpoint: String,
        session_id: SessionId,
        id: Arc<str>,
        result: std::result::Result<(), String>,
    },
    /// What the chat `session_id` said since it was last read, for its recap.
    Recent {
        endpoint: String,
        session_id: SessionId,
        result: std::result::Result<super::RecentChat, String>,
    },
}

enum Command {
    Connect(Connection),
    Send(Box<ClientMessage>),
    Transfer(super::files::Transfer),
    OpenDesktop(tokio::sync::oneshot::Sender<std::result::Result<super::DesktopViewer, String>>),
}

/// The UI submits commands and drains events; socket IO stays on Tokio workers.
#[derive(Debug)]
pub struct Gateway {
    commands: mpsc::Sender<Command>,
    updates: Option<mpsc::Receiver<Update>>,
    runtime: Option<Runtime>,
}

impl Gateway {
    /// Starts the connection actor and connects to the local gateway.
    ///
    /// # Errors
    ///
    /// When the async runtime cannot start or the first connect cannot be queued.
    pub fn start() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let (commands, receiver) = mpsc::channel(64);
        let (updates, output) = mpsc::channel(256);
        runtime.spawn(run(receiver, updates));
        let gateway = Self {
            commands,
            updates: Some(output),
            runtime: Some(runtime),
        };
        gateway.connect(Connection::Local)?;
        Ok(gateway)
    }

    /// Replaces the connection with `connection`.
    ///
    /// # Errors
    ///
    /// When the connection's queue is full or the connection has closed.
    pub fn connect(&self, connection: Connection) -> Result<()> {
        self.enqueue(Command::Connect(connection))
    }

    /// Opens the gateway's computer; the receiver gets its viewer.
    ///
    /// # Errors
    ///
    /// When the connection's queue is full or the connection has closed.
    pub fn open_desktop(
        &self,
    ) -> Result<tokio::sync::oneshot::Receiver<std::result::Result<super::DesktopViewer, String>>>
    {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        self.enqueue(Command::OpenDesktop(reply))?;
        Ok(receiver)
    }

    /// Sends `message` on the open connection.
    ///
    /// # Errors
    ///
    /// When the connection's queue is full or the connection has closed.
    pub fn send(&self, message: ClientMessage) -> Result<()> {
        self.enqueue(Command::Send(Box::new(message)))
    }

    /// Exports a chat's files into `destination`; returns the export's id.
    ///
    /// # Errors
    ///
    /// When the connection's queue is full or the connection has closed.
    pub fn export_storage(
        &self,
        session_id: SessionId,
        destination: PathBuf,
        uploads: bool,
        artifacts: bool,
    ) -> Result<Arc<str>> {
        let id = super::request_id();
        self.enqueue(Command::Transfer(super::files::Transfer::Export {
            id: Arc::clone(&id),
            session_id,
            destination,
            uploads,
            artifacts,
        }))?;
        Ok(id)
    }

    /// Downloads a chat file to `destination`.
    ///
    /// # Errors
    ///
    /// When the connection's queue is full or the connection has closed.
    pub fn download(
        &self,
        session_id: SessionId,
        file: SessionFileReference,
        destination: PathBuf,
    ) -> Result<()> {
        self.enqueue(Command::Transfer(super::files::Transfer::Download {
            session_id,
            file,
            destination,
        }))
    }

    /// Downloads an image the transcript shows, only to draw its thumbnail.
    ///
    /// # Errors
    ///
    /// When the connection's queue is full or the connection has closed.
    pub fn download_thumbnail(
        &self,
        session_id: SessionId,
        file: SessionFileReference,
        destination: PathBuf,
    ) -> Result<()> {
        self.enqueue(Command::Transfer(super::files::Transfer::Thumbnail {
            session_id,
            file,
            destination,
        }))
    }

    /// Loads what a companion-pane document shows.
    ///
    /// # Errors
    ///
    /// When the connection's queue is full or the connection has closed.
    pub fn preview(
        &self,
        id: Arc<str>,
        session_id: SessionId,
        request: super::PreviewRequest,
    ) -> Result<()> {
        self.enqueue(Command::Transfer(super::files::Transfer::Preview {
            id,
            session_id,
            request,
        }))
    }

    /// Uploads a file into a chat.
    ///
    /// # Errors
    ///
    /// When the connection's queue is full or the connection has closed.
    pub fn upload(
        &self,
        id: Arc<str>,
        session_id: SessionId,
        path: PathBuf,
    ) -> Result<Arc<AtomicBool>> {
        let cancellation = Arc::new(AtomicBool::new(false));
        self.enqueue(Command::Transfer(super::files::Transfer::Upload {
            id,
            session_id,
            path,
            cancellation: Arc::clone(&cancellation),
        }))?;
        Ok(cancellation)
    }

    /// Reads what `session_id` said after `since` (gateway milliseconds), or its last
    /// few turns, for a recap.
    ///
    /// # Errors
    ///
    /// When the connection's queue is full or the connection has closed.
    pub fn recent_history(&self, session_id: SessionId, since: Option<i64>) -> Result<()> {
        self.enqueue(Command::Transfer(super::files::Transfer::Recent {
            session_id,
            since,
        }))
    }

    /// Removes an uploaded attachment the draft no longer carries.
    ///
    /// # Errors
    ///
    /// When the connection's queue is full or the connection has closed.
    pub fn delete_attachment(
        &self,
        id: Arc<str>,
        session_id: SessionId,
        file_id: String,
    ) -> Result<()> {
        self.enqueue(Command::Transfer(super::files::Transfer::Delete {
            id,
            session_id,
            file_id,
        }))
    }

    pub fn take_updates(&mut self) -> Option<mpsc::Receiver<Update>> {
        self.updates.take()
    }

    fn enqueue(&self, command: Command) -> Result<()> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    Error::Protocol("Gateway is busy; try again shortly.".into())
                }
                mpsc::error::TrySendError::Closed(_) => {
                    Error::Protocol("Gateway connection has closed.".into())
                }
            })
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

type Sent = std::result::Result<(), Box<mpsc::error::SendError<Update>>>;
type DialResult = Result<(Endpoint, String, GatewayClient)>;
type Dial = Pin<Box<dyn Future<Output = DialResult> + Send>>;

/// Adds the host's Cloud admission to an endpoint; self-hosted endpoints pass through.
pub type Admission = fn(Endpoint) -> Result<Endpoint>;

static ADMISSION: std::sync::OnceLock<Admission> = std::sync::OnceLock::new();

/// Installs the host's Cloud admission, once, before the first connection.
pub fn set_admission(admission: Admission) {
    if ADMISSION.set(admission).is_err() {
        tracing::warn!("Cloud admission was already installed; keeping the first");
    }
}

/// `endpoint` with the host's Cloud admission; without a host, unchanged.
fn admit(endpoint: Endpoint) -> Result<Endpoint> {
    match ADMISSION.get() {
        Some(admit) => admit(endpoint),
        None => Ok(endpoint),
    }
}

const CLOUD_OPEN_TIMEOUT: Duration = Duration::from_secs(120);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

enum Awaited {
    Ready { session_request: Option<Arc<str>> },
    Session { request_id: Arc<str> },
}

struct Wait {
    phase: Awaited,
    deadline: Pin<Box<tokio::time::Sleep>>,
}

impl Wait {
    fn new(phase: Awaited) -> Self {
        Self {
            phase,
            deadline: Box::pin(tokio::time::sleep(RESPONSE_TIMEOUT)),
        }
    }

    fn is_session(&self, request_id: &str) -> bool {
        matches!(&self.phase, Awaited::Session { request_id: expected } if **expected == *request_id)
    }

    fn progressed(&mut self) {
        self.deadline
            .as_mut()
            .reset(tokio::time::Instant::now() + RESPONSE_TIMEOUT);
    }
}

/// The socket side of the gateway: one connection, its transfers, and the retry timer.
struct Transport {
    updates: mpsc::Sender<Update>,
    connection: Option<(GatewaySender, GatewayEvents)>,
    dial: Option<Dial>,
    waiting: Option<Wait>,
    /// File transfers to the connected gateway, on their own connection.
    transfers: Option<Arc<super::files::Transfers>>,
    desktop_target: Option<(Endpoint, String)>,
    desktop_stop: Option<tokio::sync::watch::Sender<bool>>,
    /// What an automatic reconnect dials; `None` after a fatal or unauthorized failure.
    target: Option<Connection>,
    attempt: u32,
    retry: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    tasks: tokio::task::JoinSet<()>,
    slots: Arc<tokio::sync::Semaphore>,
    /// What the next connection sends: the catalog revisions the window holds and the chat
    /// it resumes.
    next: ConnectOptions,
    /// The chat open on the connection, which a reconnect resumes.
    open: Option<OpenChat>,
}

struct OpenChat {
    request_id: Arc<str>,
    session_id: SessionId,
    /// The last event sequence the window received for it.
    latest: u64,
    /// The newest record received during this replay, independently of the snapshot's tip.
    replayed: Option<u64>,
    /// The replay finished, so the window holds every event up to `latest`.
    complete: bool,
}

enum Event {
    Command(Option<Command>),
    Dial(Box<DialResult>),
    Frame(Box<Result<Option<mobius_gateway::wire::ServerFrame>>>),
    WaitTimeout,
    Retry,
}

async fn run(commands: mpsc::Receiver<Command>, updates: mpsc::Sender<Update>) -> Sent {
    let transport = Transport {
        updates,
        connection: None,
        dial: None,
        waiting: None,
        transfers: None,
        desktop_target: None,
        desktop_stop: None,
        target: None,
        attempt: 0,
        retry: None,
        tasks: tokio::task::JoinSet::new(),
        slots: Arc::new(tokio::sync::Semaphore::new(4)),
        next: ConnectOptions::default(),
        open: None,
    };
    run_transport(commands, transport).await
}

async fn run_transport(mut commands: mpsc::Receiver<Command>, mut transport: Transport) -> Sent {
    loop {
        while transport.tasks.try_join_next().is_some() {}
        let event = tokio::select! {
            command = commands.recv() => Event::Command(command),
            result = next_dial(&mut transport.dial) => Event::Dial(Box::new(result)),
            frame = next_frame(&mut transport.connection) => Event::Frame(Box::new(frame)),
            () = wait_elapsed(&mut transport.waiting) => Event::WaitTimeout,
            () = elapsed(&mut transport.retry) => Event::Retry,
        };
        match event {
            Event::Command(None) => return Ok(()),
            Event::Command(Some(command)) => transport.command(command).await?,
            Event::Dial(result) => {
                transport.dial = None;
                transport.connected(*result).await?;
            }
            Event::Frame(frame) => transport.frame(*frame).await?,
            Event::WaitTimeout => transport.wait_timed_out().await?,
            Event::Retry => {
                transport.retry = None;
                let target = transport.target.clone(); // rules-allow: the dial owns its copy while the transport keeps the target for later retries
                if let Some(target) = target {
                    transport.dial(target);
                }
            }
        }
    }
}

async fn next_dial(dial: &mut Option<Dial>) -> DialResult {
    match dial {
        Some(dial) => dial.await,
        None => std::future::pending().await,
    }
}

async fn next_frame(
    connection: &mut Option<(GatewaySender, GatewayEvents)>,
) -> Result<Option<mobius_gateway::wire::ServerFrame>> {
    match connection {
        Some((_, events)) => events.next().await,
        None => std::future::pending().await,
    }
}

async fn elapsed(retry: &mut Option<std::pin::Pin<Box<tokio::time::Sleep>>>) {
    match retry {
        Some(sleep) => sleep.as_mut().await,
        None => std::future::pending().await,
    }
}

async fn wait_elapsed(waiting: &mut Option<Wait>) {
    match waiting {
        Some(wait) => wait.deadline.as_mut().await,
        None => std::future::pending().await,
    }
}

/// SwiftUI's backoff: `min(8 s, 0.5 s × 2^min(n, 4) × U(0.75, 1.25))`.
fn retry_delay(attempt: u32, unit: f64) -> Duration {
    let jitter = 0.75 + 0.5 * unit.clamp(0., 1.);
    Duration::from_secs_f64((0.5 * f64::from(1u32 << attempt.min(4)) * jitter).min(8.))
}

fn random_unit() -> f64 {
    uuid::Uuid::new_v4().as_u128().to_f64() / u128::MAX.to_f64()
}

impl Transport {
    /// Borrows only the update channel, so the transport's futures need not be `Sync`.
    fn emit(&self, update: Update) -> impl Future<Output = Sent> + use<'_> {
        let updates = &self.updates;
        async move { updates.send(update).await.map_err(Box::new) }
    }

    async fn command(&mut self, command: Command) -> Sent {
        match command {
            Command::Connect(target) => {
                let same_endpoint = self.target.as_ref() == Some(&target);
                if !same_endpoint {
                    self.next = ConnectOptions::default();
                    self.open = None;
                    self.transfers = None;
                }
                self.attempt = 0;
                self.retry = None;
                self.target = Some(target.clone()); // rules-allow: the dial owns its copy while the transport keeps the target for later retries
                let label = match &target {
                    Connection::Local => "Local gateway".to_owned(),
                    Connection::Saved(endpoint) | Connection::Pair { endpoint, .. } => {
                        endpoint.clone() // rules-allow: the update owns its label while the target keeps the endpoint
                    }
                };
                self.emit(Update::Connecting {
                    label,
                    same_endpoint,
                })
                .await?;
                self.dial(target);
                Ok(())
            }
            Command::Send(message) => self.send(*message).await,
            Command::OpenDesktop(reply) => {
                self.close_desktop();
                let desktop_target = self.desktop_target.clone(); // rules-allow: the viewer task owns its copy; the next viewer opens from the same target
                if let Some((endpoint, token)) = desktop_target {
                    let (stop, stopped) = tokio::sync::watch::channel(false);
                    self.desktop_stop = Some(stop.clone()); // rules-allow: a watch sender is a shared channel handle
                    self.tasks.spawn(async move {
                        let result = super::desktop::open(endpoint, token, stop, stopped)
                            .await
                            .map_err(|error| error.to_string());
                        if reply.send(result).is_err() {
                            tracing::debug!(
                                "the desktop viewer was no longer awaited when it opened"
                            );
                        }
                    });
                } else if reply
                    .send(Err("The gateway is disconnected".into()))
                    .is_err()
                {
                    tracing::debug!("the desktop viewer was no longer awaited when it failed");
                }
                Ok(())
            }
            Command::Transfer(transfer) => {
                if let Some(error) = transfer.start(
                    self.transfers.as_ref(),
                    &mut self.tasks,
                    Arc::clone(&self.slots),
                    self.updates.clone(), // rules-allow: an mpsc sender is a shared channel handle
                ) {
                    self.emit(error).await?;
                }
                Ok(())
            }
        }
    }

    /// Opens `target`; transfers keep their original endpoint and finish cleanup.
    fn dial(&mut self, target: Connection) {
        self.close_desktop();
        self.desktop_target = None;
        self.connection = None;
        self.waiting = None;
        self.prepare_resume();
        // Keep the actor's catalog and resume cursor when an in-flight dial is cancelled.
        let mut options = ConnectOptions {
            catalog: self.next.catalog.clone(), // rules-allow: a cancelled dial must leave the actor's catalog in place
            pipelined: self.next.pipelined.clone(), // rules-allow: a cancelled dial must leave the actor's resume request in place
        };
        self.dial = Some(Box::pin(async move { connect(target, &mut options).await }));
    }

    async fn connected(&mut self, result: DialResult) -> Sent {
        let retryable = !matches!(self.target, Some(Connection::Pair { .. }));
        match result {
            Ok((address, token, client)) => {
                self.desktop_target = Some((address.clone(), token.clone())); // rules-allow: the desktop target and the transfers each keep the endpoint and token
                let label = address.to_string();
                // A paired gateway is dialled again with its saved token, never the one-time code.
                if matches!(self.target, Some(Connection::Pair { .. })) {
                    self.target = Some(Connection::Saved(label.clone())); // rules-allow: the target and the Connected update each own the address
                }
                if self
                    .transfers
                    .as_ref()
                    .is_none_or(|transfers| *transfers.endpoint() != address)
                {
                    self.transfers = Some(super::files::Transfers::new(address, token));
                }
                self.connection = Some(client.into_parts());
                let session_request = self.next.pipelined.last().and_then(correlated_request);
                self.waiting = Some(Wait::new(Awaited::Ready { session_request }));
                self.emit(Update::Connected(label)).await?;
                if let Some(ClientMessage::OpenSession {
                    request_id,
                    session_id,
                    ..
                }) = self.next.pipelined.pop()
                {
                    self.emit(Update::Resuming(request_id, session_id)).await?;
                }
            }
            Err(error) => {
                self.transfers = None;
                match error {
                    // Keep the pairing code while the server tells us when to dial again.
                    Error::WebSocketUpgrade {
                        status: 503,
                        retry_after: Some(delay),
                    } => {
                        self.retry_after(delay, super::GATEWAY_STARTING.into())
                            .await?;
                    }
                    Error::WebSocketUpgrade { status: 503, .. } => {
                        self.fail("The gateway is starting, but its server did not provide a valid retry delay. Try connecting again.".into()).await?;
                    }
                    Error::Config(_) | Error::Unauthorized => {
                        self.fail(super::describe(&error)).await?;
                    }
                    error if retryable => self.lost(super::describe(&error)).await?,
                    error => self.fail(super::describe(&error)).await?,
                }
            }
        }
        Ok(())
    }

    async fn frame(&mut self, frame: Result<Option<mobius_gateway::wire::ServerFrame>>) -> Sent {
        match frame {
            Ok(Some(mut frame)) => {
                match &frame.message {
                    ServerMessage::Ready { .. } => {
                        self.attempt = 0;
                        self.waiting = match self.waiting.take() {
                            Some(Wait {
                                phase: Awaited::Ready { session_request },
                                ..
                            }) => session_request
                                .map(|request_id| Wait::new(Awaited::Session { request_id })),
                            waiting => waiting,
                        };
                    }
                    ServerMessage::Error { fatal, code, .. }
                        if *fatal || code == "unauthorized" =>
                    {
                        self.target = None;
                        self.waiting = None;
                    }
                    ServerMessage::Rejected { fatal: true, .. } => {
                        self.target = None;
                        self.waiting = None;
                    }
                    _ => {}
                }
                match &frame.message {
                    ServerMessage::SessionReplayComplete { request_id, .. }
                    | ServerMessage::Rejected { request_id, .. } => self.finish_wait(request_id),
                    _ => {}
                }
                self.observe(&mut frame.message);
                self.emit(Update::Message(Box::new(frame.message))).await
            }
            Ok(None) => {
                self.connection = None;
                self.lost("The gateway closed the connection.".into()).await
            }
            Err(error) => {
                self.connection = None;
                self.lost(super::describe(&error)).await
            }
        }
    }

    fn await_session(&mut self, request_id: Arc<str>) {
        if let Some(Wait {
            phase: Awaited::Ready { session_request },
            ..
        }) = self.waiting.as_mut()
        {
            *session_request = Some(request_id);
        } else {
            self.waiting = Some(Wait::new(Awaited::Session { request_id }));
        }
    }

    fn finish_wait(&mut self, request_id: &str) {
        match self.waiting.as_mut().map(|wait| &mut wait.phase) {
            Some(Awaited::Session {
                request_id: expected,
            }) if **expected == *request_id => self.waiting = None,
            Some(Awaited::Ready { session_request })
                if session_request.as_deref() == Some(request_id) =>
            {
                *session_request = None;
            }
            _ => {}
        }
    }

    async fn wait_timed_out(&mut self) -> Sent {
        let Some(wait) = self.waiting.take() else {
            return Ok(());
        };
        self.connection = None;
        self.transfers = None;
        let message = match wait.phase {
            Awaited::Ready { .. } => "The gateway did not become ready in time.",
            Awaited::Session { request_id } => {
                let message = "The gateway did not finish loading this chat in time.";
                self.emit(Update::SendFailed {
                    request_id: Some(request_id),
                    delivery_uncertain: true,
                    message: message.into(),
                })
                .await?;
                message
            }
        };
        self.lost(message.into()).await
    }

    /// The next connection reopens a fully replayed chat after its last received event.
    fn prepare_resume(&mut self) {
        if let Some(chat) = self.open.take().filter(|chat| chat.complete) {
            self.next.pipelined = vec![ClientMessage::OpenSession {
                request_id: super::request_id(),
                session_id: chat.session_id,
                last_sequence: Some(chat.latest),
            }];
        }
    }

    /// Keeps what the next connection needs: catalog revisions and the open chat.
    fn observe(&mut self, message: &mut ServerMessage) {
        let revisions = &mut self.next.catalog.known;
        match message {
            ServerMessage::Ready { payload } | ServerMessage::GatewayConfigured { payload, .. } => {
                *revisions = std::mem::take(&mut payload.revisions);
            }
            ServerMessage::SessionsChanged { revision, .. } => {
                revisions.insert(ReadySection::Sessions, std::mem::take(revision));
            }
            // The window's copy no longer matches a revision the gateway reported.
            ServerMessage::Sessions { .. } => {
                revisions.remove(&ReadySection::Sessions);
            }
            ServerMessage::Bots { .. } => {
                revisions.remove(&ReadySection::Bots);
            }
            ServerMessage::Contributions { .. } => {
                revisions.remove(&ReadySection::Config);
            }
            ServerMessage::SessionOpened {
                request_id,
                payload,
            } => {
                let Some(wait) = self
                    .waiting
                    .as_mut()
                    .filter(|wait| wait.is_session(request_id))
                else {
                    return;
                };
                wait.progressed();
                self.open = Some(OpenChat {
                    request_id: Arc::clone(request_id),
                    session_id: payload.session.session_id.share(),
                    latest: payload.latest_sequence,
                    replayed: None,
                    complete: false,
                });
            }
            ServerMessage::SessionReplayComplete {
                session_id,
                request_id,
            } => {
                if let Some(chat) = self.open.as_mut().filter(|chat| {
                    *chat.session_id == **session_id && chat.request_id == *request_id
                }) {
                    chat.complete = true;
                }
            }
            ServerMessage::SessionTranscript {
                session_id,
                through_sequence,
                ..
            } => {
                if let Some(chat) = self
                    .open
                    .as_mut()
                    .filter(|chat| chat.session_id == *session_id)
                {
                    chat.latest = chat.latest.max(*through_sequence);
                    chat.replayed = Some(*through_sequence);
                    if let Some(wait) = self
                        .waiting
                        .as_mut()
                        .filter(|wait| wait.is_session(&chat.request_id))
                    {
                        wait.progressed();
                    }
                }
            }
            ServerMessage::AgentEvent {
                session_id, record, ..
            } => {
                if let Some(chat) = self
                    .open
                    .as_mut()
                    .filter(|chat| chat.session_id == *session_id)
                {
                    chat.latest = chat.latest.max(record.sequence);
                    if !chat.complete
                        && chat
                            .replayed
                            .is_none_or(|sequence| record.sequence > sequence)
                        && let Some(wait) = self
                            .waiting
                            .as_mut()
                            .filter(|wait| wait.is_session(&chat.request_id))
                    {
                        chat.replayed = Some(record.sequence);
                        wait.progressed();
                    }
                }
            }
            _ => {}
        }
    }

    /// Ends the desktop's auxiliary connection before leaving this gateway.
    fn close_desktop(&mut self) {
        if let Some(stop) = self.desktop_stop.take()
            && let Err(error) = stop.send(true)
        {
            tracing::debug!(%error, "the desktop connection had already closed");
        }
    }

    /// Schedules another attempt at the same gateway, unless retries are blocked.
    async fn lost(&mut self, message: String) -> Sent {
        self.close_desktop();
        self.desktop_target = None;
        self.retry_after(retry_delay(self.attempt, random_unit()), message)
            .await
    }

    async fn retry_after(&mut self, delay: Duration, message: String) -> Sent {
        self.waiting = None;
        let Some(target) = &self.target else {
            return self.emit(Update::Disconnected).await;
        };
        let endpoint = match target {
            Connection::Local => None,
            Connection::Saved(endpoint) | Connection::Pair { endpoint, .. } => {
                Some(endpoint.clone()) // rules-allow: the update owns its endpoint while the target keeps it for the retry
            }
        };
        self.attempt += 1;
        self.retry = Some(Box::pin(tokio::time::sleep(delay)));
        self.emit(Update::Reconnecting {
            endpoint,
            attempt: self.attempt,
            message,
        })
        .await
    }

    async fn fail(&mut self, message: String) -> Sent {
        self.close_desktop();
        self.desktop_target = None;
        self.target = None;
        self.retry = None;
        self.waiting = None;
        self.emit(Update::Error(message)).await
    }

    async fn send(&mut self, message: ClientMessage) -> Sent {
        if matches!(
            message,
            ClientMessage::OpenSession { .. } | ClientMessage::CreateSession { .. }
        ) {
            self.open = None;
            if self.connection.is_some()
                && let Some(request_id) = correlated_request(&message)
            {
                self.await_session(request_id);
            }
        }
        let request_id = correlated_request(&message);
        let delivery_uncertain = self.connection.is_some();
        let result = match &self.connection {
            // GatewaySender closes its writer when a write is cancelled; never replay a
            // command whose frame may already have reached the gateway.
            Some((sender, _)) => tokio::time::timeout(RESPONSE_TIMEOUT, sender.send(message))
                .await
                .unwrap_or_else(|_| Err(Error::Protocol("Gateway request timed out.".into()))),
            None => Err(Error::Protocol(
                "Connect to a gateway before sending.".into(),
            )),
        };
        if let Err(error) = result {
            self.connection = None;
            self.waiting = None;
            let message = super::describe(&error);
            self.emit(Update::SendFailed {
                request_id,
                delivery_uncertain,
                message: message.clone(), // rules-allow: the failure update and the reconnect notice each own the message
            })
            .await?;
            if delivery_uncertain {
                self.lost(message).await?;
            }
        }
        Ok(())
    }
}

fn correlated_request(message: &ClientMessage) -> Option<Arc<str>> {
    match message {
        ClientMessage::Submit { submission, .. } => Some(Arc::clone(&submission.id)),
        ClientMessage::OpenComputer { request_id, .. }
        | ClientMessage::OpenSession { request_id, .. }
        | ClientMessage::CreateSession { request_id, .. }
        | ClientMessage::GetStorageUsage { request_id }
        | ClientMessage::DeleteSessions { request_id, .. }
        | ClientMessage::DeleteWorkspaceFile { request_id, .. } => Some(Arc::clone(request_id)),
        _ => None,
    }
}

async fn connect(
    target: Connection,
    options: &mut ConnectOptions,
) -> Result<(Endpoint, String, GatewayClient)> {
    let target = match state_dir() {
        Ok(directory) => local_connection(target, &directory)?,
        Err(_) => target,
    };
    match target {
        Connection::Local => connect_local(options).await,
        Connection::Saved(endpoint) => {
            let endpoint = endpoint.parse()?;
            let token = load_local_client(&endpoint)?
                .ok_or_else(|| Error::Config("Pair this gateway before connecting.".into()))?;
            let client = connect_as(&endpoint, &token, APP_CLIENT, options).await?;
            Ok((endpoint, token, client))
        }
        Connection::Pair { endpoint, code } => {
            let endpoint: Endpoint = endpoint.parse()?;
            let authenticated = admit(endpoint.clone())?; // rules-allow: admission takes the endpoint by value and the plain one is saved below
            let accounts = GatewayAccounts::load()?;
            accounts.prepare()?;
            let (client, paired) = tokio::time::timeout(
                opening_timeout(authenticated != endpoint, true),
                GatewayClient::pair(&authenticated, code, "Mobius Desktop", APP_CLIENT),
            )
            .await
            .map_err(|elapsed| {
                tracing::debug!(%elapsed, "gateway pairing timed out");
                Error::Protocol("Gateway pairing timed out.".into())
            })??;
            save_local_client(&endpoint, paired.token.clone())?; // rules-allow: the saved account and the live connection each own the token
            Ok((endpoint, paired.token, client))
        }
    }
}

fn local_connection(target: Connection, directory: &std::path::Path) -> Result<Connection> {
    if let Connection::Saved(address) = &target {
        let endpoint: Endpoint = address.parse()?;
        // Only the configured local listener belongs to this lifecycle. A saved
        // tunnel or another loopback gateway remains an independently managed connection.
        if !endpoint.is_websocket()
            && directory.is_dir()
            && dashboard_gateway_endpoint(directory).is_ok_and(|local| local == endpoint)
        {
            return Ok(Connection::Local);
        }
    }
    Ok(target)
}

async fn connect_local(options: &mut ConnectOptions) -> Result<(Endpoint, String, GatewayClient)> {
    let directory = state_dir()?;
    let snapshot = cfg!(feature = "snapshot") && std::env::var_os("MOBIUS_SNAPSHOT").is_some();
    if !snapshot && needs_bootstrap(&directory)? {
        run_embedded(vec![
            "bootstrap".into(),
            "--state-dir".into(),
            directory.clone().into_os_string(), // rules-allow: bootstrap takes owned arguments and the directory is used after it
        ])
        .await?;
    }
    let endpoint = dashboard_gateway_endpoint(&directory)?;
    if !snapshot {
        mobius_gateway::command::ensure_background_gateway(directory, configured_token).await?;
    }
    let token = configured_token(&endpoint)?.ok_or_else(|| missing_local_token(&endpoint))?;
    let client = connect_as(&endpoint, &token, APP_CLIENT, options).await?;
    Ok((endpoint, token, client))
}

fn needs_bootstrap(directory: &std::path::Path) -> Result<bool> {
    match std::fs::read_dir(directory) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Ok(false);
            }
            // The gateway initializes its own private directory. Never remove nonempty state.
            std::fs::remove_dir(directory)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

/// The window's own connection: a person's app that renders browser pages, hosts desktop
/// control for a gateway on this Mac and views a remote desktop.
const APP_CLIENT: ClientCapabilities = ClientCapabilities {
    presence: true,
    browser_renderer: true,
    desktop_host: true,
    remote_desktop_viewer: true,
};

/// A side connection that does nothing for the gateway but transfer.
pub(super) async fn connect_once(
    endpoint: &Endpoint,
    token: &str,
    options: &mut ConnectOptions,
) -> Result<GatewayClient> {
    connect_as(endpoint, token, ClientCapabilities::default(), options).await
}

pub(super) async fn connect_as(
    endpoint: &Endpoint,
    token: &str,
    client: ClientCapabilities,
    options: &mut ConnectOptions,
) -> Result<GatewayClient> {
    let authenticated = admit(endpoint.clone())?; // rules-allow: admission takes the endpoint by value and the caller keeps the plain one
    tokio::time::timeout(
        opening_timeout(authenticated != *endpoint, false),
        GatewayClient::connect_with(&authenticated, token, client, options),
    )
    .await
    .map_err(|elapsed| {
        tracing::debug!(%elapsed, "gateway connection timed out");
        Error::Protocol("Gateway connection timed out.".into())
    })?
}

fn opening_timeout(cloud_admission: bool, pairing: bool) -> Duration {
    if cloud_admission {
        CLOUD_OPEN_TIMEOUT
    } else {
        Duration::from_secs(if pairing { 30 } else { 15 })
    }
}

/// Called before GPUI initialization when the gateway relaunches this executable.
///
/// # Errors
///
/// When the gateway server fails to start or stops with an error.
pub async fn run_embedded(arguments: Vec<OsString>) -> Result<()> {
    // Keep the full gateway server future out of the desktop connection actor layout.
    Box::pin(mobius_gateway::command::run(
        arguments,
        save_local_client,
        load_local_client,
    ))
    .await
}

/// The endpoints of the gateways this Mac has paired.
///
/// # Errors
///
/// When the saved gateway accounts cannot be read.
pub fn saved_endpoints() -> Result<Vec<String>> {
    Ok(GatewayAccounts::load()?
        .endpoints()
        .map(str::to_owned)
        .collect())
}

fn save_local_client(endpoint: &Endpoint, token: String) -> Result<()> {
    let mut accounts = GatewayAccounts::load()?;
    accounts.add(endpoint, token)?;
    accounts.save()
}

pub(super) fn load_local_client(endpoint: &Endpoint) -> Result<Option<String>> {
    Ok(GatewayAccounts::load()?.token(endpoint).map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::{Command, Update, needs_bootstrap};
    use crate::fixture::opened;
    use mobius::SessionId;
    use std::time::Duration;

    async fn fake_gateway() -> (
        super::Endpoint,
        tokio::sync::mpsc::UnboundedSender<super::ServerMessage>,
        tokio::task::JoinHandle<()>,
    ) {
        use mobius_gateway::wire::{
            ClientFrame, FrameReader, ServerFrame, read_frame, write_frame,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("tcp://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let (frames, mut messages) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = socket.into_split();
            let mut reader = FrameReader::new(reader);
            read_frame::<ClientFrame>(&mut reader).await.unwrap();
            write_frame(
                &mut writer,
                &ServerFrame::new(super::ServerMessage::Authenticated),
            )
            .await
            .unwrap();
            loop {
                tokio::select! {
                    frame = read_frame::<ClientFrame>(&mut reader) => if frame.unwrap().is_none() { return; },
                    Some(message) = messages.recv() => write_frame(&mut writer, &ServerFrame::new(message)).await.unwrap(),
                }
            }
        });
        (endpoint, frames, server)
    }

    async fn connected_transport() -> (
        super::Transport,
        tokio::sync::mpsc::Receiver<Update>,
        tokio::sync::mpsc::UnboundedSender<super::ServerMessage>,
        tokio::task::JoinHandle<()>,
    ) {
        let (endpoint, frames, server) = fake_gateway().await;
        let client = super::GatewayClient::connect(&endpoint, "test", super::APP_CLIENT)
            .await
            .unwrap();
        let (updates, mut output) = tokio::sync::mpsc::channel(16);
        let mut transport = transport(updates);
        transport
            .connected(Ok((endpoint, "test".into(), client)))
            .await
            .unwrap();
        assert!(matches!(output.recv().await, Some(Update::Connected(_))));
        (transport, output, frames, server)
    }

    #[tokio::test]
    async fn authenticated_socket_that_never_becomes_ready_reconnects() {
        let (mut transport, mut output, _frames, server) = connected_transport().await;
        transport.waiting.as_mut().unwrap().deadline =
            Box::pin(tokio::time::sleep(Duration::from_millis(20)));
        let (commands, receiver) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(super::run_transport(receiver, transport));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), output.recv()).await.unwrap(),
            Some(Update::Reconnecting { attempt: 1, message, .. }) if message.contains("did not become ready")
        ));
        drop(commands);
        task.await.unwrap().unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn replacement_connect_drops_the_pending_attempt() {
        struct Cancelled(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Cancelled {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&cancelled);
        let (started, waiting) = tokio::sync::oneshot::channel();
        let (updates, mut output) = tokio::sync::mpsc::channel(4);
        let mut transport = transport(updates);
        transport.dial = Some(Box::pin(async move {
            let _cancelled = Cancelled(flag);
            started.send(()).unwrap();
            std::future::pending().await
        }));
        let (commands, receiver) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(super::run_transport(receiver, transport));
        waiting.await.unwrap();
        // Parse fails before reading any real gateway state or Keychain credentials.
        commands
            .send(Command::Connect(super::Connection::Saved("invalid".into())))
            .await
            .unwrap();
        assert!(
            matches!(output.recv().await, Some(Update::Connecting { label, .. }) if label == "invalid")
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), output.recv())
                .await
                .unwrap(),
            Some(Update::Error(_))
        ));
        assert!(cancelled.load(std::sync::atomic::Ordering::SeqCst));
        drop(commands);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn missing_open_or_replay_completion_reports_uncertain_delivery_without_recreating() {
        for opened_first in [false, true] {
            let (mut transport, mut output, _frames, server) = connected_transport().await;
            transport.waiting = None;
            transport
                .send(super::ClientMessage::CreateSession {
                    request_id: "open".into(),
                    bot_id: "bot".into(),
                    workspace: "/workspace".into(),
                })
                .await
                .unwrap();
            if opened_first {
                transport
                    .frame(Ok(Some(mobius_gateway::wire::ServerFrame::new(opened(
                        "chat", 5,
                    )))))
                    .await
                    .unwrap();
                assert!(matches!(output.recv().await, Some(Update::Message(_))));
            }
            transport.waiting.as_mut().unwrap().deadline =
                Box::pin(tokio::time::sleep(Duration::from_millis(20)));
            let (commands, receiver) = tokio::sync::mpsc::channel(1);
            let task = tokio::spawn(super::run_transport(receiver, transport));
            assert!(
                matches!(tokio::time::timeout(Duration::from_secs(1), output.recv()).await.unwrap(), Some(Update::SendFailed {
                request_id: Some(request_id), delivery_uncertain: true, ..
            }) if &*request_id == "open")
            );
            assert!(matches!(
                output.recv().await,
                Some(Update::Reconnecting { attempt: 1, .. })
            ));
            drop(commands);
            task.await.unwrap().unwrap();
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn only_current_replay_progress_extends_the_session_wait() {
        let (updates, _output) = tokio::sync::mpsc::channel(8);
        let mut transport = transport(updates);
        transport.await_session("open".into());
        transport.observe(&mut opened("chat", 5));
        // Replay record numbers start below the snapshot's latest durable sequence.
        transport.waiting.as_mut().unwrap().deadline =
            Box::pin(tokio::time::sleep(Duration::from_millis(1)));
        transport.observe(&mut event("chat", 2));
        let advanced = transport.waiting.as_ref().unwrap().deadline.deadline();
        assert!(advanced > tokio::time::Instant::now() + Duration::from_secs(29));
        for mut message in [event("other", 99), event("chat", 1), event("chat", 2)] {
            transport.observe(&mut message);
        }
        transport.finish_wait("old-open");
        assert_eq!(
            transport.waiting.as_ref().unwrap().deadline.deadline(),
            advanced
        );

        transport.await_session("replacement".into());
        transport.open = None;
        let replacement = transport.waiting.as_ref().unwrap().deadline.deadline();
        transport.observe(&mut opened("old-chat", 100));
        transport.finish_wait("open");
        assert!(transport.open.is_none());
        assert_eq!(
            transport.waiting.as_ref().unwrap().deadline.deadline(),
            replacement
        );
        transport.finish_wait("replacement");
        assert!(transport.waiting.is_none());
    }

    #[tokio::test]
    async fn fatal_authentication_failures_do_not_enter_retry() {
        for error in [
            super::Error::Unauthorized,
            super::Error::Config("invalid gateway configuration".into()),
        ] {
            let (updates, mut output) = tokio::sync::mpsc::channel(1);
            let mut transport = transport(updates);
            transport.connected(Err(error)).await.unwrap();
            assert!(matches!(output.recv().await, Some(Update::Error(_))));
            assert!(transport.target.is_none() && transport.retry.is_none());
        }
    }

    #[tokio::test]
    async fn startup_retries_follow_the_server_delay_for_saved_and_pairing_connections() {
        for target in [
            super::Connection::Saved("wss://example.com".into()),
            super::Connection::Pair {
                endpoint: "wss://example.com".into(),
                code: "one-time-code".into(),
            },
        ] {
            let (updates, mut output) = tokio::sync::mpsc::channel(1);
            let mut transport = transport(updates);
            transport.target = Some(target.clone());
            transport.attempt = 8;
            let delay = Duration::from_secs(1);
            let before = tokio::time::Instant::now();
            transport
                .connected(Err(super::Error::WebSocketUpgrade {
                    status: 503,
                    retry_after: Some(delay),
                }))
                .await
                .unwrap();
            let after = tokio::time::Instant::now();
            let deadline = transport.retry.as_ref().unwrap().deadline();
            assert!(deadline >= before + delay && deadline <= after + delay);
            assert!(transport.target.as_ref() == Some(&target));
            assert!(matches!(
                output.recv().await,
                Some(Update::Reconnecting { attempt: 9, message, .. }) if message == super::super::GATEWAY_STARTING
            ));
        }
    }

    #[tokio::test]
    async fn failed_send_schedules_reconnect_instead_of_leaving_the_ui_ready() {
        use mobius_gateway::wire::{
            ClientFrame, FrameReader, ServerFrame, read_frame, write_frame,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("tcp://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = socket.into_split();
            read_frame::<ClientFrame>(&mut FrameReader::new(reader))
                .await
                .unwrap();
            write_frame(
                &mut writer,
                &ServerFrame::new(super::ServerMessage::Authenticated),
            )
            .await
            .unwrap();
        });
        let client = super::GatewayClient::connect(&endpoint, "test", super::APP_CLIENT)
            .await
            .unwrap();
        let (updates, mut output) = tokio::sync::mpsc::channel(2);
        let mut transport = super::Transport {
            updates,
            connection: Some(client.into_parts()),
            dial: None,
            waiting: None,
            transfers: None,
            desktop_target: None,
            desktop_stop: None,
            target: Some(super::Connection::Local),
            attempt: 0,
            retry: None,
            tasks: tokio::task::JoinSet::new(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(4)),
            next: Default::default(),
            open: None,
        };
        // GatewaySender deterministically rejects a second authentication message.
        transport
            .send(super::ClientMessage::Authenticate {
                token: "test".into(),
                client: super::APP_CLIENT,
                catalog: Default::default(),
            })
            .await
            .unwrap();
        assert!(matches!(
            output.recv().await,
            Some(Update::SendFailed {
                delivery_uncertain: true,
                ..
            })
        ));
        assert!(matches!(
            output.recv().await,
            Some(Update::Reconnecting { attempt: 1, .. })
        ));
        assert!(transport.connection.is_none() && transport.retry.is_some());
        server.await.unwrap();
    }

    fn transport(updates: tokio::sync::mpsc::Sender<Update>) -> super::Transport {
        super::Transport {
            updates,
            connection: None,
            dial: None,
            waiting: None,
            transfers: None,
            desktop_target: None,
            desktop_stop: None,
            target: Some(super::Connection::Local),
            attempt: 0,
            retry: None,
            tasks: tokio::task::JoinSet::new(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(4)),
            next: Default::default(),
            open: None,
        }
    }

    fn event(session: &str, sequence: u64) -> super::ServerMessage {
        super::ServerMessage::AgentEvent {
            session_id: SessionId::new(session).unwrap(),
            record: mobius_gateway::wire::RecordedEvent {
                sequence,
                recorded_at_ms: 0,
                event: mobius::protocol::Event {
                    submission_id: None,
                    msg: mobius::protocol::EventMsg::ContextCompacted,
                },
                stream_metrics: Vec::new(),
                preview: None,
            },
            transcript: Vec::new(),
            run_stats: None,
        }
    }

    #[tokio::test]
    async fn reconnects_send_the_held_catalog_and_resume_the_open_chat() {
        use mobius_gateway::wire::{ReadySection, ServerMessage};
        let (updates, _output) = tokio::sync::mpsc::channel(8);
        let mut transport = transport(updates);
        transport.prepare_resume();
        assert!(transport.next.catalog.known.is_empty() && transport.next.pipelined.is_empty());

        let mut payload = crate::state::tests::catalog::ready(&[], 0);
        payload.revisions = [
            (ReadySection::Config, "config".into()),
            (ReadySection::Sessions, "sessions".into()),
        ]
        .into();
        let mut ready = ServerMessage::Ready { payload };
        transport.observe(&mut ready);
        let ServerMessage::Ready { payload } = &ready else {
            unreachable!()
        };
        assert!(
            payload.revisions.is_empty(),
            "the window does not need revisions"
        );
        transport.observe(&mut ServerMessage::SessionsChanged {
            sessions: Vec::new(),
            revision: "delta".into(),
        });

        // Only a chat whose replay finished resumes, from the last event received for it.
        transport.await_session("open".into());
        for mut message in [opened("chat", 5), event("chat", 7), event("other", 9)] {
            transport.observe(&mut message);
        }
        transport.prepare_resume();
        assert!(
            transport.next.pipelined.is_empty(),
            "an unfinished replay reopens in full"
        );
        transport.await_session("open".into());
        for mut message in [
            opened("chat", 5),
            event("chat", 7),
            event("other", 9),
            ServerMessage::SessionReplayComplete {
                request_id: "open".into(),
                session_id: SessionId::new("chat").unwrap(),
            },
        ] {
            transport.observe(&mut message);
        }
        transport.prepare_resume();
        assert_eq!(
            transport.next.catalog.known,
            [
                (ReadySection::Config, "config".into()),
                (ReadySection::Sessions, "delta".into())
            ]
            .into()
        );
        assert!(matches!(
            transport.next.pipelined.as_slice(),
            [super::ClientMessage::OpenSession {
                session_id,
                last_sequence: Some(7),
                ..
            }] if &**session_id == "chat"
        ));
        transport.observe(&mut ServerMessage::Sessions {
            request_id: Some("list".into()),
            sessions: Vec::new(),
        });
        assert_eq!(
            transport.next.catalog.known.keys().collect::<Vec<_>>(),
            [&ReadySection::Config]
        );
    }

    #[test]
    fn local_bootstrap_recovers_only_absent_or_empty_state() {
        let parent = tempfile::tempdir().unwrap();
        let state = parent.path().join("gateway");
        assert!(needs_bootstrap(&state).unwrap());
        std::fs::create_dir(&state).unwrap();
        assert!(needs_bootstrap(&state).unwrap());
        assert!(!state.exists());
        std::fs::create_dir(&state).unwrap();
        let existing = state.join("gateway.toml");
        std::fs::write(&existing, "preserve malformed configuration").unwrap();
        assert!(!needs_bootstrap(&state).unwrap());
        assert_eq!(
            std::fs::read_to_string(existing).unwrap(),
            "preserve malformed configuration"
        );
    }
}
