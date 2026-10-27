//! Authenticated raw, WebSocket-loopback, and TLS gateway listeners.

use crate::host::session_file_rejection;

mod desktop;
mod policy;
pub use policy::ConnectionPolicy;
mod dispatch;
mod responses;
mod transport;
mod view;
#[path = "harness/mobius/server/voice.rs"]
mod voice;

use crate::telemetry::{StopCause, Trigger};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::harness::ValidatedSubmission;
use chrono::Utc;
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
use crate::host::{GatewayHost, HostHandle, Rejection, SessionHistoryPage};
use crate::wire::{
    CatalogHint, ClientCapabilities, ClientFrame, ClientMessage, ClientStatus, DirectoryEntry,
    DirectoryListing, FrameReader, GatewayNotification, MAX_FRAME_BYTES, ProfileSnapshot,
    ServerFrame, ServerMessage, SharedFrame, read_frame, read_frame_with_limit, validate_version,
    websocket_error, write_frame,
};
use crate::{Error, Result};

use self::dispatch::{
    AuthenticatedClient, ConnectionSessionState, SelectedChat, handle_message, selected_broadcast,
};
use self::responses::{
    directory_rejection, internal_rejection, list_directories, open_selected, require_selected,
    require_uploads_enabled, routine_rejection, sequence, uploads_disabled_rejection,
    write_client_inventory, write_gateway_result, write_rejection, write_result,
    write_server_error, write_session_history,
};
use self::transport::{
    ClientConnections, ConnectionAdmission, ConnectionContext, ConnectionShared,
    PlaintextHandshake, connection_diagnostic, serve_connection, serve_plaintext_connection,
    serve_websocket, tls_acceptor,
};
use self::view::ClientView;

const ROUTINE_TICK: Duration = Duration::from_secs(15);
const MAX_DIRECTORY_ENTRIES: usize = 512;
use crate::wire::WEBSOCKET_BRIDGE_BYTES;
const ACCESS_EXPIRY_ENV: &str = "MOBIUS_GATEWAY_ACCESS_EXPIRES_AT";

const _: () = assert!(MAX_FRAME_BYTES <= u32::MAX as usize);

/// Fully assembled machine gateway and its chat registry.
pub struct GatewayServer {
    config: GatewayConfig,
    listener: TcpListener,
    access_lease: Option<AccessLease>,
    auth: Arc<AuthStore>,
    host: Arc<GatewayHost>,
    bots: Arc<BotStore>,
    ready: Option<tokio::sync::oneshot::Sender<()>>,
}

impl std::fmt::Debug for GatewayServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GatewayServer")
            .field("listener", &self.listener)
            .finish_non_exhaustive()
    }
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
        let result = match AuthStore::initialize_validated(store.auth_path(), config.auth) {
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
        let access_lease = configured_access_lease(&config.runtime)?;
        let auth = Arc::new(AuthStore::open_validated(store.auth_path(), config.auth)?);
        let credentials = Arc::new(CredentialStore::open(store.credentials_path())?);
        let bots = Arc::new(BotStore::open(store.state_dir())?);
        bots.sync_telemetry_cursors(&config.telemetry.sinks)?;
        let host = Arc::new(
            GatewayHost::start(store, config.clone(), credentials, Arc::clone(&bots)).await?, // rules-allow: the host and the server each keep their own configuration
        );
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

    /// Serves loopback clients at once and Cloudflare WebSockets once `hostname` holds the
    /// tunnel's public hostname; until then WebSocket upgrades are refused.
    pub(crate) async fn serve_cloudflare(self, hostname: Arc<OnceLock<String>>) -> Result<()> {
        self.serve_with_host(Some(hostname)).await
    }

    async fn serve_with_host(self, websocket_host: Option<Arc<OnceLock<String>>>) -> Result<()> {
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

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the shared listener state serves every connection until the loop ends; the lint misreads its use inside tokio::select!"
    )]
    async fn serve_until_inactive_with_host(
        mut self,
        shutdown: impl Future<Output = ()>,
        inactivity_timeout: Duration,
        websocket_host: Option<Arc<OnceLock<String>>>,
    ) -> Result<()> {
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
        let connection_admission = ConnectionAdmission::new(
            self.config.connections.pre_authentication,
            self.config.connections.authenticated,
        );
        let mut activity_hook =
            crate::telemetry::ActivityHook::new(self.config.telemetry.activity_hook.as_ref())?;
        let (client_revocations, _) = broadcast::channel(self.config.connections.total());
        let shared = Arc::new(ConnectionShared {
            auth: Arc::clone(&self.auth),
            host: Arc::clone(&self.host),
            bots: Arc::clone(&self.bots),
            client_connections: Arc::new(ClientConnections::default()),
            client_revocations,
            tls,
            websocket_host,
        });
        let client_connections = &shared.client_connections;
        let mut has_active_routines = self.bots.has_active_routines(Utc::now().timestamp())?
            || self.bots.has_pending_deliveries()?
            || self.bots.has_monitored_sessions()?;
        let inactivity = tokio::time::sleep(inactivity_timeout);
        tokio::pin!(inactivity);
        let mut routine_timer = tokio::time::interval(ROUTINE_TICK);
        routine_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tokio::pin!(shutdown);
        if let Some(ready) = self.ready.take()
            && ready.send(()).is_err()
        {
            tracing::debug!("gateway readiness receiver was dropped before serving");
        }
        crate::telemetry::Telemetry::tick(&self.host, 0, Trigger::Start, &mut telemetry_tasks)
            .await;
        let mut access_expired = false;
        let serving = async {
            loop {
                tokio::select! {
                    biased;
                    () = async {
                        tokio::time::sleep_until(next_nudge).await;
                        self.host.telemetry.notify.notified().await;
                    }, if routine_dispatchers.is_empty() => {
                        next_nudge = Instant::now() + Duration::from_secs(1);
                        let host = Arc::clone(&self.host);
                        routine_dispatchers.spawn(async move {
                            if let Err(error) = host.dispatch_bot_events().await {
                                tracing::warn!(code = %error.code, "Bot delivery failed");
                            }
                        });
                    }
                    _ = routine_timer.tick() => {
                        if self.access_lease.is_some_and(AccessLease::expired) {
                            access_expired = true;
                            break Ok(());
                        }
                        self.tick_telemetry(client_connections, Trigger::Interval, &mut telemetry_tasks).await;
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
                        let host = Arc::clone(&self.host);
                        // One event delivery worker at a time; all retries retain their original message ID.
                        if routine_dispatchers.is_empty() {
                            routine_dispatchers.spawn(async move {
                                if let Err(error) = host.dispatch_bot_events().await {
                                    tracing::warn!(code = %error.code, "Bot delivery failed");
                                }
                            });
                        }
                        // The clock only committed schedule.due events. The same
                        // hook action worker starts scheduled and manually requested runs.
                        drop(poll.events);

                    }
                    Some(result) = telemetry_tasks.join_next(), if !telemetry_tasks.is_empty() => {
                        if let Err(error) = result {
                            tracing::warn!(%error, "telemetry worker failed");
                        }
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
                        self.tick_telemetry(client_connections, Trigger::Interval, &mut telemetry_tasks).await;
                    }
                    accepted = async {
                        let admission = connection_admission.admit().await;
                        tokio::select! {
                            accepted = self.listener.accept() => accepted.map(|accepted| (accepted, admission, false)),
                            accepted = async { match &ingress { Some(listener) => listener.accept().await, None => std::future::pending().await } } => accepted.map(|accepted| (accepted, admission, true)),
                        }
                    }, if connections.len() < self.config.connections.total() => {
                        let ((stream, peer), admission, ingress_connection) = accepted?;
                        if let Err(error) = stream.set_nodelay(true) {
                            tracing::warn!(kind = ?error.kind(), "gateway TCP configuration failed");
                            continue;
                        }
                        if self.access_lease.is_some_and(AccessLease::expired) {
                            access_expired = true;
                            break Ok(());
                        }
                        let shared = Arc::clone(&shared);
                        connections.spawn(async move {
                            let auth_deadline = Instant::now() + Duration::from_secs(self.config.connections.authentication_timeout_seconds);
                            let connection = ConnectionContext {
                                local: peer.ip().is_loopback(),
                                desktop_transport: shared.tls.is_some(),
                                shared,
                                admission,
                                access_lease: self.access_lease,
                            };
                            let result = if ingress_connection {
                                serve_websocket(stream, connection, PlaintextHandshake { verify_websocket_host: false, auth_deadline }).await
                            } else if let Some(tls) = &connection.shared.tls {
                                let stream = match tokio::time::timeout_at(
                                    auth_deadline,
                                    tls.accept(stream),
                                )
                                .await
                                {
                                    Ok(Ok(stream)) => stream,
                                    Ok(Err(error)) => {
                                        tracing::warn!(
                                            kind = ?error.kind(), "gateway TLS handshake failed"
                                        );
                                        return;
                                    }
                                    Err(_) => {
                                        tracing::warn!("gateway TLS handshake timed out");
                                        return;
                                    }
                                };
                                serve_connection(stream, connection, auth_deadline, None).await
                            } else {
                                serve_plaintext_connection(
                                    stream,
                                    connection,
                                    PlaintextHandshake {
                                        verify_websocket_host: true,
                                        auth_deadline,
                                    },
                                )
                                .await
                            };
                            if let Err(error) = result {
                                tracing::warn!(
                                    error = %connection_diagnostic(&error), "gateway connection failed"
                                );
                            }
                        });
                    }
                    () = &mut inactivity, if connections.is_empty() && !has_active_routines && self.config.runtime.idle_exit_seconds != 0 => {
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
        };
        let (result, lease_expired) = {
            // Poll activity alongside the whole loop, including awaits inside
            // dispatch branches. This scope cancels both futures before actor cleanup.
            let activity = activity_hook.run(&self.host, || client_connections.presence_count());
            tokio::pin!(activity, serving);
            tokio::select! {
                biased;
                () = &mut shutdown => (Ok(()), false),
                () = async {
                    match self.access_lease {
                        Some(lease) => tokio::time::sleep_until(lease.deadline).await,
                        None => std::future::pending().await,
                    }
                } => (Ok(()), true),
                () = &mut activity => (Err(Error::Config("telemetry activity worker stopped".into())), false),
                result = &mut serving => (result, false),
            }
        };
        access_expired |= lease_expired;
        // Snapshot workers can also hold host state while waiting for actors.
        telemetry_tasks.shutdown().await;
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
        activity_hook.stop().await;
        result
    }

    async fn tick_telemetry(
        &self,
        connections: &ClientConnections,
        trigger: Trigger,
        tasks: &mut JoinSet<()>,
    ) {
        match connections.presence_count() {
            Ok(clients) => {
                crate::telemetry::Telemetry::tick(&self.host, clients, trigger, tasks).await;
            }
            Err(error) => tracing::warn!(%error, "telemetry client count unavailable"),
        }
    }

    fn configured_websocket_host(&self) -> Result<Option<Arc<OnceLock<String>>>> {
        self.config
            .cloudflare
            .as_ref()
            .map(|cloudflare| {
                cloudflare
                    .hostname()
                    .map(|hostname| Arc::new(OnceLock::from(hostname.to_owned())))
                    .ok_or_else(|| {
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

fn configured_access_lease(runtime: &crate::config::RuntimeConfig) -> Result<Option<AccessLease>> {
    let expires_at = match std::env::var(ACCESS_EXPIRY_ENV) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => return Err(Error::Config("gateway access expiry is invalid".into())),
    };
    access_lease(
        expires_at.as_deref(),
        runtime.require_access_lease,
        Duration::from_secs(runtime.access_grace_seconds),
        SystemTime::now(),
    )
}

fn access_lease(
    expires_at: Option<&str>,
    required: bool,
    grace: Duration,
    now: SystemTime,
) -> Result<Option<AccessLease>> {
    let Some(expires_at) = expires_at else {
        return if required {
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
        .map_err(|error| Error::Config(format!("gateway access expiry is invalid: {error}")))?;
    let expires_at = UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))
        .and_then(|expiry| expiry.checked_add(grace))
        .ok_or_else(|| Error::Config("gateway access expiry is invalid".into()))?;
    let remaining = expires_at
        .duration_since(now)
        .map_err(|error| Error::Config(format!("gateway access has expired: {error}")))?;
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
