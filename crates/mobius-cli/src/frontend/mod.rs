//! möbius terminal frontend.

use std::fmt::Write as _;

use mobius::Result;
use mobius::protocol::FrontendBlock;
use mobius_gateway::client::{GatewayEvents, GatewaySender};
use mobius_gateway::wire::{ProviderInstance, ReadyPayload, SessionReadyPayload};

mod bots;
mod catalog;
mod cloudflare_setup;
mod dashboard;
mod extensions;
#[cfg(test)]
mod fixtures;
mod gateway;
mod gateway_actions;
mod headless;
mod reinitialize;
mod response;
pub(crate) use response::request;
pub use response::{await_response, wait_ready};
mod setup;
mod terminal;
mod theme;
mod tui;

pub use headless::run as run_headless;
pub use terminal::terminal_text;
pub(crate) use tui::runtime::compose_new_chat;

pub use cloudflare_setup::{CloudflareInit, run as run_cloudflare_setup};
pub use dashboard::{run as run_gateway_dashboard, run_provider as run_gateway_provider};
pub use extensions::standalone as run_extensions;
pub use reinitialize::confirm as confirm_gateway_reinitialize;
pub use setup::run_gateway_login;

async fn submit(
    sender: &GatewaySender,
    session_id: &mobius::SessionId,
    id: std::sync::Arc<str>,
    op: mobius::protocol::Op,
) -> Result<()> {
    sender
        .send(mobius_gateway::wire::ClientMessage::Submit {
            session_id: session_id.share(),
            submission: mobius::protocol::Submission { id, op },
        })
        .await
        .map_err(crate::gateway_error)
}

/// Moves `index` by `delta` within `0..len`, wrapping at both ends.
fn wrap_step(index: usize, delta: isize, len: usize) -> usize {
    let Some(shift) = delta.unsigned_abs().checked_rem(len) else {
        return index;
    };
    let index = index % len;
    if delta.is_negative() {
        (index + len - shift) % len
    } else {
        (index + shift) % len
    }
}

#[expect(
    clippy::expect_used,
    reason = "fmt::Write for String never returns an error"
)]
fn block_text(block: &FrontendBlock) -> String {
    let mut text = String::from(block.title.as_str());
    if !block.text.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&block.text);
    }
    for part in &block.content.0 {
        text.push('\n');
        match part {
            mobius::protocol::ContentPart::Text { text: part } => text.push_str(part),
            mobius::protocol::ContentPart::Image { image } => {
                write!(
                    text,
                    "[image] {} · {} × {} · file_id={}",
                    image.file.name, image.width, image.height, image.file.id
                )
                .expect("writing to String cannot fail");
            }
            mobius::protocol::ContentPart::File { file } => {
                write!(
                    text,
                    "[file] {} · {} bytes · file_id={}",
                    file.name, file.size, file.id
                )
                .expect("writing to String cannot fail");
            }
        }
    }
    for file in &block.files {
        if !text.is_empty() {
            text.push('\n');
        }
        write!(
            text,
            "[file] {} · {} · {} bytes",
            file.name, file.media_type, file.size
        )
        .expect("writing to String cannot fail");
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

/// Runs the interactive terminal frontend.
/// # Errors
///
/// Returns an error if validation or an operation required to complete the request fails.
pub async fn run(
    sender: GatewaySender,
    events: GatewayEvents,
    gateway: &mut ReadyPayload,
    session: &mut SessionReadyPayload,
    gateway_endpoint: String,
    opening: Opening,
) -> Result<(FrontendExit, GatewaySender, GatewayEvents)> {
    let workspace = session
        .workspace
        .as_ref()
        .map(|workspace| workspace.path.as_path());
    let catalog = catalog::UiCatalog::build(&session.contributions, workspace)?;
    tui::runtime::run(
        sender,
        events,
        gateway,
        session,
        catalog,
        gateway_endpoint,
        opening,
    )
    .await
}

/// How the terminal frontend opens a chat.
#[derive(Debug, Default)]
pub struct Opening {
    /// Offer the Bot picker before the first message.
    pub choose_bot: bool,
    /// The submitted first message of a draft the gateway has just routed.
    pub first_message: Option<mobius::protocol::MessageSubmission>,
    /// Composer text carried over from a previous connection.
    pub draft: Option<String>,
}

/// Why a frontend returned control to its launcher.
#[derive(Debug)]
pub enum FrontendExit {
    /// Selects the exit case.
    Exit,
    /// Selects the resume case.
    Resume(mobius::SessionId),
    /// Selects the reload case.
    Reload,
    /// Reconnects to the same chat, carrying the composer text that was not sent.
    Reconnect {
        /// The unsent composer text.
        draft: Option<String>,
    },
    /// Draft a first message before the gateway creates and routes the session.
    NewChat {
        /// Default workspace, or a folder the user chose.
        workspace: Option<std::path::PathBuf>,
        /// Whether the workspace must bypass Auto.
        explicit_workspace: bool,
    },
    /// Selects the fresh-session case after deleting the open chat.
    Fresh,
}
