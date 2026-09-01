use diffy::Line as DiffLine;
use diffy::Patch;
use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Block;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;

use super::PreviewContent;
use super::TranscriptEntry;
use super::TranscriptTone;
use super::TuiState;
use super::attachment_label;
use super::highlight;
use super::markdown;
use super::shimmer;
use crate::frontend::catalog::MenuItem;
use crate::frontend::catalog::UiCatalog;
use crate::frontend::dashboard::centered_area;
use crate::frontend::terminal::terminal_text;
use crate::frontend::theme::Role;
use crate::frontend::theme::current;
use mobius::protocol::ActiveMessageDelivery;
use mobius::protocol::FrontendBlockFormat;
use mobius::protocol::FrontendSlot;
use mobius::protocol::FrontendTone;
use mobius::protocol::FrontendWidget;

const MAX_MENU_ROWS: usize = 6;
const COMPOSER_PROMPT: &str = "» ";
const AGENT_MARKER: &str = "• ";

pub(super) fn render(frame: &mut Frame<'_>, state: &mut TuiState, catalog: &UiCatalog) {
    let theme = current();
    frame.render_widget(
        Block::default().style(theme.style(Role::Canvas)),
        frame.area(),
    );
    let reference_suggestions = state
        .picker
        .is_none()
        .then(|| {
            state
                .reference_suggestions(catalog)
                .map(|(_, matches)| matches)
        })
        .flatten();
    let slash_suggestions = (state.picker.is_none() && reference_suggestions.is_none())
        .then(|| catalog.command_suggestions(&state.input, state.cursor))
        .flatten();
    let menu_height = reference_suggestions
        .as_ref()
        .map(Vec::len)
        .or_else(|| slash_suggestions.as_ref().map(Vec::len))
        .map_or(0, |length| {
            u16::try_from(length.clamp(1, MAX_MENU_ROWS)).unwrap_or(0)
        });
    let (input, cursor_end) = marked_input(state);
    let inner_width = frame.area().width.saturating_sub(2).max(1);
    let input_rows = Paragraph::new(input.as_str())
        .wrap(Wrap { trim: false })
        .line_count(inner_width);
    let max_composer_height = frame
        .area()
        .height
        .saturating_sub(menu_height.saturating_add(3))
        .max(3);
    let composer_height = u16::try_from(input_rows)
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .clamp(3, max_composer_height);
    let input_row = input_cursor_row(&input[..cursor_end], inner_width);
    let areas = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(menu_height),
        Constraint::Length(1),
        Constraint::Length(composer_height),
        Constraint::Length(1),
    ])
    .split(frame.area());

    render_transcript(frame, state, areas[0]);
    if let Some(suggestions) = reference_suggestions {
        state.reference_selection = state
            .reference_selection
            .min(suggestions.len().saturating_sub(1));
        render_menu(frame, areas[1], &suggestions, state.reference_selection);
    } else if let Some(suggestions) = slash_suggestions {
        state.slash_selection = state
            .slash_selection
            .min(suggestions.len().saturating_sub(1));
        render_menu(frame, areas[1], &suggestions, state.slash_selection);
    }
    frame.render_widget(Paragraph::new(composer_header_line(state)), areas[2]);
    frame.render_widget(
        Paragraph::new(input)
            .style(theme.style(Role::Text))
            .block(
                Block::bordered()
                    .border_style(theme.style(Role::Border))
                    .title(composer_title(state)),
            )
            .scroll((
                input_row.saturating_sub(composer_height.saturating_sub(3)),
                0,
            ))
            .wrap(Wrap { trim: false }),
        areas[3],
    );
    render_footer(frame, state, areas[4]);
    if let Some(picker) = state.picker.as_mut() {
        picker.selected = picker.selected.min(picker.options.len().saturating_sub(1));
        render_picker_popup(frame, picker);
    }
}

fn render_transcript(frame: &mut Frame<'_>, state: &mut TuiState, area: Rect) {
    let lines = live_transcript_lines(state, 0, area.width);
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let rendered_lines = paragraph.line_count(area.width);
    state
        .transcript_viewport
        .update(rendered_lines, usize::from(area.height));
    let scroll = state
        .transcript_viewport
        .effective_scroll()
        .min(usize::from(u16::MAX)) as u16;
    frame.render_widget(paragraph.scroll((scroll, 0)), area);
}

pub(super) fn live_transcript_lines(
    state: &mut TuiState,
    start: usize,
    width: u16,
) -> Vec<Line<'static>> {
    let previous_group = start
        .checked_sub(1)
        .and_then(|index| state.transcript.get(index))
        .and_then(|entry| entry.group.clone());
    let mut lines = transcript_lines(
        state.transcript.iter_mut().skip(start),
        width,
        previous_group,
        start > 0,
    );
    if !state.streaming.is_empty() {
        push_lines(
            &mut lines,
            &state.streaming,
            TranscriptTone::Assistant,
            FrontendBlockFormat::PlainText,
            width,
        );
    }
    if !state.reasoning.is_empty() {
        push_lines(
            &mut lines,
            &state.reasoning,
            TranscriptTone::Reasoning,
            FrontendBlockFormat::PlainText,
            width,
        );
    }
    for ((capability, _), item) in state
        .widgets
        .iter()
        .filter(|(_, item)| item.slot == FrontendSlot::TranscriptTail)
    {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        let heading = item.symbol.as_ref().map_or_else(
            || sentence_case(capability),
            |symbol| sentence_case(symbol.as_str()),
        );
        lines.push(Line::from(vec![
            Span::styled("┊ ", current().style(Role::Muted)),
            Span::styled(
                heading,
                current().style(Role::Muted).add_modifier(Modifier::ITALIC),
            ),
        ]));
        let style = current().style(tone_role(item.tone));
        lines.extend(item.text.split('\n').map(|line| {
            Line::from(vec![
                Span::styled("┊ ", current().style(Role::Muted)),
                Span::styled(line.to_owned(), style),
            ])
        }));
    }
    if lines.is_empty() {
        let card = responsive_welcome_card(state, width);
        push_lines(
            &mut lines,
            &card,
            TranscriptTone::Welcome,
            FrontendBlockFormat::PlainText,
            width,
        );
    }
    lines
}

pub(super) fn render_preview(frame: &mut Frame<'_>, state: &mut TuiState) {
    let theme = current();
    let area = centered_area(frame.area(), 92, 88);
    if area.width < 3 || area.height < 3 {
        return;
    }
    frame.render_widget(Clear, area);
    let (title, live) = {
        let Some(preview) = state.preview.as_ref() else {
            return;
        };
        let has_older = matches!(&preview.content, PreviewContent::Snapshot(snapshot) if snapshot.next.is_some());
        let older_hint = if has_older { " · O older" } else { "" };
        let title = if preview.subtitle.is_empty() {
            format!("{}{older_hint}", preview.title)
        } else {
            format!("{}{older_hint} · {}", preview.title, preview.subtitle)
        };
        (
            title,
            matches!(&preview.content, PreviewContent::LiveTranscript),
        )
    };
    let block = Block::bordered()
        .style(theme.style(Role::Canvas))
        .border_style(theme.style(Role::Info))
        .title(Line::styled(
            format!(" {title} · ↑↓/PgUp/PgDn scroll · drag to copy · Esc/Ctrl+T close "),
            theme.style(Role::Accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    let mut lines = if live {
        live_transcript_lines(state, 0, inner.width)
    } else if let Some(PreviewContent::Snapshot(snapshot)) =
        state.preview.as_mut().map(|preview| &mut preview.content)
    {
        transcript_lines(snapshot.transcript.iter_mut(), inner.width, None, false)
    } else {
        Vec::new()
    };
    if lines.is_empty() {
        lines.push(Line::styled(
            "No transcript events.",
            theme.style(Role::Muted).add_modifier(Modifier::ITALIC),
        ));
    }
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let rendered_lines = paragraph.line_count(inner.width);
    let preview = state.preview.as_mut().expect("preview checked");
    preview
        .viewport
        .update(rendered_lines, usize::from(inner.height));
    let scroll = preview
        .viewport
        .effective_scroll()
        .min(usize::from(u16::MAX)) as u16;

    frame.render_widget(paragraph.block(block).scroll((scroll, 0)), area);
}

fn transcript_lines<'a>(
    entries: impl Iterator<Item = &'a mut TranscriptEntry>,
    width: u16,
    mut previous_group: Option<super::BlockKey>,
    mut has_previous: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in entries {
        let grouped = entry.group.is_some() && entry.group == previous_group;
        if has_previous && !grouped {
            if matches!(entry.tone, TranscriptTone::User) {
                lines.push(Line::styled(
                    "─".repeat(usize::from(width)),
                    current().style(Role::Border),
                ));
            } else {
                lines.push(Line::default());
            }
        }
        if entry
            .rendered
            .as_ref()
            .is_none_or(|(cached_width, _)| *cached_width != width)
        {
            let text = if matches!(entry.tone, TranscriptTone::Welcome)
                && entry
                    .text
                    .lines()
                    .any(|line| Line::from(line).width() > usize::from(width))
            {
                "MÖBIUS · type / for commands"
            } else {
                &entry.text
            };
            let mut rendered = Vec::new();
            if entry.role.is_some() {
                push_block_lines(&mut rendered, entry, width);
            } else {
                push_lines(&mut rendered, text, entry.tone, entry.format, width);
            }
            entry.rendered = Some((width, rendered));
        }
        if let Some((_, rendered)) = &entry.rendered {
            lines.extend(rendered.iter().cloned());
        }
        previous_group.clone_from(&entry.group);
        has_previous = true;
    }
    lines
}

fn push_block_lines(lines: &mut Vec<Line<'static>>, entry: &TranscriptEntry, width: u16) {
    if entry.format == FrontendBlockFormat::UnifiedDiff
        && push_unified_diff(lines, &entry.text, usize::from(width))
    {
        return;
    }
    let Some(title) = entry.title.as_deref().filter(|title| !title.is_empty()) else {
        push_lines(lines, &entry.text, entry.tone, entry.format, width);
        return;
    };
    let theme = current();
    let marker_role = if entry.pending {
        Role::Accent
    } else {
        transcript_role(entry.tone)
    };
    let detail = entry.detail.as_deref();
    let inline_detail = detail.is_some_and(|detail| {
        !detail.contains('\n')
            && Line::from(format!("{AGENT_MARKER}{title} {detail}")).width() <= usize::from(width)
    });
    let mut header = vec![
        Span::styled(
            AGENT_MARKER,
            theme.style(marker_role).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            title.to_owned(),
            theme.style(Role::Text).add_modifier(Modifier::BOLD),
        ),
    ];
    if inline_detail {
        header.push(Span::raw(" "));
        header.push(Span::styled(
            detail.unwrap_or_default().to_owned(),
            theme.style(Role::Code),
        ));
    }
    lines.push(Line::from(header));

    let mut first = true;
    if let Some(detail) = detail.filter(|_| !inline_detail) {
        for line in detail.split('\n') {
            push_tree_line(lines, &mut first, line, theme.style(Role::Code));
        }
    }
    let output_style = theme
        .style(if matches!(entry.tone, TranscriptTone::Error) {
            Role::Error
        } else {
            Role::Text
        })
        .add_modifier(Modifier::DIM);
    if !entry.text.is_empty() {
        for line in entry.text.split('\n') {
            push_tree_line(lines, &mut first, line, output_style);
        }
    }
}

fn push_tree_line(lines: &mut Vec<Line<'static>>, first: &mut bool, text: &str, style: Style) {
    lines.push(Line::from(vec![
        Span::styled(if *first { "  └ " } else { "    " }, style),
        Span::styled(text.to_owned(), style),
    ]));
    *first = false;
}

fn push_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    tone: TranscriptTone,
    format: FrontendBlockFormat,
    width: u16,
) {
    if format == FrontendBlockFormat::UnifiedDiff
        && push_unified_diff(lines, text, usize::from(width))
    {
        return;
    }
    let theme = current();
    let mut style = theme.style(transcript_role(tone));
    if matches!(tone, TranscriptTone::Reasoning) {
        style = style.add_modifier(Modifier::ITALIC);
    } else if matches!(tone, TranscriptTone::Welcome) {
        style = style.add_modifier(Modifier::BOLD);
    }
    let first = lines.len();
    if matches!(tone, TranscriptTone::Assistant | TranscriptTone::Reasoning) {
        lines.extend(markdown::render(text, style));
    } else {
        lines.extend(text.split('\n').map(|line| {
            line.strip_prefix(AGENT_MARKER).map_or_else(
                || Line::styled(line.to_string(), style),
                |content| {
                    Line::from(vec![
                        Span::styled(AGENT_MARKER, theme.style(Role::Accent)),
                        Span::styled(content.to_string(), style),
                    ])
                },
            )
        }));
    }
    if matches!(tone, TranscriptTone::Assistant | TranscriptTone::Reasoning) {
        for (index, line) in lines[first..].iter_mut().enumerate() {
            line.spans.insert(
                0,
                Span::styled(
                    if index == 0 { AGENT_MARKER } else { "  " },
                    theme.style(Role::Accent),
                ),
            );
        }
    }
}

fn push_unified_diff(lines: &mut Vec<Line<'static>>, text: &str, width: usize) -> bool {
    let Ok(patch) = Patch::from_str(text) else {
        return false;
    };
    let theme = current();
    let raw_path = patch
        .modified()
        .or_else(|| patch.original())
        .unwrap_or("file");
    let path = terminal_text(raw_path);
    let (added, removed, max_line_number) =
        patch
            .hunks()
            .iter()
            .fold((0, 0, 0), |(added, removed, max), hunk| {
                let mut old_line = hunk.old_range().start();
                let mut new_line = hunk.new_range().start();
                let mut added = added;
                let mut removed = removed;
                let mut max = max;
                for line in hunk.lines() {
                    match line {
                        DiffLine::Insert(_) => {
                            added += 1;
                            max = max.max(new_line);
                            new_line += 1;
                        }
                        DiffLine::Delete(_) => {
                            removed += 1;
                            max = max.max(old_line);
                            old_line += 1;
                        }
                        DiffLine::Context(_) => {
                            max = max.max(new_line);
                            old_line += 1;
                            new_line += 1;
                        }
                    }
                }
                (added, removed, max)
            });
    lines.push(Line::from(vec![
        Span::styled(AGENT_MARKER, theme.style(Role::Accent)),
        Span::styled(
            "Edited ",
            theme.style(Role::Text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(path, theme.style(Role::Code)),
        Span::raw(" ("),
        Span::styled(format!("+{added}"), theme.style(Role::Success)),
        Span::raw(" "),
        Span::styled(format!("-{removed}"), theme.style(Role::Error)),
        Span::raw(")"),
    ]));

    let number_width = max_line_number.max(1).to_string().len();
    for (hunk_index, hunk) in patch.hunks().iter().enumerate() {
        if hunk_index > 0 {
            lines.push(Line::styled(
                format!("    {:>number_width$} ⋮", ""),
                theme.style(Role::Muted),
            ));
        }
        let mut old_line = hunk.old_range().start();
        let mut new_line = hunk.new_range().start();
        let hunk_text = hunk
            .lines()
            .iter()
            .map(|line| match line {
                DiffLine::Insert(content)
                | DiffLine::Delete(content)
                | DiffLine::Context(content) => *content,
            })
            .collect::<String>();
        let syntax = highlight::lines(&hunk_text, raw_path)
            .filter(|syntax| syntax.len() == hunk.lines().len());
        for (index, line) in hunk.lines().iter().enumerate() {
            let (number, sign, sign_role, background, content) = match line {
                DiffLine::Insert(content) => {
                    let number = new_line;
                    new_line += 1;
                    (
                        number,
                        "+",
                        Role::Success,
                        Some(theme.diff_add_background()),
                        content,
                    )
                }
                DiffLine::Delete(content) => {
                    let number = old_line;
                    old_line += 1;
                    (
                        number,
                        "-",
                        Role::Error,
                        Some(theme.diff_delete_background()),
                        content,
                    )
                }
                DiffLine::Context(content) => {
                    let number = new_line;
                    old_line += 1;
                    new_line += 1;
                    (number, " ", Role::Text, None, content)
                }
            };
            let mut spans = vec![
                Span::styled(
                    format!("    {number:>number_width$} "),
                    theme.style(Role::Muted),
                ),
                Span::styled(sign, theme.style(sign_role)),
            ];
            if let Some(syntax) = syntax
                .as_ref()
                .and_then(|syntax_lines| syntax_lines.get(index))
            {
                spans.extend(syntax.iter().cloned());
            } else {
                spans.push(Span::styled(
                    content.trim_end_matches(['\n', '\r']).to_string(),
                    theme.style(Role::Text),
                ));
            }
            let mut line = Line::from(spans);
            let padding = width.saturating_sub(line.width());
            if padding > 0 {
                line.push_span(Span::raw(" ".repeat(padding)));
            }
            if let Some(background) = background {
                line = line.style(Style::default().bg(background));
            }
            lines.push(line);
        }
    }
    true
}

fn transcript_role(tone: TranscriptTone) -> Role {
    match tone {
        TranscriptTone::Welcome => Role::Accent,
        TranscriptTone::Assistant | TranscriptTone::User => Role::Text,
        TranscriptTone::Reasoning => Role::Reasoning,
        TranscriptTone::Neutral => Role::Neutral,
        TranscriptTone::Success => Role::Success,
        TranscriptTone::Warning => Role::Warning,
        TranscriptTone::Error => Role::Error,
    }
}

pub(super) fn welcome_card(state: &TuiState) -> String {
    bordered_card(state.agent_summary.lines().map(str::to_owned).collect())
}

fn responsive_welcome_card(state: &TuiState, width: u16) -> String {
    let welcome = welcome_card(state);
    if card_fits(&welcome, width) {
        return welcome;
    }
    let compact = bordered_card(
        state
            .agent_summary
            .lines()
            .take(2)
            .map(str::to_owned)
            .collect(),
    );
    if card_fits(&compact, width) {
        compact
    } else {
        "MÖBIUS · type / for commands".into()
    }
}

fn card_fits(card: &str, width: u16) -> bool {
    card.lines()
        .all(|line| Line::from(line).width() <= usize::from(width))
}

fn bordered_card(rows: Vec<String>) -> String {
    let width = rows
        .iter()
        .map(|row| Line::from(row.as_str()).width())
        .max()
        .unwrap_or_default();
    let border = "─".repeat(width + 2);
    let mut lines = vec![format!("╭{border}╮")];
    lines.extend(rows.into_iter().map(|row| {
        let padding = width.saturating_sub(Line::from(row.as_str()).width());
        format!("│ {row}{} │", " ".repeat(padding))
    }));
    lines.push(format!("╰{border}╯"));
    lines.join("\n")
}

fn tone_role(tone: FrontendTone) -> Role {
    match tone {
        FrontendTone::Neutral => Role::Neutral,
        FrontendTone::Success => Role::Success,
        FrontendTone::Warning => Role::Warning,
        FrontendTone::Error => Role::Error,
    }
}

fn marked_input(state: &TuiState) -> (String, usize) {
    let (mut input, cursor) = state.visible_input();
    input.insert(cursor, '█');
    let mut marked = state
        .attachments
        .iter()
        .map(attachment_label)
        .collect::<Vec<_>>()
        .join(" · ");
    if !marked.is_empty() {
        marked.push('\n');
    }
    marked.push_str(COMPOSER_PROMPT);
    let cursor_end = marked.len() + cursor + '█'.len_utf8();
    marked.push_str(&input);
    (marked, cursor_end)
}

fn input_cursor_row(input_through_cursor: &str, width: u16) -> u16 {
    let rows = Paragraph::new(input_through_cursor)
        .wrap(Wrap { trim: false })
        .line_count(width.max(1));
    u16::try_from(rows.saturating_sub(1)).unwrap_or(u16::MAX)
}

fn render_footer(frame: &mut Frame<'_>, state: &TuiState, area: Rect) {
    frame.render_widget(
        Paragraph::new(footer_line(state, area.width)).alignment(Alignment::Right),
        area,
    );
}

fn footer_line(state: &TuiState, width: u16) -> Line<'static> {
    let theme = current();
    let reasoning = state.model.reasoning_effort.as_deref().unwrap_or("—");
    let context = state
        .usage
        .context_fill
        .map_or_else(|| "—".into(), |value| format!("{value:.1}%"));
    let cache = state
        .usage
        .cache_hit
        .map_or_else(|| "—".into(), |value| format!("{value:.1}%"));
    let folder = state
        .cwd
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(&state.cwd);
    let values = [
        (format!("cache {cache}"), Role::Neutral),
        (format!("context {context}"), Role::Code),
        (
            format!(
                "{} {}",
                display_value(&state.model.model),
                display_value(reasoning)
            ),
            Role::Reasoning,
        ),
        (display_value(folder), Role::Info),
    ];
    let mut widget_spans = widget_line(&state.widgets, FrontendSlot::Header).spans;
    let footer_widgets = widget_line(&state.widgets, FrontendSlot::ComposerFooter);
    if !footer_widgets.spans.is_empty() {
        separator(&mut widget_spans);
        widget_spans.extend(footer_widgets.spans);
    }
    let mut spans = widget_spans.clone();
    for (value, role) in &values {
        separator(&mut spans);
        spans.push(Span::styled(value.clone(), theme.style(*role)));
    }
    let full = Line::from(spans);
    if full.width() <= usize::from(width) {
        return full;
    }

    let mut spans = widget_spans;
    for (value, role) in [&values[2], &values[1], &values[3]] {
        let mut candidate = spans.clone();
        separator(&mut candidate);
        candidate.push(Span::styled(value.clone(), theme.style(*role)));
        if Line::from(candidate.clone()).width() <= usize::from(width) {
            spans = candidate;
        }
    }
    if spans.is_empty() {
        Line::styled(values[2].0.clone(), theme.style(values[2].1))
    } else {
        Line::from(spans)
    }
}

fn widget_line(
    widgets: &[((String, String), FrontendWidget)],
    slot: FrontendSlot,
) -> Line<'static> {
    let theme = current();
    let mut spans = Vec::new();
    for (_, item) in widgets.iter().filter(|(_, item)| item.slot == slot) {
        separator(&mut spans);
        let style = if slot == FrontendSlot::ComposerHeader && item.tone == FrontendTone::Neutral {
            theme.style(Role::Muted).add_modifier(Modifier::ITALIC)
        } else {
            theme.style(tone_role(item.tone))
        };
        spans.push(Span::styled(item.text.clone(), style));
    }
    Line::from(spans)
}

fn composer_header_line(state: &TuiState) -> Line<'static> {
    let mut line = status_line(state);
    let widgets = widget_line(&state.widgets, FrontendSlot::ComposerHeader);
    if line.width() > 0 && widgets.width() > 0 {
        line.push_span(Span::styled(" · ", current().style(Role::Muted)));
    }
    for span in widgets.spans {
        line.push_span(span);
    }
    line
}

fn separator(spans: &mut Vec<Span<'static>>) {
    if !spans.is_empty() {
        spans.push(Span::styled(" · ", current().style(Role::Muted)));
    }
}

fn composer_title(state: &TuiState) -> Line<'static> {
    let theme = current();
    let (status, role) = if state.approval.is_some() {
        ("approval".to_string(), Role::Warning)
    } else if state.disconnected {
        ("disconnected".to_string(), Role::Error)
    } else if state.is_working() {
        ("working".to_string(), Role::Accent)
    } else {
        ("ready".to_string(), Role::Accent)
    };
    let elapsed = state
        .turn_started_at
        .map(|started| format!(" · {}", elapsed_label(started.elapsed())))
        .unwrap_or_default();
    let title = format!(" mobius · {status}{elapsed} ");
    if state.is_working() {
        shimmer::line(
            &title,
            theme.color(Role::Accent),
            theme.color(Role::AccentStrong),
        )
    } else {
        Line::styled(title, theme.style(role))
    }
}

fn status_line(state: &TuiState) -> Line<'static> {
    let theme = current();
    if state.approval.is_some() {
        return Line::styled(
            "approval · y once · a session · n deny · q abort",
            theme.style(Role::Warning),
        );
    }
    if state.input_limit_reached {
        return Line::styled(
            "input limit reached · maximum 1 MiB",
            theme.style(Role::Warning),
        );
    }
    if state.is_working() {
        return Line::styled(
            format!(
                "enter {} · alt+enter {}",
                delivery_label(state.message_delivery()),
                delivery_label(state.alternate_message_delivery())
            ),
            theme.style(Role::Muted),
        );
    }
    Line::default()
}

const fn delivery_label(delivery: ActiveMessageDelivery) -> &'static str {
    match delivery {
        ActiveMessageDelivery::Steer => "steer",
        ActiveMessageDelivery::Queue => "queue",
    }
}

fn elapsed_label(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3_600, seconds / 60 % 60)
    }
}

pub(super) fn initial_widgets(catalog: &UiCatalog) -> Vec<((String, String), FrontendWidget)> {
    catalog
        .widgets()
        .map(|(middleware, item)| {
            let mut item = item.clone();
            item.text = bounded_terminal_text(&item.text, 32 * 1024);
            ((middleware.to_string(), item.id.clone()), item)
        })
        .collect()
}

pub(super) fn widget_status(widgets: &[((String, String), FrontendWidget)]) -> String {
    widgets
        .iter()
        .map(|(_, item)| format!(" · {}", item.text))
        .collect()
}

fn render_picker_menu(frame: &mut Frame<'_>, area: Rect, picker: &super::PickerState) {
    let areas = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(area);
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!("  {}", picker.title),
            current().style(Role::Accent).add_modifier(Modifier::BOLD),
        )),
        areas[0],
    );
    let items = picker
        .options
        .iter()
        .map(|option| {
            let description = if !option.shows_detail || option.detail.is_empty() {
                option.description.clone()
            } else {
                format!("{} · {}", option.description, option.detail)
            };
            MenuItem {
                value: String::new(),
                label: terminal_text(&option.label),
                description: terminal_text(&description),
            }
        })
        .collect::<Vec<_>>();
    render_menu(frame, areas[1], &items, picker.selected);
}

fn render_picker_popup(frame: &mut Frame<'_>, picker: &super::PickerState) {
    let area = centered_area(frame.area(), 86, 70);
    if area.width < 3 || area.height < 3 {
        return;
    }
    let theme = current();
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .style(theme.style(Role::Canvas))
        .border_style(theme.style(Role::Info))
        .title(Line::styled(
            " ↑↓ select · Enter open · Esc close ",
            theme.style(Role::Accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    render_picker_menu(frame, inner, picker);
}

fn render_menu(frame: &mut Frame<'_>, area: Rect, items: &[MenuItem], selected: usize) {
    let theme = current();
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "  no matches",
                theme.style(Role::Muted).add_modifier(Modifier::ITALIC),
            )),
            area,
        );
        return;
    }
    let name_width = items
        .iter()
        .map(|item| item.label.chars().count())
        .max()
        .unwrap_or_default();
    let start = selected.saturating_add(1).saturating_sub(MAX_MENU_ROWS);
    let lines = items
        .iter()
        .enumerate()
        .skip(start)
        .take(MAX_MENU_ROWS)
        .map(|(index, item)| {
            let is_selected = index == selected;
            let style = if is_selected {
                theme.style(Role::Selection)
            } else {
                theme.style(Role::Text)
            };
            let description_style = if is_selected {
                style
            } else {
                theme.style(Role::Muted)
            };
            Line::from(vec![
                Span::styled(if is_selected { "› " } else { "  " }, style),
                Span::styled(
                    format!(
                        "{:<width$}",
                        terminal_text(&item.label),
                        width = name_width + 2
                    ),
                    style,
                ),
                Span::styled(terminal_text(&item.description), description_style),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), area);
}

pub(super) fn bounded_terminal_text(value: &str, limit: usize) -> String {
    let mut value = terminal_text(value);
    if value.len() <= limit {
        return value;
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push_str("\n[display truncated]");
    value
}

fn display_value(value: &str) -> String {
    terminal_text(value).chars().take(120).collect()
}

fn sentence_case(value: &str) -> String {
    let mut value = terminal_text(value).replace(['_', '-'], " ");
    if let Some(first) = value.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    value
}
