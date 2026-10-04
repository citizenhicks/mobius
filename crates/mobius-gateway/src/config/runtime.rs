//! Host-independent lifecycle settings selected by the gateway operator.

use crate::{Error, Result};

mobius::embedded_config! {
    copy;
/// Lifecycle and optional private ingress for one gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// Exit after this many idle seconds. Zero disables idle shutdown.
    pub idle_exit_seconds: u64,
    /// Informational content allowance; its configured collector decides admission.
    pub storage_limit_bytes: Option<u64>,
    /// Additional Noise-only WebSocket listener.
    pub ingress: Option<std::net::SocketAddr>,
    /// Refuse startup without `MOBIUS_GATEWAY_ACCESS_EXPIRES_AT`.
    pub require_access_lease: bool,
    /// Explicit grace added to an operator-supplied access expiry.
    pub access_grace_seconds: u64,
}
    defaults = include_str!("runtime.toml");
}

impl RuntimeConfig {
    pub(super) fn validate(&self) -> Result<()> {
        super::bounded(
            "runtime.idle_exit_seconds",
            self.idle_exit_seconds,
            0..=31_536_000,
        )?;
        super::bounded(
            "runtime.access_grace_seconds",
            self.access_grace_seconds,
            0..=86_400,
        )?;
        if self
            .storage_limit_bytes
            .is_some_and(|limit| limit < 64 * 1024 * 1024)
        {
            return Err(Error::Config(
                "storage_limit_bytes must be at least 64 MiB".into(),
            ));
        }
        Ok(())
    }
}
