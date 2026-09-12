use mobius_gateway::wire::{BotRecord, ReadyPayload, RoutineScheduleKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, HighlightSpacing, List, ListState, Paragraph, Wrap};

use super::form::{BotFormMode, Form};
use super::state::{
    BOT_ROWS, BotRow, BotsState, Confirmation, Page, routine_run_label, runs_for_routine,
    sessions_for_bot,
};
use crate::frontend::terminal_text;
use crate::frontend::theme::{Role, current};

pub(super) fn render(frame: &mut ratatui::Frame<'_>, state: &BotsState, gateway: &ReadyPayload) {
    let theme = current();
    frame.render_widget(
        Block::default().style(theme.style(Role::Canvas)),
        frame.area(),
    );
    let area = content_area(frame.area());
    let [header, body, notice, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(8),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("◉ ", theme.style(Role::AccentStrong)),
                Span::styled(
                    "MÖBIUS",
                    theme.style(Role::Accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" Bots", theme.style(Role::Muted)),
            ]),
            Line::styled(
                format!("  {}", terminal_text(&gateway.machine_name)),
                theme.style(Role::Muted),
            ),
        ]),
        header,
    );
    if let Some(form) = &state.form {
        render_form(frame, body, form);
    } else {
        match &state.page {
            Page::Root => render_root(frame, body, state, gateway),
            Page::Bot(id) => render_bot(frame, body, state, gateway, id),
            Page::Conversations(id) => render_conversations(frame, body, state, gateway, id),
            Page::Routines(id) => render_routines(frame, body, state, gateway, id),
            Page::Routine { routine_id, .. } => render_routine(frame, body, state, routine_id),
            Page::Runs { routine_id, .. } => render_runs(frame, body, state, routine_id),
            Page::Run { .. } => render_run(frame, body, state),
        }
    }
    render_notice(frame, notice, state);
    frame.render_widget(
        Paragraph::new(footer_text(state)).style(theme.style(Role::Muted)),
        footer,
    );
}

fn render_form(frame: &mut ratatui::Frame<'_>, area: Rect, form: &Form) {
    let (title, mut lines, error) = match form {
        Form::Bot(form) => {
            let title = match form.mode {
                BotFormMode::Create => "Create Bot",
                BotFormMode::Update { .. } => "Edit Bot",
            };
            let mut lines = vec![
                form_line("Name", &form.name.value, form.row == 0),
                form_line("Description", &form.description.value, form.row == 1),
            ];
            if let Some(prompt) = &form.prompt {
                lines.push(form_line("System prompt", "", form.row == 2));
                let role = if form.row == 2 {
                    Role::Selection
                } else {
                    Role::Text
                };
                lines.extend(
                    terminal_text(&prompt.value)
                        .split('\n')
                        .map(|line| Line::styled(format!("  {line}"), current().style(role))),
                );
            }
            lines.push(choice_line("Save", form.row == form.save_row()));
            (
                title,
                lines,
                form.error
                    .as_deref()
                    .or(form.name.error.as_deref())
                    .or(form.description.error.as_deref())
                    .or(form
                        .prompt
                        .as_ref()
                        .and_then(|prompt| prompt.error.as_deref())),
            )
        }
        Form::Routine(form) => {
            let save_row = form.save_row();
            let mut lines = vec![
                form_line("Workspace", &form.workspace.value, form.row == 0),
                form_line("Instructions", &form.instructions.value, form.row == 1),
                choice_line(
                    &format!("Schedule · {}", schedule_kind_label(form.schedule_kind)),
                    form.row == 2,
                ),
                form_line(
                    schedule_value_label(form.schedule_kind),
                    &form.schedule_value.value,
                    form.row == 3,
                ),
                form_line("Time zone (cron)", &form.time_zone.value, form.row == 4),
                form_line(
                    "Ends at (Unix, optional)",
                    &form.ends_at.value,
                    form.row == 5,
                ),
            ];
            if form.is_update() {
                lines.push(choice_line(
                    if form.enabled {
                        "[x] Enabled"
                    } else {
                        "[ ] Enabled"
                    },
                    form.row == 6,
                ));
            }
            lines.push(choice_line("Save routine", form.row == save_row));
            let field_error = [
                &form.workspace,
                &form.instructions,
                &form.schedule_value,
                &form.time_zone,
                &form.ends_at,
            ]
            .into_iter()
            .find_map(|field| field.error.as_deref());
            (
                if form.is_update() {
                    "Edit routine"
                } else {
                    "Create routine"
                },
                lines,
                form.error.as_deref().or(field_error),
            )
        }
    };
    if let Some(error) = error {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            terminal_text(error),
            current().style(Role::Error),
        ));
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let scroll = if matches!(form, Form::Bot(bot) if bot.row >= 2) {
        paragraph
            .line_count(area.width.saturating_sub(2))
            .saturating_sub(usize::from(area.height.saturating_sub(2)))
    } else {
        0
    };
    frame.render_widget(
        paragraph
            .block(panel(title))
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        area,
    );
}

fn form_line(label: &str, value: &str, focused: bool) -> Line<'static> {
    let role = if focused { Role::Selection } else { Role::Text };
    let value = terminal_text(value).replace(['\n', '\t'], " ");
    Line::styled(
        format!("{} {label}: {value}", if focused { "›" } else { " " }),
        current().style(role),
    )
}

fn choice_line(label: &str, focused: bool) -> Line<'static> {
    Line::styled(
        format!(
            "{} {}",
            if focused { "›" } else { " " },
            terminal_text(label)
        ),
        current().style(if focused { Role::Selection } else { Role::Text }),
    )
}

const fn schedule_kind_label(kind: RoutineScheduleKind) -> &'static str {
    match kind {
        RoutineScheduleKind::Once => "once",
        RoutineScheduleKind::Interval => "interval",
        RoutineScheduleKind::Cron => "cron",
    }
}

const fn schedule_value_label(kind: RoutineScheduleKind) -> &'static str {
    match kind {
        RoutineScheduleKind::Once => "Run at (Unix)",
        RoutineScheduleKind::Interval => "Every seconds",
        RoutineScheduleKind::Cron => "Cron expression",
    }
}

fn render_root(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &BotsState,
    gateway: &ReadyPayload,
) {
    let [catalog, details] =
        Layout::vertical([Constraint::Percentage(45), Constraint::Min(5)]).areas(area);
    let theme = current();
    let rows = gateway.bots.iter().map(|bot| {
        Line::from(format!(
            " @{} · {}",
            terminal_text(&bot.handle),
            terminal_text(&bot.name)
        ))
    });
    let mut list_state =
        ListState::default().with_selected((!gateway.bots.is_empty()).then_some(state.selected));
    frame.render_stateful_widget(
        List::new(rows)
            .block(panel("Bots"))
            .highlight_symbol("› ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::default().add_modifier(Modifier::BOLD)),
        catalog,
        &mut list_state,
    );
    let lines = gateway.bots.get(state.selected).map_or_else(
        || vec![Line::styled("No Bots", theme.style(Role::Muted))],
        |bot| bot_details(bot, state, gateway),
    );
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel("Details")),
        details,
    );
}

fn bot_details(bot: &BotRecord, state: &BotsState, gateway: &ReadyPayload) -> Vec<Line<'static>> {
    let theme = current();
    let chats = sessions_for_bot(gateway, &bot.id).len();
    let routines = state
        .routines
        .iter()
        .filter(|routine| routine.bot_id == bot.id)
        .count();
    vec![
        Line::styled(
            format!(
                "{} · @{}",
                terminal_text(&bot.name),
                terminal_text(&bot.handle)
            ),
            theme.style(Role::AccentStrong).add_modifier(Modifier::BOLD),
        ),
        Line::styled(terminal_text(&bot.description), theme.style(Role::Text)),
        Line::styled(
            format!(
                "model: {}",
                terminal_text(&bot.config.config.provider.model)
            ),
            theme.style(Role::Muted),
        ),
        Line::styled(
            format!("{chats} conversations · {routines} routines"),
            theme.style(Role::Muted),
        ),
    ]
}

fn render_bot(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &BotsState,
    gateway: &ReadyPayload,
    bot_id: &str,
) {
    let Some(bot) = gateway.bots.iter().find(|bot| bot.id == bot_id) else {
        return;
    };
    let rows = BOT_ROWS;
    let chats = sessions_for_bot(gateway, bot_id).len();
    let routines = state
        .routines
        .iter()
        .filter(|routine| routine.bot_id == bot_id)
        .count();
    let lines = rows.iter().map(|row| match row {
        BotRow::Identity => Line::from(" Identity & system prompt"),
        BotRow::Model => Line::from(" Model & reasoning"),
        BotRow::Capabilities => Line::from(" Capabilities"),
        BotRow::Conversations => Line::from(format!(" Conversations · {chats}")),
        BotRow::Routines => Line::from(format!(" Routines · {routines}")),
    });
    render_list_page(
        frame,
        area,
        &format!(
            "{} · @{}",
            terminal_text(&bot.name),
            terminal_text(&bot.handle)
        ),
        lines,
        rows.len(),
        state.selected,
    );
}

fn render_conversations(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &BotsState,
    gateway: &ReadyPayload,
    bot_id: &str,
) {
    let sessions = sessions_for_bot(gateway, bot_id);
    let rows = sessions.iter().map(|session| {
        let title = session
            .title
            .as_deref()
            .or(session.first_user_message.as_deref())
            .unwrap_or(&session.session_id);
        Line::from(format!(
            " {} · {}",
            terminal_text(title),
            terminal_text(
                session
                    .session_context
                    .workspace_label
                    .as_deref()
                    .unwrap_or("workspace unavailable")
            )
        ))
    });
    render_list_page(
        frame,
        area,
        "Conversations",
        rows,
        sessions.len(),
        state.selected,
    );
}

fn render_routines(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &BotsState,
    _gateway: &ReadyPayload,
    bot_id: &str,
) {
    let routines = state
        .routines
        .iter()
        .filter(|routine| routine.bot_id == bot_id)
        .collect::<Vec<_>>();
    let rows = routines.iter().map(|routine| {
        Line::from(format!(
            " {} · {}",
            if routine.enabled && !routine.finished {
                "●"
            } else {
                "○"
            },
            terminal_text(&routine.instructions)
        ))
    });
    render_list_page(
        frame,
        area,
        "Routines",
        rows,
        routines.len(),
        state.selected,
    );
}

fn render_routine(frame: &mut ratatui::Frame<'_>, area: Rect, state: &BotsState, routine_id: &str) {
    let Some(routine) = state
        .routines
        .iter()
        .find(|routine| routine.id == routine_id)
    else {
        return;
    };
    let runs = state
        .runs
        .iter()
        .filter(|run| run.routine_id == routine_id)
        .count();
    let rows = [
        Line::from(" Edit settings"),
        Line::from(if routine.enabled {
            " Disable"
        } else {
            " Enable"
        }),
        Line::from(" Run now"),
        Line::from(format!(" Run history · {runs}")),
    ];
    let length = rows.len();
    render_list_page(
        frame,
        area,
        &terminal_text(&routine.instructions),
        rows.into_iter(),
        length,
        state.selected,
    );
}

fn render_runs(frame: &mut ratatui::Frame<'_>, area: Rect, state: &BotsState, routine_id: &str) {
    let runs = runs_for_routine(&state.runs, routine_id);
    let rows = runs.iter().map(|run| {
        let message = run
            .message
            .as_deref()
            .map(|message| format!(" · {}", terminal_text(message)))
            .unwrap_or_default();
        Line::from(format!(" {}{message}", routine_run_label(run)))
    });
    render_list_page(
        frame,
        area,
        "Routine runs",
        rows,
        runs.len(),
        state.selected,
    );
}

fn render_run(frame: &mut ratatui::Frame<'_>, area: Rect, state: &BotsState) {
    let Some(preview) = &state.preview else {
        frame.render_widget(
            Paragraph::new("Loading run…").block(panel("Routine run")),
            area,
        );
        return;
    };
    let mut lines = vec![
        Line::styled(
            routine_run_label(&preview.run),
            current().style(Role::AccentStrong),
        ),
        Line::styled(
            terminal_text(&preview.routine.workspace.display().to_string()),
            current().style(Role::Muted),
        ),
        Line::from(""),
    ];
    for record in &preview.records {
        for rendered in &record.blocks {
            let text = super::super::block_text(&rendered.block);
            lines.extend(
                terminal_text(&text)
                    .lines()
                    .map(|line| Line::from(line.to_owned())),
            );
        }
    }
    if preview.records.is_empty() {
        lines.push(Line::styled(
            "No transcript records",
            current().style(Role::Muted),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("Routine run"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_list_page<'a>(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    rows: impl Iterator<Item = Line<'a>>,
    length: usize,
    selected: usize,
) {
    let mut list_state = ListState::default().with_selected((length > 0).then_some(selected));
    frame.render_stateful_widget(
        List::new(rows)
            .block(panel(title))
            .highlight_symbol("› ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::default().add_modifier(Modifier::BOLD))
            .scroll_padding(1),
        area,
        &mut list_state,
    );
}

fn render_notice(frame: &mut ratatui::Frame<'_>, area: Rect, state: &BotsState) {
    let theme = current();
    let (text, role) = if let Some(confirmation) = &state.confirmation {
        (confirmation_text(confirmation), Role::Warning)
    } else if let Some(pending) = &state.pending {
        (format!("{}…", pending.label), Role::Muted)
    } else if let Some(notice) = &state.notice {
        (notice.text.clone(), notice.role)
    } else {
        (String::new(), Role::Muted)
    };
    frame.render_widget(Paragraph::new(text).style(theme.style(role)), area);
}

fn confirmation_text(confirmation: &Confirmation) -> String {
    match confirmation {
        Confirmation::Bot { handle, .. } => {
            format!("Delete @{handle} plus every owned conversation, routine, and run? y/n")
        }
        Confirmation::Routine { label, .. } => {
            format!("Delete routine `{}`? y/n", terminal_text(label))
        }
        Confirmation::Run { label, .. } => {
            format!(
                "Delete routine run `{}` and its transcript? y/n",
                terminal_text(label)
            )
        }
    }
}

fn footer_text(state: &BotsState) -> &'static str {
    if matches!(state.form, Some(Form::Bot(_))) {
        return "tab select · ctrl+u clear · shift+enter newline · ctrl+s save · esc cancel";
    }
    if state.form.is_some() {
        return "tab/↑↓ select · type edit · enter continue/save · esc cancel";
    }
    if state.confirmation.is_some() {
        return "y confirm · n cancel";
    }
    match state.page {
        Page::Root => "↑↓ select · n new Bot · e edit · x delete · q close",
        Page::Bot(_) => "↑↓ select · enter open · e edit identity/prompt · esc back",
        Page::Conversations(_) => "↑↓ inspect · esc back",
        Page::Routines(_) => {
            "↑↓ select · n new · enter open · e edit · space enable · r run · x delete"
        }
        Page::Routine { .. } => "↑↓ select · enter open · e edit · space enable · r run · esc back",
        Page::Runs { .. } => "↑↓ select · enter inspect · x delete · esc back",
        Page::Run { .. } => "esc back",
    }
}

fn panel(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .title(format!(" {} ", title.into()))
        .border_style(current().style(Role::Border))
}

fn content_area(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).min(92);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y.saturating_add(1),
        width,
        area.height.saturating_sub(2),
    )
}
