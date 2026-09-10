//! Small Markdown-to-ratatui renderer for assistant transcript messages.

use pulldown_cmark::Alignment;
use pulldown_cmark::CodeBlockKind;
use pulldown_cmark::Event;
use pulldown_cmark::HeadingLevel;
use pulldown_cmark::Options;
use pulldown_cmark::Parser;
use pulldown_cmark::Tag;
use pulldown_cmark::TagEnd;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::frontend::theme::Role;
use crate::frontend::theme::current;

pub(super) fn render(source: &str, base: Style, width: usize) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    let mut writer = Writer::new(base, width);
    for event in Parser::new_ext(source, options) {
        writer.event(event);
    }
    writer.finish()
}

struct Item {
    first_prefix: String,
    continuation: String,
    first_line: bool,
}

#[derive(Default)]
struct Table {
    alignments: Vec<Alignment>,
    rows: Vec<(Vec<Vec<Span<'static>>>, bool)>,
    row: Vec<Vec<Span<'static>>>,
    cell: Vec<Span<'static>>,
    in_head: bool,
}

struct Writer {
    base: Style,
    width: usize,
    lines: Vec<Line<'static>>,
    current: Option<Line<'static>>,
    styles: Vec<Style>,
    lists: Vec<Option<u64>>,
    items: Vec<Item>,
    blockquote_depth: usize,
    in_code_block: bool,
    code_language: String,
    code_buffer: String,
    needs_blank: bool,
    link: Option<String>,
    table: Option<Table>,
}

impl Writer {
    fn new(base: Style, width: usize) -> Self {
        Self {
            base,
            width: width.max(1),
            lines: Vec::new(),
            current: None,
            styles: vec![Style::default()],
            lists: Vec::new(),
            items: Vec::new(),
            blockquote_depth: 0,
            in_code_block: false,
            code_language: String::new(),
            code_buffer: String::new(),
            needs_blank: false,
            link: None,
            table: None,
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.push_styled(&code, current().style(Role::Code)),
            Event::SoftBreak => self.push(" "),
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.start_block();
                self.push_styled("———", current().style(Role::Border));
                self.flush();
                self.needs_blank = true;
            }
            Event::Html(html) | Event::InlineHtml(html) => self.text(&html),
            Event::TaskListMarker(checked) => self.push(if checked { "[x] " } else { "[ ] " }),
            Event::FootnoteReference(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.start_block(),
            Tag::Heading { level, .. } => {
                self.start_block();
                self.push_style(heading_style(level));
            }
            Tag::BlockQuote => {
                self.start_block();
                self.blockquote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.start_block();
                self.code_language = match kind {
                    CodeBlockKind::Fenced(language) => language
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .into(),
                    CodeBlockKind::Indented => String::new(),
                };
                if !self.code_language.is_empty() {
                    self.push_styled(&self.code_language.clone(), current().style(Role::Muted));
                    self.flush();
                }
                self.in_code_block = true;
            }
            Tag::List(start) => {
                self.start_block();
                self.lists.push(start);
            }
            Tag::Item => self.start_item(),
            Tag::Emphasis => {
                self.push_style(Style::default().add_modifier(Modifier::ITALIC));
            }
            Tag::Strong => {
                self.push_style(Style::default().add_modifier(Modifier::BOLD));
            }
            Tag::Strikethrough => {
                self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link {
                link_type,
                dest_url,
                ..
            } => {
                self.link = (!matches!(
                    link_type,
                    pulldown_cmark::LinkType::Autolink | pulldown_cmark::LinkType::Email
                ))
                .then(|| dest_url.into_string());
                self.push_style(
                    current()
                        .style(Role::Info)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::Table(alignments) => {
                self.start_block();
                self.table = Some(Table {
                    alignments,
                    ..Table::default()
                });
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_head = true;
                }
            }
            Tag::TableRow => {}
            Tag::TableCell => {}
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::Image { .. }
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                self.needs_blank = true;
            }
            TagEnd::Heading(_) => {
                self.pop_style();
                self.flush();
                self.needs_blank = true;
            }
            TagEnd::BlockQuote => {
                self.flush();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.needs_blank = true;
            }
            TagEnd::CodeBlock => self.end_code_block(),
            TagEnd::List(_) => {
                self.lists.pop();
                self.needs_blank = true;
            }
            TagEnd::Item => {
                self.flush();
                self.items.pop();
                self.needs_blank = false;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link => {
                self.pop_style();
                if let Some(destination) = self.link.take() {
                    self.push(" (");
                    self.push_styled(
                        &destination,
                        current()
                            .style(Role::Info)
                            .add_modifier(Modifier::UNDERLINED),
                    );
                    self.push(")");
                }
            }
            TagEnd::Table => self.end_table(),
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push((row, true));
                    table.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push((row, table.in_head));
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.row.push(std::mem::take(&mut table.cell));
                }
            }
            TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::Image
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn start_block(&mut self) {
        self.flush();
        if self.needs_blank && self.items.is_empty() && !self.lines.is_empty() {
            self.push_blank();
        }
        self.needs_blank = false;
    }

    fn start_item(&mut self) {
        self.flush();
        let depth = self.lists.len().max(1);
        let indent = " ".repeat((depth - 1) * 4);
        let marker = match self.lists.last_mut() {
            Some(Some(index)) => {
                let marker = format!("{index}. ");
                *index += 1;
                marker
            }
            Some(None) | None => "- ".into(),
        };
        self.items.push(Item {
            continuation: " ".repeat(indent.len() + marker.len()),
            first_prefix: indent + &marker,
            first_line: true,
        });
        self.needs_blank = false;
    }

    fn text(&mut self, text: &str) {
        if self.in_code_block {
            self.code_buffer.push_str(text);
            return;
        }
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.flush();
            }
            if !part.is_empty() {
                self.push(part);
            }
        }
    }

    fn push(&mut self, text: &str) {
        self.push_styled(text, Style::default());
    }

    fn push_styled(&mut self, text: &str, style: Style) {
        let style = self.styles.last().copied().unwrap_or_default().patch(style);
        if let Some(table) = self.table.as_mut() {
            table.cell.push(Span::styled(text.to_string(), style));
            return;
        }
        self.ensure_line();
        if let Some(line) = self.current.as_mut() {
            line.push_span(Span::styled(text.to_string(), style));
        }
    }

    fn ensure_line(&mut self) {
        if self.current.is_some() {
            return;
        }
        let style = if self.blockquote_depth > 0 {
            self.base.patch(current().style(Role::Muted))
        } else {
            self.base
        };
        let mut line = Line::default().style(style);
        for _ in 0..self.blockquote_depth {
            line.push_span(Span::styled("> ", current().style(Role::Neutral)));
        }
        if let Some(item) = self.items.last_mut() {
            let prefix = if std::mem::take(&mut item.first_line) {
                &item.first_prefix
            } else {
                &item.continuation
            };
            line.push_span(Span::raw(prefix.clone()));
        }
        if self.in_code_block {
            line.push_span(Span::styled("│ ", current().style(Role::Border)));
        }
        self.current = Some(line);
    }

    fn flush(&mut self) {
        if let Some(line) = self.current.take()
            && !line.spans.is_empty()
        {
            let mut continuation = "> ".repeat(self.blockquote_depth);
            if let Some(item) = self.items.last() {
                continuation.push_str(&item.continuation);
            }
            if self.in_code_block {
                continuation.push_str("│ ");
            }
            self.lines.extend(wrap_line(
                line,
                self.width,
                &continuation,
                self.in_code_block,
            ));
        }
    }

    fn push_blank(&mut self) {
        if self.lines.last().is_none_or(|line| !line.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }

    fn push_style(&mut self, style: Style) {
        let current = self.styles.last().copied().unwrap_or_default();
        self.styles.push(current.patch(style));
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn end_code_block(&mut self) {
        let code = std::mem::take(&mut self.code_buffer);
        let lines = super::highlight::lines(&code, &self.code_language).unwrap_or_else(|| {
            code.lines()
                .map(|line| vec![Span::styled(line.to_owned(), current().style(Role::Code))])
                .collect()
        });
        for spans in lines {
            self.ensure_line();
            if let Some(line) = self.current.as_mut() {
                line.spans.extend(spans);
            }
            self.flush();
        }
        self.in_code_block = false;
        self.needs_blank = true;
    }

    fn end_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let widths: Vec<usize> = (0..table.alignments.len())
            .map(|column| {
                table
                    .rows
                    .iter()
                    .filter_map(|(row, _)| row.get(column))
                    .map(|cell| cell.iter().map(Span::width).sum())
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let prefix_width =
            self.blockquote_depth * 2 + self.items.last().map_or(0, |item| item.continuation.len());
        let stacked =
            widths.iter().sum::<usize>() + widths.len().saturating_sub(1) * 3 + prefix_width
                > self.width;
        if stacked && table.rows.len() > 1 {
            for (row, header) in &table.rows {
                if *header {
                    continue;
                }
                for (column, cell) in row.iter().enumerate() {
                    if let Some(label) = table.rows.first().and_then(|(row, _)| row.get(column)) {
                        for span in label {
                            self.push_styled(
                                &span.content,
                                span.style.add_modifier(Modifier::BOLD),
                            );
                        }
                        self.push(": ");
                    }
                    for span in cell {
                        self.push_styled(&span.content, span.style);
                    }
                    self.flush();
                }
                self.push_blank();
            }
        } else {
            for (row, header) in table.rows {
                for (column, cell) in row.into_iter().enumerate() {
                    if column > 0 {
                        self.push_styled(" │ ", current().style(Role::Border));
                    }
                    let padding = widths[column].saturating_sub(cell.iter().map(Span::width).sum());
                    let left = match table.alignments[column] {
                        Alignment::Right => padding,
                        Alignment::Center => padding / 2,
                        _ => 0,
                    };
                    self.push(&" ".repeat(left));
                    for span in cell {
                        let style = if header {
                            span.style.add_modifier(Modifier::BOLD)
                        } else {
                            span.style
                        };
                        self.push_styled(&span.content, style);
                    }
                    self.push(&" ".repeat(padding - left));
                }
                self.flush();
                if header {
                    self.push_styled(
                        &widths
                            .iter()
                            .map(|width| "─".repeat(*width))
                            .collect::<Vec<_>>()
                            .join("─┼─"),
                        current().style(Role::Border),
                    );
                    self.flush();
                }
            }
        }
        self.needs_blank = true;
    }
}

fn wrap_line(
    line: Line<'static>,
    width: usize,
    continuation: &str,
    hard: bool,
) -> Vec<Line<'static>> {
    if line.width() <= width {
        return vec![line];
    }
    let graphemes: Vec<_> = line.styled_graphemes(line.style).collect();
    let continuation = if Span::raw(continuation).width() < width {
        continuation
    } else {
        ""
    };
    let mut result = Vec::new();
    let mut start = 0;
    while start < graphemes.len() {
        let prefix = if start == 0
            || Span::raw(continuation).width() + Span::raw(graphemes[start].symbol).width() > width
        {
            ""
        } else {
            continuation
        };
        let content_start = if start == 0 {
            continuation.chars().count()
        } else {
            start
        };
        let mut used = Span::raw(prefix).width();
        let mut end = start;
        let mut word_break = None;
        while end < graphemes.len() {
            let grapheme = &graphemes[end];
            let next = used + Span::raw(grapheme.symbol).width();
            if next > width {
                break;
            }
            used = next;
            if !hard && grapheme.is_whitespace() && end > content_start {
                word_break = Some(end);
            }
            end += 1;
        }
        if end < graphemes.len()
            && !graphemes[end].is_whitespace()
            && let Some(boundary) = word_break
        {
            end = boundary;
        }
        // A terminal narrower than one wide grapheme must still make progress.
        end = end.max(start + 1);
        let mut wrapped = Line::styled(prefix.to_owned(), line.style);
        for grapheme in &graphemes[start..end] {
            if let Some(last) = wrapped.spans.last_mut()
                && last.style == grapheme.style
            {
                last.content.to_mut().push_str(grapheme.symbol);
            } else {
                wrapped.push_span(Span::styled(grapheme.symbol.to_owned(), grapheme.style));
            }
        }
        result.push(wrapped);
        start = end;
        if !hard {
            while start < graphemes.len() && graphemes[start].is_whitespace() {
                start += 1;
            }
        }
    }
    result
}

fn heading_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 => Style::default()
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::UNDERLINED),
        HeadingLevel::H2 => Style::default().add_modifier(Modifier::BOLD),
        HeadingLevel::H3 => Style::default()
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::ITALIC),
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => {
            Style::default().add_modifier(Modifier::ITALIC)
        }
    }
}

#[cfg(test)]
#[path = "markdown_tests.rs"]
mod tests;
