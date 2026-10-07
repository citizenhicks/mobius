use std::io;
use std::time::Duration;

use mobius::Result;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::text::{Line, Span};

pub(super) const INPUT_POLL: Duration = Duration::from_millis(16);
pub(super) const MAX_INPUT_BATCH: usize = 64;

/// Returns the terminal text.
pub fn terminal_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| matches!(character, '\n' | '\t') || !character.is_control())
        .collect()
}

pub(super) fn borrow_line<'a>(line: &'a Line<'_>) -> Line<'a> {
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: line
            .spans
            .iter()
            .map(|span| Span::styled(span.content.as_ref(), span.style))
            .collect(),
    }
}

pub(super) fn poll_event() -> Result<Option<Event>> {
    event::poll(Duration::ZERO)?
        .then(event::read)
        .transpose()
        .map_err(Into::into)
}

pub(super) struct TerminalGuard {
    mouse_capture: bool,
}

impl TerminalGuard {
    pub(super) fn alternate() -> Result<Self> {
        enable_raw_mode()?;
        let guard = Self {
            mouse_capture: true,
        };
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            Hide
        )?;
        Ok(guard)
    }

    pub(super) fn set_mouse_capture(&mut self, enabled: bool) -> Result<()> {
        if self.mouse_capture == enabled {
            return Ok(());
        }
        if enabled {
            execute!(io::stdout(), EnableMouseCapture)?;
        } else {
            execute!(io::stdout(), DisableMouseCapture)?;
        }
        self.mouse_capture = enabled;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            Show,
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
    }
}

pub(super) fn masked_credential(value: &str, limit: usize) -> String {
    let count = value.chars().count();
    let mut masked = "•".repeat(count.min(limit));
    if count > limit {
        masked.push('…');
    }
    masked
}

pub(super) fn content_area(area: ratatui::layout::Rect, max_width: u16) -> ratatui::layout::Rect {
    let width = area.width.saturating_sub(4).min(max_width);
    ratatui::layout::Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y.saturating_add(1),
        width,
        area.height.saturating_sub(2),
    )
}

/// Appends printable text up to a UTF-8 byte bound; returns whether text was rejected.
pub(super) fn append_text(target: &mut String, text: &str, limit: usize) -> bool {
    for character in text.chars().filter(|character| !character.is_control()) {
        if target.len() + character.len_utf8() > limit {
            return true;
        }
        target.push(character);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_line_projection_keeps_style_and_borrows_text() {
        let original = Line::from(vec![Span::styled(
            String::from("cached text"),
            ratatui::style::Style::default().fg(ratatui::style::Color::Red),
        )])
        .alignment(ratatui::layout::Alignment::Right);
        let projected = borrow_line(&original);
        assert_eq!(projected, original);
        assert!(matches!(
            projected.spans[0].content,
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(
            projected.spans[0].content.as_ptr(),
            original.spans[0].content.as_ptr()
        );
    }

    #[test]
    fn bounded_input_preserves_utf8_and_filters_control_characters() {
        let mut text = String::new();
        assert!(!append_text(&mut text, "a\n\t", 4));
        assert!(append_text(&mut text, "€z", 4));
        assert_eq!(text, "a€");
        assert_eq!(masked_credential(&text, 1), "•…");
    }
}
