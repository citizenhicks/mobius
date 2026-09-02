//! Authenticated raw, WebSocket-loopback, and TLS gateway listeners.

mod dispatch;
mod responses;
mod transport;

use std::collections::BTreeMap;
use std::fs;
use std::fs::File;
use std::future::Future;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use futures_util::StreamExt as _;
use mobius::agent::validate_submission;
use mobius::middleware::session_files::{PendingSessionFileWrite, SessionFileStore};
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
    FrameReader, MAX_FRAME_BYTES, ServerFrame, ServerMessage, framed_to_websocket, read_frame,
    validate_version, websocket_error, websocket_to_framed, write_frame,
};
use crate::{Error, Result};

use self::dispatch::*;
use self::responses::*;
use self::transport::*;

const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CONNECTIONS: usize = 32;
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(72 * 60 * 60);
const ROUTINE_TICK: Duration = Duration::from_secs(15);
const MAX_DIRECTORY_ENTRIES: usize = 512;
const MAX_PENDING_UPLOADS: usize = 8;
const WEBSOCKET_BRIDGE_BYTES: usize = 16 * 1024;

const _: () = assert!(MAX_FRAME_BYTES <= u32::MAX as usize);

/// Fully assembled machine gateway and its chat registry.
pub struct GatewayServer {
    config: GatewayConfig,
    listener: TcpListener,
    auth: Arc<AuthStore>,
    host: GatewayHost,
    bots: Arc<BotStore>,
}

impl GatewayServer {
    /// Opens protected state and the machine-wide chat registry.
    pub async fn open(state_dir: PathBuf) -> Result<Self> {
        let (store, config) = ConfigStore::open(state_dir)?;
        let listener = TcpListener::bind(config.listen).await?;
        Self::assemble(store, config, listener).await
    }

    /// Binds and initializes a fresh local gateway before exposing its one-use pairing grant.
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
        let auth = Arc::new(AuthStore::open(store.auth_path())?);
        let credentials = Arc::new(CredentialStore::open(store.credentials_path())?);
        let bots = Arc::new(BotStore::open(store.state_dir())?);
        let host = GatewayHost::start(store, config.clone(), credentials, Arc::clone(&bots))?;
        Ok(Self {
            config,
            listener,
            auth,
            host,
            bots,
        })
    }

    /// Serves until a process shutdown signal or 72 hours of inactivity.
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
        self,
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
        let client_connections = Arc::new(ClientConnections::default());
        let (client_revocations, _) = broadcast::channel(MAX_CONNECTIONS);
        let mut has_active_routines = self.bots.has_active_routines(Utc::now().timestamp())?;
        let inactivity = tokio::time::sleep(inactivity_timeout);
        tokio::pin!(inactivity);
        let mut routine_timer = tokio::time::interval(ROUTINE_TICK);
        routine_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                biased;
                () = &mut shutdown => return Ok(()),
                _ = routine_timer.tick() => {
                    let now = Utc::now().timestamp();
                    let routines_active = self.bots.has_active_routines(now)?;
                    if has_active_routines && !routines_active && connections.is_empty() {
                        inactivity.as_mut().reset(tokio::time::Instant::now() + inactivity_timeout);
                    }
                    has_active_routines = routines_active;
                    let due = self.bots.take_due(now)?;
                    if !due.is_empty() {
                        let host = self.host.clone();
                        tokio::spawn(async move {
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
                accepted = self.listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                    let (stream, _) = accepted?;
                    let auth = Arc::clone(&self.auth);
                    let host = self.host.clone();
                    let bots = Arc::clone(&self.bots);
                    let client_connections = Arc::clone(&client_connections);
                    let client_revocations = client_revocations.clone();
                    let tls = tls.clone();
                    let websocket_host = websocket_host.clone();
                    connections.spawn(async move {
                        if let Some(tls) = tls {
                            if let Ok(Ok(stream)) =
                                tokio::time::timeout(AUTH_TIMEOUT, tls.accept(stream)).await
                            {
                                let _ = serve_connection(
                                    stream,
                                    auth,
                                    host,
                                    bots,
                                    client_connections,
                                    client_revocations,
                                    Instant::now() + AUTH_TIMEOUT,
                                )
                                .await;
                            }
                        } else {
                            let _ = serve_plaintext_connection(
                                stream,
                                auth,
                                host,
                                bots,
                                client_connections,
                                client_revocations,
                                PlaintextHandshake {
                                    expected_websocket_host: websocket_host,
                                    auth_deadline: Instant::now() + AUTH_TIMEOUT,
                                },
                            )
                            .await;
                        }
                    });
                }
                () = &mut inactivity, if connections.is_empty() && !has_active_routines => {
                    has_active_routines = self.bots.has_active_routines(Utc::now().timestamp())?;
                    if !has_active_routines {
                        return Ok(());
                    }
                }
            }
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

#[cfg(test)]
mod tests;
