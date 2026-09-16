//! CLI-local, one-file-at-a-time diff preview.

use diffy::Patch;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use super::{Viewport, terminal_text, view};
use crate::frontend::theme::{Role, current};

struct FileDiff {
    text: String,
    path: String,
    status: &'static str,
    added: usize,
    removed: usize,
}

impl FileDiff {
    fn new(text: &str) -> Self {
        let patch = Patch::from_str(text).ok();
        let metadata = text.lines().take_while(|line| !line.starts_with("@@ "));
        let path = patch
            .as_ref()
            .and_then(|patch| {
                patch
                    .modified()
                    .filter(|path| *path != "/dev/null")
                    .or_else(|| patch.original())
            })
            .or_else(|| {
                metadata.clone().find_map(|line| {
                    line.strip_prefix("+++ ")
                        .filter(|path| *path != "/dev/null")
                })
            })
            .or_else(|| metadata.clone().find_map(|line| line.strip_prefix("--- ")))
            .map(|path| {
                path.strip_prefix("b/")
                    .or_else(|| path.strip_prefix("a/"))
                    .unwrap_or(path)
            })
            .or_else(|| {
                metadata
                    .clone()
                    .find_map(|line| line.strip_prefix("rename to "))
            })
            .or_else(|| {
                metadata.clone().find_map(|line| {
                    let paths = line.strip_prefix("diff --git a/")?;
                    paths.match_indices(" b/").find_map(|(index, _)| {
                        (paths[..index] == paths[index + 3..]).then_some(&paths[index + 3..])
                    })
                })
            })
            .unwrap_or_else(|| text.lines().next().unwrap_or("Diff"));
        // Keep quoted paths and ambiguous Git headers intact rather than guessing at spaces.
        let path = terminal_text(path).replace(['\n', '\t'], " ");
        let status = if metadata
            .clone()
            .any(|line| line.starts_with("new file mode ") || line == "--- /dev/null")
        {
            "A"
        } else if metadata
            .clone()
            .any(|line| line.starts_with("deleted file mode ") || line == "+++ /dev/null")
        {
            "D"
        } else if metadata
            .clone()
            .any(|line| line.starts_with("rename from "))
        {
            "R"
        } else if metadata.clone().any(|line| line.starts_with("copy from ")) {
            "C"
        } else {
            "M"
        };
        let mut in_hunk = false;
        let (mut added, mut removed) = (0, 0);
        for line in text.lines() {
            if line.starts_with("@@ ") {
                in_hunk = true;
            } else if in_hunk && line.starts_with('+') {
                added += 1;
            } else if in_hunk && line.starts_with('-') {
                removed += 1;
            }
        }
        Self {
            text: text.to_owned(),
            path,
            status,
            added,
            removed,
        }
    }
}

pub(super) struct DiffBrowser {
    files: Vec<FileDiff>,
    selected: usize,
    files_focused: bool,
    list: ListState,
    viewport: Viewport,
    cache: Option<(usize, u16)>,
    lines: Vec<Line<'static>>,
    hunks: Vec<usize>,
    files_area: Rect,
    diff_area: Rect,
}

impl DiffBrowser {
    pub(super) fn new(text: String) -> Self {
        let text = terminal_text(&text).replace('\t', "    ");
        let files: Vec<_> = view::diff_sections(&text).map(FileDiff::new).collect();
        Self {
            selected: 0,
            files_focused: true,
            list: ListState::default().with_selected((!files.is_empty()).then_some(0)),
            files,
            viewport: Viewport {
                scroll: 0,
                ..Viewport::default()
            },
            cache: None,
            lines: Vec::new(),
            hunks: Vec::new(),
            files_area: Rect::default(),
            diff_area: Rect::default(),
        }
    }

    fn select(&mut self, index: usize) {
        let index = index.min(self.files.len() - 1);
        if self.selected != index {
            self.selected = index;
            self.list.select(Some(index));
            self.viewport.top();
            self.cache = None;
        }
    }

    fn move_vertical(&mut self, down: bool, rows: usize) {
        if self.files_focused {
            self.select(if down {
                self.selected.saturating_add(rows)
            } else {
                self.selected.saturating_sub(rows)
            });
        } else if down {
            self.viewport.scroll_down(rows);
        } else {
            self.viewport.scroll_up(rows);
        }
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) {
        if self.files.is_empty() {
            return;
        }
        let page = if self.files_focused {
            usize::from(self.files_area.height).max(1)
        } else {
            self.viewport.page_height()
        };
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => self.files_focused = !self.files_focused,
            KeyCode::Down | KeyCode::Char('j') => self.move_vertical(true, 1),
            KeyCode::Up | KeyCode::Char('k') => self.move_vertical(false, 1),
            KeyCode::PageDown => self.move_vertical(true, page),
            KeyCode::PageUp => self.move_vertical(false, page),
            KeyCode::Home => {
                if self.files_focused {
                    self.select(0);
                } else {
                    self.viewport.top();
                }
            }
            KeyCode::End => {
                if self.files_focused {
                    self.select(self.files.len() - 1);
                } else {
                    self.viewport.bottom();
                }
            }
            KeyCode::Enter if self.files_focused => self.files_focused = false,
            KeyCode::Char('[') | KeyCode::Char(']') => {
                self.ensure_lines(self.diff_area.width.max(1));
                let scroll = self.viewport.effective_scroll();
                let target = if key.code == KeyCode::Char(']') {
                    self.hunks.iter().copied().find(|row| *row > scroll)
                } else {
                    self.hunks.iter().copied().rev().find(|row| *row < scroll)
                };
                if let Some(row) = target {
                    self.viewport.scroll = row;
                }
                self.files_focused = false;
            }
            _ => {}
        }
    }

    pub(super) fn handle_mouse(&mut self, event: MouseEvent) -> bool {
        let position = (event.column, event.row).into();
        let files = self.files_area.contains(position);
        if !files && !self.diff_area.contains(position) {
            return false;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.files_focused = files;
                if files {
                    self.select(self.list.offset() + usize::from(event.row - self.files_area.y));
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                self.files_focused = files;
                self.move_vertical(event.kind == MouseEventKind::ScrollDown, 3);
            }
            _ => return false,
        }
        true
    }

    fn ensure_lines(&mut self, width: u16) {
        if self.cache == Some((self.selected, width)) {
            return;
        }
        self.lines.clear();
        let text = &self.files[self.selected].text;
        if view::push_diff_patch(&mut self.lines, text, usize::from(width)) {
            self.lines.retain(|line| {
                !line.spans.first().is_some_and(|span| {
                    span.content.starts_with("diff --git ") || span.content.starts_with("index ")
                }) && !line
                    .spans
                    .get(1)
                    .is_some_and(|span| span.content == "Edited ")
            });
        } else {
            self.lines.extend(text.lines().map(|line| {
                let role = if line.starts_with("@@ ") {
                    Role::Accent
                } else if line.starts_with('+') && !line.starts_with("+++ ") {
                    Role::Success
                } else if line.starts_with('-') && !line.starts_with("--- ") {
                    Role::Error
                } else {
                    Role::Text
                };
                Line::styled(line.to_owned(), current().style(role))
            }));
        }
        let mut row = 0;
        self.hunks = self
            .lines
            .iter()
            .filter_map(|line| {
                let hunk = line
                    .spans
                    .first()
                    .is_some_and(|span| span.content.starts_with("@@ "))
                    .then_some(row);
                row += Paragraph::new(Text::from(line.clone()))
                    .wrap(Wrap { trim: false })
                    .line_count(width);
                hunk
            })
            .collect();
        self.cache = Some((self.selected, width));
    }

    pub(super) fn render(&mut self, frame: &mut Frame, area: Rect, title: &str) {
        let theme = current();
        frame.render_widget(Clear, area);
        let block = Block::default()
            .style(theme.style(Role::Canvas))
            .borders(Borders::ALL)
            .title(terminal_text(title).replace('\t', "    "))
            .title_bottom(" Tab panes · ↑↓/jk move · [] hunks · Esc close ")
            .border_style(theme.style(Role::Border));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if self.files.is_empty() {
            frame.render_widget(Paragraph::new("No changes."), inner);
            return;
        }
        let panes = if area.width < 80 {
            if self.files_focused {
                [inner, Rect::default()]
            } else {
                [Rect::default(), inner]
            }
        } else {
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(32), Constraint::Percentage(68)])
                .split(inner);
            [panes[0], panes[1]]
        };
        self.files_area = Rect::default();
        self.diff_area = Rect::default();
        if panes[0].width > 0 {
            let block = Block::default()
                .borders(Borders::RIGHT)
                .title("Files")
                .border_style(theme.style(if self.files_focused {
                    Role::Accent
                } else {
                    Role::Border
                }));
            self.files_area = block.inner(panes[0]);
            let items = self.files.iter().map(|file| {
                let (directory, name) = file.path.rsplit_once('/').unwrap_or(("", &file.path));
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{} ", file.status), theme.style(Role::Accent)),
                    Span::styled(format!("+{} ", file.added), theme.style(Role::Success)),
                    Span::styled(format!("-{} ", file.removed), theme.style(Role::Error)),
                    Span::styled(name.to_owned(), theme.style(Role::Text)),
                    Span::styled(format!(" {directory}"), theme.style(Role::Muted)),
                ]))
            });
            frame.render_stateful_widget(
                List::new(items)
                    .block(block)
                    .highlight_symbol("› ")
                    .highlight_style(theme.style(Role::Selection)),
                panes[0],
                &mut self.list,
            );
        }
        if panes[1].width > 0 {
            let block = Block::default()
                .title(self.files[self.selected].path.clone())
                .border_style(theme.style(if self.files_focused {
                    Role::Border
                } else {
                    Role::Accent
                }));
            self.diff_area = block.inner(panes[1]);
            self.ensure_lines(self.diff_area.width);
            let scroll = self.viewport.scroll;
            let content_height = self
                .lines
                .iter()
                .map(|line| {
                    Paragraph::new(Text::from(line.clone()))
                        .wrap(Wrap { trim: false })
                        .line_count(self.diff_area.width)
                })
                .sum();
            self.viewport
                .update(content_height, usize::from(self.diff_area.height));
            // Unlike transcript tail-following, a diff remains at the top after resize.
            if scroll == 0 {
                self.viewport.top();
            }
            frame.render_widget(
                Paragraph::new(Text::from(self.lines.clone()))
                    .wrap(Wrap { trim: false })
                    .block(block)
                    .scroll((
                        u16::try_from(self.viewport.effective_scroll()).unwrap_or(u16::MAX),
                        0,
                    )),
                panes[1],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend};

    const PATCH: &str =
        "diff --git a/file.rs b/file.rs\n--- a/file.rs\n+++ b/file.rs\n@@ -1 +1 @@\n-old\n+new\n";

    fn key(browser: &mut DiffBrowser, code: KeyCode) {
        browser.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn empty_diff_shows_no_changes_without_a_fake_file() {
        let mut browser = DiffBrowser::new(String::new());
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        for code in [KeyCode::Tab, KeyCode::End, KeyCode::Char(']')] {
            key(&mut browser, code);
        }
        terminal
            .draw(|frame| browser.render(frame, frame.area(), "Diff"))
            .unwrap();
        assert!(browser.files.is_empty());
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("No changes."));
        assert!(!text.contains("+0 -0"));
    }

    #[test]
    fn files_preserve_metadata_and_truncated_patches() {
        let modified = FileDiff::new(
            "diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n--- /dev/null\n+++ /dev/null\n",
        );
        assert_eq!((modified.status, modified.path.as_str()), ("M", "file"));
        assert_eq!(
            FileDiff::new("diff --git a/b/file b/b/file\nold mode 100644\nnew mode 100755\n").path,
            "b/file"
        );
        assert_eq!(
            FileDiff::new("diff --git a/old b/b/new.rs\nrename from old\nrename to b/new.rs\n")
                .path,
            "b/new.rs"
        );
        let text = format!(
            "{PATCH}diff --git \"a/old name\" \"b/new name\"\nsimilarity index 100%\nrename from old name\nrename to new name\ndiff --git a/image b/image\nBinary files a/image and b/image differ\ndiff --git a/c b/c\nold mode 100644\nnew mode 100755\n--- a/c\n+++ b/c\n@@ -1,5 +1,5 @@\n-removed\n+added\n[diff truncated]\n"
        );
        let mut browser = DiffBrowser::new(text);
        assert_eq!(browser.files.len(), 4);
        assert_eq!((browser.files[0].added, browser.files[0].removed), (1, 1));
        assert_eq!(
            (browser.files[1].status, browser.files[1].path.as_str()),
            ("R", "new name")
        );
        browser.select(2);
        assert_eq!(browser.files[2].path, "image");
        browser.ensure_lines(60);
        assert!(
            browser
                .lines
                .iter()
                .any(|line| line.to_string().contains("Binary files"))
        );
        browser.select(3);
        browser.ensure_lines(60);
        assert!(
            browser
                .lines
                .iter()
                .any(|line| line.to_string().contains("[diff truncated]"))
        );
        assert!(
            browser
                .lines
                .iter()
                .any(|line| line.to_string().contains("new mode"))
        );
    }

    #[test]
    fn navigation_resets_file_scroll_and_switches_focus() {
        let mut browser = DiffBrowser::new(format!("{PATCH}{PATCH}"));
        key(&mut browser, KeyCode::Down);
        assert_eq!(browser.selected, 1);
        key(&mut browser, KeyCode::Tab);
        browser.viewport.update(100, 10);
        key(&mut browser, KeyCode::Down);
        assert_eq!(browser.viewport.effective_scroll(), 1);
        key(&mut browser, KeyCode::Tab);
        key(&mut browser, KeyCode::Up);
        assert_eq!((browser.selected, browser.viewport.scroll), (0, 0));
    }

    #[test]
    fn hunk_navigation_counts_wrapped_rows() {
        let text = format!("{PATCH}@@ -10 +10 @@\n-old\n+{} @@ fake\n", "x".repeat(160));
        let mut browser = DiffBrowser::new(text);
        let mut terminal = Terminal::new(TestBackend::new(100, 6)).unwrap();
        terminal
            .draw(|frame| browser.render(frame, frame.area(), "Diff"))
            .unwrap();
        assert_eq!(browser.hunks.len(), 2);
        assert_eq!(browser.lines.len(), 6);
        assert!(browser.viewport.content_height > browser.lines.len());
        key(&mut browser, KeyCode::Char(']'));
        assert_eq!(browser.viewport.scroll, browser.hunks[1]);
        key(&mut browser, KeyCode::Char('['));
        assert_eq!(browser.viewport.scroll, browser.hunks[0]);
    }

    #[test]
    fn mouse_selects_files_and_scrolls_the_pane_under_the_pointer() {
        let mut browser = DiffBrowser::new(format!(
            "{PATCH}{}@@ -10 +10 @@\n-old\n+new\n",
            PATCH.replace("file.rs", "next.rs")
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 6)).unwrap();
        terminal
            .draw(|frame| browser.render(frame, frame.area(), "Diff"))
            .unwrap();
        assert!(browser.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: browser.files_area.x,
            row: browser.files_area.y + 1,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(browser.selected, 1);
        terminal
            .draw(|frame| browser.render(frame, frame.area(), "Diff"))
            .unwrap();
        assert!(browser.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: browser.diff_area.x,
            row: browser.diff_area.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(!browser.files_focused);
        assert_eq!(browser.selected, 1);
        assert!(browser.viewport.effective_scroll() > 0);
    }

    #[test]
    fn renders_wide_and_narrow_with_diff_colors_and_safe_text() {
        let mut browser = DiffBrowser::new(PATCH.replace("new", "new\tvalue\u{1b}"));
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(format!("{}\n", "Z".repeat(110)).repeat(24)),
                    frame.area(),
                );
                browser.render(frame, frame.area(), "Diff");
            })
            .unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| cell.symbol() != "Z")
        );
        assert!(browser.files_area.width > 0 && browser.diff_area.width > 0);
        assert_eq!(
            terminal.backend().buffer()[(browser.diff_area.x, browser.diff_area.y + 2)].bg,
            current().diff_add_background()
        );
        assert_eq!(
            terminal.backend().buffer()[(90, 20)].fg,
            current().color(Role::Canvas)
        );
        assert_eq!(browser.viewport.effective_scroll(), 0);
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .any(|cell| cell.bg == current().diff_add_background())
        );
        assert!(
            browser
                .lines
                .iter()
                .any(|line| line.to_string().contains("new    value"))
        );
        assert!(
            browser
                .lines
                .iter()
                .all(|line| !line.to_string().contains('\u{1b}'))
        );
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal
            .draw(|frame| browser.render(frame, frame.area(), "Diff"))
            .unwrap();
        assert_eq!(browser.diff_area.width, 0);
        key(&mut browser, KeyCode::Tab);
        terminal
            .draw(|frame| browser.render(frame, frame.area(), "Diff"))
            .unwrap();
        assert_eq!(browser.files_area.width, 0);
        assert!(browser.diff_area.width > 0);
    }
}
