//! Validated Bot compositions and submission/workspace boundaries.

use super::rejection::{
    Rejection, harness_unavailable, internal, invalid_bot, invalid_config, invalid_workspace,
};
use super::state::GatewayState;

use crate::config::GatewayConfig;
use crate::harness::ValidatedSubmission;
use crate::harness::mobius::extensions::ExtensionStore;
use crate::harness::mobius::provider_catalog::configured_model_choices;
use crate::wire::{AgentComposition, HarnessConfig};
use mobius::protocol::Submission;
use std::path::Path;
/// A Bot-defaults composition checked against the gateway's catalog and installed extensions.
#[derive(Debug)]
pub(crate) struct ValidatedComposition {
    config: AgentComposition,
}

impl ValidatedComposition {
    pub(crate) fn into_inner(self) -> AgentComposition {
        self.config
    }
}

impl std::ops::Deref for ValidatedComposition {
    type Target = AgentComposition;

    fn deref(&self) -> &AgentComposition {
        &self.config
    }
}

/// Checks a Bot configuration: a möbius composition against the gateway, others against
/// their harness.
pub(super) fn validate_bot_config(
    state: &GatewayState,
    config: HarnessConfig,
) -> std::result::Result<crate::harness::ValidatedConfig, Rejection> {
    match config {
        HarnessConfig::Mobius(composition) => {
            let settings = check_composition(state, &*state.config()?, &composition)?;
            Ok(crate::harness::ValidatedConfig::mobius(
                composition,
                settings,
            ))
        }
        config => crate::harness::validate_cli(config).map_err(harness_unavailable),
    }
}

/// Checks a Bot or Bot-defaults composition against the gateway.
pub(super) fn validate_composition(
    state: &GatewayState,
    gateway: &GatewayConfig,
    config: AgentComposition,
) -> std::result::Result<ValidatedComposition, Rejection> {
    check_composition(state, gateway, &config)?;
    Ok(ValidatedComposition { config })
}

/// The middleware settings of a composition the gateway's catalog and extensions support.
pub(super) fn check_composition(
    state: &GatewayState,
    gateway: &GatewayConfig,
    config: &AgentComposition,
) -> std::result::Result<crate::harness::mobius::middleware_manifest::MobiusSettings, Rejection> {
    let models =
        configured_model_choices(gateway, &state.store, &state.credentials).map_err(internal)?;
    let settings = crate::config::validate_bot_compatibility(gateway, config, models.catalogs())
        .map_err(invalid_config)?;
    ExtensionStore::new(&state.store)
        .resolve(gateway, &config.extensions)
        .map_err(invalid_config)?;
    Ok(settings)
}

pub(super) fn validate_bot_workspace(
    state: &GatewayState,
    bot_id: &str,
    workspace: &Path,
) -> std::result::Result<crate::config::CanonicalWorkspace, Rejection> {
    state.bots.require_bot(bot_id).map_err(invalid_bot)?;
    crate::config::validate_chat_workspace(
        workspace,
        state.store.state_dir(),
        state.config()?.tls.as_ref(),
    )
    .map_err(invalid_workspace)
}

/// Checks a client submission once, at the gateway boundary.
pub(crate) fn validated(
    submission: Submission,
) -> std::result::Result<ValidatedSubmission, Rejection> {
    ValidatedSubmission::new(submission)
        .map_err(|error| Rejection::new("invalid_submission", error.to_string()))
}
