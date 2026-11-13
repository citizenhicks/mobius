//! Gateway-native provider login and Bot setup wizard.

mod runtime;
mod state;
pub(in crate::frontend) mod view;

use mobius::{Error, Result};
use mobius_gateway::client::{GatewayEvents, GatewaySender};
use mobius_gateway::config::{
    MAX_PROVIDER_API_KEY_BYTES as MAX_API_KEY_BYTES, MAX_PROVIDER_LABEL_BYTES,
};
use mobius_gateway::wire::ReadyPayload;

use self::runtime::{apply, apply_gateway, edit};
use self::state::{SaveTarget, SetupState};
use super::terminal::{MAX_ENDPOINT_BYTES, Screen, open_screen};

const MAX_MODEL_IDS_BYTES: usize = 16 * 1024;
const MIN_INLINE_DESCRIPTION_WIDTH: usize = 20;

const UPDATE_BOT_LABEL: &str = "Update this Bot";
const UPDATE_BOT_DESCRIPTION: &str = "Apply these settings to every chat owned by this Bot";
const SAVE_DEFAULT_LABEL: &str = "Save Bot template";
const SAVE_DEFAULT_DESCRIPTION: &str = "Use these settings when creating Bots";

/// The focused setup flow requested by the CLI shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetupMode {
    Login,
    Bot,
    BotModel,
}

/// Edits one durable Bot without requiring an open chat.
pub(in crate::frontend) async fn run_bot(
    terminal: &mut Screen,
    mode: SetupMode,
    preferred_provider: Option<&str>,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    bot_id: &str,
) -> Result<()> {
    let bot = gateway
        .bots
        .iter()
        .find(|bot| *bot.id == *bot_id)
        .ok_or_else(|| Error::Config("the selected Bot is not in the gateway catalog".into()))?;
    let mut state = SetupState::new(
        mode,
        preferred_provider,
        gateway,
        (**runtime::mobius_composition(bot)?).clone(), // rules-allow: setup edits a snapshot while events update the gateway payload
        SaveTarget::Bot,
    )?;
    terminal.clear()?;

    while edit(terminal, &mut state, sender, events, gateway).await? {
        match apply(terminal, &mut state, sender, events, gateway, bot_id).await {
            Ok(()) => return Ok(()),
            Err(error) => state.recover_error(error)?,
        }
    }
    Ok(())
}

/// Runs provider setup without creating or changing a chat.
/// Returns `false` when setup is cancelled.
pub(crate) async fn run_gateway(
    terminal: &mut Screen,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
) -> Result<bool> {
    let state = login_state(gateway)?;
    login(terminal, state, sender, events, gateway).await
}

/// Runs gateway-scoped provider setup before any chat exists.
/// Returns `false` when setup is cancelled.
/// # Errors
///
/// Returns an error if validation or an operation required to complete the request fails.
pub async fn run_gateway_login(
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
) -> Result<bool> {
    let (_guard, mut terminal) = open_screen()?;
    let mut state = login_state(gateway)?;
    state.start_chat = true;
    login(&mut terminal, state, sender, events, gateway).await
}

fn login_state(gateway: &ReadyPayload) -> Result<SetupState> {
    let original = gateway
        .agent_catalog
        .bot_defaults
        .as_ref()
        .map_or_default(|defaults| defaults.config.clone()); // rules-allow: setup edits a snapshot while events update the gateway payload
    SetupState::new(
        SetupMode::Login,
        None,
        gateway,
        original,
        SaveTarget::Template,
    )
}

async fn login(
    terminal: &mut Screen,
    mut state: SetupState,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
) -> Result<bool> {
    terminal.clear()?;
    while edit(terminal, &mut state, sender, events, gateway).await? {
        match apply_gateway(terminal, &mut state, sender, events, gateway).await {
            Ok(()) => return Ok(true),
            Err(error) => state.recover_error(error)?,
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests;
