//! Reusable async client for CLI and native frontends.

use std::collections::VecDeque;
use std::env;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;

use rustls::ClientConfig;
use rustls::RootCertStore;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::uri::Authority;
use tokio_tungstenite::tungstenite::http::{
    HeaderValue, Request,
    header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL},
};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

#[cfg(unix)]
use crate::wire::read_frame_with_limit;
use crate::wire::{
    CatalogHint, ClientCapabilities, ClientFrame, ClientMessage, FrameReader,
    MAX_PRE_AUTH_FRAME_BYTES, ServerFrame, ServerMessage, read_frame, validate_version,
    websocket_error, write_frame,
};
use crate::{Error, Result};

const DEFAULT_ENDPOINT: &str = "tcp://127.0.0.1:8741";
use crate::wire::WEBSOCKET_BRIDGE_BYTES;
/// Maximum number of frames a focused client flow may temporarily defer.
pub const MAX_PENDING_FRAMES: usize = 1024;

trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> Transport for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

type BoxedTransport = Box<dyn Transport>;

/// Validated plaintext-loopback, authenticated-root TLS, or WSS endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    security: Security,
    host: String,
    port: u16,
    websocket_authorization: Option<HeaderValue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Security {
    Plaintext,
    Tls,
    WebSocketTls,
}

/// Token returned while pairing a new client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedClient {
    /// The client identifier.
    pub client_id: String,
    /// The token.
    pub token: String,
}

/// What a connection sends with its authentication.
#[derive(Debug, Default)]
pub struct ConnectOptions {
    /// The catalog this client already holds, or does not need.
    pub catalog: CatalogHint,
    /// Requests sent right behind authentication; the gateway answers them after Ready.
    pub pipelined: Vec<ClientMessage>,
}

/// Connected client before its command and event halves are separated.
#[derive(Debug)]
pub struct GatewayClient {
    sender: GatewaySender,
    events: GatewayEvents,
}

/// Cloneable framed command writer.
#[derive(Clone)]
pub struct GatewaySender {
    writer: Arc<Mutex<Option<WriteHalf<BoxedTransport>>>>,
}

impl fmt::Debug for GatewaySender {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewaySender")
            .finish_non_exhaustive()
    }
}

/// Single-owner framed event reader.
pub struct GatewayEvents {
    reader: FrameReader<ReadHalf<BoxedTransport>>,
    pending: VecDeque<ServerFrame>,
    scoped_deferred: usize,
}

impl fmt::Debug for GatewayEvents {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayEvents")
            .field("pending", &self.pending)
            .field("scoped_deferred", &self.scoped_deferred)
            .finish_non_exhaustive()
    }
}

/// A focused read which restores every explicitly deferred frame when it ends.
#[derive(Debug)]
pub struct GatewayEventScope<'a> {
    events: &'a mut GatewayEvents,
    deferred: Vec<ServerFrame>,
}

impl Endpoint {
    /// Resolves `MOBIUS_GATEWAY_ENDPOINT`, defaulting to local plaintext.
    /// # Errors
    ///
    /// Returns an error if the input cannot be parsed or validated.
    pub fn from_env() -> Result<Self> {
        env::var("MOBIUS_GATEWAY_ENDPOINT")
            .unwrap_or_else(|_| DEFAULT_ENDPOINT.into())
            .parse()
    }

    /// Returns whether this endpoint uses loopback-only plaintext transport.
    #[must_use]
    pub const fn is_plaintext(&self) -> bool {
        matches!(self.security, Security::Plaintext)
    }

    /// Returns whether this endpoint uses secure WebSocket transport.
    #[must_use]
    pub const fn is_websocket(&self) -> bool {
        matches!(self.security, Security::WebSocketTls)
    }

    /// Adds a bearer credential to the secure WebSocket upgrade only.
    /// The caller must verify that this endpoint is trusted to receive the credential.
    /// Gateway protocol authentication remains separate.
    /// # Errors
    ///
    /// Returns an error for other transports or an invalid bearer credential.
    pub fn with_websocket_bearer(mut self, token: &str) -> Result<Self> {
        if !self.is_websocket()
            || token.is_empty()
            || token.bytes().any(|byte| !byte.is_ascii_graphic())
        {
            return Err(Error::Config(
                "invalid secure WebSocket bearer credential".into(),
            ));
        }
        let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|error| {
            Error::Config(format!(
                "invalid secure WebSocket bearer credential: {error}"
            ))
        })?;
        value.set_sensitive(true);
        self.websocket_authorization = Some(value);
        Ok(self)
    }

    /// Returns the validated endpoint host for server-side routing policy.
    #[must_use]
    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    async fn connect(&self, credential: &str) -> Result<BoxedTransport> {
        if self.is_websocket() {
            crate::channel::credential_key(credential)?;
            return self.connect_websocket(credential).await;
        }
        let address = format_address(&self.host, self.port);
        let stream = TcpStream::connect(&address).await?;
        self.secure_tcp(stream).await
    }

    async fn secure_tcp(&self, stream: TcpStream) -> Result<BoxedTransport> {
        stream.set_nodelay(true)?;
        if self.security == Security::Plaintext {
            let peer = stream.peer_addr()?;
            if !peer.ip().is_loopback() {
                return Err(Error::Config(
                    "plaintext gateway connections are restricted to loopback".into(),
                ));
            }
            return Ok(Box::new(stream));
        }

        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()); // rules-allow: the root store owns its trust anchors
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = ServerName::try_from(self.host.clone()) // rules-allow: the TLS session owns its server name
            .map_err(|error| {
                Error::Config(format!("TLS endpoint has an invalid server name: {error}"))
            })?;
        let stream = TlsConnector::from(Arc::new(config))
            .connect(name, stream)
            .await
            .map_err(|error| {
                Error::Protocol(format!("TLS handshake failed: {:?}", error.kind()))
            })?;
        Ok(Box::new(stream))
    }

    #[cfg(unix)]
    pub(crate) async fn local_gateway_version(
        &self,
        mut listen: std::net::SocketAddr,
        token: &str,
    ) -> Result<String> {
        if self.is_websocket() {
            return Err(Error::Config(
                "local gateway control requires TCP or TLS".into(),
            ));
        }
        if listen.ip().is_unspecified() {
            listen.set_ip(match listen.ip() {
                IpAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                IpAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
            });
        }
        let mut version = crate::wire::PROTOCOL_VERSION;
        for attempt in 0..2 {
            let transport = self.secure_tcp(TcpStream::connect(listen).await?).await?;
            let (reader, mut writer) = tokio::io::split(transport);
            let mut reader = FrameReader::new(reader);
            write_frame(
                &mut writer,
                &ClientFrame {
                    version,
                    message: ClientMessage::Authenticate {
                        token: token.into(),
                        client: ClientCapabilities::default(),
                        catalog: CatalogHint::default(),
                    },
                },
            )
            .await?;
            let response = read_frame_with_limit::<ServerFrame>(&mut reader, 4 * 1024)
                .await?
                .ok_or_else(|| {
                    Error::Protocol("gateway closed during version authentication".into())
                })?;
            match response.message {
                ServerMessage::Error { code, .. }
                    if attempt == 0 && code == "protocol_version" && response.version > 0 =>
                {
                    version = response.version;
                    continue;
                }
                ServerMessage::Authenticated if response.version == version => {}
                ServerMessage::Error { code, message, .. } => {
                    return Err(connection_error(&code, &message));
                }
                _ => {
                    return Err(Error::Protocol(
                        "gateway did not authenticate the version check".into(),
                    ));
                }
            }
            let mut frame = read_frame::<serde_json::Value>(&mut reader)
                .await?
                .ok_or_else(|| {
                    Error::Protocol("gateway disconnected before reporting its version".into())
                })?;
            if frame.get("type").and_then(serde_json::Value::as_str) != Some("ready")
                || frame.get("version").and_then(serde_json::Value::as_u64)
                    != Some(u64::from(version))
            {
                return Err(Error::Protocol(
                    "gateway did not report a valid ready frame".into(),
                ));
            }
            return match frame
                .get_mut("payload")
                .and_then(|payload| payload.get_mut("gateway_version"))
                .map(serde_json::Value::take)
            {
                Some(serde_json::Value::String(version)) => Ok(version),
                _ => Err(Error::Protocol("gateway did not report its version".into())),
            };
        }
        Err(Error::Protocol("gateway did not report its version".into()))
    }

    async fn connect_websocket(&self, credential: &str) -> Result<BoxedTransport> {
        let config = WebSocketConfig::default()
            .max_message_size(Some(crate::channel::MAX_RECORD))
            .max_frame_size(Some(crate::channel::MAX_RECORD));
        let (mut websocket, response) =
            connect_async_with_config(self.websocket_request()?, Some(config), true)
                .await
                .map_err(|error| match error {
                    tokio_tungstenite::tungstenite::Error::Http(response)
                        if self.websocket_authorization.is_some()
                            && matches!(response.status().as_u16(), 401 | 403) =>
                    {
                        Error::Config(
                            "gateway access was denied; sign in to the gateway service again"
                                .into(),
                        )
                    }
                    error => websocket_error(error),
                })?;
        if response
            .headers()
            .get(SEC_WEBSOCKET_PROTOCOL)
            .and_then(|value| value.to_str().ok())
            != Some(crate::channel::SUBPROTOCOL)
        {
            return Err(Error::Config(
                "gateway does not support the encrypted WebSocket protocol; update the gateway"
                    .into(),
            ));
        }
        let state = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::channel::client_handshake(&mut websocket, credential),
        )
        .await
        .map_err(|error| {
            tracing::debug!(%error, "gateway channel handshake timed out");
            Error::Unauthorized
        })??;
        let (transport, bridge) = tokio::io::duplex(WEBSOCKET_BRIDGE_BYTES);
        tokio::spawn(async move {
            if let Err(error) = crate::channel::bridge(websocket, state, bridge).await {
                tracing::debug!(%error, "gateway channel closed");
            }
        });
        Ok(Box::new(transport))
    }

    fn websocket_request(&self) -> Result<Request<()>> {
        let mut request = self
            .to_string()
            .into_client_request()
            .map_err(websocket_error)?;
        request.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(crate::channel::SUBPROTOCOL),
        );
        if let Some(authorization) = &self.websocket_authorization {
            request
                .headers_mut()
                .insert(AUTHORIZATION, authorization.clone()); // rules-allow: every connection attempt sends the stored credential
        }
        Ok(request)
    }
}

impl FromStr for Endpoint {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        let (security, authority) = if let Some(authority) = value.strip_prefix("tcp://") {
            (Security::Plaintext, authority)
        } else if let Some(authority) = value.strip_prefix("tls://") {
            (Security::Tls, authority)
        } else if let Some(authority) = value.strip_prefix("wss://") {
            (Security::WebSocketTls, authority)
        } else {
            return Err(Error::Config(
                "gateway endpoint must use tcp://, tls://, or wss://".into(),
            ));
        };
        if authority.contains(['/', '?', '#', '@']) {
            return Err(Error::Config(
                "gateway endpoint must contain only a host and port".into(),
            ));
        }
        let authority = authority.parse::<Authority>().map_err(|error| {
            Error::Config(format!(
                "gateway endpoint has an invalid host or port: {error}"
            ))
        })?;
        let host = authority
            .host()
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or_else(|| authority.host());
        if host.is_empty() {
            return Err(Error::Config("gateway endpoint requires a host".into()));
        }
        let port = match authority.port_u16() {
            Some(port) => port,
            None if authority.as_str().len() != authority.host().len() => {
                return Err(Error::Config("gateway endpoint has an invalid port".into()));
            }
            None if security == Security::WebSocketTls => 443,
            None => return Err(Error::Config("gateway endpoint requires a port".into())),
        };
        if port == 0 {
            return Err(Error::Config(
                "gateway endpoint port must be greater than zero".into(),
            ));
        }
        if security == Security::Plaintext && !plaintext_host_is_loopback(host) {
            return Err(Error::Config(
                "tcp:// endpoints are restricted to loopback; use tls:// or wss:// remotely".into(),
            ));
        }
        Ok(Self {
            security,
            host: host.into(),
            port,
            websocket_authorization: None,
        })
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = match self.security {
            Security::Plaintext => "tcp",
            Security::Tls => "tls",
            Security::WebSocketTls => "wss",
        };
        if self.security == Security::WebSocketTls && self.port == 443 {
            if self.host.contains(':') {
                return write!(formatter, "{scheme}://[{}]", self.host);
            }
            return write!(formatter, "{scheme}://{}", self.host);
        }
        write!(
            formatter,
            "{scheme}://{}",
            format_address(&self.host, self.port)
        )
    }
}

impl GatewayClient {
    /// Authenticates an existing client and leaves the gateway Ready frame for `events`.
    /// # Errors
    ///
    /// Returns an error if the transport fails or returns invalid data.
    pub async fn connect(
        endpoint: &Endpoint,
        token: &str,
        client: ClientCapabilities,
    ) -> Result<Self> {
        Self::connect_with(endpoint, token, client, &mut ConnectOptions::default()).await
    }

    /// [`Self::connect`] with a catalog hint and pipelined requests, which `options` holds
    /// again afterwards, whether or not the connection succeeded.
    /// # Errors
    ///
    /// Returns an error if the transport fails or returns invalid data.
    pub async fn connect_with(
        endpoint: &Endpoint,
        token: &str,
        capabilities: ClientCapabilities,
        options: &mut ConnectOptions,
    ) -> Result<Self> {
        // A WebSocket's first encrypted record, which may carry both, must stay small.
        let pipelined_bytes = options.pipelined.iter().try_fold(0, |total, message| {
            serde_json::to_vec(message).map(|encoded| total + encoded.len())
        })?;
        if pipelined_bytes > MAX_PRE_AUTH_FRAME_BYTES / 2 {
            return Err(Error::Config(
                "requests sent with authentication must stay under 2 KiB".into(),
            ));
        }
        let transport = endpoint.connect(token).await?;
        let (reader, writer) = tokio::io::split(transport);
        let client = Self::from_parts(reader, writer);
        let authentication = ClientFrame::new(ClientMessage::Authenticate {
            // The owned wire message is the one copy; callers keep their token.
            token: token.to_owned(),
            client: capabilities,
            catalog: std::mem::take(&mut options.catalog),
        });
        let pipelined: Vec<_> = std::mem::take(&mut options.pipelined)
            .into_iter()
            .map(ClientFrame::new)
            .collect();
        let written = client
            .sender
            .write_frames(&authentication, &pipelined)
            .await;
        if let ClientMessage::Authenticate { catalog, .. } = authentication.message {
            options.catalog = catalog;
        }
        options.pipelined = pipelined.into_iter().map(|frame| frame.message).collect();
        written?;
        client.expect_authenticated().await
    }

    /// Consumes a pending pairing code and returns a connected independent client.
    /// # Errors
    ///
    /// Returns an error if authentication fails or its stored state is invalid.
    pub async fn pair(
        endpoint: &Endpoint,
        code: impl Into<String>,
        client_label: impl Into<String>,
        capabilities: ClientCapabilities,
    ) -> Result<(Self, PairedClient)> {
        let code = code.into();
        let transport = endpoint.connect(&code).await?;
        let (reader, writer) = tokio::io::split(transport);
        let mut client = Self::from_parts(reader, writer);
        client
            .sender
            .write(ClientMessage::Pair {
                code,
                client_label: client_label.into(),
                client: capabilities,
            })
            .await?;
        let frame = client
            .events
            .handshake("gateway closed during pairing")
            .await?;
        let paired = match frame.message {
            ServerMessage::Paired { client_id, token } => PairedClient { client_id, token },
            ServerMessage::Error { code, message, .. } => {
                return Err(connection_error(&code, &message));
            }
            _ => {
                return Err(Error::Protocol(
                    "gateway did not return a paired response".into(),
                ));
            }
        };
        client = client.expect_authenticated().await?;
        Ok((client, paired))
    }

    /// Separates the clonable command writer from the single event reader.
    #[must_use]
    pub fn into_parts(self) -> (GatewaySender, GatewayEvents) {
        (self.sender, self.events)
    }

    fn from_parts(reader: ReadHalf<BoxedTransport>, writer: WriteHalf<BoxedTransport>) -> Self {
        Self {
            sender: GatewaySender {
                writer: Arc::new(Mutex::new(Some(writer))),
            },
            events: GatewayEvents {
                reader: FrameReader::new(reader),
                pending: VecDeque::new(),
                scoped_deferred: 0,
            },
        }
    }

    async fn expect_authenticated(mut self) -> Result<Self> {
        let frame = self
            .events
            .handshake("gateway closed during authentication")
            .await?;
        match frame.message {
            ServerMessage::Authenticated => Ok(self),
            ServerMessage::Error { code, message, .. } => Err(connection_error(&code, &message)),
            _ => Err(Error::Protocol(
                "gateway did not acknowledge authentication".into(),
            )),
        }
    }
}

fn connection_error(code: &str, message: &str) -> Error {
    if code == "unauthorized" {
        Error::Unauthorized
    } else {
        Error::Protocol(message.into())
    }
}

impl GatewaySender {
    /// Sends one authenticated operation.
    /// # Errors
    ///
    /// Returns an error if the transport fails or returns invalid data.
    pub async fn send(&self, message: ClientMessage) -> Result<()> {
        if matches!(
            message,
            ClientMessage::Pair { .. }
                | ClientMessage::RepairPairing { .. }
                | ClientMessage::Authenticate { .. }
        ) {
            return Err(Error::Protocol(
                "authentication messages are valid only during connection setup".into(),
            ));
        }
        self.write(message).await
    }

    async fn write(&self, message: ClientMessage) -> Result<()> {
        self.write_frames(&ClientFrame::new(message), &[]).await
    }

    /// Writes `first` and then `then` without another writer interleaving.
    async fn write_frames(&self, first: &ClientFrame, then: &[ClientFrame]) -> Result<()> {
        let mut slot = self.writer.lock().await;
        let mut writer = slot.take().ok_or_else(|| {
            Error::Protocol("gateway writer is closed after a failed or cancelled write".into())
        })?;
        write_frame(&mut writer, first).await?;
        for frame in then {
            write_frame(&mut writer, frame).await?;
        }
        *slot = Some(writer);
        drop(slot);
        Ok(())
    }
}

impl GatewayEvents {
    /// Receives the next server frame; the protocol version is checked once at the handshake.
    /// # Errors
    ///
    /// Returns an error if the transport fails or returns invalid data.
    pub async fn next(&mut self) -> Result<Option<ServerFrame>> {
        if let Some(frame) = self.pending.pop_front() {
            return Ok(Some(frame));
        }
        read_frame::<ServerFrame>(&mut self.reader).await
    }

    /// Receives a connection-setup frame and checks its protocol version.
    async fn handshake(&mut self, closed: &str) -> Result<ServerFrame> {
        let frame = self
            .next()
            .await?
            .ok_or_else(|| Error::Protocol(closed.into()))?;
        validate_version(frame.version)?;
        Ok(frame)
    }

    /// Starts a focused response wait without consuming unrelated gateway events.
    pub fn scoped(&mut self) -> GatewayEventScope<'_> {
        GatewayEventScope {
            events: self,
            deferred: Vec::new(),
        }
    }
}

impl GatewayEventScope<'_> {
    /// Receives the next frame from the shared ordered stream.
    /// # Errors
    ///
    /// Returns an error if the transport fails or returns invalid data.
    pub async fn next(&mut self) -> Result<Option<ServerFrame>> {
        self.events.next().await
    }

    /// Reborrows the underlying stream for a nested focused operation.
    pub fn reborrow(&mut self) -> &mut GatewayEvents {
        self.events
    }

    /// Defers one unrelated frame for the scope's caller to consume later.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn defer(&mut self, frame: ServerFrame) -> Result<()> {
        if self
            .events
            .pending
            .len()
            .saturating_add(self.events.scoped_deferred)
            >= MAX_PENDING_FRAMES
        {
            return Err(Error::Protocol(format!(
                "gateway event backlog exceeds {MAX_PENDING_FRAMES} frames"
            )));
        }
        self.deferred.push(frame);
        self.events.scoped_deferred += 1;
        Ok(())
    }
}

impl Drop for GatewayEventScope<'_> {
    fn drop(&mut self) {
        self.events.scoped_deferred -= self.deferred.len();
        for frame in self.deferred.drain(..).rev() {
            self.events.pending.push_front(frame);
        }
    }
}

/// Resolves the bearer token expected by the reusable CLI client.
/// # Errors
///
/// Returns an error if validation or an operation required by this function fails.
pub fn token_from_env() -> Result<String> {
    env::var("MOBIUS_GATEWAY_TOKEN")
        .ok()
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| Error::Config("set MOBIUS_GATEWAY_TOKEN before connecting".into()))
}

fn plaintext_host_is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn format_address(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn websocket_redirects_are_rejected_without_exposing_response_secrets() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let endpoint = "wss://gateway.example"
            .parse::<Endpoint>()
            .expect("endpoint")
            .with_websocket_bearer("cloud-secret")
            .expect("bearer");
        let (client, mut server) = tokio::io::duplex(2048);
        let response = tokio::spawn(async move {
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                headers.push(server.read_u8().await.expect("request header"));
            }
            assert!(
                String::from_utf8(headers)
                    .expect("headers")
                    .contains("Bearer cloud-secret")
            );
            server.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://evil.example\r\nContent-Length: 12\r\n\r\ncloud-secret")
                .await.expect("redirect response");
        });
        let error =
            tokio_tungstenite::client_async(endpoint.websocket_request().expect("request"), client)
                .await
                .expect_err("a redirect must not follow the bearer");
        let error = websocket_error(error).to_string();
        assert!(error.contains("HTTP 302"));
        assert!(!error.contains("cloud-secret"));
        response.await.expect("server");
    }

    #[test]
    fn endpoint_rejects_remote_plaintext() {
        let error = "tcp://example.com:8741"
            .parse::<Endpoint>()
            .expect_err("remote plaintext must fail");

        assert!(error.to_string().contains("use tls://"));
        assert!("tcp://127.0.0.1:0".parse::<Endpoint>().is_err());
    }

    #[tokio::test]
    async fn scoped_wait_restores_deferred_frames_on_early_return() {
        let (transport, _peer) = tokio::io::duplex(64);
        let (reader, _writer) = tokio::io::split(Box::new(transport) as BoxedTransport);
        let mut events = GatewayEvents {
            reader: FrameReader::new(reader),
            pending: VecDeque::from([
                ServerFrame::new(ServerMessage::Accepted {
                    request_id: "unrelated".into(),
                }),
                ServerFrame::new(ServerMessage::Accepted {
                    request_id: "expected".into(),
                }),
            ]),
            scoped_deferred: 0,
        };

        {
            let mut scope = events.scoped();
            let unrelated = scope.next().await.expect("next").expect("unrelated");
            scope.defer(unrelated).expect("defer");
            let expected = scope.next().await.expect("next").expect("expected");
            assert!(matches!(
                expected.message,
                ServerMessage::Accepted { request_id } if request_id.as_ref() == "expected"
            ));
        }

        let restored = events.next().await.expect("next").expect("restored");
        assert!(matches!(
            restored.message,
            ServerMessage::Accepted { request_id } if request_id.as_ref() == "unrelated"
        ));
    }
}
