use std::fmt::Write as _;
use std::time::Duration;

use futures_util::SinkExt as _;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{Message, protocol::Role};

use crate::wire::{FrameReader, MAX_FRAME_BYTES, read_frame, write_frame};

use super::{PATTERN, PROLOGUE, bridge, builder};

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
    bytes.iter().fold(String::new(), |mut hex, byte| {
        write!(hex, "{byte:02x}").expect("write hex");
        hex
    })
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
