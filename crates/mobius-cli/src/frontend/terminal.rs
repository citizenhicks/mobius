use std::io;
use std::time::Duration;

use mobius::Result;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
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
const MAX_INPUT_BATCH: usize = 64;
/// Terminal input cap for gateway and provider endpoint fields.
pub(super) const MAX_ENDPOINT_BYTES: usize = 4 * 1024;

/// Returns the terminal text.
pub fn terminal_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| matches!(character, '\n' | '\t') || !character.is_control())
        .collect()
}

/// Terminal text flattened onto one line: newlines and tabs become spaces.
pub(super) fn terminal_line(value: &str) -> String {
    terminal_text(value).replace(['\n', '\t'], " ")
}

/// A compact elapsed time: `42s`, `3m 07s` or `2h 05m`.
pub(super) fn elapsed_label(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3_600, seconds / 60 % 60)
    }
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

/// The alternate-screen terminal every full-screen view draws on.
pub(super) type Screen = Terminal<CrosstermBackend<io::Stdout>>;

/// Opens a full-screen view without mouse capture, so the terminal keeps native text selection.
pub(super) fn open_screen() -> Result<(TerminalGuard, Screen)> {
    let mut guard = TerminalGuard::alternate()?;
    guard.set_mouse_capture(false)?;
    Ok((guard, Terminal::new(CrosstermBackend::new(io::stdout()))?))
}

fn poll_event() -> Result<Option<Event>> {
    event::poll(Duration::ZERO)?
        .then(event::read)
        .transpose()
        .map_err(Into::into)
}

/// The terminal events already waiting, at most one bounded batch per call.
pub(super) fn input_batch() -> impl Iterator<Item = Result<Event>> {
    (0..MAX_INPUT_BATCH).map_while(|_| poll_event().transpose())
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
    #[expect(
        clippy::print_stderr,
        reason = "drop cannot return an error, and stderr is the only place left to report a failed terminal restore"
    )]
    fn drop(&mut self) {
        if let Err(error) = disable_raw_mode() {
            eprintln!("could not leave terminal raw mode: {error}");
        }
        if let Err(error) = execute!(
            io::stdout(),
            Show,
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        ) {
            eprintln!("could not restore the terminal screen: {error}");
        }
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

/// Appends printable single-line text up to a UTF-8 byte bound; returns whether text was rejected.
pub(super) fn append_text(target: &mut String, text: &str, limit: usize) -> bool {
    append_bounded(target, text, limit, false)
}

/// Appends printable text, plus newlines and tabs when `multiline`, up to a UTF-8 byte bound;
/// returns whether text was rejected.
pub(super) fn append_bounded(
    target: &mut String,
    text: &str,
    limit: usize,
    multiline: bool,
) -> bool {
    for character in text.chars().filter(|character| {
        (multiline && matches!(character, '\n' | '\t')) || !character.is_control()
    }) {
        if target.len() + character.len_utf8() > limit {
            return true;
        }
        target.push(character);
    }
    false
}

/// One bounded text input; it drops control characters other than newlines in multiline input.
pub(super) struct TextForm {
    pub(super) value: String,
    pub(super) limit: usize,
    pub(super) multiline: bool,
    pub(super) error: Option<String>,
}

impl TextForm {
    pub(super) fn new(value: impl Into<String>, limit: usize) -> Self {
        Self {
            value: value.into(),
            limit,
            multiline: false,
            error: None,
        }
    }

    pub(super) fn multiline(value: impl Into<String>, limit: usize) -> Self {
        Self {
            value: value.into(),
            limit,
            multiline: true,
            error: None,
        }
    }

    pub(super) fn push(&mut self, value: &str) {
        self.error = append_bounded(&mut self.value, value, self.limit, self.multiline)
            .then(|| format!("input is limited to {} bytes", self.limit));
    }

    pub(super) fn backspace(&mut self) {
        self.value.pop();
        self.error = None;
    }
}

pub(super) fn truncate_bytes(value: &mut String, limit: usize) {
    value.truncate(value.floor_char_boundary(limit));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_input_preserves_utf8_and_filters_control_characters() {
        let mut text = String::new();
        assert!(!append_text(&mut text, "a\n\t", 4));
        assert!(append_text(&mut text, "€z", 4));
        assert_eq!(text, "a€");
        let mut text = String::new();
        assert!(!append_bounded(&mut text, "a\n\t\u{7}", 8, true));
        assert_eq!(text, "a\n\t");
        assert_eq!(masked_credential(&text, 1), "•…");
    }
}
