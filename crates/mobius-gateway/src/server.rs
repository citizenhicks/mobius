//! Authenticated raw, WebSocket-loopback, and TLS gateway listeners.

mod dispatch;
mod responses;
mod transport;
mod voice;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use futures_util::StreamExt as _;
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
use tokio_tungstenite::tungstenite::http::header::{HOST, ORIGIN};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::auth::{AuthStore, ClientIdentity, PairingGrant};
use crate::bots::BotStore;
use crate::config::{ConfigStore, CredentialStore, GatewayConfig, TlsConfig};
use crate::host::{GatewayHost, HostHandle, Rejection};
use crate::wire::{
    ClientFrame, ClientKind, ClientMessage, ClientStatus, DirectoryEntry, DirectoryListing,
    FrameReader, GatewayNotification, MAX_FRAME_BYTES, ProfileSnapshot, ServerFrame, ServerMessage,
    SharedFrame, framed_to_websocket, read_frame, read_frame_with_limit, validate_version,
    websocket_error, websocket_to_framed, write_frame,
};
use crate::{Error, Result};

use self::dispatch::*;
use self::responses::*;
use self::transport::*;

const PRE_AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_AUTHENTICATED_CONNECTIONS: usize = 32;
const MAX_PRE_AUTH_CONNECTIONS: usize = 8;
const MAX_CONNECTIONS: usize = MAX_AUTHENTICATED_CONNECTIONS + MAX_PRE_AUTH_CONNECTIONS;
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(72 * 60 * 60);
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

    /// Serves until a process shutdown signal or 72 hours of inactivity.
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
                INACTIVITY_TIMEOUT,
                websocket_host,
            )
            .await
        }
        #[cfg(not(unix))]
        self.serve_until_inactive_with_host(
            async {
                let _ = tokio::signal::ctrl_c().await;
            },
            INACTIVITY_TIMEOUT,
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
        self.serve_until_inactive_with_host(shutdown, INACTIVITY_TIMEOUT, websocket_host)
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
        let mut connections = JoinSet::new();
        let mut routine_dispatchers = JoinSet::new();
        let connection_admission =
            ConnectionAdmission::new(MAX_PRE_AUTH_CONNECTIONS, MAX_AUTHENTICATED_CONNECTIONS);
        let client_connections = Arc::new(ClientConnections::default());
        let (client_revocations, _) = broadcast::channel(MAX_CONNECTIONS);
        let mut has_active_routines = self.bots.has_active_routines(Utc::now().timestamp())?;
        let inactivity = tokio::time::sleep(inactivity_timeout);
        tokio::pin!(inactivity);
        let mut routine_timer = tokio::time::interval(ROUTINE_TICK);
        routine_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tokio::pin!(shutdown);
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(());
        }
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
                _ = routine_timer.tick() => {
                    if self.access_lease.is_some_and(AccessLease::expired) {
                        access_expired = true;
                        break Ok(());
                    }
                    let now = Utc::now().timestamp();
                    let poll = self.bots.poll_due(now)?;
                    let routines_active = poll.active;
                    if has_active_routines && !routines_active && connections.is_empty() {
                        inactivity.as_mut().reset(tokio::time::Instant::now() + inactivity_timeout);
                    }
                    has_active_routines = routines_active;
                    let due = poll.due;
                    if !due.is_empty() {
                        let host = self.host.clone();
                        routine_dispatchers.spawn(async move {
                            for (routine_id, run) in due {
                                if let Err(error) = host.run_due_routine(routine_id.clone(), run).await {
                                    eprintln!(
                                        "routine run failed: routine_id={routine_id} code={} message={}",
                                        error.code, error.message
                                    );
                                }
                            }
                        });
                    }
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {
                    if connections.is_empty() {
                        has_active_routines =
                            self.bots.has_active_routines(Utc::now().timestamp())?;
                        if !has_active_routines {
                            inactivity.as_mut().reset(tokio::time::Instant::now() + inactivity_timeout);
                        }
                    }
                }
                Some(_) = routine_dispatchers.join_next(), if !routine_dispatchers.is_empty() => {}
                accepted = async {
                    let admission = connection_admission.admit().await;
                    self.listener.accept().await.map(|accepted| (accepted, admission))
                }, if connections.len() < MAX_CONNECTIONS => {
                    let ((stream, peer), admission) = accepted?;
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
                            auth,
                            host,
                            bots,
                            client_connections,
                            client_revocations,
                            admission,
                            access_lease: self.access_lease,
                        };
                        let result = if let Some(tls) = tls {
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
                () = &mut inactivity, if connections.is_empty() && !has_active_routines => {
                    has_active_routines = self.bots.has_active_routines(Utc::now().timestamp())?;
                    if !has_active_routines {
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
        result
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
