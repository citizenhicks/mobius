//! Authenticated, frontend-neutral access to independent möbius chats.

mod assembly;
pub mod auth;
pub mod client;
mod cloudflare;
pub mod command;
pub mod config;
mod cron;
mod extensions;
mod host;
mod middleware_manifest;
mod provider_catalog;
pub mod sandbox;
pub mod server;
pub mod wire;

/// Errors returned by the gateway library.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("gateway configuration error: {0}")]
    Config(String),
    #[error("gateway protocol error: {0}")]
    Protocol(String),
    #[error("gateway authentication failed")]
    Unauthorized,
    #[error(transparent)]
    Mobius(#[from] mobius::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Result type shared by gateway modules.
pub type Result<T> = std::result::Result<T, Error>;
