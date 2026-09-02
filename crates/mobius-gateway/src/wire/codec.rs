use super::*;

/// Cancellation-safe reader for length-prefixed gateway frames.
pub struct FrameReader<R> {
    reader: R,
    buffer: Vec<u8>,
}

impl<R> FrameReader<R> {
    /// Wraps one transport reader and retains partial frames between reads.
    pub const fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: Vec::new(),
        }
    }
}

pub(super) fn deserialize_frame<'de, D>(
    deserializer: D,
) -> std::result::Result<(u16, Value), D::Error>
where
    D: serde::Deserializer<'de>,
{
    let Value::Object(mut object) = Value::deserialize(deserializer)? else {
        return Err(D::Error::custom("gateway frame must be a JSON object"));
    };
    let version = object
        .remove("version")
        .ok_or_else(|| D::Error::missing_field("version"))?;
    let version = serde_json::from_value(version).map_err(D::Error::custom)?;
    Ok((version, Value::Object(object)))
}

/// Reads one length-prefixed JSON value, returning `None` only for a clean EOF.
pub async fn read_frame<T>(reader: &mut FrameReader<impl AsyncRead + Unpin>) -> Result<Option<T>>
where
    T: DeserializeOwned,
{
    read_frame_with_limit(reader, MAX_FRAME_BYTES).await
}

pub(crate) async fn read_frame_with_limit<T>(
    reader: &mut FrameReader<impl AsyncRead + Unpin>,
    max_bytes: usize,
) -> Result<Option<T>>
where
    T: DeserializeOwned,
{
    loop {
        let needed = if reader.buffer.len() >= 4 {
            let prefix = reader.buffer[..4]
                .try_into()
                .map_err(|_| Error::Protocol("frame length is unsupported".into()))?;
            let length = usize::try_from(u32::from_be_bytes(prefix))
                .map_err(|_| Error::Protocol("frame length is unsupported".into()))?;
            if length == 0 || length > max_bytes {
                return Err(Error::Protocol(format!(
                    "frame length must be 1–{max_bytes} bytes"
                )));
            }
            let frame_end = 4 + length;
            if reader.buffer.len() >= frame_end {
                let frame = serde_json::from_slice(&reader.buffer[4..frame_end])?;
                reader.buffer.drain(..frame_end);
                return Ok(Some(frame));
            }
            frame_end - reader.buffer.len()
        } else {
            4 - reader.buffer.len()
        };
        let mut chunk = [0_u8; 8 * 1024];
        let chunk_bytes = needed.min(chunk.len());
        let read = reader.reader.read(&mut chunk[..chunk_bytes]).await?;
        if read == 0 {
            if reader.buffer.is_empty() {
                return Ok(None);
            }
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }
        reader.buffer.extend_from_slice(&chunk[..read]);
    }
}

/// Writes one bounded length-prefixed JSON value.
pub async fn write_frame<T>(writer: &mut (impl AsyncWrite + Unpin), value: &T) -> Result<()>
where
    T: Serialize,
{
    let payload = serde_json::to_vec(value)?;
    if payload.is_empty() || payload.len() > MAX_FRAME_BYTES {
        return Err(Error::Protocol(format!(
            "encoded frame must be 1–{MAX_FRAME_BYTES} bytes"
        )));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| Error::Protocol("encoded frame length is unsupported".into()))?;
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

pub(crate) async fn websocket_to_framed(
    mut incoming: impl Stream<Item = std::result::Result<Message, WebSocketError>> + Unpin,
    mut writer: impl AsyncWrite + Unpin,
) -> Result<()> {
    while let Some(message) = incoming.next().await {
        match message.map_err(websocket_error)? {
            Message::Binary(payload) if (1..=MAX_FRAME_BYTES).contains(&payload.len()) => {
                let length = u32::try_from(payload.len())
                    .map_err(|_| Error::Protocol("WebSocket message is too large".into()))?;
                writer.write_all(&length.to_be_bytes()).await?;
                writer.write_all(&payload).await?;
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => return Ok(()),
            Message::Binary(payload) => {
                return Err(Error::Protocol(format!(
                    "WebSocket message length must be 1–{MAX_FRAME_BYTES} bytes, got {}",
                    payload.len()
                )));
            }
            Message::Text(_) | Message::Frame(_) => {
                return Err(Error::Protocol(
                    "WebSocket messages must be binary JSON frames".into(),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) async fn framed_to_websocket(
    mut reader: impl AsyncRead + Unpin,
    mut outgoing: impl Sink<Message, Error = WebSocketError> + Unpin,
) -> Result<()> {
    loop {
        let mut prefix = [0_u8; 4];
        let first =
            match tokio::time::timeout(WEBSOCKET_KEEPALIVE_INTERVAL, reader.read(&mut prefix[..1]))
                .await
            {
                Ok(read) => read?,
                Err(_) => {
                    outgoing
                        .send(Message::Ping(Vec::new().into()))
                        .await
                        .map_err(websocket_error)?;
                    continue;
                }
            };
        if first == 0 {
            return outgoing.close().await.map_err(websocket_error);
        }
        reader.read_exact(&mut prefix[1..]).await?;
        let length = usize::try_from(u32::from_be_bytes(prefix))
            .map_err(|_| Error::Protocol("frame length is unsupported".into()))?;
        if length == 0 || length > MAX_FRAME_BYTES {
            return Err(Error::Protocol(format!(
                "frame length must be 1–{MAX_FRAME_BYTES} bytes"
            )));
        }
        let mut payload = vec![0_u8; length];
        reader.read_exact(&mut payload).await?;
        outgoing
            .send(Message::Binary(payload.into()))
            .await
            .map_err(websocket_error)?;
    }
}

pub(crate) fn websocket_error(error: WebSocketError) -> Error {
    Error::Protocol(format!("WebSocket transport failed: {error}"))
}

/// Rejects frames from incompatible clients before interpreting their message.
pub fn validate_version(version: u16) -> Result<()> {
    if version != PROTOCOL_VERSION {
        return Err(Error::Protocol(format!(
            "unsupported protocol version {version}; expected {PROTOCOL_VERSION}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_session_id(session_id: &str) -> Result<()> {
    if session_id.trim().is_empty() || session_id.len() > 4 * 1024 {
        return Err(Error::Config("session ID must be 1–4096 bytes".into()));
    }
    Ok(())
}
