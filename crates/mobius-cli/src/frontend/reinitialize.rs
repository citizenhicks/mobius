//! Confirmation before replacing existing gateway state.

use std::path::Path;

use mobius::Result;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::text::Line;

use super::terminal::{next_input_batch, open_screen};
use super::terminal_text;
use super::theme::{Role, current};

/// Asks whether existing gateway state may be permanently replaced.
/// # Errors
///
/// Returns an error if validation or an operation required by this function fails.
pub async fn confirm(state_dir: &Path) -> Result<bool> {
    let (_guard, mut terminal) = open_screen()?;
    terminal.clear()?;
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|frame| render(frame, state_dir))?;
            dirty = false;
        }
        for event in next_input_batch().await {
            let event = event?;
            if matches!(event, Event::Resize(_, _)) {
                dirty = true;
            }
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
    if crate::frontend::terminal::exit_key(key) {
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
    super::terminal::render_dialog(frame, " Reinitialize möbius Gateway? ", lines);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_yes_confirms_reinitialization() {
        let key = KeyEvent::new(
            KeyCode::Char('y'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        );

        assert_eq!(decision(key), Some(true));
    }
}
