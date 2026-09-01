use std::time::{SystemTime, UNIX_EPOCH};

use mobius::protocol::{
    FrontendActionListItem, FrontendBlock, FrontendListItemState, FrontendTone, FrontendWidget,
    FrontendWidgetContent,
};
use mobius_gateway::wire::{ClientKind, DailyUsage, ProfileSnapshot, SessionActivityState};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, HighlightSpacing, List, ListState, Paragraph, Wrap};

use super::runtime::{ordered_clients, ordered_sessions};
use super::state::{ActionInput, CapabilityOverlay, DashboardFocus, DashboardState};
use crate::frontend::block_text;
use crate::frontend::provider_instance_label;
use crate::frontend::terminal::terminal_text;
use crate::frontend::theme::{Role, current};

pub(super) struct DashboardAreas {
    pub(super) header: Rect,
    pub(super) devices: Rect,
    pub(super) chats: Rect,
    pub(super) providers: Rect,
    pub(super) defaults: Rect,
    pub(super) usage: Rect,
    pub(super) footer: Rect,
}

pub(super) fn render(frame: &mut ratatui::Frame<'_>, state: &mut DashboardState) {
    let theme = current();
    frame.render_widget(
        Block::default().style(theme.style(Role::Canvas)),
        frame.area(),
    );
    let areas = dashboard_areas(frame.area());
    render_header(frame, areas.header, state);
    render_devices(frame, areas.devices, state);
    render_chats(frame, areas.chats, state);
    render_providers(frame, areas.providers, state);
    render_defaults(frame, areas.defaults, state);
    render_usage(frame, areas.usage, state.profile.as_ref());
    let (footer, role) = if let Some((_, label)) = &state.pending_unpair {
        (
            format!(" Unpair {}? · y confirm · n cancel ", terminal_text(label)),
            Role::Warning,
        )
    } else if let Some(error) = &state.error {
        (error.clone(), Role::Error)
    } else if state.pending_open.is_some() {
        (" opening chat capabilities… ".into(), Role::Muted)
    } else {
        (
            " tab devices/chats · ↑↓ scroll · enter chat capabilities · u unpair · p provider · d defaults · r refresh · q quit ".into(),
            Role::Muted,
        )
    };
    frame.render_widget(
        Paragraph::new(terminal_text(&footer)).style(theme.style(role)),
        areas.footer,
    );
    if let Some(overlay) = state.overlay.as_mut() {
        render_capability_overlay(frame, overlay);
    }
}

pub(in crate::frontend) fn render_capability_overlay(
    frame: &mut ratatui::Frame<'_>,
    overlay: &mut CapabilityOverlay,
) {
    let area = centered_area(frame.area(), 86, 82);
    frame.render_widget(Clear, area);
    let outer = panel(&overlay.title, true);
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let footer_text = if let Some(input) = overlay.input.as_ref() {
        render_action_input(frame, body, input);
        " type to edit · enter save · esc cancel "
    } else if overlay.open.is_none() {
        render_navigation_widgets(frame, body, overlay);
        " ↑↓ select · enter open/run · esc close "
    } else {
        let content = overlay
            .open_widget()
            .and_then(|widget| widget.content.clone());
        match content {
            Some(FrontendWidgetContent::Blocks { title, blocks }) => {
                render_blocks(frame, body, &title, &blocks);
                if overlay
                    .open_widget()
                    .is_some_and(|widget| widget.action.is_some())
                {
                    " enter/a run · esc back "
                } else {
                    " esc back "
                }
            }
            Some(FrontendWidgetContent::Picker { title, options }) => {
                render_overlay_picker(frame, body, &title, &options, &mut overlay.option_list);
                " ↑↓ select · enter run · esc back "
            }
            Some(FrontendWidgetContent::ActionList { title, items }) => {
                render_action_list(
                    frame,
                    body,
                    &title,
                    &items,
                    &mut overlay.option_list,
                    overlay.action_index,
                );
                " ↑↓ note · ←→ action · enter run · esc back "
            }
            None => {
                frame.render_widget(Paragraph::new(" No content"), body);
                " esc back "
            }
        }
    };
    frame.render_widget(
        Paragraph::new(footer_text).style(current().style(Role::Muted)),
        footer,
    );
}

pub(super) fn render_action_input(frame: &mut ratatui::Frame<'_>, area: Rect, input: &ActionInput) {
    let mut value = input.text.clone();
    value.insert(input.cursor, '█');
    frame.render_widget(
        Paragraph::new(value)
            .style(current().style(Role::Text))
            .block(
                Block::bordered()
                    .border_style(current().style(Role::Border))
                    .title(" Edit "),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

pub(in crate::frontend) fn centered_area(area: Rect, width: u16, height: u16) -> Rect {
    let [vertical] = Layout::vertical([Constraint::Percentage(height)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [center] = Layout::horizontal([Constraint::Percentage(width)])
        .flex(ratatui::layout::Flex::Center)
        .areas(vertical);
    center
}

pub(super) fn render_navigation_widgets(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    overlay: &mut CapabilityOverlay,
) {
    let theme = current();
    let lines = if overlay.widgets.is_empty() {
        empty("No capability views")
    } else {
        overlay
            .widgets
            .iter()
            .map(|((capability, _), widget)| {
                Line::from(vec![
                    Span::styled(
                        format!(" {}", terminal_text(widget_title(widget))),
                        theme
                            .style(tone_role(widget.tone))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" · {}", terminal_text(capability)),
                        theme.style(Role::Muted),
                    ),
                ])
            })
            .collect()
    };
    frame.render_stateful_widget(
        List::new(lines)
            .highlight_symbol("› ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::default().add_modifier(Modifier::BOLD)),
        area,
        &mut overlay.widget_list,
    );
}

fn widget_title(widget: &FrontendWidget) -> &str {
    match widget.content.as_ref() {
        Some(
            FrontendWidgetContent::Blocks { title, .. }
            | FrontendWidgetContent::Picker { title, .. }
            | FrontendWidgetContent::ActionList { title, .. },
        ) => title,
        None => &widget.text,
    }
}

pub(super) fn render_blocks(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    blocks: &[FrontendBlock],
) {
    let theme = current();
    let mut lines = vec![Line::styled(
        format!(" {}", terminal_text(title)),
        theme.style(Role::AccentStrong).add_modifier(Modifier::BOLD),
    )];
    for block in blocks {
        if lines.len() > 1 {
            lines.push(Line::default());
        }
        lines.extend(
            terminal_text(&block_text(block))
                .lines()
                .map(|line| Line::styled(line.to_owned(), theme.style(tone_role(block.tone)))),
        );
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

pub(super) fn render_overlay_picker(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    options: &[mobius::protocol::FrontendPickerOption],
    state: &mut ListState,
) {
    let theme = current();
    let [header, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    frame.render_widget(
        Paragraph::new(format!(" {}", terminal_text(title)))
            .style(theme.style(Role::AccentStrong).add_modifier(Modifier::BOLD)),
        header,
    );
    let lines = options
        .iter()
        .map(|option| {
            let detail = if !option.shows_detail || option.detail.is_empty() {
                option.description.clone()
            } else {
                format!("{} · {}", option.description, option.detail)
            };
            Line::from(vec![
                Span::styled(
                    format!(" {}", terminal_text(&option.label)),
                    theme.style(Role::Text).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" · {}", terminal_text(&detail)),
                    theme.style(Role::Muted),
                ),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_stateful_widget(
        List::new(lines)
            .highlight_symbol("› ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::default().add_modifier(Modifier::BOLD)),
        list,
        state,
    );
}

pub(super) fn render_action_list(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    items: &[FrontendActionListItem],
    state: &mut ListState,
    selected_action: usize,
) {
    let theme = current();
    let [header, list] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);
    frame.render_widget(
        Paragraph::new(format!(" {}", terminal_text(title)))
            .style(theme.style(Role::AccentStrong).add_modifier(Modifier::BOLD)),
        header,
    );
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(" No items").style(theme.style(Role::Muted)),
            list,
        );
        return;
    }
    let selected_item = state.selected().unwrap_or_default();
    let width = usize::from(list.width.saturating_sub(3));
    let lines = items
        .iter()
        .enumerate()
        .map(|(item_index, item)| {
            let (marker, note_style) = match item.state {
                FrontendListItemState::Plain => ("", theme.style(Role::Text)),
                FrontendListItemState::Pending => ("○ ", theme.style(Role::Muted)),
                FrontendListItemState::InProgress => ("◉ ", theme.style(Role::AccentStrong)),
                FrontendListItemState::Completed => (
                    "✓ ",
                    theme.style(Role::Muted).add_modifier(Modifier::CROSSED_OUT),
                ),
            };
            let actions_width = item
                .actions
                .iter()
                .map(|action| Line::from(format!("[{}]", terminal_text(&action.label))).width())
                .sum::<usize>()
                .saturating_add(item.actions.len().saturating_sub(1) * 2);
            let note_width = width.saturating_sub(actions_width.saturating_add(1));
            let note = truncate_terminal_width(
                &format!("{marker}{}", terminal_text(&item.text)),
                note_width,
            );
            let used = Line::from(note.as_str())
                .width()
                .saturating_add(actions_width);
            let padding = width.saturating_sub(used).max(1);
            let mut spans = vec![Span::styled(
                format!(" {note}{}", " ".repeat(padding)),
                note_style,
            )];
            for (action_index, action) in item.actions.iter().enumerate() {
                if action_index > 0 {
                    spans.push(Span::raw("  "));
                }
                let style = if item_index == selected_item && action_index == selected_action {
                    theme
                        .style(Role::AccentStrong)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    theme.style(tone_role(action.tone))
                };
                spans.push(Span::styled(
                    format!("[{}]", terminal_text(&action.label)),
                    style,
                ));
            }
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    frame.render_stateful_widget(
        List::new(lines)
            .highlight_symbol("› ")
            .highlight_spacing(HighlightSpacing::Always),
        list,
        state,
    );
}

pub(super) fn truncate_terminal_width(value: &str, width: usize) -> String {
    if Line::from(value).width() <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut clipped = String::new();
    let mut used = 0;
    let content_width = width.saturating_sub(1);
    for character in value.chars() {
        let character_width = Line::from(character.to_string()).width();
        if used + character_width > content_width {
            break;
        }
        clipped.push(character);
        used += character_width;
    }
    clipped.push('…');
    clipped
}

const fn tone_role(tone: FrontendTone) -> Role {
    match tone {
        FrontendTone::Neutral => Role::Neutral,
        FrontendTone::Success => Role::Success,
        FrontendTone::Warning => Role::Warning,
        FrontendTone::Error => Role::Error,
    }
}

pub(super) fn dashboard_areas(area: Rect) -> DashboardAreas {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(12),
        Constraint::Length(2),
    ])
    .areas(area);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);
    let [devices, chats] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(left);
    let [providers, defaults, usage] = Layout::vertical([
        Constraint::Percentage(34),
        Constraint::Percentage(38),
        Constraint::Percentage(28),
    ])
    .areas(right);
    DashboardAreas {
        header,
        devices,
        chats,
        providers,
        defaults,
        usage,
        footer,
    }
}

pub(super) fn render_header(frame: &mut ratatui::Frame<'_>, area: Rect, state: &DashboardState) {
    let theme = current();
    let active_devices = state
        .clients
        .iter()
        .filter(|client| client.connections > 0)
        .count();
    let active_chats = state
        .gateway
        .sessions
        .iter()
        .filter(|session| session.activity.state != SessionActivityState::Idle)
        .count();
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    " MÖBIUS GATEWAY ",
                    theme.style(Role::AccentStrong).add_modifier(Modifier::BOLD),
                ),
                Span::styled(&state.endpoint, theme.style(Role::Muted)),
            ]),
            Line::styled(
                format!(
                    " {active_devices}/{} devices active · {active_chats}/{} chats active",
                    state.clients.len(),
                    state.gateway.sessions.len()
                ),
                theme.style(Role::Text),
            ),
        ]),
        area,
    );
}

pub(super) fn panel(title: impl Into<String>, focused: bool) -> Block<'static> {
    let theme = current();
    Block::default()
        .title(format!(" {} ", title.into()))
        .title_style(theme.style(Role::AccentStrong).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(theme.style(if focused {
            Role::AccentStrong
        } else {
            Role::Neutral
        }))
        .style(theme.style(Role::Canvas))
}

pub(super) fn render_devices(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &mut DashboardState,
) {
    let theme = current();
    let clients = ordered_clients(&state.clients);
    let lines = if clients.is_empty() {
        empty("No paired devices")
    } else {
        clients
            .into_iter()
            .map(|client| {
                let (symbol, status, role) = match client.connections {
                    0 => ("○", "offline".into(), Role::Muted),
                    1 => ("●", "active".into(), Role::Success),
                    connections => ("●", format!("{connections} connections"), Role::Success),
                };
                let kinds = if client.kinds.is_empty() {
                    "paired".into()
                } else {
                    client
                        .kinds
                        .iter()
                        .map(|kind| client_kind(*kind))
                        .collect::<Vec<_>>()
                        .join(" + ")
                };
                let current =
                    if Some(client.client_id.as_str()) == state.current_client_id.as_deref() {
                        " · this device"
                    } else {
                        ""
                    };
                Line::styled(
                    format!(
                        " {symbol} {kinds} · {} · {status}{current}",
                        terminal_text(&client.label)
                    ),
                    theme.style(role),
                )
            })
            .collect()
    };
    frame.render_stateful_widget(
        List::new(lines)
            .block(panel("Devices", state.focus == DashboardFocus::Devices))
            .highlight_symbol("› ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::default().add_modifier(Modifier::BOLD))
            .scroll_padding(1),
        area,
        &mut state.device_list,
    );
}

pub(super) fn render_chats(frame: &mut ratatui::Frame<'_>, area: Rect, state: &mut DashboardState) {
    let theme = current();
    let sessions = ordered_sessions(&state.gateway.sessions);
    let lines = if sessions.is_empty() {
        empty("No chats")
    } else {
        sessions
            .into_iter()
            .map(|session| {
                let title = session
                    .title
                    .as_deref()
                    .or(session.first_user_message.as_deref())
                    .unwrap_or(&session.session_id);
                let (symbol, role) = match session.activity.state {
                    SessionActivityState::Idle => ("○", Role::Muted),
                    SessionActivityState::Running => ("●", Role::Success),
                    SessionActivityState::AwaitingApproval => ("●", Role::Warning),
                };
                Line::styled(
                    format!(
                        " {symbol} {} · {}",
                        activity_label(session.activity.state),
                        terminal_text(title)
                    ),
                    theme.style(role),
                )
            })
            .collect()
    };
    frame.render_stateful_widget(
        List::new(lines)
            .block(panel("Chats", state.focus == DashboardFocus::Chats))
            .highlight_symbol("› ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::default().add_modifier(Modifier::BOLD))
            .scroll_padding(1),
        area,
        &mut state.chat_list,
    );
}

pub(super) fn render_providers(frame: &mut ratatui::Frame<'_>, area: Rect, state: &DashboardState) {
    let lines = if state.gateway.provider_instances.is_empty() {
        empty("No providers configured · press p")
    } else {
        state
            .gateway
            .provider_instances
            .iter()
            .map(|instance| {
                Line::from(format!(
                    " ● {} · {}",
                    terminal_text(&instance.label),
                    terminal_text(&instance.selection.model)
                ))
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("Providers", false))
            .wrap(Wrap { trim: true }),
        area,
    );
}

pub(super) fn render_defaults(frame: &mut ratatui::Frame<'_>, area: Rect, state: &DashboardState) {
    let lines = state.gateway.default_config.as_ref().map_or_else(
        || empty("No defaults · configure a provider first"),
        |default| {
            let config = &default.config;
            let provider = provider_instance_label(
                &state.gateway.provider_instances,
                &config.provider.instance,
            )
            .unwrap_or(&config.provider.provider);
            let enabled = state
                .gateway
                .middleware_features
                .iter()
                .filter(|feature| feature.required || config.middleware.enabled(&feature.id))
                .count();
            vec![
                Line::from(format!(
                    " Model      {} / {}",
                    terminal_text(provider),
                    terminal_text(&config.provider.model)
                )),
                Line::from(format!(
                    " Reasoning  {}",
                    config
                        .provider
                        .reasoning_effort
                        .as_deref()
                        .unwrap_or("provider default")
                )),
                Line::from(format!(" Search     {:?}", config.provider.web_search)),
                Line::from(format!(" Middleware {enabled}")),
            ]
        },
    );
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("Defaults · d to change", false))
            .wrap(Wrap { trim: true }),
        area,
    );
}

pub(super) fn render_usage(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    profile: Option<&ProfileSnapshot>,
) {
    let lines = profile.map_or_else(
        || empty("Loading usage…"),
        |profile| {
            let today = current_unix_day()
                .map_or(0_i64, |day| token_total_for_day(&profile.daily_usage, day));
            let total = profile.daily_usage.iter().fold(0_i64, |total, entry| {
                total.saturating_add(entry.usage.total_tokens)
            });
            let stats = &profile.run_stats;
            vec![
                Line::from(format!(" Today      {} tokens", number(today))),
                Line::from(format!(
                    " Runs       {} · {} failed · {} aborted",
                    number(stats.run_count),
                    number(stats.failed_run_count),
                    number(stats.aborted_run_count)
                )),
                Line::from(format!(
                    " Calls      {} model · {} tool · {} failed",
                    number(stats.model_calls),
                    number(stats.tool_calls),
                    number(stats.failed_tool_calls)
                )),
                Line::from(format!(" Run time   {}", elapsed_ms(stats.elapsed_ms))),
                Line::from(format!(" 364 days   {} tokens", number(total))),
            ]
        },
    );
    frame.render_widget(Paragraph::new(lines).block(panel("Usage", false)), area);
}

pub(super) fn token_total_for_day(usage: &[DailyUsage], unix_day: u64) -> i64 {
    usage
        .iter()
        .filter(|entry| entry.unix_day == unix_day)
        .fold(0_i64, |total, entry| {
            total.saturating_add(entry.usage.total_tokens)
        })
}

pub(super) fn elapsed_ms(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3_600, seconds / 60 % 60)
    }
}

pub(super) fn empty(message: &str) -> Vec<Line<'static>> {
    vec![Line::styled(
        format!(" {}", terminal_text(message)),
        current().style(Role::Muted),
    )]
}

const fn client_kind(kind: ClientKind) -> &'static str {
    match kind {
        ClientKind::Cli => "CLI",
        ClientKind::Macos => "macOS",
        ClientKind::Ios => "iOS",
        ClientKind::Ipados => "iPadOS",
        ClientKind::GatewayDashboard => "Dashboard",
    }
}

const fn activity_label(state: SessionActivityState) -> &'static str {
    match state {
        SessionActivityState::Idle => "idle",
        SessionActivityState::Running => "running",
        SessionActivityState::AwaitingApproval => "approval",
    }
}

pub(super) fn current_unix_day() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() / 86_400)
}

pub(super) fn number(value: impl ToString) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(character);
    }
    formatted
}
