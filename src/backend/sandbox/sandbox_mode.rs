//! Isolation modes a sandbox backend enforces.

use serde::Deserialize;
use serde::Serialize;

/// Deny-by-default macOS Seatbelt prelude shared by first-party sandbox backends.
///
/// Backends must append their own filesystem allow rules before using it.
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub const MACOS_SEATBELT_BASE_POLICY: &str = include_str!("seatbelt_base_policy.sbpl");

/// macOS platform services required by commands with approved network access.
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub const MACOS_SEATBELT_NETWORK_POLICY: &str = include_str!("seatbelt_network_policy.sbpl");

/// Whether a sandbox backend permits network access for one command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkAccess {
    #[default]
    /// Selects the denied case.
    Denied,
    /// Selects the allowed case.
    Allowed,
}

/// Whether an operation uses workspace isolation or host-wide access.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    #[default]
    /// Selects the workspace write case.
    WorkspaceWrite,
    /// Selects the danger full access case.
    DangerFullAccess,
}

/// How Linux commands see `/proc` while retaining user and PID namespace isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcfsMode {
    /// Mounts a private proc filesystem for the command's PID namespace.
    Private,
    /// Masks `/proc` with an empty filesystem.
    ///
    /// Select this explicitly only when the host already provides PID isolation.
    Empty,
}

impl Default for ProcfsMode {
    fn default() -> Self {
        super::text::DEFINITION.defaults_procfs_mode
    }
}
