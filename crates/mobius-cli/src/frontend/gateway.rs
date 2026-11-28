//! Gateway account management and gateway-wide preferences.
mod accounts;
mod routing;
pub(crate) use accounts::run;
pub(super) use routing::{dictation, new_chats};
