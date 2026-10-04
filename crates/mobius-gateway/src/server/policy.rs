//! Operator-selected connection and resident-session capacity.

use crate::Result;

mobius::embedded_config! {
    copy;
/// Resource policy for accepted connections and active chats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionPolicy {
    /// Maximum authenticated connections.
    pub authenticated: usize,
    /// Maximum connections still authenticating.
    pub pre_authentication: usize,
    /// Deadline covering the complete transport and authentication handshake.
    pub authentication_timeout_seconds: u64,
    /// Maximum connected, starting, or running chats.
    pub active_sessions: usize,
    /// Maximum unfinished uploads on each connection.
    pub pending_uploads: usize,
}
    defaults = include_str!("policy.toml");
}

impl ConnectionPolicy {
    /// Checks resource limits before opening listeners.
    /// # Errors
    /// Returns an error for zero limits or values exceeding the process safety budget.
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("authenticated", self.authenticated),
            ("pre_authentication", self.pre_authentication),
            ("active_sessions", self.active_sessions),
            ("pending_uploads", self.pending_uploads),
        ] {
            crate::config::bounded(
                &format!("connections.{name}"),
                value,
                1..=crate::config::MAX_CAPACITY,
            )?;
        }
        crate::config::bounded(
            "connections.authentication_timeout_seconds",
            self.authentication_timeout_seconds,
            1..=3_600,
        )?;
        Ok(())
    }

    pub(super) fn total(&self) -> usize {
        self.authenticated.saturating_add(self.pre_authentication)
    }
}
