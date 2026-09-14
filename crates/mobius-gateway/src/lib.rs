//! Authenticated, frontend-neutral access to independent möbius chats.

mod assembly;
pub mod auth;
pub mod bots;
pub mod client;
mod cloudflare;
pub mod command;
mod computer_runtime;
pub mod config;
mod extensions;
mod host;
mod middleware_manifest;
mod provider_catalog;
mod publication;
pub mod sandbox;
pub mod server;
pub mod wire;

pub use extensions::MAX_EXTENSION_SOURCE_BYTES;

/// Errors returned by the gateway library.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("gateway configuration error: {0}")]
    /// Selects the config case.
    Config(String),
    #[error("gateway protocol error: {0}")]
    /// Selects the protocol case.
    Protocol(String),
    #[error("gateway authentication failed")]
    /// Selects the unauthorized case.
    Unauthorized,
    #[error(transparent)]
    /// Selects the mobius case.
    Mobius(#[from] mobius::Error),
    #[error(transparent)]
    /// Selects the I/O case.
    Io(#[from] std::io::Error),
    #[error(transparent)]
    /// Selects the JSON case.
    Json(#[from] serde_json::Error),
    #[error("Bot storage error")]
    /// Selects the SQLite case.
    Sqlite(#[from] rusqlite::Error),
}

/// Result type shared by gateway modules.
pub type Result<T> = std::result::Result<T, Error>;
