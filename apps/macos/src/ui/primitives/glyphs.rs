//! Hugeicons glyph lookup (R4).

use crate::ui::theme::Palette;
use gpui::{App, Radians, SharedString, Styled};
use gpui_component::Icon;
use mobius::protocol::FrontendSymbol;
use std::f32::consts::FRAC_PI_2;

/// An SVG glyph using the appearance's monochrome ink.
#[must_use]
pub fn icon(name: &str, palette: &Palette) -> Icon {
    Icon::empty()
        .path(SharedString::from(format!("icons/{name}.svg")))
        .text_color(palette.icon_ink())
}

pub fn back_icon(cx: &App) -> Icon {
    icon("hi.arrowUp", cx.global::<Palette>()).rotate(Radians(-FRAC_PI_2))
}

pub fn forward_icon(cx: &App) -> Icon {
    icon("hi.arrowUp", cx.global::<Palette>()).rotate(Radians(FRAC_PI_2))
}

/// A glyph by asset name, or an already transformed icon.
pub trait IntoGlyph {
    fn into_glyph(self, cx: &App) -> Icon;
}

impl IntoGlyph for &str {
    fn into_glyph(self, cx: &App) -> Icon {
        icon(self, cx.global::<Palette>())
    }
}

impl IntoGlyph for Icon {
    fn into_glyph(self, cx: &App) -> Icon {
        self.text_color(cx.global::<Palette>().icon_ink())
    }
}

/// The filled twin a rail shows for a selected glyph, or the glyph itself.
pub fn filled_glyph(glyph: &'static str) -> &'static str {
    const FILLED: &[(&str, &str)] = &[
        ("hi.stickyNote02", "hi.stickyNote02.fill"),
        ("hi.squaresFour", "hi.squaresFour.fill"),
        ("hi.bell", "hi.bell.fill"),
        ("hi.gear", "hi.gear.fill"),
        ("hi.note01", "hi.note01.fill"),
    ];
    FILLED
        .iter()
        .find_map(|&(outline, filled)| (outline == glyph).then_some(filled))
        .unwrap_or(glyph)
}

pub fn symbol_icon_or(symbol: Option<&FrontendSymbol>, fallback: &'static str) -> &'static str {
    known_symbol_icon(symbol).unwrap_or(fallback)
}

/// A transcript preview's glyph: its own symbol, or the Bot face of a subagent's
/// transcript, the only preview the gateway sends without one.
pub fn preview_glyph(symbol: Option<&FrontendSymbol>) -> &'static str {
    symbol_icon_or(symbol, "bot.face")
}

/// The glyph for a known gateway symbol. Beside a label an unknown symbol is dropped
/// rather than drawn as a placeholder (MobiusSymbol.knownGlyph).
pub fn known_symbol_icon(symbol: Option<&FrontendSymbol>) -> Option<&'static str> {
    const SYMBOLS: &[(&str, &str)] = &[
        ("agent", "bot.face"),
        ("brain", "hi.stickyNote02"),
        ("branch", "hi.gitBranch"),
        ("chat", "hi.chatCircle"),
        ("chat_gpt", "hi.chatGpt"),
        ("claude", "hi.claude"),
        ("deepseek", "hi.deepseek"),
        ("delete", "hi.trash"),
        ("edit", "hi.pencilSimple"),
        ("kimi", "hi.kimiAi"),
        ("mistral", "hi.mistral"),
        ("moon", "hi.moon"),
        ("open_router", "provider.openRouter"),
        ("plus", "hi.plus"),
        ("progress", "hi.arrowClockwise"),
        ("promote", "hi.arrowCircleUp"),
        ("queue", "hi.queue01"),
        ("route", "hi.path"),
        ("search", "hi.magnifyingGlass"),
        ("shield", "hi.shield02"),
        ("shield_alert", "hi.shieldAlert"),
        ("shield_check", "hi.shieldCheck"),
        ("question", "hi.userQuestion01"),
        ("shield_off", "hi.shieldOff"),
        ("sparkle", "hi.sparkle"),
        ("storage", "hi.hardDrives"),
        ("steer", "hi.arrowUpRight01"),
        ("task", "hi.checkCircle"),
        ("voice", "hi.audioWave01"),
    ];
    let name = symbol?.as_str();
    SYMBOLS
        .iter()
        .find(|(token, _)| *token == name)
        .map(|(_, icon)| *icon)
}
