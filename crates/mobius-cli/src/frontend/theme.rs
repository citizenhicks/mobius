use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{HighlightSpacing, List, ListItem};

#[derive(Clone, Copy)]
pub(crate) enum Role {
    Canvas,
    Text,
    Muted,
    Border,
    Accent,
    AccentStrong,
    Info,
    Reasoning,
    Code,
    Neutral,
    Success,
    Warning,
    Error,
    Selection,
}

pub(crate) struct Theme {
    surface: Color,
    foreground: Color,
    muted: Color,
    border: Color,
    accent: Color,
    accent_strong: Color,
    info: Color,
    reasoning: Color,
    code: Color,
    neutral: Color,
    success: Color,
    warning: Color,
    error: Color,
    diff_add: Color,
    diff_delete: Color,
}

const NORD: Theme = Theme {
    surface: Color::Rgb(59, 66, 82),
    foreground: Color::Rgb(216, 222, 233),
    muted: Color::Rgb(76, 86, 106),
    border: Color::Rgb(67, 76, 94),
    accent: Color::Rgb(136, 192, 208),
    accent_strong: Color::Rgb(143, 188, 187),
    info: Color::Rgb(129, 161, 193),
    reasoning: Color::Rgb(180, 142, 173),
    code: Color::Rgb(163, 190, 140),
    neutral: Color::Rgb(94, 129, 172),
    success: Color::Rgb(163, 190, 140),
    warning: Color::Rgb(235, 203, 139),
    error: Color::Rgb(191, 97, 106),
    diff_add: Color::Rgb(33, 58, 43),
    diff_delete: Color::Rgb(74, 34, 29),
};

pub(crate) const fn current() -> &'static Theme {
    &NORD
}

impl Theme {
    pub(crate) const fn color(&self, role: Role) -> Color {
        match role {
            Role::Canvas | Role::Text => self.foreground,
            Role::Muted => self.muted,
            Role::Border => self.border,
            Role::Accent => self.accent,
            Role::AccentStrong | Role::Selection => self.accent_strong,
            Role::Info => self.info,
            Role::Reasoning => self.reasoning,
            Role::Code => self.code,
            Role::Neutral => self.neutral,
            Role::Success => self.success,
            Role::Warning => self.warning,
            Role::Error => self.error,
        }
    }

    pub(crate) fn style(&self, role: Role) -> Style {
        if matches!(role, Role::Selection) {
            Style::default()
                .fg(self.color(role))
                .bg(self.surface)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.color(role))
        }
    }

    pub(crate) const fn diff_add_background(&self) -> Color {
        self.diff_add
    }

    pub(crate) const fn diff_delete_background(&self) -> Color {
        self.diff_delete
    }
}

/// Dither shades fading in toward a heading, and back out after it.
pub(crate) const DITHER_IN: &str = "▓▒░";
pub(crate) const DITHER_OUT: &str = "░▒▓";
/// The dither mark a divider carries between its rule segments.
pub(crate) const DITHER_MARK: &str = "░▒▓▒░";

/// The "▓▒░ MÖBIUS section" line that heads every full-screen view.
pub(super) fn brand_header(section: &str) -> Line<'static> {
    let theme = current();
    Line::from(vec![
        Span::styled(format!("{DITHER_IN} "), theme.style(Role::AccentStrong)),
        Span::styled(
            "MÖBIUS",
            theme.style(Role::Accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {section}"), theme.style(Role::Muted)),
    ])
}

/// A list whose selected row is bold behind a "› " marker.
pub(super) fn selectable_list<'a>(
    items: impl IntoIterator<Item = impl Into<ListItem<'a>>>,
) -> List<'a> {
    List::new(items)
        .highlight_symbol("› ")
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
}
