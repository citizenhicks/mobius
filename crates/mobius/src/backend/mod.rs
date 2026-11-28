//! Runtime adapters used by the agent loop.

pub mod checkpoint;
pub mod model;
pub mod sandbox;

/// Immutable session files shared by observations, uploads, and artifacts.
pub mod session_files;

/// Bounds one read against the recorded file size and returns the next offset, if any.
fn chunk_bounds(size: u64, offset: u64, limit: usize) -> Option<(usize, Option<u64>)> {
    let remaining = size.checked_sub(offset)?;
    let length = usize::try_from(remaining).unwrap_or(usize::MAX).min(limit);
    let end = offset + length as u64;
    Some((length, (end < size).then_some(end)))
}

/// Classifies a WebSocket failure without exposing URLs, headers, or peer-controlled text.
#[must_use]
pub fn websocket_error_kind(error: &tokio_tungstenite::tungstenite::Error) -> &'static str {
    use tokio_tungstenite::tungstenite::Error as WebSocketError;
    match error {
        WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed => "closed",
        WebSocketError::Io(_) => "I/O",
        WebSocketError::Tls(_) => "TLS",
        WebSocketError::Capacity(_) => "capacity",
        WebSocketError::Protocol(_) => "protocol",
        WebSocketError::WriteBufferFull(_) => "write buffer",
        WebSocketError::Utf8(_) => "UTF-8",
        WebSocketError::AttackAttempt => "attack rejected",
        WebSocketError::Url(_) => "URL",
        WebSocketError::Http(_) => "HTTP handshake",
        WebSocketError::HttpFormat(_) => "HTTP format",
    }
}
