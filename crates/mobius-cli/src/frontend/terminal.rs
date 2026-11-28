use std::io;
use std::sync::{Arc, Mutex as StdMutex, Once, OnceLock};
use std::thread::ThreadId;

use mobius::protocol::FrontendWidget;
use mobius::{Error, Result};
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
use tokio::sync::{Mutex, mpsc};

pub(super) fn exit_key(key: ratatui::crossterm::event::KeyEvent) -> bool {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'd'))
}

const MAX_INPUT_BATCH: usize = 64;
const INPUT_CHANNEL_CAPACITY: usize = 256;
static ACTIVE_GUARD_OWNER: StdMutex<Option<(ThreadId, usize)>> = StdMutex::new(None);
static PANIC_HOOK: Once = Once::new();
/// Terminal input cap for gateway and provider endpoint fields.
pub(super) const MAX_ENDPOINT_BYTES: usize = 4 * 1024;

/// Returns the terminal text.
pub fn terminal_text(value: &str) -> String {
    value
        .chars()
        .filter(|&character| terminal_character(character))
        .collect()
}

fn terminal_character(character: char) -> bool {
    matches!(character, '\n' | '\t') || !character.is_control()
}

pub(super) const MAX_ENTRY_BYTES: usize = 40_000;
pub(super) const CONTRIBUTED_WIDGET_BYTES: usize = 32 * 1024;

pub(super) fn bounded_widget(mut item: Arc<FrontendWidget>, limit: usize) -> Arc<FrontendWidget> {
    if item.text.len() > limit || !item.text.chars().all(terminal_character) {
        let text = &mut Arc::make_mut(&mut item).text;
        text.retain(terminal_character);
        truncate_display_text(text, limit);
    }
    item
}

pub(super) fn bounded_terminal_text(value: &str, limit: usize) -> String {
    let mut value = terminal_text(value);
    truncate_display_text(&mut value, limit);
    value
}

pub(super) fn truncate_display_text(value: &mut String, limit: usize) {
    if value.len() > limit {
        truncate_bytes(value, limit);
        value.push_str("\n[display truncated]");
    }
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

fn input_receiver() -> &'static Mutex<mpsc::Receiver<Result<Event>>> {
    static RECEIVER: OnceLock<Mutex<mpsc::Receiver<Result<Event>>>> = OnceLock::new();
    RECEIVER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
        if let Err(error) = std::thread::Builder::new()
            .name("mobius-tty-input".into())
            .spawn(move || {
                loop {
                    let next = event::read().map_err(Error::from);
                    if sender.blocking_send(next).is_err() {
                        break;
                    }
                }
            })
        {
            drop(error);
        }
        Mutex::new(receiver)
    })
}

/// Waits for the next terminal event and drains any immediately queued burst, up to one bounded
/// batch. Cancel-safe for use inside `tokio::select!`.
pub(super) async fn next_input_batch() -> Vec<Result<Event>> {
    let mut receiver = input_receiver().lock().await;
    let Some(first) = receiver.recv().await else {
        return vec![Err(Error::Stopped("terminal input closed".into()))];
    };
    let mut batch = Vec::with_capacity(1);
    batch.push(first);
    while batch.len() < MAX_INPUT_BATCH {
        match receiver.try_recv() {
            Ok(event) => batch.push(event),
            Err(_) => break,
        }
    }
    batch
}

fn register_guard_owner() {
    if let Ok(mut slot) = ACTIVE_GUARD_OWNER.lock() {
        let current = std::thread::current().id();
        match slot.as_mut() {
            Some((owner, depth)) if *owner == current => *depth += 1,
            _ => *slot = Some((current, 1)),
        }
    }
}

fn unregister_guard_owner() {
    if let Ok(mut slot) = ACTIVE_GUARD_OWNER.lock()
        && let Some((_, depth)) = slot.as_mut()
    {
        *depth = depth.saturating_sub(1);
        if *depth == 0 {
            *slot = None;
        }
    }
}

fn should_restore_terminal_on_current_thread() -> bool {
    ACTIVE_GUARD_OWNER.try_lock().is_ok_and(|slot| {
        slot.is_some_and(|(owner, depth)| depth > 0 && owner == std::thread::current().id())
    })
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if should_restore_terminal_on_current_thread() {
                drop(disable_raw_mode());
                drop(execute!(
                    io::stdout(),
                    Show,
                    DisableMouseCapture,
                    DisableBracketedPaste,
                    LeaveAlternateScreen
                ));
            }
            previous(info);
        }));
    });
}

pub(super) struct TerminalGuard {
    mouse_capture: bool,
}

impl TerminalGuard {
    pub(super) fn alternate() -> Result<Self> {
        install_panic_hook();
        enable_raw_mode()?;
        register_guard_owner();
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
        unregister_guard_owner();
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

pub(super) fn render_dialog(frame: &mut ratatui::Frame<'_>, title: &str, lines: Vec<Line<'_>>) {
    use super::theme::{Role, current};
    use ratatui::widgets::{Block, Paragraph, Wrap};
    let content = Paragraph::new(lines)
        .block(Block::bordered().title(title))
        .style(current().style(Role::Canvas))
        .wrap(Wrap { trim: false });
    frame.render_widget(content, frame.area());
}

pub(super) fn page_canvas(frame: &mut ratatui::Frame<'_>, max_width: u16) -> ratatui::layout::Rect {
    use super::theme::{Role, current};
    let area = frame.area();
    frame.render_widget(
        ratatui::widgets::Block::default().style(current().style(Role::Canvas)),
        area,
    );

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

    /// Edit one bounded field; Enter accepts it and Escape cancels.
    pub(super) fn input(&mut self, event: Event) -> Option<bool> {
        use ratatui::crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
        match event {
            Event::Paste(text) => self.push(&text),
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match key.code {
                    KeyCode::Esc => return Some(false),
                    KeyCode::Enter => return Some(true),
                    KeyCode::Backspace => self.backspace(),
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.value.clear();
                    }
                    KeyCode::Char(c) => self.push(&c.to_string()),
                    _ => {}
                }
            }
            _ => {}
        }
        None
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

/// Counts supplement the session state, which already reports a pending approval.
pub(super) fn chat_indicators(activity: &mobius_gateway::wire::SessionActivity) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    for (label, count) in [
        ("pending questions", activity.attention),
        ("running commands", activity.running_commands),
    ] {
        if count > 0 {
            #[expect(clippy::expect_used, reason = "fmt::Write for String is infallible")]
            write!(text, " · {label} {count}").expect("writing to String");
        }
    }
    text
}

pub(super) fn choice_row(
    lines: &mut Vec<Line<'static>>,
    index: usize,
    label: &str,
    description: &str,
    selected: bool,
) {
    use super::terminal_text;
    use super::theme::{Role, current};
    let theme = current();
    let role = if selected {
        Role::Selection
    } else {
        Role::Text
    };
    lines.push(Line::styled(
        format!(
            "{} {}. {}",
            if selected { "›" } else { " " },
            index + 1,
            terminal_text(label)
        ),
        theme.style(role),
    ));
    lines.push(Line::styled(
        format!("     {}", terminal_text(description)),
        theme.style(if selected {
            Role::Selection
        } else {
            Role::Muted
        }),
    ));
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

    #[test]
    fn panic_restore_only_applies_to_guard_owning_thread() {
        register_guard_owner();
        assert!(
            should_restore_terminal_on_current_thread(),
            "owning thread should restore terminal on panic"
        );
        let background = std::thread::spawn(should_restore_terminal_on_current_thread)
            .join()
            .expect("background thread join");
        assert!(
            !background,
            "background thread panic must not tear down the active TUI terminal"
        );
        unregister_guard_owner();
        assert!(
            !should_restore_terminal_on_current_thread(),
            "released guard should no longer trigger panic restore"
        );
    }
}
