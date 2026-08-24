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

use self::runtime::{connect, dashboard_loop};

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
