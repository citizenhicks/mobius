//! Gateway-native provider login and Bot setup wizard.

mod runtime;
mod state;
pub(in crate::frontend) mod view;

use std::io;

use mobius::{Error, Result};
use mobius_gateway::client::{GatewayEvents, GatewaySender};
use mobius_gateway::wire::{ReadyPayload, SessionReadyPayload};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use self::runtime::{apply, apply_gateway, edit};
use self::state::SetupState;
use super::terminal::TerminalGuard;

const MAX_API_KEY_BYTES: usize = 16 * 1024;
const MAX_ENDPOINT_BYTES: usize = 4 * 1024;
// Matches the gateway's provider label bound.
const MAX_PROVIDER_LABEL_BYTES: usize = 128;
const MAX_MODEL_IDS_BYTES: usize = 16 * 1024;
const MIN_INLINE_DESCRIPTION_WIDTH: usize = 20;

const UPDATE_BOT_LABEL: &str = "Update this Bot";
const UPDATE_BOT_DESCRIPTION: &str = "Apply these settings to every chat owned by this Bot";
const SAVE_DEFAULT_LABEL: &str = "Save Bot template";
const SAVE_DEFAULT_DESCRIPTION: &str = "Use these settings when creating Bots";

pub(in crate::frontend) type SetupTerminal = Terminal<CrosstermBackend<io::Stdout>>;

/// The focused setup flow requested by the CLI shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetupMode {
    Login,
    Bot,
    BotModel,
}

/// Runs one gateway-backed setup flow and updates its machine and chat snapshots.
pub(crate) async fn run(
    terminal: &mut SetupTerminal,
    mode: SetupMode,
    preferred_provider: Option<&str>,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    session: &mut SessionReadyPayload,
) -> Result<()> {
    run_bot(
        terminal,
        mode,
        preferred_provider,
        sender,
        events,
        gateway,
        &session.session.context.bot_id,
    )
    .await
}

/// Edits one durable Bot without requiring an open chat.
pub(in crate::frontend) async fn run_bot(
    terminal: &mut SetupTerminal,
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
        .find(|bot| bot.id == bot_id)
        .ok_or_else(|| Error::Config("the selected Bot is not in the gateway catalog".into()))?;
    let mut state = SetupState::new(
        mode,
        preferred_provider,
        gateway,
        bot.config.config.clone(),
        false,
    )?;
    terminal.clear()?;

    if !edit(terminal, &mut state, sender, events, gateway).await? {
        return Ok(());
    }
    apply(terminal, &mut state, sender, events, gateway, bot_id).await?;
    Ok(())
}

/// Runs provider or Bot-template setup without creating or changing a chat.
pub(crate) async fn run_gateway(
    terminal: &mut SetupTerminal,
    mode: SetupMode,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
) -> Result<()> {
    let original = gateway
        .bot_defaults
        .as_ref()
        .map(|default| default.config.clone())
        .unwrap_or_default();
    if mode == SetupMode::Bot && gateway.bot_defaults.is_none() {
        return Err(Error::Config(
            "configure a provider before changing the Bot template".into(),
        ));
    }
    let mut state = SetupState::new(mode, None, gateway, original, true)?;
    terminal.clear()?;
    if !edit(terminal, &mut state, sender, events, gateway).await? {
        return Ok(());
    }
    apply_gateway(terminal, &mut state, sender, events, gateway).await
}

/// Runs gateway-scoped provider setup before any chat exists.
pub async fn run_gateway_login(
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
) -> Result<()> {
    let mut guard = TerminalGuard::alternate()?;
    guard.set_mouse_capture(false)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    run_gateway(&mut terminal, SetupMode::Login, sender, events, gateway).await
}

#[cfg(test)]
mod tests;
