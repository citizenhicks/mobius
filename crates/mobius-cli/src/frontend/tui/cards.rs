//! Decision cards drawn under the chat: a dotted braille rim with a dithered glow in the top-right
//! corner, no fill, so they read on any terminal background.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{TuiState, dither};
use crate::frontend::terminal::terminal_text;
use crate::frontend::theme::{Role, current};
use mobius::protocol::{ExecApprovalRequestEvent, FrontendActionListItem};

/// The widest a card grows, so wide terminals keep it a readable block.
const MAX_CARD_WIDTH: u16 = 64;
const GLOW_COLUMNS: u16 = 10;
const GLOW_ROWS: u16 = 3;
/// Preview lines an approval card shows for one call before folding the rest.
const MAX_APPROVAL_PREVIEW_LINES: usize = 8;

/// The front approval request: each call by its tool's own preview, then the choices in a row.
pub(super) fn push_approval_card(
    lines: &mut Vec<Line<'static>>,
    state: &TuiState,
    request: &ExecApprovalRequestEvent,
    width: u16,
) {
    let theme = current();
    let mut body = Vec::new();
    for (index, call) in request.calls.iter().enumerate() {
        let (title, preview) = call.preview.as_ref().map_or_else(
            || (call.name.to_string(), call.arguments.to_string()),
            |preview| (preview.title.clone(), preview.text.clone()), // rules-allow: the card owns display copies of the preview
        );
        let mut heading = Vec::new();
        if index == 0 {
            heading.push(Span::styled(" ALLOW ", theme.chip(Role::Warning)));
            heading.push(Span::raw(" "));
        } else {
            heading.push(Span::raw("        "));
        }
        heading.push(Span::styled(
            terminal_text(&title),
            theme.style(Role::Text).add_modifier(Modifier::BOLD),
        ));
        body.push(Line::from(heading));
        let preview = terminal_text(&preview);
        let mut preview_lines = preview.lines();
        for line in preview_lines.by_ref().take(MAX_APPROVAL_PREVIEW_LINES) {
            body.push(Line::styled(line.to_owned(), theme.style(Role::Code)));
        }
        let hidden = preview_lines.count();
        if hidden > 0 {
            body.push(Line::styled(
                format!("… +{hidden} lines"),
                theme.style(Role::Muted),
            ));
        }
    }
    let mut reason = terminal_text(&request.reason);
    if state.approvals.len() > 1 {
        reason = format!("{reason} · 1 of {} pending", state.approvals.len());
    }
    body.push(Line::styled(reason, theme.style(Role::Muted)));
    body.push(Line::default());
    let mut choices = Vec::new();
    for (index, label) in super::input::APPROVAL_CHOICES.iter().enumerate() {
        if index > 0 {
            choices.push(Span::raw("  "));
        }
        let stop = index + 1 == super::input::APPROVAL_CHOICES.len();
        let key_role = if stop { Role::Error } else { Role::Accent };
        if index == state.approval_selection {
            choices.push(Span::styled(
                format!(" {} {label} ", index + 1),
                theme.chip(key_role),
            ));
        } else {
            choices.push(Span::styled(
                format!("{}", index + 1),
                theme.style(key_role).add_modifier(Modifier::BOLD),
            ));
            choices.push(Span::styled(
                format!(" {label}"),
                theme.style(if stop { Role::Error } else { Role::Text }),
            ));
        }
    }
    body.push(Line::from(choices));
    push_card(lines, body, Role::Warning, width);
    lines.push(Line::styled(
        "  ←→ choose · Enter confirm · 1-4 pick · or type why not",
        theme.style(Role::Muted),
    ));
}

/// An open capability question with its choices numbered for a single key press.
pub(super) fn push_question_card(
    lines: &mut Vec<Line<'static>>,
    state: &TuiState,
    question: &FrontendActionListItem,
    width: u16,
) {
    let theme = current();
    let mut body = vec![Line::from(vec![
        Span::styled(" ASK ", theme.chip(Role::Accent)),
        Span::raw(" "),
        Span::styled(
            terminal_text(&question.text),
            theme.style(Role::Text).add_modifier(Modifier::BOLD),
        ),
    ])];
    let mut choices = Vec::new();
    for (index, action) in question.actions.iter().enumerate() {
        if index > 0 {
            choices.push(Span::raw("  "));
        }
        choices.push(Span::styled(
            format!("{}", index + 1),
            theme.style(Role::Accent).add_modifier(Modifier::BOLD),
        ));
        choices.push(Span::styled(
            format!(" {}", terminal_text(&action.label)),
            theme.style(Role::Text),
        ));
    }
    body.push(Line::from(choices));
    push_card(lines, body, Role::Accent, width);
    lines.push(if state.question_answer.is_some() {
        Line::styled(
            "  type your answer and press Enter · Esc cancels",
            theme.style(Role::Warning),
        )
    } else {
        Line::styled(
            "  press a number to answer, or keep chatting",
            theme.style(Role::Muted),
        )
    });
}

/// Frames `body` on a dotted rail of `role` colour, between top and bottom rims that fade out to
/// the right, with a glow in the top-right corner. The card stays open on the right, so a row
/// that runs long never misaligns an edge; rows are still cut at the card's width.
fn push_card(lines: &mut Vec<Line<'static>>, body: Vec<Line<'static>>, role: Role, width: u16) {
    let theme = current();
    let rim = theme.style(role);
    let width = width.min(MAX_CARD_WIDTH);
    // The rail and its space come before the content.
    let Some(inner) = usize::from(width)
        .checked_sub(2)
        .filter(|inner| *inner > usize::from(GLOW_COLUMNS))
    else {
        lines.extend(body);
        return;
    };
    let glow_width = usize::from(GLOW_COLUMNS);
    let rim_width = width.saturating_sub(1);
    lines.push(Line::styled(format!("⢀{}", dither::rim(rim_width, 3)), rim));
    for (row, line) in (0_u16..).zip(body) {
        // The glow gives way to any row that needs the full width.
        let glow = (row < GLOW_ROWS && line.width() + glow_width < inner)
            .then(|| dither::corner_glow(GLOW_COLUMNS, row, GLOW_ROWS));
        let mut spans = vec![Span::styled("⢸ ", rim)];
        match glow {
            Some(glow) => {
                let room = inner - glow_width;
                let used = line.width();
                spans.extend(line.spans);
                spans.push(Span::raw(" ".repeat(room - used)));
                spans.push(Span::styled(glow, rim));
            }
            None => spans.extend(fit(line, inner).spans),
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::styled(format!("⠈{}", dither::rim(rim_width, 0)), rim));
}

/// `line` cut to at most `width` display columns.
fn fit(line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line;
    }
    let mut used = 0;
    let mut spans = Vec::new();
    for span in line.spans {
        let style: Style = span.style;
        let mut kept = String::new();
        for character in span.content.chars() {
            let cell = Span::raw(character.to_string()).width();
            if used + cell > width.saturating_sub(1) {
                break;
            }
            used += cell;
            kept.push(character);
        }
        let truncated = kept.len() < span.content.len();
        spans.push(Span::styled(kept, style));
        if truncated {
            spans.push(Span::styled("…", style));
            break;
        }
    }
    Line::from(spans)
}
