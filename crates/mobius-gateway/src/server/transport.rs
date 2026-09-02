use super::*;

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

pub(super) async fn serve_plaintext_connection(
    stream: TcpStream,
    auth: Arc<AuthStore>,
    host: GatewayHost,
    bots: Arc<BotStore>,
    client_connections: Arc<ClientConnections>,
    client_revocations: broadcast::Sender<String>,
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
            auth,
            host,
            bots,
            client_connections,
            client_revocations,
            PlaintextHandshake {
                expected_websocket_host,
                auth_deadline,
            },
        )
        .await
    } else {
        serve_connection(
            stream,
            auth,
            host,
            bots,
            client_connections,
            client_revocations,
            auth_deadline,
        )
        .await
    }
}

pub(super) async fn serve_websocket(
    stream: TcpStream,
    auth: Arc<AuthStore>,
    host: GatewayHost,
    bots: Arc<BotStore>,
    client_connections: Arc<ClientConnections>,
    client_revocations: broadcast::Sender<String>,
    handshake: PlaintextHandshake,
) -> Result<()> {
    let PlaintextHandshake {
        expected_websocket_host,
        auth_deadline,
    } = handshake;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES));
    let websocket = tokio::time::timeout_at(
        auth_deadline,
        accept_hdr_async_with_config(
            stream,
            WebSocketUpgradePolicy {
                expected_host: expected_websocket_host,
            },
            Some(config),
        ),
    )
    .await
    .map_err(|_| Error::Unauthorized)?
    .map_err(websocket_error)?;
    let (outgoing, incoming) = websocket.split();
    let (gateway_stream, bridge_stream) = tokio::io::duplex(WEBSOCKET_BRIDGE_BYTES);
    let (bridge_reader, bridge_writer) = tokio::io::split(bridge_stream);
    let gateway_and_outgoing = async {
        let (gateway, outgoing) = tokio::join!(
            serve_connection(
                gateway_stream,
                auth,
                host,
                bots,
                client_connections,
                client_revocations,
                auth_deadline,
            ),
            framed_to_websocket(bridge_reader, outgoing),
        );
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
    auth: Arc<AuthStore>,
    host: GatewayHost,
    bots: Arc<BotStore>,
    client_connections: Arc<ClientConnections>,
    client_revocations: broadcast::Sender<String>,
    auth_deadline: Instant,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut revocations = client_revocations.subscribe();
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = FrameReader::new(reader);
    let first = tokio::time::timeout_at(auth_deadline, read_frame::<ClientFrame>(&mut reader))
        .await
        .map_err(|_| Error::Unauthorized)??
        .ok_or(Error::Unauthorized)?;
    if let Err(error) = validate_version(first.version) {
        write_server_error(&mut writer, "protocol_version", error.to_string(), true).await?;
        return Ok(());
    }
    let (client_id, client_kind) = match first.message {
        ClientMessage::Pair {
            code,
            client_label,
            client_kind,
        } => match auth.pair(&code, &client_label) {
            Ok(issued) => {
                let client_id = issued.client_id.clone();
                write_frame(
                    &mut writer,
                    &ServerFrame::new(ServerMessage::Paired {
                        client_id: issued.client_id,
                        token: issued.token,
                    }),
                )
                .await?;
                (client_id, client_kind)
            }
            Err(_) => {
                write_server_error(&mut writer, "unauthorized", "pairing failed", true).await?;
                return Ok(());
            }
        },
        ClientMessage::Authenticate { token, client_kind } => match auth.authenticate(&token) {
            Ok(identity) => (identity.id, client_kind),
            Err(_) => {
                write_server_error(&mut writer, "unauthorized", "authentication failed", true)
                    .await?;
                return Ok(());
            }
        },
        _ => {
            write_server_error(
                &mut writer,
                "authentication_required",
                "the first frame must authenticate or pair",
                true,
            )
            .await?;
            return Ok(());
        }
    };

    let _client_connection = client_connections.register(client_id.clone(), client_kind)?;

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
    let session_files = host.session_file_store().await;
    let mut uploads: BTreeMap<(String, String), PendingSessionFileWrite> = BTreeMap::new();

    loop {
        let incoming = tokio::select! {
            biased;
            revoked = revocations.recv() => {
                match revoked {
                    Ok(revoked) if revoked == client_id => return Ok(()),
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)
                        | broadcast::error::RecvError::Closed) => return Ok(()),
                }
                None
            }
            incoming = read_frame::<ClientFrame>(&mut reader) => Some(incoming),
            outgoing = gateway_broadcasts.recv() => {
                match outgoing {
                    Ok(frame) => write_frame(&mut writer, &frame).await?,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let ready = host
                            .ready()
                            .await
                            .map_err(|rejection| Error::Protocol(rejection.message))?;
                        write_frame(
                            &mut writer,
                            &ServerFrame::new(ServerMessage::Ready { payload: ready }),
                        )
                        .await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
                None
            }
            outgoing = selected_broadcast(&mut selected) => {
                match outgoing {
                    Ok(frame) => {
                        let active = selected
                            .as_mut()
                            .expect("a selected-chat broadcast requires a selected chat");
                        if !sequence(&frame)
                            .is_some_and(|value| value <= active.delivered_sequence)
                        {
                            if let Some(value) = sequence(&frame) {
                                active.delivered_sequence = value;
                            }
                            write_frame(&mut writer, &frame).await?;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        write_server_error(
                            &mut writer,
                            "client_lagged",
                            "the client fell behind the event stream; reconnect with the last sequence",
                            true,
                        )
                        .await?;
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
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
        let client = AuthenticatedClient {
            id: &client_id,
            connections: &client_connections,
            revocations: &client_revocations,
        };
        handle_message(
            frame.message,
            &auth,
            &host,
            &bots,
            &client,
            ConnectionSessionState {
                selected: &mut selected,
                session_files: &session_files,
                bots: &bots,
                uploads: &mut uploads,
            },
            &mut writer,
        )
        .await?;
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
    let mut reader = BufReader::new(File::open(path)?);
    let certificates = rustls_pemfile::certs(&mut reader).collect::<std::io::Result<Vec<_>>>()?;
    if certificates.is_empty() {
        return Err(Error::Config("TLS certificate file is empty".into()));
    }
    Ok(certificates)
}

pub(super) fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let mut reader = BufReader::new(File::open(path)?);
    rustls_pemfile::private_key(&mut reader)?
        .ok_or_else(|| Error::Config("TLS private-key file is empty".into()))
}
