//! Pinned Noise channels inside WebSockets; relay-visible records contain no gateway JSON.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use snow::resolvers::{CryptoResolver as _, DefaultResolver};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, DuplexStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

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
        let private: [u8; 32] = std::fs::read(&path)?
            .try_into()
            .map_err(|_| Error::Config("gateway channel key changed while reading".into()))?;
        let mut dh = DefaultResolver
            .resolve_dh(&snow::params::DHChoice::Curve25519)
            .ok_or_else(|| Error::Config("gateway channel key agreement is unavailable".into()))?;
        dh.set(&private);
        let public = dh.pubkey().try_into().map_err(|_| Error::Unauthorized)?;
        Ok(Self { private, public })
    }

    pub(crate) fn credential(&self, secret: &str) -> String {
        let key: String = self
            .public
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("m1.{key}.{secret}")
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

fn crypto_error(_: snow::Error) -> Error {
    Error::Protocol("encrypted gateway channel authentication failed".into())
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
    for (byte, hex) in public.iter_mut().zip(key.as_bytes().as_chunks::<2>().0) {
        let text = std::str::from_utf8(hex).map_err(|_| invalid())?;
        *byte = u8::from_str_radix(text, 16).map_err(|_| invalid())?;
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
    websocket
        .send(Message::Binary(message[..length].to_vec().into()))
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

async fn next_binary<S>(websocket: &mut WebSocketStream<S>) -> Result<Option<Vec<u8>>>
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
            Some(Message::Binary(bytes)) => return Ok(Some(bytes.to_vec())),
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
        if pending.len() >= 4 {
            let length =
                u32::from_be_bytes(pending[..4].try_into().map_err(|_| Error::Unauthorized)?)
                    as usize;
            if length == 0 || length > limit {
                return Err(Error::Unauthorized);
            }
            if pending.len() >= 4 + length {
                return Ok(pending);
            }
        }
        let message = next_binary(websocket).await?.ok_or(Error::Unauthorized)?;
        if message.len() <= TAG_BYTES || message.len() > limit + 4 + TAG_BYTES {
            return Err(Error::Unauthorized);
        }
        let mut plaintext = vec![0; message.len() - TAG_BYTES];
        let length = state
            .read_message(&message, &mut plaintext)
            .map_err(crypto_error)?;
        pending.extend_from_slice(&plaintext[..length]);
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
            let length = match tokio::time::timeout(KEEPALIVE, reader.read(&mut plaintext)).await {
                Ok(read) => read?,
                Err(_) => {
                    tokio::time::timeout(
                        WRITE_TIMEOUT,
                        outgoing.send(Message::Ping(Vec::new().into())),
                    )
                    .await
                    .map_err(|_| Error::Unauthorized)?
                    .map_err(websocket_error)?;
                    continue;
                }
            };
            if length == 0 {
                return tokio::time::timeout(WRITE_TIMEOUT, outgoing.close())
                    .await
                    .map_err(|_| Error::Unauthorized)?
                    .map_err(websocket_error);
            }
            let length = state
                .lock()
                .map_err(|_| Error::Unauthorized)?
                .write_message(&plaintext[..length], &mut ciphertext)
                .map_err(crypto_error)?;
            tokio::time::timeout(
                WRITE_TIMEOUT,
                outgoing.send(Message::Binary(ciphertext[..length].to_vec().into())),
            )
            .await
            .map_err(|_| Error::Unauthorized)?
            .map_err(websocket_error)?;
        }
    };
    let receiving = async {
        let mut plaintext = vec![0; MAX_PLAINTEXT];
        while let Some(message) = incoming.next().await {
            match message.map_err(websocket_error)? {
                Message::Binary(ciphertext)
                    if (TAG_BYTES + 1..=MAX_RECORD).contains(&ciphertext.len()) =>
                {
                    let length = state
                        .lock()
                        .map_err(|_| Error::Unauthorized)?
                        .read_message(&ciphertext, &mut plaintext)
                        .map_err(crypto_error)?;
                    writer.write_all(&plaintext[..length]).await?;
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
