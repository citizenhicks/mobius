//! Pinned Noise channels inside WebSockets; relay-visible records contain no gateway JSON.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use snow::resolvers::{CryptoResolver as _, DefaultResolver};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, DuplexStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{Bytes, Message};

use crate::wire::websocket_error;
use crate::{Error, Result};

pub(crate) const PATTERN: &str = "Noise_NK_25519_ChaChaPoly_SHA256";
pub(crate) const PROLOGUE: &[u8] = b"mobius-gateway-channel-v1";
pub(crate) const SUBPROTOCOL: &str = "mobius-noise-v1";
pub(crate) const MAX_RECORD: usize = 65_535;
pub(crate) const TAG_BYTES: usize = 16;
const MAX_PLAINTEXT: usize = MAX_RECORD - TAG_BYTES;
const KEEPALIVE: Duration = Duration::from_secs(30);
use crate::wire::WRITE_TIMEOUT;

pub(crate) struct Identity {
    private: [u8; 32],
    public: [u8; 32],
}

impl Identity {
    pub(crate) fn open(auth_path: &Path) -> Result<Self> {
        let path = auth_path.with_extension("channel-key");
        if !path.try_exists()? {
            let key = builder()?.generate_keypair().map_err(crypto_error)?;
            match crate::publication::publish(&path, &key.private, true) {
                Ok(()) => {}
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() != 32 {
            return Err(Error::Config(
                "gateway channel key must be a 32-byte private file".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(Error::Config(
                    "gateway channel key must be readable only by its owner".into(),
                ));
            }
        }
        let private: [u8; 32] = std::fs::read(&path)?.try_into().map_err(|bytes: Vec<u8>| {
            Error::Config(format!(
                "gateway channel key changed while reading: {} bytes",
                bytes.len()
            ))
        })?;
        let mut dh = DefaultResolver
            .resolve_dh(&snow::params::DHChoice::Curve25519)
            .ok_or_else(|| Error::Config("gateway channel key agreement is unavailable".into()))?;
        dh.set(&private);
        let public = dh
            .pubkey()
            .try_into()
            .map_err(rejected("gateway channel public key is malformed"))?;
        Ok(Self { private, public })
    }

    #[expect(
        clippy::expect_used,
        reason = "fmt::Write for String never returns an error"
    )]
    pub(crate) fn credential(&self, secret: &str) -> String {
        let mut credential = String::from("m1.");
        for byte in self.public {
            write!(credential, "{byte:02x}").expect("writing to String cannot fail");
        }
        credential.push('.');
        credential.push_str(secret);
        credential
    }

    pub(crate) fn responder(&self) -> Result<snow::HandshakeState> {
        builder()?
            .local_private_key(&self.private)
            .map_err(crypto_error)?
            .build_responder()
            .map_err(crypto_error)
    }
}

fn builder() -> Result<snow::Builder<'static>> {
    snow::Builder::new(PATTERN.parse().map_err(crypto_error)?)
        .prologue(PROLOGUE)
        .map_err(crypto_error)
}

fn crypto_error(error: impl std::fmt::Display) -> Error {
    Error::Protocol(format!(
        "encrypted gateway channel authentication failed: {error}"
    ))
}

/// Keeps channel failures opaque to the peer while recording why they happened.
fn rejected<E: std::fmt::Display>(context: &'static str) -> impl FnOnce(E) -> Error {
    move |error| {
        tracing::debug!(%error, "{context}");
        Error::Unauthorized
    }
}

/// Decodes one lowercase hexadecimal digit already checked by the caller.
const fn hex_value(digit: u8) -> u8 {
    if digit.is_ascii_digit() {
        digit - b'0'
    } else {
        digit - b'a' + 10
    }
}

pub(crate) fn credential_key(credential: &str) -> Result<[u8; 32]> {
    let invalid = || {
        Error::Config("this WebSocket gateway requires a new pairing code with its encryption identity; pair again".into())
    };
    let (key, secret) = credential
        .strip_prefix("m1.")
        .and_then(|value| value.split_once('.'))
        .ok_or_else(invalid)?;
    if key.len() != 64
        || !matches!(secret.len(), 32 | 64)
        || !key
            .bytes()
            .chain(secret.bytes())
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid());
    }
    let mut public = [0u8; 32];
    for (byte, [high, low]) in public.iter_mut().zip(key.as_bytes().as_chunks::<2>().0) {
        *byte = hex_value(*high) << 4 | hex_value(*low);
    }
    Ok(public)
}

pub(crate) async fn client_handshake<S>(
    websocket: &mut WebSocketStream<S>,
    credential: &str,
) -> Result<snow::TransportState>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let public = credential_key(credential)?;
    let mut handshake = builder()?
        .remote_public_key(&public)
        .map_err(crypto_error)?
        .build_initiator()
        .map_err(crypto_error)?;
    send_handshake(websocket, &mut handshake).await?;
    read_handshake(websocket, &mut handshake).await?;
    handshake.into_transport_mode().map_err(crypto_error)
}

pub(crate) async fn server_handshake<S>(
    websocket: &mut WebSocketStream<S>,
    mut handshake: snow::HandshakeState,
) -> Result<snow::TransportState>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    read_handshake(websocket, &mut handshake).await?;
    send_handshake(websocket, &mut handshake).await?;
    handshake.into_transport_mode().map_err(crypto_error)
}

async fn send_handshake<S>(
    websocket: &mut WebSocketStream<S>,
    handshake: &mut snow::HandshakeState,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut message = [0u8; 48];
    let length = handshake
        .write_message(&[], &mut message)
        .map_err(crypto_error)?;
    let message = message
        .get(..length)
        .ok_or_else(|| Error::Protocol("gateway channel handshake overflowed".into()))?;
    websocket
        .send(Message::Binary(message.to_vec().into()))
        .await
        .map_err(websocket_error)
}

async fn read_handshake<S>(
    websocket: &mut WebSocketStream<S>,
    handshake: &mut snow::HandshakeState,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let message = next_binary(websocket).await?.ok_or(Error::Unauthorized)?;
    if message.len() != 48 {
        return Err(Error::Unauthorized);
    }
    handshake
        .read_message(&message, &mut [])
        .map_err(crypto_error)?;
    Ok(())
}

async fn next_binary<S>(websocket: &mut WebSocketStream<S>) -> Result<Option<Bytes>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        match websocket
            .next()
            .await
            .transpose()
            .map_err(websocket_error)?
        {
            Some(Message::Binary(bytes)) => return Ok(Some(bytes)),
            Some(Message::Ping(_) | Message::Pong(_)) => {}
            None | Some(Message::Close(_)) => return Ok(None),
            _ => {
                return Err(Error::Protocol(
                    "encrypted gateway messages must be binary".into(),
                ));
            }
        }
    }
}

/// Buffers only the first bounded authentication frame, preserving any pipelined bytes.
pub(crate) async fn read_authentication<S>(
    websocket: &mut WebSocketStream<S>,
    state: &mut snow::TransportState,
    limit: usize,
) -> Result<Vec<u8>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut pending = Vec::new();
    loop {
        if let Some(header) = pending.first_chunk::<4>() {
            let length = usize::try_from(u32::from_be_bytes(*header))
                .map_err(rejected("gateway channel frame length is unsupported"))?;
            if length == 0 || length > limit {
                return Err(Error::Unauthorized);
            }
            if pending.len() >= 4 + length {
                return Ok(pending);
            }
        }
        let message = next_binary(websocket).await?.ok_or(Error::Unauthorized)?;
        if message.len() <= TAG_BYTES {
            return Err(Error::Unauthorized);
        }
        let mut plaintext = vec![0; message.len() - TAG_BYTES];
        let length = state
            .read_message(&message, &mut plaintext)
            .map_err(crypto_error)?;
        pending.extend_from_slice(plaintext.get(..length).ok_or(Error::Unauthorized)?);
    }
}

pub(crate) async fn bridge<S>(
    websocket: WebSocketStream<S>,
    state: snow::TransportState,
    stream: DuplexStream,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut outgoing, mut incoming) = websocket.split();
    let (mut reader, mut writer) = tokio::io::split(stream);
    let state = Mutex::new(state);
    let sending = async {
        let mut plaintext = vec![0; MAX_PLAINTEXT];
        let mut ciphertext = vec![0; MAX_RECORD];
        loop {
            let length = if let Ok(read) =
                tokio::time::timeout(KEEPALIVE, reader.read(&mut plaintext)).await
            {
                read?
            } else {
                tokio::time::timeout(
                    WRITE_TIMEOUT,
                    outgoing.send(Message::Ping(Vec::new().into())),
                )
                .await
                .map_err(rejected("gateway channel keepalive timed out"))?
                .map_err(websocket_error)?;
                continue;
            };
            if length == 0 {
                return tokio::time::timeout(WRITE_TIMEOUT, outgoing.close())
                    .await
                    .map_err(rejected("gateway channel close timed out"))?
                    .map_err(websocket_error);
            }
            let plaintext = plaintext.get(..length).ok_or(Error::Unauthorized)?;
            let length = state
                .lock()
                .map_err(rejected("gateway channel state lock is poisoned"))?
                .write_message(plaintext, &mut ciphertext)
                .map_err(crypto_error)?;
            let ciphertext = ciphertext.get(..length).ok_or(Error::Unauthorized)?;
            tokio::time::timeout(
                WRITE_TIMEOUT,
                outgoing.send(Message::Binary(ciphertext.to_vec().into())),
            )
            .await
            .map_err(rejected("gateway channel write timed out"))?
            .map_err(websocket_error)?;
        }
    };
    let receiving = async {
        let mut plaintext = vec![0; MAX_PLAINTEXT];
        while let Some(message) = incoming.next().await {
            match message.map_err(websocket_error)? {
                Message::Binary(ciphertext) if ciphertext.len() > TAG_BYTES => {
                    let length = state
                        .lock()
                        .map_err(rejected("gateway channel state lock is poisoned"))?
                        .read_message(&ciphertext, &mut plaintext)
                        .map_err(crypto_error)?;
                    writer
                        .write_all(plaintext.get(..length).ok_or(Error::Unauthorized)?)
                        .await?;
                }
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Close(_) => break,
                _ => return Err(Error::Unauthorized),
            }
        }
        writer.shutdown().await?;
        Ok(())
    };
    tokio::select! {
        result = sending => result,
        result = receiving => result,
    }
}

#[cfg(test)]
mod tests;
