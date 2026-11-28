//! Gateway-scoped Bot and routine management.

use std::sync::Arc;

mod form;
mod render;
mod runtime;
mod state;

pub(in crate::frontend) use self::runtime::run;

use super::setup::SetupMode;
use mobius_gateway::wire::ClientMessage;

enum Action {
    None,
    Exit,
    OpenSession(mobius::SessionId),
    /// Runs setup for the Bot shown on the current `Page::Bot`.
    Setup(SetupMode),
    Send {
        request_id: Arc<str>,
        message: Box<ClientMessage>,
        label: &'static str,
    },
}

fn request_action(
    label: &'static str,
    make_message: impl FnOnce(Arc<str>) -> ClientMessage,
) -> Action {
    let request_id = crate::fresh_id();
    let message = make_message(Arc::clone(&request_id));
    Action::Send {
        request_id,
        message: Box::new(message),
        label,
    }
}

/// The model a Bot listing names: the möbius route's model, or the CLI harness's model.
pub(super) fn bot_model(bot: &mobius_gateway::wire::BotRecord) -> &str {
    use mobius_gateway::wire::HarnessConfig;
    match &bot.config.config {
        HarnessConfig::Mobius(composition) => &composition.provider.model,
        HarnessConfig::Codex(options) | HarnessConfig::Claude(options) => {
            options.model.as_deref().unwrap_or("default model")
        }
    }
}

/// The harness label and model a Bot listing shows.
pub(super) fn bot_runtime(
    bot: &mobius_gateway::wire::BotRecord,
    gateway: &mobius_gateway::wire::ReadyPayload,
) -> String {
    let harness = gateway
        .harnesses
        .iter()
        .find(|harness| harness.kind == bot.harness)
        .map_or("", |harness| harness.label.as_str());
    format!("{harness} · {}", bot_model(bot))
}

#[cfg(test)]
mod tests;
