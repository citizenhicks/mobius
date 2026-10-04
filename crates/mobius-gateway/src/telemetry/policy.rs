//! Operator-selected telemetry transport policy.

use crate::Result;

mobius::embedded_config! {
    copy;
/// HTTP policy for explicitly configured collectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelemetryPolicy {
    /// Permit cleartext HTTP beyond loopback for an operator-managed private network.
    pub allow_insecure_http: bool,
    /// Deadline for each collector HTTP request.
    pub request_timeout_seconds: u64,
    /// Deadline for the full fresh measurement and upload admission operation.
    pub upload_admission_timeout_seconds: u64,
}
    defaults = include_str!("policy.toml");
}
impl TelemetryPolicy {
    /// Validates telemetry deadlines.
    /// # Errors
    /// Returns an error for zero or unbounded request deadlines.
    pub fn validate(&self) -> Result<()> {
        crate::config::bounded(
            "telemetry.request_timeout_seconds",
            self.request_timeout_seconds,
            1..=3_600,
        )?;
        crate::config::bounded(
            "telemetry.upload_admission_timeout_seconds",
            self.upload_admission_timeout_seconds,
            1..=3_600,
        )?;
        Ok(())
    }
}
