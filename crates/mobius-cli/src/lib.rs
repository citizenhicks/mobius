//! Shared terminal frontends and local gateway-account storage.

pub mod command;
pub mod frontend;
pub mod gateway_accounts;

/// Converts a gateway transport/protocol failure without losing its diagnostic text.
pub fn gateway_error(error: mobius_gateway::Error) -> mobius::Error {
    mobius::Error::Stopped(error.to_string())
}
