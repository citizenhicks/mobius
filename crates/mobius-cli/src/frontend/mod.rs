//! möbius terminal frontend.

use mobius::Result;
use mobius::protocol::FrontendBlock;
use mobius_gateway::client::{GatewayEvents, GatewaySender};
use mobius_gateway::wire::{ProviderInstance, ReadyPayload, SessionReadyPayload};

mod bots;
mod catalog;
mod cloudflare_setup;
mod dashboard;
mod extensions;
mod gateway;
mod gateway_actions;
mod headless;
mod reinitialize;
mod setup;
mod terminal;
mod theme;
mod tui;

pub use headless::run as run_headless;
pub use terminal::terminal_text;

pub use cloudflare_setup::{CloudflareInit, run as run_cloudflare_setup};
pub use dashboard::{run as run_gateway_dashboard, run_provider as run_gateway_provider};
pub use reinitialize::confirm as confirm_gateway_reinitialize;
pub use setup::run_gateway_login;

pub async fn run_extensions(
    sender: GatewaySender,
    events: GatewayEvents,
    gateway: ReadyPayload,
) -> Result<()> {
    extensions::standalone(sender, events, gateway).await
}

fn block_text(block: &FrontendBlock) -> String {
    let mut text = block.title.clone();
    if !block.text.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&block.text);
    }
    for file in &block.files {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!(
            "[file] {} · {} · {} bytes",
            file.name, file.media_type, file.size
        ));
    }
    text
}

fn provider_instance_label<'a>(
    instances: &'a [ProviderInstance],
    instance: &str,
) -> Option<&'a str> {
    instances
        .iter()
        .find(|entry| entry.selection.instance == instance)
        .map(|entry| entry.label.as_str())
}

pub async fn run(
    sender: GatewaySender,
    events: GatewayEvents,
    gateway: &mut ReadyPayload,
    session: &mut SessionReadyPayload,
    local_gateway: bool,
    gateway_endpoint: String,
) -> Result<(FrontendExit, GatewaySender, GatewayEvents)> {
    let workspace = session.workspace.path.clone();
    let catalog = catalog::UiCatalog::build(&session.contributions, &workspace)?;
    tui::runtime::run(
        sender,
        events,
        gateway,
        session,
        catalog,
        local_gateway,
        gateway_endpoint,
    )
    .await
}

/// Why a frontend returned control to its launcher.
pub enum FrontendExit {
    Exit,
    Resume(String),
    Reload,
    Reconnect,
}
