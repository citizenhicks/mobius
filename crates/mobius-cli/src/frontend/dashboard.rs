//! Live gateway dashboard and gateway-scoped setup entrypoints.

mod runtime;
mod state;
mod view;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use mobius::Result;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use super::setup;
use super::terminal::{TerminalGuard, open_screen};

pub(in crate::frontend) use self::runtime::{OverlayKey, insert_overlay_input, overlay_key};
use self::runtime::{connect, dashboard_loop};
pub(in crate::frontend) use self::state::CapabilityOverlay;
pub(in crate::frontend) use self::view::{centered_area, render_capability_overlay};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Runs the gateway dashboard.
/// # Errors
///
/// Returns an error if validation or an operation required to complete the request fails.
pub async fn run(state_dir: PathBuf) -> Result<()> {
    let (sender, mut events, mut state) = connect(state_dir).await?;
    let _guard = TerminalGuard::alternate()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    dashboard_loop(&mut terminal, sender, &mut events, &mut state).await
}

/// Runs provider setup from the gateway dashboard.
/// # Errors
///
/// Returns an error if validation or an operation required to complete the request fails.
pub async fn run_provider(state_dir: PathBuf) -> Result<()> {
    let (sender, mut events, mut state) = connect(state_dir).await?;
    let (_guard, mut terminal) = open_screen()?;
    setup::run_gateway(&mut terminal, &sender, &mut events, &mut state.gateway).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
