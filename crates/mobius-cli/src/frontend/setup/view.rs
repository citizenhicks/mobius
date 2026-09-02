use mobius::Result;
use mobius::protocol::{FrontendSetting, FrontendSettingKind, FrontendSettingValue};
use mobius_gateway::wire::{ExtensionKind, MiddlewareConfig, ProviderAuthKind};
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use super::state::{AuthField, MiddlewareRow, Page, Progress, SetupState};
use super::{
    MIN_INLINE_DESCRIPTION_WIDTH, SAVE_DEFAULT_DESCRIPTION, SAVE_DEFAULT_LABEL, SetupMode,
    SetupTerminal, UPDATE_BOT_DESCRIPTION, UPDATE_BOT_LABEL,
};
use crate::frontend::terminal::terminal_text;
use crate::frontend::theme::{Role, current};

#[derive(Clone, Copy)]
pub(super) struct AgentLayout {
    pub(super) width: usize,
    pub(super) value_column: usize,
    pub(super) description_column: Option<usize>,
}

pub(super) fn draw(terminal: &mut SetupTerminal, state: &SetupState) -> Result<()> {
    terminal.draw(|frame| render(frame, state))?;
    Ok(())
}

pub(super) fn render(frame: &mut ratatui::Frame<'_>, state: &SetupState) {
    let theme = current();
    frame.render_widget(
        Block::default().style(theme.style(Role::Canvas)),
        frame.area(),
    );
    let area = content_area(frame.area());
    let mut lines = header(state);
    lines.push(Line::from(""));
    if let Some(progress) = &state.progress {
        if let Some(page) = login_page(state) {
            lines.push(Line::styled(
                format!("Page {page} of 3"),
                theme.style(Role::Muted),
            ));
            lines.push(Line::from(""));
        }
        render_progress(&mut lines, progress);
    } else {
        render_editing(&mut lines, state, area.width);
    }
    let scroll = selection_scroll(&lines, area);
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme.style(Role::Canvas))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

pub(in crate::frontend) fn selection_scroll(lines: &[Line<'_>], area: Rect) -> u16 {
    if area.width == 0 || area.height == 0 {
        return 0;
    }
    let selection = current().style(Role::Selection);
    let Some(start) = lines.iter().position(|line| line.style == selection) else {
        return 0;
    };
    let end = lines[start..]
        .iter()
        .position(|line| line.style != selection)
        .map_or(lines.len(), |length| start + length);
    let selected_end = Paragraph::new(lines[..end].to_vec())
        .wrap(Wrap { trim: false })
        .line_count(area.width);
    selected_end
        .saturating_sub(usize::from(area.height))
        .min(usize::from(u16::MAX)) as u16
}

pub(in crate::frontend) fn content_area(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).min(82);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y.saturating_add(1),
        width,
        area.height.saturating_sub(2),
    )
}

pub(super) fn header(state: &SetupState) -> Vec<Line<'static>> {
    brand_header(match state.mode {
        SetupMode::Login => "provider login",
        SetupMode::Bot | SetupMode::BotModel => "Bot setup",
    })
}

pub(in crate::frontend) fn brand_header(section: &str) -> Vec<Line<'static>> {
    let theme = current();
    vec![Line::from(vec![
        Span::styled("◉ ", theme.style(Role::AccentStrong)),
        Span::styled(
            "MÖBIUS",
            theme.style(Role::Accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {section}"), theme.style(Role::Muted)),
    ])]
}

pub(super) fn render_editing(lines: &mut Vec<Line<'static>>, state: &SetupState, width: u16) {
    let theme = current();
    if let Some(page) = login_page(state) {
        lines.push(Line::styled(
            format!("Page {page} of 3"),
            theme.style(Role::Muted),
        ));
    }
    lines.push(Line::from(""));
    let (title, context) = page_prompt(state);
    lines.push(Line::styled(
        format!("  {title}"),
        theme.style(Role::Text).add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::styled(
        format!("  {context}"),
        theme.style(Role::Muted),
    ));
    lines.push(Line::from(""));
    render_page(lines, state, width);
    if let Some(error) = &state.error {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            format!("  {}", terminal_text(error)),
            theme.style(Role::Error),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(footer(state), theme.style(Role::Muted)));
}

pub(super) fn login_page(state: &SetupState) -> Option<u8> {
    (state.mode == SetupMode::Login).then(|| match state.page {
        Page::Provider => 1,
        Page::Authentication => 2,
        Page::Models => 3,
        Page::Agent => unreachable!("agent page is not part of provider login"),
    })
}

pub(super) fn page_prompt(state: &SetupState) -> (&'static str, String) {
    match state.page {
        Page::Provider => (
            "Choose a model provider",
            "Providers are loaded from the connected gateway.".into(),
        ),
        Page::Authentication => (
            "Set up provider access",
            "Credentials are write-only and sent securely to the gateway.".into(),
        ),
        Page::Models => (
            "Models & reasoning",
            "Review the manifest, select defaults, or enter a custom model ID.".into(),
        ),
        Page::Agent => (
            "Bot settings",
            "Toggle capabilities, adjust settings, and select installed extensions.".into(),
        ),
    }
}

pub(super) fn render_page(lines: &mut Vec<Line<'static>>, state: &SetupState, width: u16) {
    match state.page {
        Page::Provider => render_provider_page(lines, state),
        Page::Authentication => render_authentication_page(lines, state),
        Page::Models => render_models_page(lines, state),
        Page::Agent => render_agent_page(lines, state, width),
    }
}

fn render_provider_page(lines: &mut Vec<Line<'static>>, state: &SetupState) {
    for (index, entry) in state.providers.iter().enumerate() {
        let configured = match &entry.instance {
            Some(instance) if instance.configured => "configured",
            Some(_) => "login required",
            None => "add setup",
        };
        choice(
            lines,
            entry
                .instance
                .as_ref()
                .map_or(&entry.status.label, |instance| &instance.label),
            &format!("{} · {configured}", entry.status.description),
            index == state.provider,
            if index == state.provider {
                "●"
            } else {
                "○"
            },
        );
    }
}

fn render_authentication_page(lines: &mut Vec<Line<'static>>, state: &SetupState) {
    let theme = current();
    let focused = state.auth_field == AuthField::Label;
    lines.push(Line::styled(
        format!(
            "{} Name  {}▏",
            if focused { "›" } else { " " },
            terminal_text(&state.label)
        ),
        theme.style(if focused { Role::Selection } else { Role::Text }),
    ));
    lines.push(Line::styled(
        format!(
            "    Shown in model pickers. Empty uses `{}`.",
            state.definition().label
        ),
        theme.style(Role::Muted),
    ));
    lines.push(Line::from(""));

    match state.definition().auth {
        ProviderAuthKind::ApiKey => {
            let focused = state.auth_field == AuthField::Credential;
            lines.push(Line::styled(
                format!(
                    "{} API key  {}▏",
                    if focused { "›" } else { " " },
                    masked_credential(&state.credential)
                ),
                theme.style(if focused { Role::Selection } else { Role::Text }),
            ));
            lines.push(Line::styled(
                if state.has_matching_credential() {
                    "    Paste a new key, or leave empty to reuse the gateway credential.".into()
                } else {
                    state
                        .definition()
                        .default_api_key_env
                        .as_deref()
                        .map_or_else(
                            || "    Paste a key configured for this gateway endpoint.".into(),
                            |environment| {
                                format!(
                                    "    Paste a key, or leave empty to use {environment} when set."
                                )
                            },
                        )
                },
                theme.style(Role::Muted),
            ));
        }
        ProviderAuthKind::DeviceCode => {
            lines.push(Line::styled(
                format!(
                    "  Press Enter to start {} device login.",
                    state.definition().label
                ),
                theme.style(Role::Info),
            ));
        }
    }
    if state.definition().configurable_base_url() {
        let focused = state.auth_field == AuthField::Endpoint;
        lines.push(Line::from(""));
        lines.push(Line::styled(
            format!(
                "{} Base URL  {}▏",
                if focused { "›" } else { " " },
                terminal_text(&state.endpoint)
            ),
            theme.style(if focused { Role::Selection } else { Role::Text }),
        ));
        lines.push(Line::styled(
            "    Credential storage is bound to this exact endpoint.",
            theme.style(Role::Muted),
        ));
    }
}

fn render_models_page(lines: &mut Vec<Line<'static>>, state: &SetupState) {
    let theme = current();
    lines.push(Line::styled(
        if state.definition().model_ids_configurable {
            "  Model IDs"
        } else {
            "  Model"
        },
        theme.style(Role::Muted),
    ));
    for (index, model) in state.definition().models.iter().enumerate() {
        choice(
            lines,
            &model.label,
            &model.description,
            state.row == index,
            if state.model == index { "●" } else { "○" },
        );
    }
    if state.definition().model_ids_configurable {
        choice(
            lines,
            "Model IDs",
            if state.custom_model.is_empty() {
                "Comma-separated; the first model is selected"
            } else {
                &state.custom_model
            },
            state.row == 0,
            "●",
        );
    }
    lines.push(Line::from(""));
    lines.push(Line::styled("  Reasoning", theme.style(Role::Muted)));
    let reasoning_start = state.model_choice_count();
    choice(
        lines,
        "Provider default",
        "Use the selected model's default reasoning",
        state.row == reasoning_start,
        if state.reasoning == 0 { "●" } else { "○" },
    );
    for (index, preset) in state
        .definition()
        .models
        .get(state.model)
        .into_iter()
        .flat_map(|model| &model.reasoning)
        .enumerate()
    {
        choice(
            lines,
            &preset.label,
            &preset.description,
            state.row == reasoning_start + index + 1,
            if state.reasoning == index + 1 {
                "●"
            } else {
                "○"
            },
        );
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "  Hosted web search",
        theme.style(Role::Muted),
    ));
    if state.definition().web_search.len() == 1 {
        let search = &state.definition().web_search[0];
        choice(lines, &search.label, &search.description, false, "[fixed]");
    } else {
        let search_start = state.model_choice_count() + state.reasoning_choice_count();
        for (index, search) in state.definition().web_search.iter().enumerate() {
            choice(
                lines,
                &search.label,
                &search.description,
                state.row == search_start + index,
                if state.web_search == index {
                    "●"
                } else {
                    "○"
                },
            );
        }
    }
    render_apply_actions(lines, state, state.models_action_start(), None);
}

fn render_agent_page(lines: &mut Vec<Line<'static>>, state: &SetupState, width: u16) {
    let layout = agent_layout(state, usize::from(width));
    for row in 0..state.middleware_row_count() {
        match state
            .middleware_row(row)
            .expect("visible agent rows have a catalog entry")
        {
            MiddlewareRow::Feature(feature_index) => {
                let feature = &state.features[feature_index];
                let disclosure = if !state.feature_has_children(feature_index) {
                    ""
                } else if state.expanded_features.contains(&feature.id) {
                    " ▾"
                } else {
                    " ▸"
                };
                agent_choice(
                    lines,
                    &format!("{}{disclosure}", feature.label),
                    &feature.description,
                    state.row == row,
                    if feature.required || state.middleware.enabled(&feature.id) {
                        "[x]"
                    } else {
                        "[ ]"
                    },
                    layout,
                );
            }
            MiddlewareRow::Setting { feature, setting } => {
                let feature = &state.features[feature];
                let setting = &feature.settings[setting];
                let (value, role) =
                    middleware_setting_value(&state.middleware, &feature.id, setting);
                setting_choice(
                    lines,
                    &setting.label,
                    &value,
                    role,
                    &setting.description,
                    state.row == row,
                    layout,
                );
            }
            MiddlewareRow::Extension { extension, .. } => {
                let extension = &state.available_extensions[extension];
                let version = extension
                    .version
                    .as_deref()
                    .map_or(String::new(), |version| format!(" · {version}"));
                let hooks = if !extension.hooks.is_empty() && !extension.hooks_trusted {
                    " · hooks disabled until trusted"
                } else {
                    ""
                };
                extension_choice(
                    lines,
                    &extension.name,
                    &format!(
                        "{}{}{} · {}",
                        extension_kind(extension.kind),
                        version,
                        hooks,
                        extension.description
                    ),
                    state.row == row,
                    if state.selected_extensions.contains(&extension.id) {
                        "[x]"
                    } else {
                        "[ ]"
                    },
                    layout,
                );
            }
        }
    }
    render_apply_actions(lines, state, state.agent_action_start(), Some(layout));
}

pub(super) fn agent_layout(state: &SetupState, width: usize) -> AgentLayout {
    let value_column = state
        .features
        .iter()
        .flat_map(|feature| &feature.settings)
        .map(|setting| 8 + display_width(&terminal_text(&setting.label)) + 2)
        .max()
        .unwrap_or(8);
    let setting_end = state
        .features
        .iter()
        .flat_map(|feature| {
            feature.settings.iter().map(|setting| {
                let (value, _) = middleware_setting_value(&state.middleware, &feature.id, setting);
                value_column + display_width(&format!("‹ {} ›", terminal_text(&value)))
            })
        })
        .max()
        .unwrap_or(0);
    let feature_end = state
        .features
        .iter()
        .map(|feature| 6 + display_width(&terminal_text(&feature.label)))
        .max()
        .unwrap_or(0);
    let extension_end = state
        .available_extensions
        .iter()
        .map(|extension| 10 + display_width(&terminal_text(&extension.name)))
        .max()
        .unwrap_or(0);
    let action_end = 6 + display_width(if state.default_only {
        SAVE_DEFAULT_LABEL
    } else {
        UPDATE_BOT_LABEL
    });
    let description_column = setting_end
        .max(feature_end)
        .max(extension_end)
        .max(action_end)
        + 2;

    AgentLayout {
        width,
        value_column,
        description_column: (description_column + MIN_INLINE_DESCRIPTION_WIDTH <= width)
            .then_some(description_column),
    }
}

pub(super) fn middleware_setting_value(
    config: &MiddlewareConfig,
    middleware: &str,
    setting: &FrontendSetting,
) -> (String, Role) {
    match (&setting.kind, config.setting(middleware, &setting.id)) {
        (FrontendSettingKind::Integer { .. }, Some(FrontendSettingValue::Integer(value))) => {
            (value.to_string(), Role::Accent)
        }
        (
            FrontendSettingKind::Select { options, .. },
            Some(FrontendSettingValue::String(value)),
        ) => (
            options
                .iter()
                .find(|option| option.value == *value)
                .map_or_else(|| value.clone(), |option| option.label.clone()),
            Role::Accent,
        ),
        (FrontendSettingKind::Select { unset_label, .. }, None) => (
            unset_label.clone().unwrap_or_else(|| "Not selected".into()),
            Role::Info,
        ),
        _ => ("Invalid value".into(), Role::Error),
    }
}

pub(super) fn setting_choice(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    value: &str,
    value_role: Role,
    description: &str,
    focused: bool,
    layout: AgentLayout,
) {
    let theme = current();
    let row_role = if focused { Role::Selection } else { Role::Text };
    let value_role = if focused { Role::Selection } else { value_role };
    let mut label = format!(
        "{} {:3}   {}",
        if focused { "›" } else { " " },
        "",
        terminal_text(label)
    );
    label.push_str(&" ".repeat(layout.value_column.saturating_sub(display_width(&label))));
    push_described_row(
        lines,
        vec![
            Span::styled(label, theme.style(row_role)),
            Span::styled(
                format!("‹ {} ›", terminal_text(value)),
                theme.style(value_role),
            ),
        ],
        description,
        focused,
        layout,
    );
}

pub(super) fn agent_choice(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    description: &str,
    focused: bool,
    marker: &str,
    layout: AgentLayout,
) {
    let role = if focused { Role::Selection } else { Role::Text };
    push_described_row(
        lines,
        vec![Span::styled(
            format!(
                "{} {:3} {}",
                if focused { "›" } else { " " },
                marker,
                terminal_text(label)
            ),
            current().style(role),
        )],
        description,
        focused,
        layout,
    );
}

pub(super) fn extension_choice(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    description: &str,
    focused: bool,
    marker: &str,
    layout: AgentLayout,
) {
    let role = if focused { Role::Selection } else { Role::Text };
    push_described_row(
        lines,
        vec![Span::styled(
            format!(
                "{}     {:3} {}",
                if focused { "›" } else { " " },
                marker,
                terminal_text(label)
            ),
            current().style(role),
        )],
        description,
        focused,
        layout,
    );
}

const fn extension_kind(kind: ExtensionKind) -> &'static str {
    match kind {
        ExtensionKind::Skill => "skill",
        ExtensionKind::Plugin => "plugin",
    }
}

pub(super) fn push_described_row(
    lines: &mut Vec<Line<'static>>,
    mut content: Vec<Span<'static>>,
    description: &str,
    focused: bool,
    layout: AgentLayout,
) {
    let theme = current();
    let row_style = theme.style(if focused { Role::Selection } else { Role::Text });
    let description_style = theme.style(if focused {
        Role::Selection
    } else {
        Role::Muted
    });
    let content_width = Line::from(content.clone()).width();

    if let Some(column) = layout
        .description_column
        .filter(|column| *column >= content_width)
    {
        let mut wrapped = wrap_description(description, layout.width - column).into_iter();
        content.push(Span::styled(
            format!(
                "{}{}",
                " ".repeat(column - content_width),
                wrapped.next().unwrap_or_default()
            ),
            description_style,
        ));
        lines.push(Line::from(content).style(row_style));
        lines.extend(wrapped.map(|line| {
            Line::from(Span::styled(
                format!("{}{}", " ".repeat(column), line),
                description_style,
            ))
            .style(row_style)
        }));
        return;
    }

    lines.push(Line::from(content).style(row_style));
    let column = 6.min(layout.width.saturating_sub(1));
    lines.extend(
        wrap_description(description, layout.width.saturating_sub(column))
            .into_iter()
            .map(|line| {
                Line::from(Span::styled(
                    format!("{}{}", " ".repeat(column), line),
                    description_style,
                ))
                .style(row_style)
            }),
    );
}

pub(super) fn wrap_description(value: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![terminal_text(value)];
    }
    let value = terminal_text(value);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;

    for word in value.split_whitespace() {
        let word_width = display_width(word);
        if current_width > 0 && current_width + 1 + word_width <= width {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
            continue;
        }
        if current_width > 0 {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        for character in word.chars() {
            let character_width = display_width(&character.to_string());
            if current_width > 0 && current_width + character_width > width {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(character);
            current_width += character_width;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

pub(super) fn display_width(value: &str) -> usize {
    Span::raw(value).width()
}

pub(super) fn render_apply_actions(
    lines: &mut Vec<Line<'static>>,
    state: &SetupState,
    start: usize,
    layout: Option<AgentLayout>,
) {
    lines.push(Line::from(""));
    if state.default_only {
        apply_choice(
            lines,
            SAVE_DEFAULT_LABEL,
            SAVE_DEFAULT_DESCRIPTION,
            state.row == start,
            layout,
        );
        return;
    }
    apply_choice(
        lines,
        UPDATE_BOT_LABEL,
        UPDATE_BOT_DESCRIPTION,
        state.row == start,
        layout,
    );
}

pub(super) fn apply_choice(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    description: &str,
    focused: bool,
    layout: Option<AgentLayout>,
) {
    if let Some(layout) = layout {
        agent_choice(lines, label, description, focused, "→", layout);
    } else {
        choice(lines, label, description, focused, "→");
    }
}

pub(in crate::frontend) fn choice(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    description: &str,
    focused: bool,
    marker: &str,
) {
    let theme = current();
    let role = if focused { Role::Selection } else { Role::Text };
    lines.push(
        Line::from(vec![
            Span::styled(
                format!(
                    "{} {:3} {}  ",
                    if focused { "›" } else { " " },
                    marker,
                    terminal_text(label)
                ),
                theme.style(role),
            ),
            Span::styled(
                terminal_text(description),
                theme.style(if focused {
                    Role::Selection
                } else {
                    Role::Muted
                }),
            ),
        ])
        .style(theme.style(role)),
    );
}

pub(super) fn masked_credential(credential: &str) -> String {
    let count = credential.chars().count();
    let mut masked = "•".repeat(count.min(32));
    if count > 32 {
        masked.push('…');
    }
    masked
}

pub(super) fn footer(state: &SetupState) -> &'static str {
    if state.remove_confirmation.is_some() {
        return "  Remove selected provider? · y confirm · n cancel";
    }
    match state.page {
        Page::Provider if state.instance().is_some() => {
            "  ↑↓ select · enter edit · x remove · esc cancel"
        }
        Page::Provider => "  ↑↓ select · enter add · esc cancel",
        Page::Authentication => "  type/paste · tab switch field · enter continue · esc back",
        Page::Models => "  ↑↓ move · space select · enter activate · esc back",
        Page::Agent => {
            "  ↑↓ move · space change · ←→ close/open/adjust · enter open/apply · esc cancel"
        }
    }
}

pub(super) fn render_progress(lines: &mut Vec<Line<'static>>, progress: &Progress) {
    let theme = current();
    lines.push(Line::styled(
        format!("  {}", progress.title),
        theme.style(Role::Text).add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::styled(
        format!("  {}", terminal_text(&progress.detail)),
        theme.style(Role::Muted),
    ));
    if let Some((verification_url, user_code)) = &progress.verification {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            format!("  {}", terminal_text(verification_url)),
            theme.style(Role::Info),
        ));
        lines.push(Line::styled(
            format!("  Code: {}", terminal_text(user_code)),
            theme.style(Role::AccentStrong).add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        if progress.verification.is_some() {
            "  waiting for the gateway… · esc return to möbius"
        } else {
            "  waiting for the gateway…"
        },
        theme.style(Role::Muted),
    ));
}
