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

use super::setup::{self, SetupMode};
use super::terminal::TerminalGuard;

pub(in crate::frontend) use self::runtime::{
    activate_overlay, handle_action_input_key, insert_overlay_input, move_overlay_action,
    move_overlay_selection, prepare_overlay_operation, select_overlay_edge,
};
use self::runtime::{connect, dashboard_loop};
pub(in crate::frontend) use self::state::CapabilityOverlay;
pub(in crate::frontend) use self::view::{centered_area, render_capability_overlay};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

pub async fn run(state_dir: PathBuf) -> Result<()> {
    let (sender, mut events, mut state) = connect(state_dir).await?;
    let _guard = TerminalGuard::alternate()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    dashboard_loop(&mut terminal, sender, &mut events, &mut state).await
}

pub async fn run_provider(state_dir: PathBuf) -> Result<()> {
    let (sender, mut events, mut state) = connect(state_dir).await?;
    let mut guard = TerminalGuard::alternate()?;
    guard.set_mouse_capture(false)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    setup::run_gateway(
        &mut terminal,
        SetupMode::Login,
        &sender,
        &mut events,
        &mut state.gateway,
    )
    .await
}

#[cfg(test)]
mod tests;
