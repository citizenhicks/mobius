//! The möbius framework harness: Bot composition, providers, middleware and extensions.

pub(crate) mod assembly;
pub(crate) mod extensions;
pub(crate) mod middleware_manifest;
pub(crate) mod persistent_chat;
pub(crate) mod provider_catalog;
mod session;

/// Framework records the gateway shares with every harness: submissions, admissions,
/// abort reasons and the default step budget of a möbius composition.
pub(crate) use ::mobius::agent::{
    DEFAULT_MAX_MODEL_STEPS, FRONTEND_DISCONNECTED_REASON, MessageAcceptance,
    TURN_INTERRUPTED_REASON, TURN_RESTARTED_REASON, ValidatedSubmission,
};
pub(crate) use session::{Mobius, RealtimeModel};
