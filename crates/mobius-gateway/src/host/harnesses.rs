//! Harness options and on-demand detection.

use super::gateway_host::GatewayHost;
use super::rejection::Rejection;

use crate::wire::ReadyPayload;
use crate::wire::{CliHarnessConfig, HarnessKind};

impl GatewayHost {
    /// Saves one CLI harness's executable override and new-Bot defaults. No CLI harness has an
    /// adapter on this gateway, so every request is rejected.
    pub(crate) async fn configure_harness(
        &self,
        kind: HarnessKind,
        _cli_path: Option<std::path::PathBuf>,
        _defaults: CliHarnessConfig,
    ) -> std::result::Result<ReadyPayload, Rejection> {
        let _mutation = self.begin_mutation().await?;
        Err(match kind {
            HarnessKind::Mobius => Rejection::new(
                "invalid_config",
                "möbius is configured through its providers and Bot defaults",
            ),
            kind => super::harness_unavailable(crate::harness::unavailable(kind)),
        })
    }

    /// Sends every client the harnesses again.
    pub(crate) async fn refresh_harnesses(&self) -> std::result::Result<ReadyPayload, Rejection> {
        self.publish_ready_after_unlock(self.state.lock().await)
            .await
    }
}
