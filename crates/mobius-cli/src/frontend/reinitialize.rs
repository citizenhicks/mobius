//! Confirmation before replacing existing gateway state.

use std::io;
use std::path::Path;

use mobius::Result;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::time::MissedTickBehavior;

use super::terminal::{INPUT_POLL, MAX_INPUT_BATCH, TerminalGuard, poll_event};
use super::terminal_text;
use super::theme::{Role, current};

/// Asks whether existing gateway state may be permanently replaced.
/// # Errors
///
/// Returns an error if validation or an operation required by this function fails.
pub async fn confirm(state_dir: &Path) -> Result<bool> {
    let mut guard = TerminalGuard::alternate()?;
    guard.set_mouse_capture(false)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let mut tick = tokio::time::interval(INPUT_POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        terminal.draw(|frame| render(frame, state_dir))?;
        tick.tick().await;
        for _ in 0..MAX_INPUT_BATCH {
            let Some(event) = poll_event()? else {
                break;
            };
            if let Event::Key(key) = event
                && let Some(confirm) = decision(key)
            {
                return Ok(confirm);
            }
        }
    }
}

fn decision(key: KeyEvent) -> Option<bool> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'd'))
    {
        return Some(false);
    }
    match key.code {
        KeyCode::Char('y' | 'Y') => Some(true),
        KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
        _ => None,
    }
}

fn render(frame: &mut ratatui::Frame<'_>, state_dir: &Path) {
    let theme = current();
    let lines = vec![
        Line::from(""),
        Line::styled(
            "  Gateway state already exists:",
            theme.style(Role::Warning),
        ),
        Line::styled(
            format!("  {}", terminal_text(&state_dir.display().to_string())),
            theme.style(Role::Text),
        ),
        Line::from(""),
        Line::styled(
            "  Reinitialize it? This permanently deletes its configuration, chats, providers, and paired devices.",
            theme.style(Role::Error),
        ),
        Line::from(""),
        Line::styled(
            "  y reinitialize · n/esc keep existing",
            theme.style(Role::Muted),
        ),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Reinitialize möbius Gateway? "),
            )
            .style(theme.style(Role::Canvas))
            .wrap(Wrap { trim: false }),
        frame.area(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_yes_confirms_reinitialization() {
        let key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);

        assert_eq!(decision(key), Some(true));
    }

    #[test]
    fn enter_does_not_confirm_reinitialization() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(decision(key), None);
    }
}
