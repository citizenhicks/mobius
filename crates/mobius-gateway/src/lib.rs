//! Authenticated, frontend-neutral access to independent möbius chats.

pub mod auth;
pub mod bots;
mod channel;
pub mod client;
mod cloudflare;
pub mod command;
pub mod computer_runtime;
pub mod config;
/// Shared saved gateway accounts and endpoint selection.
pub mod gateway_accounts;
mod git;
mod harness;
mod host;
mod hostnames;
mod process_environment;
mod publication;
pub mod sandbox;
pub mod server;
pub mod storage_usage;
pub mod telemetry;
pub mod wire;

pub use harness::mobius::extensions::MAX_EXTENSION_SOURCE_BYTES;
/// The transcript reducer, for frontends that build pages from scripted events in tests.
#[cfg(feature = "fixtures")]
pub use host::TranscriptReducer;

/// A nonempty field limited in bytes and free of control characters.
/// The caller decides whether whitespace is significant or must be canonical.
fn single_line_field(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

/// Errors returned by the gateway library.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A request, setting or input the caller can fix; the message says what to change.
    #[error("{0}")]
    Invalid(String),
    /// A failure the caller cannot fix: a broken invariant, damaged saved state or failed system call.
    #[error("gateway internal error: {0}")]
    Internal(String),
    #[error("gateway protocol error: {0}")]
    /// Selects the protocol case.
    Protocol(String),
    #[error("gateway protocol error: WebSocket handshake failed: HTTP {status}")]
    /// A rejected WebSocket upgrade, retaining only safe HTTP retry metadata.
    WebSocketUpgrade {
        /// HTTP status returned by the gateway service.
        status: u16,
        /// A single positive delta-seconds Retry-After value, when valid.
        retry_after: Option<std::time::Duration>,
    },
    #[error("{0}")]
    /// The Bot's harness cannot start chats on this gateway.
    HarnessUnavailable(String),
    #[error("gateway authentication failed")]
    /// Selects the unauthorized case.
    Unauthorized,
    #[error(transparent)]
    /// Selects the mobius case.
    Mobius(#[from] mobius::Error),
    #[error(transparent)]
    /// Selects the I/O case.
    Io(#[from] std::io::Error),
    #[error("state replacement was applied, but completion could not be confirmed: {source}")]
    /// The destination changed; callers must not restore stale memory or blindly retry.
    PublicationApplied {
        /// Failure after the replacement became visible, such as parent-directory synchronization.
        #[source]
        source: Box<Self>,
    },
    #[error(transparent)]
    /// Selects the JSON case.
    Json(#[from] serde_json::Error),
    #[error("Bot storage error")]
    /// Selects the SQLite case.
    Sqlite(#[from] rusqlite::Error),
}

impl Error {
    /// Why a model provider call failed, when this error carries one.
    #[must_use]
    pub fn failure(&self) -> Option<mobius::FailureClass> {
        match self {
            Self::Mobius(error) => error.failure(),
            _ => None,
        }
    }
}

/// Result type shared by gateway modules.
pub type Result<T> = std::result::Result<T, Error>;

/// Maps a poisoned standard-library lock to an error naming that lock.
pub(crate) fn poisoned<T>(lock: &'static str) -> impl FnOnce(std::sync::PoisonError<T>) -> Error {
    move |error| Error::Internal(format!("{lock} lock is poisoned: {error}"))
}
