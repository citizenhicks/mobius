//! Gateway-native provider login and agent setup wizard.

mod runtime;
mod state;
mod view;

use std::io;

use mobius::{Error, Result};
use mobius_gateway::client::{GatewayEvents, GatewaySender};
use mobius_gateway::wire::{ReadyPayload, SessionReadyPayload};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use self::runtime::{apply, apply_gateway, edit};
use self::state::SetupState;

const MAX_API_KEY_BYTES: usize = 16 * 1024;
const MAX_ENDPOINT_BYTES: usize = 4 * 1024;
// Matches the gateway's provider label bound.
const MAX_PROVIDER_LABEL_BYTES: usize = 128;
const MAX_MODEL_IDS_BYTES: usize = 16 * 1024;
const MIN_INLINE_DESCRIPTION_WIDTH: usize = 20;

const CHANGE_CHAT_LABEL: &str = "Change for this chat only";
const CHANGE_CHAT_DESCRIPTION: &str = "Restart the active chat without changing future chats";
const SAVE_DEFAULT_LABEL: &str = "Save as default";
const SAVE_DEFAULT_DESCRIPTION: &str = "Use these settings for future chats only";

type SetupTerminal = Terminal<CrosstermBackend<io::Stdout>>;

/// The focused setup flow requested by the CLI shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetupMode {
    Login,
    Agent,
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
    let mut state = SetupState::new(
        mode,
        preferred_provider,
        gateway,
        session.config.config.clone(),
        false,
    )?;
    terminal.clear()?;

    if !edit(terminal, &mut state, sender, events, gateway).await? {
        return Ok(());
    }
    apply(terminal, &mut state, sender, events, gateway, session).await?;
    Ok(())
}

/// Runs provider or default-agent setup without creating or changing a chat.
pub(crate) async fn run_gateway(
    terminal: &mut SetupTerminal,
    mode: SetupMode,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
) -> Result<()> {
    let original = gateway
        .default_config
        .as_ref()
        .map(|default| default.config.clone())
        .unwrap_or_default();
    if mode == SetupMode::Agent && gateway.default_config.is_none() {
        return Err(Error::Config(
            "configure a provider before changing gateway defaults".into(),
        ));
    }
    let mut state = SetupState::new(mode, None, gateway, original, true)?;
    terminal.clear()?;
    if !edit(terminal, &mut state, sender, events, gateway).await? {
        return Ok(());
    }
    apply_gateway(terminal, &mut state, sender, events, gateway).await
}

#[cfg(test)]
mod tests;
