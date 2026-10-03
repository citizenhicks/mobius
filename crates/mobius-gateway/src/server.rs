//! Authenticated raw, WebSocket-loopback, and TLS gateway listeners.

use crate::host::session_file_rejection;

mod desktop;
mod dispatch;
mod responses;
mod transport;
mod view;
mod voice;

use crate::telemetry::{StopCause, Trigger};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use mobius::agent::validate_submission;
use mobius::backend::session_files::{PendingSessionFileWrite, SessionFileStore};
use mobius::protocol::Op;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::accept_hdr_async_with_config;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::http::header::{HOST, ORIGIN, SEC_WEBSOCKET_PROTOCOL};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use uuid::Uuid;

use crate::auth::{AuthStore, ClientIdentity, PairingGrant};
use crate::bots::BotStore;
use crate::config::{ConfigStore, CredentialStore, GatewayConfig, TlsConfig};
use crate::host::{GatewayHost, HostHandle, Rejection};
use crate::wire::{
    CatalogHint, ClientFrame, ClientKind, ClientMessage, ClientStatus, DirectoryEntry,
    DirectoryListing, FrameReader, GatewayNotification, MAX_FRAME_BYTES, ProfileSnapshot,
    ServerFrame, ServerMessage, SharedFrame, read_frame, read_frame_with_limit, validate_version,
    websocket_error, write_frame,
};
use crate::{Error, Result};

use self::dispatch::*;
use self::responses::*;
use self::transport::*;
use self::view::ClientView;

const PRE_AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_AUTHENTICATED_CONNECTIONS: usize = 32;
const MAX_PRE_AUTH_CONNECTIONS: usize = 8;
const MAX_CONNECTIONS: usize = MAX_AUTHENTICATED_CONNECTIONS + MAX_PRE_AUTH_CONNECTIONS;
const ROUTINE_TICK: Duration = Duration::from_secs(15);
const MAX_DIRECTORY_ENTRIES: usize = 512;
const MAX_PENDING_UPLOADS: usize = 8;
const WEBSOCKET_BRIDGE_BYTES: usize = 16 * 1024;
const ACCESS_EXPIRY_ENV: &str = "MOBIUS_GATEWAY_ACCESS_EXPIRES_AT";
const ACCESS_GRACE: Duration = Duration::from_secs(5 * 60);

const _: () = assert!(MAX_FRAME_BYTES <= u32::MAX as usize);

/// Fully assembled machine gateway and its chat registry.
pub struct GatewayServer {
    config: GatewayConfig,
    listener: TcpListener,
    access_lease: Option<AccessLease>,
    auth: Arc<AuthStore>,
    host: GatewayHost,
    bots: Arc<BotStore>,
    ready: Option<tokio::sync::oneshot::Sender<()>>,
}

impl GatewayServer {
    /// Opens protected state and the machine-wide chat registry.
    /// # Errors
    ///
    /// Returns an error if the resource cannot be read, decoded, or validated.
    pub async fn open(state_dir: PathBuf) -> Result<Self> {
        let (store, config) = ConfigStore::open(state_dir)?;
        let listener = TcpListener::bind(config.listen).await?;
        Self::assemble(store, config, listener).await
    }

    /// Binds and initializes a fresh local gateway before exposing its one-use pairing grant.
    /// # Errors
    ///
    /// Returns an error if configuration is invalid or a required resource cannot be initialized.
    pub async fn bootstrap(
        state_dir: PathBuf,
        listen: std::net::SocketAddr,
    ) -> Result<(Self, PairingGrant)> {
        let listener = TcpListener::bind(listen).await?;
        let listen = listener.local_addr()?;
        let (store, config) = ConfigStore::initialize(state_dir, listen, None)?;
        let initialized_state = store.state_dir().to_path_buf();
        let result = match AuthStore::initialize(store.auth_path()) {
            Ok((_, grant)) => Self::assemble(store, config, listener)
                .await
                .map(|server| (server, grant)),
            Err(error) => Err(error),
        };
        match result {
            Ok(result) => Ok(result),
            Err(error) => {
                fs::remove_dir_all(&initialized_state).map_err(|cleanup| {
                    Error::Config(format!(
                        "{error}; failed to remove incomplete gateway state at {}: {cleanup}",
                        initialized_state.display()
                    ))
                })?;
                Err(error)
            }
        }
    }

    async fn assemble(
        store: ConfigStore,
        config: GatewayConfig,
        listener: TcpListener,
    ) -> Result<Self> {
        let access_lease = configured_access_lease()?;
        let auth = Arc::new(AuthStore::open(store.auth_path())?);
        let credentials = Arc::new(CredentialStore::open(store.credentials_path())?);
        let bots = Arc::new(BotStore::open(store.state_dir())?);
        bots.sync_telemetry_cursors(&config.telemetry.sinks)?;
        let host =
            GatewayHost::start(store, config.clone(), credentials, Arc::clone(&bots)).await?;
        Ok(Self {
            config,
            listener,
            access_lease,
            auth,
            host,
            bots,
            ready: None,
        })
    }

    pub(crate) fn notify_ready(&mut self) -> tokio::sync::oneshot::Receiver<()> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.ready = Some(sender);
        receiver
    }

    /// Serves until shutdown or the configured idle interval (72 hours by default).
    /// # Errors
    ///
    /// Returns an error if validation or an operation required to complete the request fails.
    pub async fn serve(self) -> Result<()> {
        let websocket_host = self.configured_websocket_host()?;
        self.serve_with_host(websocket_host).await
    }

    /// Serves Cloudflare WebSockets using the resolved public hostname.
    pub(crate) async fn serve_cloudflare(self, hostname: String) -> Result<()> {
        let cloudflare = self.config.cloudflare.as_ref().ok_or_else(|| {
            Error::Config("a Cloudflare hostname requires tunnel configuration".into())
        })?;
        if cloudflare
            .hostname()
            .is_some_and(|configured| configured != hostname)
        {
            return Err(Error::Config(
                "runtime Cloudflare hostname does not match gateway configuration".into(),
            ));
        }
        self.serve_with_host(Some(hostname)).await
    }

    async fn serve_with_host(self, websocket_host: Option<String>) -> Result<()> {
        let inactivity_timeout = Duration::from_secs(self.config.runtime.idle_exit_seconds);
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            let mut interrupts = signal(SignalKind::interrupt())?;
            let mut terminations = signal(SignalKind::terminate())?;
            self.serve_until_inactive_with_host(
                async move {
                    tokio::select! {
                        _ = interrupts.recv() => {}
                        _ = terminations.recv() => {}
                    }
                },
                inactivity_timeout,
                websocket_host,
            )
            .await
        }
        #[cfg(not(unix))]
        self.serve_until_inactive_with_host(
            async {
                let _ = tokio::signal::ctrl_c().await;
            },
            inactivity_timeout,
            websocket_host,
        )
        .await
    }

    /// Serves until shutdown or the same inactivity policy as [`Self::serve`].
    ///
    /// Signal shutdown and await this future to close connections, finish routine
    /// dispatch, and stop resident sessions through their normal cleanup. Merely
    /// dropping the future does not perform graceful shutdown.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required to complete the request fails.
    pub async fn serve_until(self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let websocket_host = self.configured_websocket_host()?;
        let inactivity_timeout = Duration::from_secs(self.config.runtime.idle_exit_seconds);
        self.serve_until_inactive_with_host(shutdown, inactivity_timeout, websocket_host)
            .await
    }

    #[cfg(test)]
    async fn serve_until_inactive(
        self,
        shutdown: impl Future<Output = ()>,
        inactivity_timeout: Duration,
    ) -> Result<()> {
        let websocket_host = self.configured_websocket_host()?;
        self.serve_until_inactive_with_host(shutdown, inactivity_timeout, websocket_host)
            .await
    }

    async fn serve_until_inactive_with_host(
        mut self,
        shutdown: impl Future<Output = ()>,
        inactivity_timeout: Duration,
        websocket_host: Option<String>,
    ) -> Result<()> {
        self.config.validate()?;
        let tls = self.config.tls.as_ref().map(tls_acceptor).transpose()?;
        if tls.is_none() && !self.listener.local_addr()?.ip().is_loopback() {
            return Err(Error::Config(
                "plaintext listeners are restricted to loopback".into(),
            ));
        }
        let ingress = match self.config.runtime.ingress {
            Some(address) => Some(TcpListener::bind(address).await?),
            None => None,
        };
        let mut telemetry_tasks = JoinSet::new();
        let mut stop_cause = StopCause::Signal;
        let mut connections = JoinSet::new();
        let mut routine_dispatchers = JoinSet::new();
        let mut next_nudge = Instant::now();
        let mut next_hold = Instant::now();
        let mut hold_active = true;
        let connection_admission =
            ConnectionAdmission::new(MAX_PRE_AUTH_CONNECTIONS, MAX_AUTHENTICATED_CONNECTIONS);
        let client_connections = Arc::new(ClientConnections::default());
        let (client_revocations, _) = broadcast::channel(MAX_CONNECTIONS);
        let mut has_active_routines = self.bots.has_active_routines(Utc::now().timestamp())?
            || self.bots.has_pending_deliveries()?
            || self.bots.has_monitored_sessions()?;
        let inactivity = tokio::time::sleep(inactivity_timeout);
        tokio::pin!(inactivity);
        let mut routine_timer = tokio::time::interval(ROUTINE_TICK);
        routine_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tokio::pin!(shutdown);
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(());
        }
        crate::telemetry::Telemetry::tick(&self.host, 0, Trigger::Start, &mut telemetry_tasks)
            .await;
        let mut access_expired = false;
        let result = async {
            loop {
            tokio::select! {
                biased;
                () = &mut shutdown => break Ok(()),
                _ = async {
                    match self.access_lease {
                        Some(lease) => tokio::time::sleep_until(lease.deadline).await,
                        None => std::future::pending().await,
                    }
                } => {
                    access_expired = true;
                    break Ok(());
                }
                _ = async {
                    tokio::time::sleep_until(next_nudge).await;
                    self.host.telemetry.notify.notified().await;
                }, if routine_dispatchers.is_empty() => {
                    next_nudge = Instant::now() + Duration::from_secs(1);
                    let host = self.host.clone();
                    routine_dispatchers.spawn(async move {
                        if let Err(error) = host.dispatch_bot_events().await { eprintln!("Bot delivery failed: code={}", error.code); }
                    });
                }
                _ = routine_timer.tick() => {
                    if self.access_lease.is_some_and(AccessLease::expired) {
                        access_expired = true;
                        break Ok(());
                    }
                    self.tick_telemetry(&client_connections, Trigger::Interval, &mut telemetry_tasks).await;
                    #[cfg(unix)]
                    if let Some(socket) = &self.config.runtime.hold_socket
                        && Instant::now() >= next_hold {
                        let busy = match (self.host.runtime_activity().await, client_connections.native_count()) {
                            (Ok(activity), Ok(clients)) => !activity.idle || clients > 0,
                            _ => { eprintln!("provider hold activity unavailable; retaining hold"); true }
                        };
                        if busy || hold_active {
                            match crate::telemetry::update_hold(socket, busy).await {
                                Ok(()) => hold_active = busy,
                                Err(error) => eprintln!("provider hold update failed: {error}"),
                            }
                        }
                        next_hold = Instant::now() + Duration::from_secs(60);
                    }
                    // Keep reservation and the shutdown decision under the same
                    // admission gate; future schedules alone do not keep it open.
                    let Ok(_admission) = self.host.begin_mutation().await else { continue; };
                    let now = Utc::now().timestamp();
                    let poll = self.bots.poll_due(now)?;
                    let routines_active = poll.active || self.bots.has_pending_deliveries()? || self.bots.has_monitored_sessions()?;
                    if has_active_routines && !routines_active && connections.is_empty() {
                        inactivity.as_mut().reset(tokio::time::Instant::now() + inactivity_timeout);
                    }
                    has_active_routines = routines_active;
                    let host = self.host.clone();
                    // One event delivery worker at a time; all retries retain their original message ID.
                    if routine_dispatchers.is_empty() {
                        routine_dispatchers.spawn(async move {
                            if let Err(error) = host.dispatch_bot_events().await { eprintln!("Bot delivery failed: code={}",error.code); }
                        });
                    }
                    // The clock only committed schedule.due events. The same
                    // hook action worker starts scheduled and manually requested runs.
                    let _ = poll.events;

                }
                Some(result) = telemetry_tasks.join_next(), if !telemetry_tasks.is_empty() => {
                    if let Err(error) = result { eprintln!("telemetry worker failed: {error}"); }
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {
                    if connections.is_empty() {
                        has_active_routines =
                            self.bots.has_active_routines(Utc::now().timestamp())? || self.bots.has_pending_deliveries()? || self.bots.has_monitored_sessions()?;
                        if !has_active_routines {
                            inactivity.as_mut().reset(tokio::time::Instant::now() + inactivity_timeout);
                        }
                    }
                }
                Some(_) = routine_dispatchers.join_next(), if !routine_dispatchers.is_empty() => {
                    self.tick_telemetry(&client_connections, Trigger::Interval, &mut telemetry_tasks).await;
                }
                accepted = async {
                    let admission = connection_admission.admit().await;
                    tokio::select! {
                        accepted = self.listener.accept() => accepted.map(|accepted| (accepted, admission, false)),
                        accepted = async { match &ingress { Some(listener) => listener.accept().await, None => std::future::pending().await } } => accepted.map(|accepted| (accepted, admission, true)),
                    }
                }, if connections.len() < MAX_CONNECTIONS => {
                    let ((stream, peer), admission, ingress_connection) = accepted?;
                    if self.access_lease.is_some_and(AccessLease::expired) {
                        access_expired = true;
                        break Ok(());
                    }
                    let auth = Arc::clone(&self.auth);
                    let host = self.host.clone();
                    let bots = Arc::clone(&self.bots);
                    let client_connections = Arc::clone(&client_connections);
                    let client_revocations = client_revocations.clone();
                    let tls = tls.clone();
                    let websocket_host = websocket_host.clone();
                    connections.spawn(async move {
                        let auth_deadline = Instant::now() + PRE_AUTH_TIMEOUT;
                        let connection = ConnectionContext {
                            local: peer.ip().is_loopback(),
                            desktop_transport: tls.is_some(),
                            auth,
                            host,
                            bots,
                            client_connections,
                            client_revocations,
                            admission,
                            access_lease: self.access_lease,
                        };
                        let result = if ingress_connection {
                            serve_websocket(stream, connection, PlaintextHandshake { expected_websocket_host: None, auth_deadline }).await
                        } else if let Some(tls) = tls {
                            let stream = match tokio::time::timeout_at(
                                auth_deadline,
                                tls.accept(stream),
                            )
                            .await
                            {
                                Ok(Ok(stream)) => stream,
                                Ok(Err(error)) => {
                                    eprintln!("gateway TLS handshake failed: {:?}", error.kind());
                                    return;
                                }
                                Err(_) => {
                                    eprintln!("gateway TLS handshake timed out");
                                    return;
                                }
                            };
                            serve_connection(stream, connection, auth_deadline, None).await
                        } else {
                            serve_plaintext_connection(
                                stream,
                                connection,
                                PlaintextHandshake {
                                    expected_websocket_host: websocket_host,
                                    auth_deadline,
                                },
                            )
                            .await
                        };
                        if let Err(error) = result {
                            eprintln!("gateway connection failed: {}", connection_diagnostic(&error));
                        }
                    });
                }
                () = &mut inactivity, if connections.is_empty() && !has_active_routines && (!cfg!(unix) || self.config.runtime.hold_socket.is_none()) => {
                    has_active_routines = self.bots.has_active_routines(Utc::now().timestamp())? || self.bots.has_pending_deliveries()? || self.bots.has_monitored_sessions()?;
                    if !has_active_routines {
                        if crate::telemetry::Telemetry::pending(&self.host).await {
                            inactivity.as_mut().reset(Instant::now() + ROUTINE_TICK);
                            continue;
                        }
                        stop_cause = StopCause::Idle;
                        break Ok(());
                    }
                }
            }
            }
        }
        .await;
        connections.shutdown().await;
        if access_expired {
            routine_dispatchers.shutdown().await;
        } else {
            while routine_dispatchers.join_next().await.is_some() {}
        }
        self.host.shutdown().await;
        let cause = if access_expired {
            StopCause::LeaseExpired
        } else if result.is_err() {
            StopCause::Error
        } else {
            stop_cause
        };
        crate::telemetry::Telemetry::stop(&self.host, cause, &mut telemetry_tasks).await;
        #[cfg(unix)]
        if let Some(socket) = &self.config.runtime.hold_socket
            && hold_active
            && let Err(error) = crate::telemetry::update_hold(socket, false).await
        {
            eprintln!("provider hold release failed: {error}");
        }
        result
    }

    async fn tick_telemetry(
        &self,
        connections: &ClientConnections,
        trigger: Trigger,
        tasks: &mut JoinSet<()>,
    ) {
        match connections.native_count() {
            Ok(clients) => {
                crate::telemetry::Telemetry::tick(&self.host, clients, trigger, tasks).await
            }
            Err(error) => eprintln!("telemetry client count unavailable: {error}"),
        }
    }

    fn configured_websocket_host(&self) -> Result<Option<String>> {
        self.config
            .cloudflare
            .as_ref()
            .map(|cloudflare| {
                cloudflare.hostname().map(str::to_owned).ok_or_else(|| {
                    Error::Config(
                        "quick tunnel hostname is unavailable before cloudflared starts".into(),
                    )
                })
            })
            .transpose()
    }

    /// Returns the bound address from persisted configuration.
    #[must_use]
    pub const fn listen_addr(&self) -> std::net::SocketAddr {
        self.config.listen
    }
}

#[derive(Clone, Copy)]
struct AccessLease {
    expires_at: SystemTime,
    deadline: Instant,
}

impl AccessLease {
    fn expired(self) -> bool {
        SystemTime::now() >= self.expires_at || Instant::now() >= self.deadline
    }
}

fn configured_access_lease() -> Result<Option<AccessLease>> {
    let expires_at = match std::env::var(ACCESS_EXPIRY_ENV) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => return Err(Error::Config("gateway access expiry is invalid".into())),
    };
    // Cloud Sprites already set this version marker; a missing lease must stop them.
    access_lease(
        expires_at.as_deref(),
        std::env::var_os("MOBIUS_GATEWAY_VERSION").is_some(),
        SystemTime::now(),
    )
}

fn access_lease(
    expires_at: Option<&str>,
    cloud_managed: bool,
    now: SystemTime,
) -> Result<Option<AccessLease>> {
    let Some(expires_at) = expires_at else {
        return if cloud_managed {
            Err(Error::Config("gateway access expiry is required".into()))
        } else {
            Ok(None)
        };
    };
    if expires_at.is_empty() || !expires_at.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::Config("gateway access expiry is invalid".into()));
    }
    let seconds = expires_at
        .parse::<u64>()
        .map_err(|_| Error::Config("gateway access expiry is invalid".into()))?;
    let expires_at = UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))
        .and_then(|expiry| expiry.checked_add(ACCESS_GRACE))
        .ok_or_else(|| Error::Config("gateway access expiry is invalid".into()))?;
    let remaining = expires_at
        .duration_since(now)
        .map_err(|_| Error::Config("gateway access has expired".into()))?;
    if remaining.is_zero() {
        return Err(Error::Config("gateway access has expired".into()));
    }
    let deadline = Instant::now()
        .checked_add(remaining)
        .ok_or_else(|| Error::Config("gateway access expiry is invalid".into()))?;
    Ok(Some(AccessLease {
        expires_at,
        deadline,
    }))
}

#[cfg(test)]
mod tests;
