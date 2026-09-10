use super::*;

use std::collections::VecDeque;
use std::fs::File;
use std::pin::Pin;
use std::task::{Context, Poll};

use rustls::pki_types::pem::{self, PemObject as _};
use serde::Deserialize;
use tokio::io::{AsyncWriteExt as _, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::{Message, Role};

pub(super) const MAX_PRE_AUTH_FRAME_BYTES: usize = 4 * 1024;

pub(super) struct ConnectionAdmission {
    pre_auth: Arc<Semaphore>,
    authenticated: Arc<Semaphore>,
}

pub(super) struct PreAuthConnectionAdmission {
    _permit: OwnedSemaphorePermit,
    authenticated: Arc<Semaphore>,
}

pub(super) struct ConnectionContext {
    pub(super) local: bool,
    pub(super) auth: Arc<AuthStore>,
    pub(super) host: GatewayHost,
    pub(super) bots: Arc<BotStore>,
    pub(super) client_connections: Arc<ClientConnections>,
    pub(super) client_revocations: broadcast::Sender<String>,
    pub(super) admission: PreAuthConnectionAdmission,
}

struct PendingProfile {
    request_id: String,
    future: Pin<Box<dyn Future<Output = std::result::Result<ProfileSnapshot, Rejection>> + Send>>,
}

impl ConnectionAdmission {
    pub(super) fn new(pre_auth: usize, authenticated: usize) -> Self {
        Self {
            pre_auth: Arc::new(Semaphore::new(pre_auth)),
            authenticated: Arc::new(Semaphore::new(authenticated)),
        }
    }

    pub(super) async fn admit(&self) -> PreAuthConnectionAdmission {
        PreAuthConnectionAdmission {
            _permit: Arc::clone(&self.pre_auth)
                .acquire_owned()
                .await
                .expect("connection admission semaphore stays open"),
            authenticated: Arc::clone(&self.authenticated),
        }
    }
}

impl PreAuthConnectionAdmission {
    pub(super) fn promote(self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.authenticated).try_acquire_owned().ok()
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct PreAuthClientFrame {
    version: u16,
    #[serde(flatten)]
    message: PreAuthClientMessage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PreAuthClientMessage {
    Pair {
        code: String,
        client_label: String,
        client_kind: ClientKind,
    },
    Authenticate {
        token: String,
        client_kind: ClientKind,
    },
    #[serde(other)]
    Unsupported,
}

struct PreAuthWebSocket {
    stream: TcpStream,
    pending: VecDeque<u8>,
    handshake_match: usize,
    handshake_complete: bool,
    authentication_complete: bool,
}

impl PreAuthWebSocket {
    const fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            pending: VecDeque::new(),
            handshake_match: 0,
            handshake_complete: false,
            authentication_complete: false,
        }
    }

    fn complete(&mut self) {
        self.authentication_complete = true;
    }

    fn handshake_end(&mut self, bytes: &[u8]) -> Option<usize> {
        const END: &[u8] = b"\r\n\r\n";
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == END[self.handshake_match] {
                self.handshake_match += 1;
                if self.handshake_match == END.len() {
                    return Some(index + 1);
                }
            } else {
                self.handshake_match = usize::from(*byte == END[0]);
            }
        }
        None
    }
}

impl AsyncRead for PreAuthWebSocket {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.authentication_complete {
            if !this.pending.is_empty() {
                let bytes = this.pending.make_contiguous();
                let read = bytes.len().min(buffer.remaining());
                buffer.put_slice(&bytes[..read]);
                this.pending.drain(..read);
                return Poll::Ready(Ok(()));
            }
            return Pin::new(&mut this.stream).poll_read(context, buffer);
        }
        if this.handshake_complete {
            if let Some(byte) = this.pending.pop_front() {
                buffer.put_slice(&[byte]);
                return Poll::Ready(Ok(()));
            }
            // The WebSocket is rebuilt with the 50 MiB limit after authentication; do not strand
            // bytes from the next message in the old decoder while crossing that boundary.
            let mut byte = [0_u8; 1];
            let mut staged = ReadBuf::new(&mut byte);
            return match Pin::new(&mut this.stream).poll_read(context, &mut staged) {
                Poll::Ready(Ok(())) => {
                    buffer.put_slice(staged.filled());
                    Poll::Ready(Ok(()))
                }
                result => result,
            };
        }

        let mut bytes = [0_u8; 1024];
        let bytes_to_read = bytes.len().min(buffer.remaining());
        let mut staged = ReadBuf::new(&mut bytes[..bytes_to_read]);
        match Pin::new(&mut this.stream).poll_read(context, &mut staged) {
            Poll::Ready(Ok(())) => {
                let bytes = staged.filled();
                if let Some(end) = this.handshake_end(bytes) {
                    this.handshake_complete = true;
                    this.pending.extend(&bytes[end..]);
                    buffer.put_slice(&bytes[..end]);
                } else {
                    buffer.put_slice(bytes);
                }
                Poll::Ready(Ok(()))
            }
            result => result,
        }
    }
}

impl AsyncWrite for PreAuthWebSocket {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(context, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(context)
    }
}

pub(super) struct WebSocketUpgradePolicy {
    pub(super) expected_host: Option<String>,
}

pub(super) struct PlaintextHandshake {
    pub(super) expected_websocket_host: Option<String>,
    pub(super) auth_deadline: Instant,
}

impl Callback for WebSocketUpgradePolicy {
    fn on_request(
        self,
        request: &Request,
        response: Response,
    ) -> std::result::Result<Response, ErrorResponse> {
        if request.uri().path_and_query().map(|value| value.as_str()) != Some("/") {
            return Err(websocket_rejection(StatusCode::NOT_FOUND));
        }
        if request.headers().contains_key(ORIGIN) {
            return Err(websocket_rejection(StatusCode::FORBIDDEN));
        }
        if let Some(expected) = self.expected_host
            && !request_host_matches(request, &expected)
        {
            return Err(websocket_rejection(StatusCode::FORBIDDEN));
        }
        Ok(response)
    }
}

pub(super) fn request_host_matches(request: &Request, expected: &str) -> bool {
    let mut values = request.headers().get_all(HOST).iter();
    let Some(actual) = values.next().and_then(|value| value.to_str().ok()) else {
        return false;
    };
    values.next().is_none()
        && (actual.eq_ignore_ascii_case(expected)
            || actual
                .strip_suffix(":443")
                .is_some_and(|host| host.eq_ignore_ascii_case(expected)))
}

pub(super) fn websocket_rejection(status: StatusCode) -> ErrorResponse {
    let mut response = ErrorResponse::new(None);
    *response.status_mut() = status;
    response
}

#[derive(Default)]
pub(super) struct ClientConnections {
    entries: Mutex<BTreeMap<(String, ClientKind), usize>>,
}

pub(super) struct ClientConnectionGuard {
    connections: Arc<ClientConnections>,
    key: (String, ClientKind),
}

impl ClientConnections {
    pub(super) fn register(
        self: &Arc<Self>,
        client_id: String,
        kind: ClientKind,
    ) -> Result<ClientConnectionGuard> {
        let key = (client_id, kind);
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| Error::Config("client-connection lock is poisoned".into()))?;
        let connections = entries.entry(key.clone()).or_default();
        *connections = connections
            .checked_add(1)
            .ok_or_else(|| Error::Config("client connection count overflow".into()))?;
        drop(entries);
        Ok(ClientConnectionGuard {
            connections: Arc::clone(self),
            key,
        })
    }

    pub(super) fn snapshot(&self, paired: &[ClientIdentity]) -> Result<Vec<ClientStatus>> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| Error::Config("client-connection lock is poisoned".into()))?;
        Ok(paired
            .iter()
            .map(|identity| {
                let mut kinds = Vec::new();
                let mut connections = 0;
                for ((client_id, kind), count) in &*entries {
                    if client_id == &identity.id
                        && *kind != ClientKind::GatewayDashboard
                        && *count > 0
                    {
                        kinds.push(*kind);
                        connections += *count;
                    }
                }
                ClientStatus {
                    client_id: identity.id.clone(),
                    label: identity.label.clone(),
                    kinds,
                    connections,
                }
            })
            .collect())
    }
}

impl Drop for ClientConnectionGuard {
    fn drop(&mut self) {
        let Ok(mut entries) = self.connections.entries.lock() else {
            return;
        };
        let Some(connections) = entries.get_mut(&self.key) else {
            return;
        };
        if *connections > 1 {
            *connections -= 1;
        } else {
            entries.remove(&self.key);
        }
    }
}

pub(super) fn connection_diagnostic(error: &Error) -> String {
    match error {
        Error::Io(error) => format!("I/O {:?}", error.kind()),
        Error::Json(error) => format!(
            "JSON {:?} at {}:{}",
            error.classify(),
            error.line(),
            error.column()
        ),
        Error::Config(_) => "configuration".into(),
        Error::Protocol(_) => "protocol".into(),
        Error::Unauthorized => "authentication".into(),
        Error::Mobius(_) => "agent".into(),
        Error::Sqlite(_) => "storage".into(),
    }
}

pub(super) async fn serve_plaintext_connection(
    stream: TcpStream,
    connection: ConnectionContext,
    handshake: PlaintextHandshake,
) -> Result<()> {
    let PlaintextHandshake {
        expected_websocket_host,
        auth_deadline,
    } = handshake;
    let mut first = [0_u8; 1];
    let read = tokio::time::timeout_at(auth_deadline, stream.peek(&mut first))
        .await
        .map_err(|_| Error::Unauthorized)??;
    if read == 1 && first[0] == b'G' {
        serve_websocket(
            stream,
            connection,
            PlaintextHandshake {
                expected_websocket_host,
                auth_deadline,
            },
        )
        .await
    } else {
        serve_connection(stream, connection, auth_deadline, None).await
    }
}

pub(super) async fn serve_websocket(
    stream: TcpStream,
    mut connection: ConnectionContext,
    handshake: PlaintextHandshake,
) -> Result<()> {
    connection.local = false;
    let PlaintextHandshake {
        expected_websocket_host,
        auth_deadline,
    } = handshake;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_PRE_AUTH_FRAME_BYTES))
        .max_frame_size(Some(MAX_PRE_AUTH_FRAME_BYTES));
    let mut websocket = tokio::time::timeout_at(
        auth_deadline,
        accept_hdr_async_with_config(
            PreAuthWebSocket::new(stream),
            WebSocketUpgradePolicy {
                expected_host: expected_websocket_host,
            },
            Some(config),
        ),
    )
    .await
    .map_err(|_| Error::Unauthorized)?
    .map_err(websocket_error)?;
    let payload = loop {
        let message = tokio::time::timeout_at(auth_deadline, websocket.next())
            .await
            .map_err(|_| Error::Unauthorized)?
            .ok_or(Error::Unauthorized)?
            .map_err(websocket_error)?;
        match message {
            Message::Binary(payload) if (1..=MAX_PRE_AUTH_FRAME_BYTES).contains(&payload.len()) => {
                break payload;
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => return Err(Error::Unauthorized),
            Message::Binary(payload) => {
                return Err(Error::Protocol(format!(
                    "pre-authentication WebSocket message length must be 1–{MAX_PRE_AUTH_FRAME_BYTES} bytes, got {}",
                    payload.len()
                )));
            }
            Message::Text(_) | Message::Frame(_) => {
                return Err(Error::Protocol(
                    "WebSocket messages must be binary JSON frames".into(),
                ));
            }
        }
    };

    let (gateway_stream, mut bridge_stream) = tokio::io::duplex(WEBSOCKET_BRIDGE_BYTES);
    let length = u32::try_from(payload.len())
        .map_err(|_| Error::Protocol("WebSocket message is too large".into()))?;
    bridge_stream.write_all(&length.to_be_bytes()).await?;
    bridge_stream.write_all(&payload).await?;
    let (authenticated_tx, authenticated_rx) = oneshot::channel();
    let gateway = serve_connection(
        gateway_stream,
        connection,
        auth_deadline,
        Some(authenticated_tx),
    );
    tokio::pin!(gateway);
    tokio::pin!(authenticated_rx);
    let authentication_succeeded = tokio::select! {
        result = &mut gateway => {
            result?;
            return framed_to_websocket(bridge_stream, websocket).await;
        }
        result = &mut authenticated_rx => result.is_ok(),
    };
    if !authentication_succeeded {
        gateway.await?;
        return framed_to_websocket(bridge_stream, websocket).await;
    }

    let mut stream = websocket.into_inner();
    stream.complete();
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES));
    let websocket = WebSocketStream::from_raw_socket(stream, Role::Server, Some(config)).await;
    let (outgoing, incoming) = websocket.split();
    let (bridge_reader, bridge_writer) = tokio::io::split(bridge_stream);
    let gateway_and_outgoing = async {
        let (gateway, outgoing) =
            tokio::join!(&mut gateway, framed_to_websocket(bridge_reader, outgoing),);
        gateway?;
        outgoing
    };
    tokio::pin!(gateway_and_outgoing);
    tokio::select! {
        result = &mut gateway_and_outgoing => result,
        result = websocket_to_framed(incoming, bridge_writer) => {
            result?;
            gateway_and_outgoing.await
        }
    }
}

pub(super) async fn serve_connection<S>(
    stream: S,
    connection: ConnectionContext,
    auth_deadline: Instant,
    authentication_complete: Option<oneshot::Sender<()>>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let ConnectionContext {
        local,
        auth,
        host,
        bots,
        client_connections,
        client_revocations,
        admission,
    } = connection;
    let mut revocations = client_revocations.subscribe();
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = FrameReader::new(reader);
    let first = tokio::time::timeout_at(
        auth_deadline,
        read_frame_with_limit::<PreAuthClientFrame>(&mut reader, MAX_PRE_AUTH_FRAME_BYTES),
    )
    .await
    .map_err(|_| Error::Unauthorized)??
    .ok_or(Error::Unauthorized)?;
    if let Err(error) = validate_version(first.version) {
        write_server_error(&mut writer, "protocol_version", error.to_string(), true).await?;
        return Ok(());
    }
    let Some(_authenticated_admission) = admission.promote() else {
        write_server_error(
            &mut writer,
            "server_busy",
            "the gateway has reached its authenticated connection limit",
            true,
        )
        .await?;
        return Ok(());
    };
    let Some((client_id, client_kind)) =
        authenticate_client(first.message, &auth, &mut writer).await?
    else {
        return Ok(());
    };

    let _client_connection = client_connections.register(client_id.clone(), client_kind)?;
    let _ = authentication_complete.map(|complete| complete.send(()));

    write_frame(&mut writer, &ServerFrame::new(ServerMessage::Authenticated)).await?;
    let mut gateway_broadcasts = host.subscribe();
    let ready = host
        .ready()
        .await
        .map_err(|rejection| Error::Protocol(rejection.message))?;
    write_frame(
        &mut writer,
        &ServerFrame::new(ServerMessage::Ready { payload: ready }),
    )
    .await?;
    let mut selected: Option<SelectedChat> = None;
    let mut voice = None;
    let mut desktop = None;
    let session_files = host.session_file_store().await;
    let mut uploads: BTreeMap<(String, String), PendingSessionFileWrite> = BTreeMap::new();
    let mut pending_profile = None;
    let mut queued_profile_request = None;

    loop {
        let incoming = tokio::select! {
            biased;
            revoked = revocations.recv() => {
                if !matches!(revoked, Ok(revoked) if revoked != client_id) {
                    return Ok(());
                }
                None
            }
            incoming = read_frame::<ClientFrame>(&mut reader) => Some(incoming),
            outgoing = crate::computer_runtime::desktop::next_update(&mut desktop) => {
                crate::computer_runtime::desktop::write_update(&mut desktop, outgoing, &mut writer).await?;
                None
            }
            outgoing = super::voice::next_update(&mut voice) => {
                super::voice::write_update(&mut voice, outgoing, &mut writer).await?;
                None
            }
            profile = next_profile(&mut pending_profile) => {
                complete_profile_request(
                    profile,
                    &host,
                    &mut pending_profile,
                    &mut queued_profile_request,
                    &mut writer,
                )
                .await?;
                None
            }
            outgoing = gateway_broadcasts.recv() => {
                if !handle_gateway_broadcast(outgoing, &host, &mut writer).await? {
                    return Ok(());
                }
                None
            }
            outgoing = selected_broadcast(&mut selected) => {
                if !handle_selected_broadcast(outgoing, &mut writer).await? {
                    return Ok(());
                }
                None
            }
        };
        let Some(incoming) = incoming else {
            continue;
        };
        let Some(frame) = incoming? else {
            return Ok(());
        };
        if let Err(error) = validate_version(frame.version) {
            write_server_error(&mut writer, "protocol_version", error.to_string(), true).await?;
            return Ok(());
        }
        let Some(message) = handle_profile_message(
            frame.message,
            &host,
            &mut pending_profile,
            &mut queued_profile_request,
            &mut writer,
        )
        .await?
        else {
            continue;
        };
        let client = AuthenticatedClient {
            local,
            kind: client_kind,
            id: &client_id,
            connections: &client_connections,
            revocations: &client_revocations,
        };
        handle_message(
            message,
            &auth,
            &host,
            &bots,
            &client,
            ConnectionSessionState {
                selected: &mut selected,
                session_files: &session_files,
                bots: &bots,
                uploads: &mut uploads,
                voice: &mut voice,
                desktop: &mut desktop,
            },
            &mut writer,
        )
        .await?;
    }
}

async fn authenticate_client(
    message: PreAuthClientMessage,
    auth: &AuthStore,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<Option<(String, ClientKind)>> {
    match message {
        PreAuthClientMessage::Pair {
            code,
            client_label,
            client_kind,
        } => match auth.pair(&code, &client_label) {
            Ok(issued) => {
                let client_id = issued.client_id.clone();
                write_frame(
                    writer,
                    &ServerFrame::new(ServerMessage::Paired {
                        client_id: issued.client_id,
                        token: issued.token,
                    }),
                )
                .await?;
                Ok(Some((client_id, client_kind)))
            }
            Err(_) => {
                write_server_error(writer, "unauthorized", "pairing failed", true).await?;
                Ok(None)
            }
        },
        PreAuthClientMessage::Authenticate { token, client_kind } => {
            match auth.authenticate(&token) {
                Ok(identity) => Ok(Some((identity.id, client_kind))),
                Err(_) => {
                    write_server_error(writer, "unauthorized", "authentication failed", true)
                        .await?;
                    Ok(None)
                }
            }
        }
        PreAuthClientMessage::Unsupported => {
            write_server_error(
                writer,
                "authentication_required",
                "the first frame must authenticate or pair",
                true,
            )
            .await?;
            Ok(None)
        }
    }
}

async fn handle_profile_message(
    message: ClientMessage,
    host: &GatewayHost,
    pending: &mut Option<PendingProfile>,
    queued_request: &mut Option<(String, bool)>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<Option<ClientMessage>> {
    let ClientMessage::GetProfile {
        request_id,
        include_provider_usage,
    } = message
    else {
        return Ok(Some(message));
    };
    if pending.is_some() {
        queue_profile_request(writer, queued_request, request_id, include_provider_usage).await?;
    } else {
        *pending = Some(profile_request(host, request_id, include_provider_usage));
    }
    Ok(None)
}

async fn next_profile(
    pending: &mut Option<PendingProfile>,
) -> (String, std::result::Result<ProfileSnapshot, Rejection>) {
    let Some(pending) = pending.as_mut() else {
        return std::future::pending().await;
    };
    let request_id = pending.request_id.clone();
    (request_id, pending.future.as_mut().await)
}

async fn complete_profile_request(
    profile: (String, std::result::Result<ProfileSnapshot, Rejection>),
    host: &GatewayHost,
    pending: &mut Option<PendingProfile>,
    queued_request: &mut Option<(String, bool)>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let (request_id, result) = profile;
    *pending = queued_request
        .take()
        .map(|(request_id, include_provider_usage)| {
            profile_request(host, request_id, include_provider_usage)
        });
    match result {
        Ok(profile) => {
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::Profile {
                    request_id,
                    profile,
                }),
            )
            .await
        }
        Err(rejection) => write_rejection(writer, request_id, rejection).await,
    }
}

async fn queue_profile_request(
    writer: &mut (impl AsyncWrite + Unpin),
    queued_request: &mut Option<(String, bool)>,
    request_id: String,
    include_provider_usage: bool,
) -> Result<()> {
    if let Some((displaced, _)) = queued_request.replace((request_id, include_provider_usage)) {
        write_rejection(
            writer,
            displaced,
            Rejection {
                code: "profile_superseded",
                message: "profile request superseded by a newer request".into(),
                fatal: false,
            },
        )
        .await?;
    }
    Ok(())
}

fn profile_request(
    host: &GatewayHost,
    request_id: String,
    include_provider_usage: bool,
) -> PendingProfile {
    let gateway = host.clone();
    PendingProfile {
        request_id,
        future: Box::pin(async move {
            gateway.reconcile_pending_bot_deletion().await?;
            gateway.profile(include_provider_usage).await
        }),
    }
}

async fn write_gateway_broadcast(
    writer: &mut (impl AsyncWrite + Unpin),
    host: &GatewayHost,
    mut frame: ServerFrame,
) -> Result<()> {
    // Queued catalogs can predate a mutation response on this connection.
    match &mut frame.message {
        ServerMessage::Bots { bots, .. } => {
            *bots = host
                .bots()
                .await
                .map_err(|rejection| Error::Protocol(rejection.message))?;
        }
        ServerMessage::Swarms { swarms, .. } => {
            *swarms = host
                .swarms()
                .await
                .map_err(|rejection| Error::Protocol(rejection.message))?;
        }
        _ => {}
    }
    write_frame(writer, &frame).await
}

async fn handle_gateway_broadcast(
    outgoing: std::result::Result<ServerFrame, broadcast::error::RecvError>,
    host: &GatewayHost,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<bool> {
    match outgoing {
        Ok(frame) => write_gateway_broadcast(writer, host, frame)
            .await
            .map(|()| true),
        Err(broadcast::error::RecvError::Lagged(_)) => {
            let ready = host
                .ready()
                .await
                .map_err(|rejection| Error::Protocol(rejection.message))?;
            write_frame(
                writer,
                &ServerFrame::new(ServerMessage::Ready { payload: ready }),
            )
            .await?;
            Ok(true)
        }
        Err(broadcast::error::RecvError::Closed) => Ok(false),
    }
}

async fn handle_selected_broadcast(
    outgoing: std::result::Result<ServerFrame, broadcast::error::RecvError>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<bool> {
    match outgoing {
        Ok(frame) => write_frame(writer, &frame).await.map(|()| true),
        Err(broadcast::error::RecvError::Lagged(_)) => {
            write_server_error(
                writer,
                "client_lagged",
                "the client fell behind the event stream; reconnect with the last sequence",
                true,
            )
            .await?;
            Ok(false)
        }
        Err(broadcast::error::RecvError::Closed) => Ok(false),
    }
}

pub(super) fn tls_acceptor(config: &TlsConfig) -> Result<TlsAcceptor> {
    let certificates = load_certificates(&config.certificate)?;
    let private_key = load_private_key(&config.private_key)?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .map_err(|error| Error::Config(format!("invalid TLS certificate or key: {error}")))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

pub(super) fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let certificates = CertificateDer::pem_reader_iter(File::open(path)?)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(pem_error)?;
    if certificates.is_empty() {
        return Err(Error::Config("TLS certificate file is empty".into()));
    }
    Ok(certificates)
}

pub(super) fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::pem_reader_iter(File::open(path)?)
        .next()
        .transpose()
        .map_err(pem_error)?
        .ok_or_else(|| Error::Config("TLS private-key file is empty".into()))
}

fn pem_error(error: pem::Error) -> std::io::Error {
    match error {
        pem::Error::Io(error) => error,
        error => std::io::Error::new(std::io::ErrorKind::InvalidData, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{FrameReader, RunStats};
    use tokio::sync::oneshot;

    fn profile() -> ProfileSnapshot {
        ProfileSnapshot {
            user_name: None,
            daily_usage: Vec::new(),
            provider_usage: Vec::new(),
            run_stats: RunStats::default(),
            recent_run_groups: Vec::new(),
        }
    }

    #[tokio::test]
    async fn profile_queue_rejects_displaced_id_and_preserves_active_and_latest() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        let mut reader = FrameReader::new(reader);
        let (release, wait) = oneshot::channel();
        let mut pending = Some(PendingProfile {
            request_id: "profile-1".into(),
            future: Box::pin(async move {
                wait.await.expect("active profile release");
                Ok::<_, Rejection>(profile())
            }),
        });
        let mut queued = None;

        queue_profile_request(&mut writer, &mut queued, "profile-2".into(), false)
            .await
            .expect("first queued profile");
        queue_profile_request(&mut writer, &mut queued, "profile-3".into(), true)
            .await
            .expect("latest queued profile");

        let frame = read_frame::<ServerFrame>(&mut reader)
            .await
            .expect("superseded response")
            .expect("superseded frame");
        assert!(matches!(
            frame.message,
            ServerMessage::Rejected {
                request_id,
                code,
                fatal: false,
                ..
            } if request_id == "profile-2" && code == "profile_superseded"
        ));
        assert_eq!(queued.as_ref(), Some(&("profile-3".into(), true)));

        release.send(()).expect("release active profile");
        let (request_id, result) = next_profile(&mut pending).await;
        assert_eq!(request_id, "profile-1");
        assert!(result.is_ok());
    }
}
