//! Pairing policy selected locally by the operator.

use crate::Result;

mobius::embedded_config! {
    copy;
/// Limits for paired devices and one-use pairing credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthConfig {
    /// Maximum paired clients, including the local client credential.
    pub paired_clients: usize,
    /// Lifetime of each newly issued pairing code.
    pub pairing_lifetime_seconds: u64,
}
    defaults = include_str!("defaults.toml");
}
impl AuthConfig {
    /// Validates authentication capacity and credential lifetime.
    /// # Errors
    /// Returns an error if a value is zero or outside the bounded authentication budget.
    pub fn validate(&self) -> Result<()> {
        crate::config::bounded(
            "auth.paired_clients",
            self.paired_clients,
            1..=crate::config::MAX_CAPACITY,
        )?;
        crate::config::bounded(
            "auth.pairing_lifetime_seconds",
            self.pairing_lifetime_seconds,
            1..=86_400,
        )?;
        Ok(())
    }
}
