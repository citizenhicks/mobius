use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncWriteExt as _, DuplexStream, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::{HeaderValue, Request, header::SEC_WEBSOCKET_PROTOCOL};
use tokio_tungstenite::tungstenite::{Message, protocol::Role};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async, client_async, connect_async_with_config,
};

use crate::server::GatewayServer;
use crate::wire::{
    ClientFrame, ClientKind, ClientMessage, FrameReader, MAX_FRAME_BYTES, ServerFrame,
    ServerMessage, read_frame, write_frame,
};

use super::{Identity, PATTERN, PROLOGUE, bridge, builder, client_handshake};

const LABEL: &str = "encrypted-relay-probe-private-label";
const REQUEST: &str = "encrypted-relay-probe-private-command";
const SAMPLES: usize = 40;
const WARMUPS: usize = 3;
type Capture = Arc<Mutex<Vec<Message>>>;

fn websocket_request(address: SocketAddr) -> Request<()> {
    let mut request = format!("ws://{address}").into_client_request().unwrap();
    request.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static("mobius-noise-v1"),
    );
    request
}

struct ProbeUpgrade;

impl tokio_tungstenite::tungstenite::handshake::server::Callback for ProbeUpgrade {
    fn on_request(
        self,
        _: &Request<()>,
        mut response: tokio_tungstenite::tungstenite::handshake::server::Response,
    ) -> std::result::Result<
        tokio_tungstenite::tungstenite::handshake::server::Response,
        tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
    > {
        response.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("mobius-noise-v1"),
        );
        Ok(response)
    }
}

fn fixed_handshakes() -> (snow::HandshakeState, snow::HandshakeState, [u8; 32]) {
    use snow::resolvers::{CryptoResolver as _, DefaultResolver};
    let mut dh = DefaultResolver
        .resolve_dh(&snow::params::DHChoice::Curve25519)
        .unwrap();
    dh.set(&[1; 32]);
    let public: [u8; 32] = dh.pubkey().try_into().unwrap();
    let initiator = builder()
        .unwrap()
        .remote_public_key(&public)
        .unwrap()
        .fixed_ephemeral_key_for_testing_only(&[2; 32])
        .build_initiator()
        .unwrap();
    let responder = builder()
        .unwrap()
        .local_private_key(&[1; 32])
        .unwrap()
        .fixed_ephemeral_key_for_testing_only(&[3; 32])
        .build_responder()
        .unwrap();
    (initiator, responder, public)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn transport_pair() -> (snow::TransportState, snow::TransportState) {
    let (mut initiator, mut responder, _) = fixed_handshakes();
    let mut message = [0; 48];
    initiator.write_message(&[], &mut message).unwrap();
    responder.read_message(&message, &mut []).unwrap();
    responder.write_message(&[], &mut message).unwrap();
    initiator.read_message(&message, &mut []).unwrap();
    (
        initiator.into_transport_mode().unwrap(),
        responder.into_transport_mode().unwrap(),
    )
}

#[test]
fn shared_noise_test_vector() {
    let (mut initiator, mut responder, public) = fixed_handshakes();
    let mut message = [0; 128];
    let length = initiator.write_message(&[], &mut message).unwrap();
    let client_handshake = hex(&message[..length]);
    responder.read_message(&message[..length], &mut []).unwrap();
    let length = responder.write_message(&[], &mut message).unwrap();
    let server_handshake = hex(&message[..length]);
    initiator.read_message(&message[..length], &mut []).unwrap();
    let mut initiator = initiator.into_transport_mode().unwrap();
    let mut responder = responder.into_transport_mode().unwrap();
    let client_plaintext = b"mobius client test vector";
    let server_plaintext = b"mobius gateway test vector";
    let length = initiator
        .write_message(client_plaintext, &mut message)
        .unwrap();
    let client_ciphertext = hex(&message[..length]);
    let mut plaintext = [0; 128];
    let count = responder
        .read_message(&message[..length], &mut plaintext)
        .unwrap();
    assert_eq!(&plaintext[..count], client_plaintext);
    assert!(
        responder
            .read_message(&message[..length], &mut plaintext)
            .is_err(),
        "replayed record must fail"
    );
    let length = responder
        .write_message(server_plaintext, &mut message)
        .unwrap();
    let server_ciphertext = hex(&message[..length]);
    let count = initiator
        .read_message(&message[..length], &mut plaintext)
        .unwrap();
    assert_eq!(&plaintext[..count], server_plaintext);
    let length = responder
        .write_message(server_plaintext, &mut message)
        .unwrap();
    message[length - 1] ^= 1;
    assert!(
        initiator
            .read_message(&message[..length], &mut plaintext)
            .is_err(),
        "tampered record must fail"
    );
    let fixture = serde_json::json!({
        "pattern": PATTERN, "prologue": String::from_utf8_lossy(PROLOGUE),
        "server_private_key": hex(&[1; 32]), "server_public_key": hex(&public),
        "client_ephemeral_key": hex(&[2; 32]), "server_ephemeral_key": hex(&[3; 32]),
        "client_handshake": client_handshake, "server_handshake": server_handshake,
        "client_plaintext": hex(client_plaintext), "client_ciphertext": client_ciphertext,
        "server_plaintext": hex(server_plaintext), "server_ciphertext": server_ciphertext,
    });
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("test-vector.json")).unwrap();
    assert_eq!(fixture, expected);
}

// Keep this in-memory correctness deadline independent of host CPU speed.
#[tokio::test(start_paused = true)]
async fn encrypted_records_preserve_maximum_application_frame() {
    let (initiator, responder) = transport_pair();
    let (client_socket, server_socket) = tokio::io::duplex(16 * 1024);
    let client_socket = WebSocketStream::from_raw_socket(client_socket, Role::Client, None).await;
    let server_socket = WebSocketStream::from_raw_socket(server_socket, Role::Server, None).await;
    let (mut client, client_bridge) = tokio::io::duplex(16 * 1024);
    let (server, server_bridge) = tokio::io::duplex(16 * 1024);
    let sending = tokio::spawn(bridge(client_socket, initiator, client_bridge));
    let receiving = tokio::spawn(bridge(server_socket, responder, server_bridge));
    // Two JSON quotes make the encoded payload exactly the application's 50 MiB limit.
    let payload = "x".repeat(MAX_FRAME_BYTES - 2);
    let mut reader = FrameReader::new(server);
    let (written, read) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(
            write_frame(&mut client, &payload),
            read_frame::<String>(&mut reader)
        )
    })
    .await
    .expect("bounded large frame transfer");
    written.unwrap();
    assert_eq!(read.unwrap().unwrap(), payload);
    sending.abort();
    receiving.abort();
}

#[tokio::test]
async fn truncated_encrypted_frame_is_not_delivered() {
    let (mut initiator, responder) = transport_pair();
    let (client_socket, server_socket) = tokio::io::duplex(1024);
    let mut client_socket =
        WebSocketStream::from_raw_socket(client_socket, Role::Client, None).await;
    let server_socket = WebSocketStream::from_raw_socket(server_socket, Role::Server, None).await;
    let (stream, remote) = tokio::io::duplex(1024);
    let receiving = tokio::spawn(bridge(server_socket, responder, remote));
    let mut ciphertext = [0; 64];
    // A valid authenticated record contains an incomplete two-byte JSON frame.
    let length = initiator
        .write_message(b"\0\0\0\x02{", &mut ciphertext)
        .unwrap();
    client_socket
        .send(Message::Binary(ciphertext[..length].to_vec().into()))
        .await
        .unwrap();
    client_socket.close(None).await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        read_frame::<serde_json::Value>(&mut FrameReader::new(stream)),
    )
    .await
    .unwrap();
    assert!(
        matches!(result, Err(crate::Error::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof)
    );
    receiving.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn encrypted_channel_sends_idle_websocket_keepalives() {
    let (initiator, _) = transport_pair();
    let (client_socket, server_socket) = tokio::io::duplex(1024);
    let client_socket = WebSocketStream::from_raw_socket(client_socket, Role::Client, None).await;
    let mut server_socket =
        WebSocketStream::from_raw_socket(server_socket, Role::Server, None).await;
    let (_stream, remote) = tokio::io::duplex(1024);
    let serving = tokio::spawn(bridge(client_socket, initiator, remote));
    let message = tokio::time::timeout(
        super::KEEPALIVE + Duration::from_secs(1),
        server_socket.next(),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert!(matches!(message, Message::Ping(payload) if payload.is_empty()));
    serving.abort();
}

#[tokio::test(start_paused = true)]
async fn stalled_encrypted_write_closes_the_channel() {
    let (initiator, _) = transport_pair();
    let (socket, _blocked_peer) = tokio::io::duplex(1);
    let socket = WebSocketStream::from_raw_socket(socket, Role::Client, None).await;
    let (mut stream, remote) = tokio::io::duplex(1024);
    let serving = tokio::spawn(bridge(socket, initiator, remote));
    stream
        .write_all(b"a record that cannot fit the socket buffer")
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(super::WRITE_TIMEOUT + Duration::from_secs(1), serving)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
}

#[test]
fn malformed_credentials_and_exposed_private_keys_are_rejected() {
    for credential in [
        "old-secret",
        "m1..",
        "m1.short.00000000000000000000000000000000",
    ] {
        assert!(super::credential_key(credential).is_err());
    }
    let directory = tempfile::tempdir().unwrap();
    let auth_path = directory.path().join("auth.json");
    let identity = Identity::open(&auth_path).unwrap();
    let persisted = Identity::open(&auth_path).unwrap();
    assert_eq!(identity.public, persisted.public);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let key_path = auth_path.with_extension("channel-key");
        assert_eq!(
            std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Identity::open(&auth_path).is_err());
    }
}

async fn reject_plaintext_and_wrong_pin(address: SocketAddr, code: &str) {
    assert!(
        connect_async_with_config(format!("ws://{address}"), None, true)
            .await
            .is_err(),
        "missing encryption subprotocol must fail before upgrade"
    );
    let (mut websocket, _) = connect_async_with_config(websocket_request(address), None, true)
        .await
        .unwrap();
    let plaintext = serde_json::to_vec(&ClientFrame::new(ClientMessage::Pair {
        code: code.into(),
        client_label: LABEL.into(),
        client_kind: ClientKind::Cli,
    }))
    .unwrap();
    websocket
        .send(Message::Binary(plaintext.into()))
        .await
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(5), websocket.next())
        .await
        .unwrap();
    assert!(
        matches!(response, None | Some(Err(_)) | Some(Ok(Message::Close(_)))),
        "plaintext WebSocket must close without processing pairing"
    );
    let wrong_identity = Identity {
        private: [4; 32],
        public: [5; 32],
    };
    let (mut websocket, _) = connect_async_with_config(websocket_request(address), None, true)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            client_handshake(&mut websocket, &wrong_identity.credential(&"0".repeat(32)))
        )
        .await
        .unwrap()
        .is_err(),
        "wrong gateway pin must fail"
    );
}

struct ProbeClient {
    reader: FrameReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    bridge: JoinHandle<crate::Result<()>>,
}

impl ProbeClient {
    async fn connect(address: SocketAddr, credential: &str) -> (Self, Duration, Duration) {
        let started = Instant::now();
        let (mut websocket, response) =
            connect_async_with_config(websocket_request(address), None, true)
                .await
                .expect("probe WebSocket upgrade");
        assert_eq!(
            response.headers()[SEC_WEBSOCKET_PROTOCOL],
            "mobius-noise-v1"
        );
        let upgraded = started.elapsed();
        let noise_started = Instant::now();
        let transport = client_handshake(&mut websocket, credential)
            .await
            .expect("pinned gateway handshake");
        let noise = noise_started.elapsed();
        let (stream, remote) = tokio::io::duplex(16 * 1024);
        let bridge = tokio::spawn(bridge(websocket, transport, remote));
        let (reader, writer) = tokio::io::split(stream);
        (
            Self {
                reader: FrameReader::new(reader),
                writer,
                bridge,
            },
            upgraded,
            noise,
        )
    }

    async fn send(&mut self, message: ClientMessage) {
        write_frame(&mut self.writer, &ClientFrame::new(message))
            .await
            .expect("encrypted command");
    }

    async fn next(&mut self) -> ServerMessage {
        tokio::time::timeout(
            Duration::from_secs(5),
            read_frame::<ServerFrame>(&mut self.reader),
        )
        .await
        .expect("gateway response timeout")
        .expect("gateway response")
        .expect("gateway remained connected")
        .message
    }

    async fn ready(&mut self) {
        while !matches!(self.next().await, ServerMessage::Ready { .. }) {}
    }

    async fn list_clients(&mut self) {
        self.send(ClientMessage::ListClients {
            request_id: REQUEST.into(),
        })
        .await;
        loop {
            if let ServerMessage::Clients {
                request_id,
                clients,
                ..
            } = self.next().await
            {
                assert_eq!(request_id, REQUEST);
                assert!(clients.iter().any(|client| client.label == LABEL));
                return;
            }
        }
    }

    async fn close(mut self) {
        self.writer.shutdown().await.expect("close probe stream");
        let _ = tokio::time::timeout(Duration::from_secs(1), &mut self.bridge).await;
    }
}

impl Drop for ProbeClient {
    fn drop(&mut self) {
        self.bridge.abort();
    }
}

async fn capture_relay(listener: TcpListener, gateway: SocketAddr, capture: Capture) {
    let mut connections = JoinSet::new();
    loop {
        let (stream, _) = listener.accept().await.expect("relay connection");
        stream.set_nodelay(true).expect("relay TCP_NODELAY");
        let capture = Arc::clone(&capture);
        connections.spawn(async move {
            let mut client = accept_hdr_async(stream, ProbeUpgrade)
                .await
                .expect("relay client upgrade");
            let stream = TcpStream::connect(gateway)
                .await
                .expect("relay upstream TCP");
            stream.set_nodelay(true).expect("upstream TCP_NODELAY");
            let (mut server, _) = client_async(websocket_request(gateway), stream)
                .await
                .expect("relay gateway upgrade");
            loop {
                let (message, destination) = tokio::select! {
                    message = client.next() => (message, &mut server),
                    message = server.next() => (message, &mut client),
                };
                let Some(Ok(message)) = message else { break };
                let closing = message.is_close();
                match &message {
                    Message::Binary(_) | Message::Text(_) => {
                        capture.lock().expect("capture lock").push(message.clone())
                    }
                    _ => {}
                }
                if destination.send(message).await.is_err() || closing {
                    break;
                }
            }
        });
        while connections.try_join_next().is_some() {}
    }
}

fn report_latency(name: &str, samples: impl Iterator<Item = Duration>) {
    let mut values: Vec<f64> = samples
        .map(|sample| sample.as_secs_f64() * 1_000.0)
        .collect();
    values.sort_by(f64::total_cmp);
    let median = (values[SAMPLES / 2 - 1] + values[SAMPLES / 2]) / 2.0;
    let p95 = values[(SAMPLES * 95).div_ceil(100) - 1];
    eprintln!("{name}: median {median:.3} ms, p95 {p95:.3} ms");
}

/// Run with `cargo test -p mobius-gateway encrypted_relay_probe -- --nocapture`.
/// Measures loopback with a forwarding relay; excludes TLS, cloud auth, and network/wake time.
#[tokio::test]
async fn encrypted_relay_probe() {
    let root = tempfile::tempdir().expect("isolated gateway directory");
    let (mut gateway, grant) =
        GatewayServer::bootstrap(root.path().join("gateway"), "127.0.0.1:0".parse().unwrap())
            .await
            .expect("isolated gateway");
    let gateway_address = gateway.listen_addr();
    let ready = gateway.notify_ready();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(gateway.serve_until(async {
        let _ = stopped.await;
    }));
    ready.await.expect("gateway listener ready");
    reject_plaintext_and_wrong_pin(gateway_address, &grant.code).await;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("relay listener");
    let address = listener.local_addr().expect("relay address");
    let capture = Capture::default();
    let relay = tokio::spawn(capture_relay(
        listener,
        gateway_address,
        Arc::clone(&capture),
    ));
    let (mut client, _, _) = ProbeClient::connect(address, &grant.code).await;
    client
        .send(ClientMessage::Pair {
            code: grant.code.clone(),
            client_label: LABEL.into(),
            client_kind: ClientKind::Cli,
        })
        .await;
    let ServerMessage::Paired { token, .. } = client.next().await else {
        panic!("gateway did not issue a device token");
    };
    client.ready().await;
    client.list_clients().await;
    client.close().await;

    let mut samples = Vec::with_capacity(SAMPLES);
    for sample in 0..WARMUPS + SAMPLES {
        let started = Instant::now();
        let (mut client, upgrade, noise) = ProbeClient::connect(address, &token).await;
        let authentication_started = Instant::now();
        client
            .send(ClientMessage::Authenticate {
                token: token.clone(),
                client_kind: ClientKind::Cli,
            })
            .await;
        assert!(matches!(client.next().await, ServerMessage::Authenticated));
        let authentication = authentication_started.elapsed();
        client.ready().await;
        let ready = started.elapsed();
        let command_started = Instant::now();
        client.list_clients().await;
        if sample >= WARMUPS {
            samples.push([
                upgrade,
                noise,
                authentication,
                ready,
                command_started.elapsed(),
            ]);
        }
        client.close().await;
    }
    stop.send(()).expect("stop gateway");
    serving
        .await
        .expect("gateway task")
        .expect("clean gateway shutdown");
    relay.abort();
    let captured = capture.lock().expect("capture lock");
    assert!(!captured.is_empty());
    assert!(
        captured
            .iter()
            .all(|message| matches!(message, Message::Binary(_))),
        "relay observed a plaintext text frame"
    );
    let visible: Vec<u8> = captured
        .iter()
        .flat_map(|message| message.clone().into_data())
        .collect();
    for forbidden in [
        grant.code.rsplit('.').next().unwrap(),
        token.rsplit('.').next().unwrap(),
        LABEL,
        REQUEST,
    ] {
        assert!(
            !visible
                .windows(forbidden.len())
                .any(|window| window == forbidden.as_bytes()),
            "relay observed private plaintext"
        );
    }
    assert!(captured.iter().all(|record| record.len() <= 65_535));
    eprintln!(
        "Encrypted loopback relay: {SAMPLES} samples, {WARMUPS} warmups; {} captured binary records; no fixture plaintext found",
        captured.len()
    );
    for (index, name) in [
        "WebSocket upgrade",
        "Noise handshake",
        "Device auth",
        "Connect to Ready",
        "Established command RTT",
    ]
    .into_iter()
    .enumerate()
    {
        report_latency(name, samples.iter().map(|sample| sample[index]));
    }
}
