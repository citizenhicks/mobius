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
use tokio_tungstenite::tungstenite::protocol::Role;

pub(super) use crate::wire::MAX_PRE_AUTH_FRAME_BYTES;

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
    pub(super) desktop_transport: bool,
    pub(super) auth: Arc<AuthStore>,
    pub(super) host: GatewayHost,
    pub(super) bots: Arc<BotStore>,
    pub(super) client_connections: Arc<ClientConnections>,
    pub(super) client_revocations: broadcast::Sender<String>,
    pub(super) admission: PreAuthConnectionAdmission,
    pub(super) access_lease: Option<AccessLease>,
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
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum PreAuthClientMessage {
    Pair {
        code: String,
        client_label: String,
        client_kind: ClientKind,
    },
    RepairPairing {
        code: String,
        replacing_token_digest: [u8; 32],
        client_label: String,
        client_kind: ClientKind,
    },
    Authenticate {
        token: String,
        client_kind: ClientKind,
        #[serde(default)]
        catalog: CatalogHint,
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
            // The encrypted record limit increases after authentication; do not strand
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
        mut response: Response,
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
        let mut protocols = request.headers().get_all(SEC_WEBSOCKET_PROTOCOL).iter();
        if protocols.next().and_then(|value| value.to_str().ok())
            != Some(crate::channel::SUBPROTOCOL)
            || protocols.next().is_some()
        {
            return Err(websocket_rejection(StatusCode::BAD_REQUEST));
        }
        response.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            crate::channel::SUBPROTOCOL
                .parse()
                .expect("static subprotocol is a header value"),
        );
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
    activity: Option<GatewayHost>,
    bots: Arc<BotStore>,
}

fn native_client_present(entries: &BTreeMap<(String, ClientKind), usize>, client_id: &str) -> bool {
    entries.iter().any(|((id, kind), count)| {
        id == client_id && *kind != ClientKind::GatewayDashboard && *count > 0
    })
}

fn record_client_presence(bots: &BotStore, client_id: &str, connected: bool) -> Result<()> {
    use crate::wire::{HookData, HookEvent, HookSource};
    let transition = uuid::Uuid::new_v4();
    let occurred_at = Utc::now().timestamp();
    for bot in bots.bots()? {
        let data = if connected {
            HookData::ClientConnected {
                client_id: client_id.into(),
            }
        } else {
            HookData::ClientDisconnected {
                client_id: client_id.into(),
            }
        };
        bots.record_hook(&HookEvent {
            id: format!("client-{transition}-{}", bot.id),
            source: HookSource::Client {
                client_id: client_id.into(),
            },
            cause_id: None,
            ancestry: Vec::new(),
            bot_id: bot.id,
            occurred_at,
            data,
        })?;
    }
    Ok(())
}

impl ClientConnections {
    pub(super) fn native_count(&self) -> Result<usize> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| Error::Config("client-connection lock is poisoned".into()))?;
        Ok(entries
            .iter()
            .filter(|((_, kind), _)| *kind != ClientKind::GatewayDashboard)
            .map(|(_, count)| count)
            .sum())
    }

    pub(super) fn register(
        self: &Arc<Self>,
        client_id: String,
        kind: ClientKind,
        bots: Arc<BotStore>,
    ) -> Result<ClientConnectionGuard> {
        let key = (client_id, kind);
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| Error::Config("client-connection lock is poisoned".into()))?;
        let first_native =
            kind != ClientKind::GatewayDashboard && !native_client_present(&entries, &key.0);
        let connections = entries.entry(key.clone()).or_default();
        *connections = connections
            .checked_add(1)
            .ok_or_else(|| Error::Config("client connection count overflow".into()))?;
        if first_native && let Err(error) = record_client_presence(&bots, &key.0, true) {
            eprintln!(
                "client lifecycle persistence failed: {}",
                connection_diagnostic(&error)
            );
        }
        drop(entries);
        Ok(ClientConnectionGuard {
            connections: Arc::clone(self),
            key,
            activity: None,
            bots,
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
        let close = || {
            let Ok(mut entries) = self.connections.entries.lock() else {
                return;
            };
            let Some(connections) = entries.get_mut(&self.key) else {
                return;
            };
            if let Some(host) = &self.activity {
                host.mark_runtime_activity();
            }
            if *connections > 1 {
                *connections -= 1;
            } else {
                entries.remove(&self.key);
            }
            if self.key.1 != ClientKind::GatewayDashboard
                && !native_client_present(&entries, &self.key.0)
                && let Err(error) = record_client_presence(&self.bots, &self.key.0, false)
            {
                eprintln!(
                    "client lifecycle persistence failed: {}",
                    connection_diagnostic(&error)
                );
            }
        };
        // Drop must commit the final presence fact before the guard disappears.
        // A multithreaded runtime can hand its worker to another thread during SQLite I/O.
        if tokio::runtime::Handle::try_current().is_ok_and(|handle| {
            handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
        }) {
            tokio::task::block_in_place(close);
        } else {
            close();
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
        Error::Protocol(_) | Error::WebSocketUpgrade { .. } => "protocol".into(),
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
    connection.desktop_transport = true;
    let PlaintextHandshake {
        expected_websocket_host,
        auth_deadline,
    } = handshake;
    let config = WebSocketConfig::default()
        .max_message_size(Some(
            MAX_PRE_AUTH_FRAME_BYTES + 4 + crate::channel::TAG_BYTES,
        ))
        .max_frame_size(Some(
            MAX_PRE_AUTH_FRAME_BYTES + 4 + crate::channel::TAG_BYTES,
        ));
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
    let mut state = tokio::time::timeout_at(
        auth_deadline,
        crate::channel::server_handshake(&mut websocket, connection.auth.channel_handshake()?),
    )
    .await
    .map_err(|_| Error::Unauthorized)??;
    let payload = tokio::time::timeout_at(
        auth_deadline,
        crate::channel::read_authentication(&mut websocket, &mut state, MAX_PRE_AUTH_FRAME_BYTES),
    )
    .await
    .map_err(|_| Error::Unauthorized)??;

    let (gateway_stream, mut bridge_stream) = tokio::io::duplex(WEBSOCKET_BRIDGE_BYTES);
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
            return crate::channel::bridge(websocket, state, bridge_stream).await;
        }
        result = &mut authenticated_rx => result.is_ok(),
    };
    if !authentication_succeeded {
        gateway.await?;
        return crate::channel::bridge(websocket, state, bridge_stream).await;
    }

    let mut stream = websocket.into_inner();
    stream.complete();
    let config = WebSocketConfig::default()
        .max_message_size(Some(crate::channel::MAX_RECORD))
        .max_frame_size(Some(crate::channel::MAX_RECORD));
    let websocket = WebSocketStream::from_raw_socket(stream, Role::Server, Some(config)).await;
    let (served, bridged) = tokio::join!(
        gateway,
        crate::channel::bridge(websocket, state, bridge_stream)
    );
    served?;
    bridged
}

async fn register_client_connection(
    connections: &Arc<ClientConnections>,
    id: String,
    kind: ClientKind,
    host: &GatewayHost,
    bots: &Arc<BotStore>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<Option<ClientConnectionGuard>> {
    let admission = if kind == ClientKind::GatewayDashboard {
        None
    } else {
        match host.begin_access().await {
            Ok(admission) => Some(admission),
            Err(rejection) => {
                write_server_error(writer, rejection.code, rejection.message, false).await?;
                return Ok(None);
            }
        }
    };
    let connections = Arc::clone(connections);
    let bots = Arc::clone(bots);
    let mut connection = tokio::task::spawn_blocking(move || connections.register(id, kind, bots))
        .await
        .map_err(|error| Error::Config(format!("client registration task failed: {error}")))??;
    if admission.is_some() {
        host.mark_runtime_activity();
        connection.activity = Some(host.clone());
    }
    drop(admission);
    Ok(Some(connection))
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
        desktop_transport,
        auth,
        host,
        bots,
        client_connections,
        client_revocations,
        admission,
        access_lease,
    } = connection;
    if access_lease.is_some_and(AccessLease::expired) {
        return Ok(());
    }
    let revocations = client_revocations.subscribe();
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = FrameReader::new(reader);
    let Some((client_id, client_kind, _authenticated_admission, view)) =
        authenticate_connection(&mut reader, &mut writer, &auth, admission, auth_deadline).await?
    else {
        return Ok(());
    };
    if access_lease.is_some_and(AccessLease::expired) {
        return Ok(());
    }

    let Some(_client_connection) = register_client_connection(
        &client_connections,
        client_id.clone(),
        client_kind,
        &host,
        &bots,
        &mut writer,
    )
    .await?
    else {
        return Ok(());
    };
    let _ = authentication_complete.map(|complete| complete.send(()));

    write_frame(&mut writer, &ServerFrame::new(ServerMessage::Authenticated)).await?;
    serve_authenticated_connection(
        reader,
        writer,
        &host,
        AuthenticatedClient {
            local,
            desktop_transport,
            connection_id: Uuid::new_v4(),
            access_lease,
            kind: client_kind,
            id: &client_id,
            connections: &client_connections,
            revocations: &client_revocations,
            bots: &bots,
            auth: &auth,
        },
        view,
        revocations,
    )
    .await
}

async fn serve_authenticated_connection(
    mut reader: FrameReader<impl AsyncRead + Unpin>,
    mut writer: impl AsyncWrite + Unpin,
    host: &GatewayHost,
    client: AuthenticatedClient<'_>,
    mut view: ClientView,
    mut revocations: broadcast::Receiver<String>,
) -> Result<()> {
    let client_id = client.id;
    let bots = client.bots;
    let mut gateway_broadcasts = host.subscribe();
    let mut desktop_changes = host.remote_desktop.subscribe();
    view.desktop_transport = client.desktop_transport;
    view.local = client.local;
    let ready = host
        .ready()
        .await
        .map_err(|rejection| Error::Protocol(rejection.message))?;
    view.write_catalog(
        &mut writer,
        ServerFrame::new(ServerMessage::Ready { payload: ready }),
    )
    .await?;
    if client.desktop_transport && host.remote_desktop.available() {
        super::desktop::write_control_state(host, client.connection_id, None, &mut writer).await?;
    }
    let mut selected: Option<SelectedChat> = None;
    let mut voice = None;
    let mut desktop = None;
    let mut browser = None;
    let mut remote_desktop = None;
    let mut pending_desktop_control = None;
    let session_files = host.session_file_store().await;
    let mut uploads: BTreeMap<(String, String), PendingSessionFileWrite> = BTreeMap::new();
    let mut pending_requests = JoinSet::new();
    let mut pending_profile = None;
    let mut queued_profile_request = None;
    let mut disabled_notifications = BTreeSet::new();

    loop {
        // Check revocation before fairly polling ordinary input and output.
        if connection_revoked(&mut revocations, client_id) {
            return Ok(());
        }
        let incoming = tokio::select! {
            revoked = revocations.recv() => {
                if client_revoked(revoked, client_id) {
                    return Ok(());
                }
                None
            }
            incoming = read_frame::<ClientFrame>(&mut reader) => Some(incoming),
            outgoing = crate::computer_runtime::next_app_update(&mut desktop, &mut browser) => {
                crate::computer_runtime::write_app_update(outgoing, &mut desktop, &mut browser, &mut writer).await?;
                None
            }
            outgoing = super::desktop::next_update(&mut remote_desktop, &mut desktop_changes, &mut pending_desktop_control) => {
                super::desktop::write_update(outgoing, host, &client, &mut pending_requests, &mut writer).await?;
                None
            }
            outgoing = super::voice::next_update(&mut voice) => {
                super::voice::write_update(&mut voice, outgoing, &mut writer).await?;
                None
            }
            profile = next_profile(&mut pending_profile) => {
                complete_profile_request(
                    profile,
                    host,
                    &mut pending_profile,
                    &mut queued_profile_request,
                    &mut writer,
                )
                .await?;
                None
            }
            Some(message) = pending_requests.join_next() => {
                write_request_result(message, &mut writer).await?;
                None
            }
            outgoing = gateway_broadcasts.recv() => {
                handle_gateway_broadcast(outgoing, host, &disabled_notifications, &mut view, &mut writer).await?;
                None
            }
            outgoing = selected_broadcast(&mut selected) => {
                handle_selected_broadcast(outgoing, &mut selected, &mut writer).await?;
                None
            }
        };
        let Some(incoming) = incoming else {
            continue;
        };
        let Some(frame) = incoming? else {
            return Ok(());
        };
        if !dispatch_authenticated_frame(
            frame,
            &client,
            host,
            ConnectionSessionState {
                disabled_notifications: &mut disabled_notifications,
                view: &mut view,
                selected: &mut selected,
                requests: &mut pending_requests,
                session_files: &session_files,
                bots,
                uploads: &mut uploads,
                voice: &mut voice,
                desktop: &mut desktop,
                browser: &mut browser,
                remote_desktop: &mut remote_desktop,
                pending_desktop_control: &mut pending_desktop_control,
            },
            &mut pending_profile,
            &mut queued_profile_request,
            &mut writer,
        )
        .await?
        {
            return Ok(());
        }
    }
}

async fn dispatch_authenticated_frame(
    frame: ClientFrame,
    client: &AuthenticatedClient<'_>,
    host: &GatewayHost,
    connection: ConnectionSessionState<'_>,
    pending_profile: &mut Option<PendingProfile>,
    queued_profile_request: &mut Option<(String, bool)>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<bool> {
    if client.access_lease.is_some_and(AccessLease::expired) {
        return Ok(false);
    }
    if let Err(error) = validate_version(frame.version) {
        write_server_error(writer, "protocol_version", error.to_string(), true).await?;
        return Ok(false);
    }
    if let Some(message) = handle_profile_message(
        frame.message,
        host,
        pending_profile,
        queued_profile_request,
        writer,
    )
    .await?
    {
        handle_message(
            message,
            client.auth,
            host,
            client.bots,
            client,
            connection,
            writer,
        )
        .await?;
    }
    Ok(true)
}

fn connection_revoked(revocations: &mut broadcast::Receiver<String>, client_id: &str) -> bool {
    loop {
        match revocations.try_recv() {
            Ok(revoked) if revoked != client_id => {}
            Err(broadcast::error::TryRecvError::Empty) => return false,
            _ => return true,
        }
    }
}

fn client_revoked(
    result: std::result::Result<String, broadcast::error::RecvError>,
    client_id: &str,
) -> bool {
    !matches!(result, Ok(revoked) if revoked != client_id)
}

async fn authenticate_connection(
    reader: &mut FrameReader<impl AsyncRead + Unpin>,
    writer: &mut (impl AsyncWrite + Unpin),
    auth: &AuthStore,
    admission: PreAuthConnectionAdmission,
    auth_deadline: Instant,
) -> Result<Option<(String, ClientKind, OwnedSemaphorePermit, ClientView)>> {
    let mut first = tokio::time::timeout_at(
        auth_deadline,
        read_frame_with_limit::<PreAuthClientFrame>(reader, MAX_PRE_AUTH_FRAME_BYTES),
    )
    .await
    .map_err(|_| Error::Unauthorized)??
    .ok_or(Error::Unauthorized)?;
    if let Err(error) = validate_version(first.version) {
        write_server_error(writer, "protocol_version", error.to_string(), true).await?;
        return Ok(None);
    }
    let Some(_authenticated_admission) = admission.promote() else {
        write_server_error(
            writer,
            "server_busy",
            "the gateway has reached its authenticated connection limit",
            true,
        )
        .await?;
        return Ok(None);
    };
    let view = ClientView::new(match &mut first.message {
        PreAuthClientMessage::Authenticate { catalog, .. } => std::mem::take(catalog),
        _ => CatalogHint::default(),
    });
    let Some((client_id, client_kind)) = authenticate_client(first.message, auth, writer).await?
    else {
        return Ok(None);
    };

    Ok(Some((
        client_id,
        client_kind,
        _authenticated_admission,
        view,
    )))
}

async fn authenticate_client(
    message: PreAuthClientMessage,
    auth: &AuthStore,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<Option<(String, ClientKind)>> {
    let (issued, client_kind) = match message {
        PreAuthClientMessage::Pair {
            code,
            client_label,
            client_kind,
        } => (auth.pair(&code, &client_label), client_kind),
        PreAuthClientMessage::RepairPairing {
            code,
            replacing_token_digest,
            client_label,
            client_kind,
        } => (
            auth.repair_pairing(&code, &replacing_token_digest, &client_label),
            client_kind,
        ),
        PreAuthClientMessage::Authenticate {
            token, client_kind, ..
        } => {
            return match auth.authenticate(&token) {
                Ok(identity) => Ok(Some((identity.id, client_kind))),
                Err(_) => {
                    write_server_error(writer, "unauthorized", "authentication failed", true)
                        .await?;
                    Ok(None)
                }
            };
        }
        PreAuthClientMessage::Unsupported => {
            write_server_error(
                writer,
                "authentication_required",
                "the first frame must authenticate or pair",
                true,
            )
            .await?;
            return Ok(None);
        }
    };
    match issued {
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
    view: &mut ClientView,
    mut frame: ServerFrame,
) -> Result<()> {
    // Queued catalogs can predate a mutation response on this connection.
    if let ServerMessage::Bots { bots, .. } = &mut frame.message {
        *bots = host
            .bots()
            .await
            .map_err(|rejection| Error::Protocol(rejection.message))?;
    }
    view.write_broadcast(writer, frame).await
}

async fn handle_gateway_broadcast(
    outgoing: std::result::Result<ServerFrame, broadcast::error::RecvError>,
    host: &GatewayHost,
    disabled: &BTreeSet<GatewayNotification>,
    view: &mut ClientView,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    match outgoing {
        Ok(frame) if notification_disabled(&frame.message, disabled) => Ok(()),
        Ok(frame) => write_gateway_broadcast(writer, host, view, frame).await,
        Err(broadcast::error::RecvError::Lagged(_)) => {
            let ready = host
                .ready()
                .await
                .map_err(|rejection| Error::Protocol(rejection.message))?;
            view.write_catalog(
                writer,
                ServerFrame::new(ServerMessage::Ready { payload: ready }),
            )
            .await?;
            Ok(())
        }
        Err(broadcast::error::RecvError::Closed) => {
            Err(Error::Protocol("gateway event stream ended".into()))
        }
    }
}

fn notification_disabled(
    message: &ServerMessage,
    disabled: &BTreeSet<GatewayNotification>,
) -> bool {
    let notification = match message {
        ServerMessage::Sessions {
            request_id: None, ..
        } => GatewayNotification::Sessions,
        ServerMessage::Bots {
            request_id: None, ..
        } => GatewayNotification::Bots,
        _ => return false,
    };
    disabled.contains(&notification)
}

async fn handle_selected_broadcast(
    outgoing: std::result::Result<SharedFrame, broadcast::error::RecvError>,
    selected: &mut Option<SelectedChat>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    match outgoing {
        Ok(frame) => frame.write(writer).await,
        Err(broadcast::error::RecvError::Lagged(_)) => {
            write_server_error(
                writer,
                "client_lagged",
                "the client fell behind the event stream; reconnect with the last sequence",
                true,
            )
            .await?;
            Err(Error::Protocol(
                "client fell behind the event stream".into(),
            ))
        }
        Err(broadcast::error::RecvError::Closed) => {
            *selected = None;
            Ok(())
        }
    }
}

async fn write_request_result(
    result: std::result::Result<ServerMessage, tokio::task::JoinError>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    match result {
        Ok(message) => write_frame(writer, &ServerFrame::new(message)).await,
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => Err(Error::Protocol(format!(
            "connection request task failed: {error}"
        ))),
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
    use sha2::{Digest as _, Sha256};
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
    async fn repair_pairing_rotates_the_requested_client() {
        let directory = tempfile::tempdir().expect("state directory");
        let (auth, grant) =
            AuthStore::initialize(directory.path().join("auth.json")).expect("initialize auth");
        let original = auth.pair(&grant.code, "iPhone").expect("pair client");
        let replacement = auth.create_pairing_code().expect("repair code");
        let replacing_token_digest = Sha256::digest(original.token.as_bytes()).into();
        let (mut writer, reader) = tokio::io::duplex(4096);
        let mut reader = FrameReader::new(reader);

        let identity = authenticate_client(
            PreAuthClientMessage::RepairPairing {
                code: replacement.code,
                replacing_token_digest,
                client_label: "iPhone".into(),
                client_kind: ClientKind::Ios,
            },
            &auth,
            &mut writer,
        )
        .await
        .expect("repair pairing");
        let frame = read_frame::<ServerFrame>(&mut reader)
            .await
            .expect("paired response")
            .expect("paired frame");
        let ServerMessage::Paired { client_id, token } = frame.message else {
            panic!("expected paired response");
        };

        assert_eq!(
            identity,
            Some((original.client_id.clone(), ClientKind::Ios))
        );
        assert_eq!(client_id, original.client_id);
        assert!(auth.authenticate(&original.token).is_err());
        assert!(auth.authenticate(&token).is_ok());
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
