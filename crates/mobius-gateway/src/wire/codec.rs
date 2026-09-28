use super::*;

/// An immutable session frame shared by replay and live subscribers.
#[derive(Debug, Clone)]
pub(crate) struct SharedFrame {
    frame: std::sync::Arc<ServerFrame>,
    payload: Option<std::sync::Arc<[u8]>>,
}

impl SharedFrame {
    pub(crate) fn new(frame: ServerFrame) -> Self {
        Self {
            frame: std::sync::Arc::new(frame),
            payload: None,
        }
    }

    pub(crate) fn encoded(frame: ServerFrame) -> Result<Self> {
        let payload = serde_json::to_vec(&frame)?;
        if payload.len() > MAX_FRAME_BYTES {
            return Err(Error::Protocol(format!(
                "agent event exceeds the {MAX_FRAME_BYTES}-byte gateway frame limit"
            )));
        }
        Ok(Self {
            frame: std::sync::Arc::new(frame),
            payload: Some(payload.into()),
        })
    }

    pub(crate) fn encoded_len(&self) -> usize {
        self.payload
            .as_ref()
            .expect("replay frames are encoded")
            .len()
    }

    pub(crate) async fn write(&self, writer: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
        match &self.payload {
            Some(payload) => write_payload(writer, payload).await,
            None => write_frame(writer, &*self.frame).await,
        }
    }
}

impl std::ops::Deref for SharedFrame {
    type Target = ServerFrame;

    fn deref(&self) -> &Self::Target {
        &self.frame
    }
}

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
/// # Errors
///
/// Returns an error if the resource cannot be read, decoded, or validated.
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
                reader.buffer.clear();
                // Reuse ordinary frames without pinning bulk-transfer memory for idle clients.
                if reader.buffer.capacity() > 64 * 1024 {
                    reader.buffer = Vec::new();
                }
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
/// Discard the connection after a write error: the frame may be partially written.
/// # Errors
///
/// Returns an error if the value cannot be encoded or persisted.
pub async fn write_frame<T>(writer: &mut (impl AsyncWrite + Unpin), value: &T) -> Result<()>
where
    T: Serialize,
{
    let payload = serde_json::to_vec(value)?;
    write_payload(writer, &payload).await
}

pub(crate) async fn write_payload(
    writer: &mut (impl AsyncWrite + Unpin),
    payload: &[u8],
) -> Result<()> {
    if payload.is_empty() || payload.len() > MAX_FRAME_BYTES {
        return Err(Error::Protocol(format!(
            "encoded frame must be 1–{MAX_FRAME_BYTES} bytes"
        )));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| Error::Protocol("encoded frame length is unsupported".into()))?;
    tokio::time::timeout(WRITE_TIMEOUT, async {
        writer.write_all(&length.to_be_bytes()).await?;
        writer.write_all(payload).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
    Ok(())
}

pub(crate) fn websocket_error(error: WebSocketError) -> Error {
    let kind = match error {
        WebSocketError::Io(error) => {
            return Error::Protocol(format!("WebSocket I/O failure: {:?}", error.kind()));
        }
        WebSocketError::Http(response) => {
            return Error::Protocol(format!(
                "WebSocket handshake failed: HTTP {}",
                response.status().as_u16()
            ));
        }
        WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed => "closed",
        WebSocketError::Tls(_) => "TLS",
        WebSocketError::Capacity(_) => "capacity",
        WebSocketError::Protocol(_) => "protocol",
        WebSocketError::WriteBufferFull(_) => "write buffer",
        WebSocketError::Utf8(_) => "UTF-8",
        WebSocketError::AttackAttempt => "attack rejected",
        WebSocketError::Url(_) => "URL",
        WebSocketError::HttpFormat(_) => "HTTP format",
    };
    Error::Protocol(format!("WebSocket {kind} failure"))
}

/// Rejects frames from incompatible clients before interpreting their message.
/// # Errors
///
/// Returns an error if the supplied value is invalid.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completed_bulk_frames_do_not_pin_peak_memory_on_idle_connections() {
        let payload = "x".repeat(MAX_FRAME_BYTES - 2);
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &payload).await.unwrap();
        write_frame(&mut bytes, &"next").await.unwrap();
        let mut reader = FrameReader::new(bytes.as_slice());
        assert_eq!(
            read_frame::<String>(&mut reader).await.unwrap(),
            Some(payload)
        );
        eprintln!(
            "frame-reader retained bytes after 50 MiB frame: {}",
            reader.buffer.capacity()
        );
        assert!(reader.buffer.capacity() <= 64 * 1024);
        assert_eq!(
            read_frame::<String>(&mut reader).await.unwrap().as_deref(),
            Some("next")
        );
    }
}
