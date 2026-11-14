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

    /// A small filled label, such as a mode or a card's kind: dark text on the role's colour.
    pub(crate) fn chip(&self, role: Role) -> Style {
        Style::default()
            .fg(CHIP_TEXT)
            .bg(self.color(role))
            .add_modifier(Modifier::BOLD)
    }
}

/// The mark: a sun setting on a horizon, over ground that thins out through a hand-placed
/// dither. Two braille rows for the chat header; each cell carries its own shade, the sun
/// brightest at its centre and the ground fading through the frost blues.
pub(crate) const LOGO: [[(&str, Role); 6]; 2] = [
    [
        ("⣠", Role::Accent),
        ("⣶", Role::Text),
        ("⣿", Role::Text),
        ("⣿", Role::Text),
        ("⣶", Role::Text),
        ("⣄", Role::Accent),
    ],
    [
        ("⢒", Role::Neutral),
        ("⠐", Role::Neutral),
        ("⢂", Role::Accent),
        ("⠒", Role::Accent),
        ("⢐", Role::Neutral),
        ("⠂", Role::Neutral),
    ],
];
/// The same sun as one row, for single-line headings.
pub(crate) const LOGO_LINE: &str = "⣠⣶⣿⣿⣶⣄";
/// Text on a filled chip, dark enough to read on every accent.
const CHIP_TEXT: Color = Color::Rgb(46, 52, 64);

/// The "loop MÖBIUS section" line that heads every full-screen view.
pub(super) fn brand_header(section: &str) -> Line<'static> {
    let theme = current();
    Line::from(vec![
        Span::styled(format!("{LOGO_LINE} "), theme.style(Role::Accent)),
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
