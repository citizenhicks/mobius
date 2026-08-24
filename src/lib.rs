//! A small, modular Rust framework for one linear agent session.
//!
//! Applications compose an [`agent::Agent`] from explicit model, sandbox, checkpoint, and
//! middleware adapters. Frontends remain separate: they submit [`protocol::Op`] values and
//! render the frontend-neutral [`protocol::Event`] stream.

use std::future::Future;
use std::pin::Pin;

pub mod agent;
pub mod backend;
pub mod middleware;
pub mod protocol;

/// A boxed asynchronous operation used by runtime-pluggable interfaces.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A model-provider failure with retry metadata preserved for callers.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ProviderError {
    message: String,
    status: Option<u16>,
    retryable: bool,
    retry_after: Option<String>,
    kind: ProviderErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderErrorKind {
    Other,
    StreamInterrupted,
}

impl ProviderError {
    /// Creates a non-retryable provider failure without an HTTP response.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
            retryable: false,
            retry_after: None,
            kind: ProviderErrorKind::Other,
        }
    }

    /// Creates a retryable provider failure without an HTTP response.
    #[must_use]
    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            retryable: true,
            ..Self::new(message)
        }
    }

    /// Creates a retryable response-stream interruption without exposing transport details.
    #[must_use]
    pub fn stream_interrupted(retry_after: Option<String>) -> Self {
        Self {
            message: "model response stream was interrupted".into(),
            status: None,
            retryable: true,
            retry_after,
            kind: ProviderErrorKind::StreamInterrupted,
        }
    }

    pub(crate) fn http(
        message: impl Into<String>,
        status: u16,
        retry_after: Option<String>,
    ) -> Self {
        Self {
            message: message.into(),
            status: Some(status),
            retryable: status == 408 || status == 429 || (500..=599).contains(&status),
            retry_after,
            kind: ProviderErrorKind::Other,
        }
    }

    /// Returns the provider's HTTP status code, when one was received.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        self.status
    }

    /// Reports whether retrying the operation is normally safe.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.retryable
    }

    /// Reports whether a response ended before its completion record arrived.
    #[must_use]
    pub fn is_stream_interrupted(&self) -> bool {
        self.kind == ProviderErrorKind::StreamInterrupted
    }

    /// Returns the provider's raw `Retry-After` header value.
    #[must_use]
    pub fn retry_after(&self) -> Option<&str> {
        self.retry_after.as_deref()
    }
}

impl From<String> for ProviderError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for ProviderError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

/// Errors returned by möbius modules.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("duplicate registration: {0}")]
    Duplicate(String),
    #[error("unknown registration: {0}")]
    Unknown(String),
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("authentication error: {0}")]
    Auth(String),
    #[error("sandbox rejected path: {0}")]
    Sandbox(String),
    #[error("tool error: {0}")]
    Tool(String),
    #[error("checkpoint error: {0}")]
    Checkpoint(String),
    #[error("agent busy: {0}")]
    Busy(String),
    #[error("agent stopped: {0}")]
    Stopped(String),
    #[error("{primary}; rollback failed: {rollback}")]
    Rollback {
        primary: Box<Error>,
        rollback: Box<Error>,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("checkpoint storage error")]
    Sqlite(
        #[source]
        #[from]
        rusqlite::Error,
    ),
}

/// Result type shared by möbius modules.
pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn preview_json(value: &serde_json::Value) -> String {
    let value = value.to_string();
    if value.len() <= 10_000 {
        return value;
    }
    format!("{}…", truncate_utf8(&value, 10_000))
}

pub(crate) fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_errors_do_not_expose_engine_messages() {
        let error = Error::from(rusqlite::Error::InvalidQuery);

        assert_eq!(error.to_string(), "checkpoint storage error");
    }
}
