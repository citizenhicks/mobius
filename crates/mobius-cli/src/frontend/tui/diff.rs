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
        let metadata: Vec<&str> = text
            .lines()
            .take_while(|line| !line.starts_with("@@ "))
            .collect();
        let path = patch
            .as_ref()
            .and_then(|patch| {
                patch
                    .modified()
                    .filter(|path| *path != "/dev/null")
                    .or_else(|| patch.original())
            })
            .or_else(|| {
                metadata.iter().find_map(|line| {
                    line.strip_prefix("+++ ")
                        .filter(|path| *path != "/dev/null")
                })
            })
            .or_else(|| metadata.iter().find_map(|line| line.strip_prefix("--- ")))
            .map(|path| {
                path.strip_prefix("b/")
                    .or_else(|| path.strip_prefix("a/"))
                    .unwrap_or(path)
            })
            .or_else(|| {
                metadata
                    .iter()
                    .find_map(|line| line.strip_prefix("rename to "))
            })
            .or_else(|| {
                metadata.iter().find_map(|line| {
                    let paths = line.strip_prefix("diff --git a/")?;
                    paths.match_indices(" b/").find_map(|(index, _)| {
                        let modified = paths.get(index + 3..)?;
                        (paths.get(..index) == Some(modified)).then_some(modified)
                    })
                })
            })
            .unwrap_or_else(|| text.lines().next().unwrap_or("Diff"));
        // Keep quoted paths and ambiguous Git headers intact rather than guessing at spaces.
        let path = crate::frontend::terminal::terminal_line(path);
        let status = if metadata
            .iter()
            .any(|line| line.starts_with("new file mode ") || *line == "--- /dev/null")
        {
            "A"
        } else if metadata
            .iter()
            .any(|line| line.starts_with("deleted file mode ") || *line == "+++ /dev/null")
        {
            "D"
        } else if metadata.iter().any(|line| line.starts_with("rename from ")) {
            "R"
        } else if metadata.iter().any(|line| line.starts_with("copy from ")) {
            "C"
        } else {
            "M"
        };
        let (added, removed) = changed_lines(text);
        Self {
            text: text.to_owned(),
            path,
            status,
            added,
            removed,
        }
    }
}

fn changed_lines(text: &str) -> (usize, usize) {
    text.lines()
        .skip_while(|line| !line.starts_with("@@ "))
        .fold((0, 0), |(added, removed), line| {
            (
                added + usize::from(line.starts_with('+')),
                removed + usize::from(line.starts_with('-')),
            )
        })
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
    pub(super) fn new(text: &str) -> Self {
        let text = terminal_text(text).replace('\t', "    ");
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
            KeyCode::Char('[' | ']') => {
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
        let Some(FileDiff { text, .. }) = self.files.get(self.selected) else {
            return;
        };
        if view::push_diff_patch(&mut self.lines, text, usize::from(width)) {
            self.lines.retain(|line| {
                !line.spans.first().is_some_and(|span| {
                    span.content.starts_with("diff --git ") || span.content.starts_with("index ")
                }) && line
                    .spans
                    .get(1)
                    .is_none_or(|span| span.content != "Edited ")
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
                row += Paragraph::new(Text::from(crate::frontend::terminal::borrow_line(line)))
                    .wrap(Wrap { trim: false })
                    .line_count(width);
                hunk
            })
            .collect();
        self.cache = Some((self.selected, width));
    }

    fn selected_path(&self) -> &str {
        self.files
            .get(self.selected)
            .map_or("", |file| file.path.as_str())
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
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(32), Constraint::Percentage(68)])
                .areas(inner)
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
                    Span::styled(name, theme.style(Role::Text)),
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
            let block = Block::default().border_style(theme.style(if self.files_focused {
                Role::Border
            } else {
                Role::Accent
            }));
            self.diff_area = Block::default().title(self.selected_path()).inner(panes[1]);
            self.ensure_lines(self.diff_area.width);
            let scroll = self.viewport.scroll;
            let content_height = self
                .lines
                .iter()
                .map(|line| {
                    Paragraph::new(Text::from(crate::frontend::terminal::borrow_line(line)))
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
                Paragraph::new(Text::from(
                    self.lines
                        .iter()
                        .map(crate::frontend::terminal::borrow_line)
                        .collect::<Vec<_>>(),
                ))
                .wrap(Wrap { trim: false })
                .block(block.title(self.selected_path()))
                .scroll((
                    u16::try_from(self.viewport.effective_scroll()).unwrap_or(u16::MAX),
                    0,
                )),
                panes[1],
            );
        }
    }
}
