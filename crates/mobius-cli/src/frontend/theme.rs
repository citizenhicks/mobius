use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;

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
    diff_add: Color::Rgb(59, 66, 82),
    diff_delete: Color::Rgb(67, 76, 94),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_uses_exact_nord_colors() {
        assert_eq!(
            [
                NORD.surface,
                NORD.foreground,
                NORD.muted,
                NORD.border,
                NORD.accent,
                NORD.accent_strong,
                NORD.info,
                NORD.reasoning,
                NORD.code,
                NORD.neutral,
                NORD.success,
                NORD.warning,
                NORD.error,
                NORD.diff_add,
                NORD.diff_delete,
            ],
            [
                Color::Rgb(59, 66, 82),
                Color::Rgb(216, 222, 233),
                Color::Rgb(76, 86, 106),
                Color::Rgb(67, 76, 94),
                Color::Rgb(136, 192, 208),
                Color::Rgb(143, 188, 187),
                Color::Rgb(129, 161, 193),
                Color::Rgb(180, 142, 173),
                Color::Rgb(163, 190, 140),
                Color::Rgb(94, 129, 172),
                Color::Rgb(163, 190, 140),
                Color::Rgb(235, 203, 139),
                Color::Rgb(191, 97, 106),
                Color::Rgb(59, 66, 82),
                Color::Rgb(67, 76, 94),
            ]
        );
    }
}
