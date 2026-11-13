//! The type scale (`docs/DESIGN.md`). Every text size comes from `Type`; colour stays at the call
//! site, and a call site may only raise the weight for emphasis.

use gpui::{
    Div, FontWeight, HighlightStyle, Hsla, IntoElement, ParentElement, SharedString,
    StyleRefinement, Styled, div, px,
};
use gpui_component::text::TextViewStyle;
use serde::{Deserialize, Serialize};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FontSet {
    Geist,
    IbmPlex,
    Roboto,
    Space,
    #[default]
    #[serde(other)]
    System,
}

thread_local! {
    // GPUI has one UI thread and an app-wide Theme. This is its derived typography
    // projection for Styled helpers, which have no App context; Preferences::install owns it.
    static FONT_SET: Cell<FontSet> = const { Cell::new(FontSet::System) };
}

impl FontSet {
    pub const ALL: [Self; 5] = [
        Self::System,
        Self::Geist,
        Self::IbmPlex,
        Self::Roboto,
        Self::Space,
    ];

    pub fn current() -> Self {
        FONT_SET.get()
    }

    pub(in crate::ui) fn install(self) {
        FONT_SET.set(self);
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Geist => "Geist",
            Self::IbmPlex => "IBM Plex",
            Self::Roboto => "Roboto",
            Self::Space => "Space Grotesk",
        }
    }

    pub fn regular(self) -> &'static str {
        match self {
            Self::System => TEXT_FAMILY,
            Self::Geist => "Geist",
            Self::IbmPlex => "IBM Plex Sans",
            Self::Roboto => "Roboto",
            Self::Space => "Space Grotesk",
        }
    }

    pub fn mono(self) -> &'static str {
        match self {
            Self::System => MONO_FAMILY,
            Self::Geist => "Geist Mono",
            Self::IbmPlex => "IBM Plex Mono",
            Self::Roboto => "Roboto Mono",
            Self::Space => "Space Mono",
        }
    }

    pub fn lettering(self) -> &'static str {
        if self == Self::System {
            "SF Pro Rounded"
        } else {
            self.regular()
        }
    }
}

#[derive(Clone, Copy)]
pub enum Type {
    /// Page, section, card, dialog and empty-state titles.
    Headline,
    /// Rows, menus, inputs and sidebar rows.
    Body,
    /// Transcript messages: Body with a reading line height.
    Prose,
    /// Chat title, button labels and banner titles.
    BodyMedium,
    /// The label over a group or a field, one step under a heading.
    Subhead,
    /// Subtitles, help and supporting text.
    Caption,
    /// Pills, tabs, segments and compact labels.
    Label,
    /// Ids, versions, paths, fingerprints and counters. Never prose.
    Mono,
    /// Code panes and tool output.
    Code,
    /// Pairing and device codes.
    CodeDisplay,
}

impl Type {
    pub fn size(self) -> gpui::Pixels {
        px(self.metrics().0)
    }

    pub fn line_height(self) -> gpui::Pixels {
        px(self.metrics().1)
    }

    /// Size, line height and weight. Two sizes, 14 and 12: a role is told apart by weight or
    /// design, never by a third size. Code is SF Mono Medium at the smaller size.
    fn metrics(self) -> (f32, f32, FontWeight) {
        match self {
            Self::Headline | Self::CodeDisplay => (14., 20., FontWeight::BOLD),
            Self::Body => (14., 20., FontWeight::NORMAL),
            Self::Prose => (14., 22., FontWeight::NORMAL),
            Self::BodyMedium => (14., 20., FontWeight::MEDIUM),
            Self::Subhead => (12., 16., FontWeight::SEMIBOLD),
            Self::Caption | Self::Mono => (12., 16., FontWeight::NORMAL),
            Self::Label => (12., 16., FontWeight::MEDIUM),
            Self::Code => (12., 18., FontWeight::MEDIUM),
        }
    }

    fn is_mono(self) -> bool {
        matches!(self, Self::Mono | Self::Code | Self::CodeDisplay)
    }
}

/// SF Pro Text: the window root sets it with `Type::Body`.
pub const TEXT_FAMILY: &str = "SF Pro Text";

/// SF Mono, reachable only through the hidden system family name.
pub const MONO_FAMILY: &str = ".AppleSystemUIFontMonospaced";

pub trait Typography: Styled + Sized {
    fn typography(self, style: Type) -> Self {
        let (size, line, weight) = style.metrics();
        let styled = self
            .text_size(px(size))
            .line_height(px(line))
            .font_weight(weight);
        if style.is_mono() {
            styled.font_family(FontSet::current().mono())
        } else {
            styled
        }
    }
}

impl<T: Styled> Typography for T {}

/// A dialog or sheet title on the heading role; gpui-component's own title is larger.
pub fn heading(text: impl Into<SharedString>) -> Div {
    div().typography(Type::Headline).child(text.into())
}

/// Markdown headings stay on the heading role, so no heading level is larger than body text;
/// code blocks are SF Mono Medium like `Type::Code`; inline code is bold SF Mono in `color`
/// on the theme's code wash, inside the faint ring of [`inline_code_ring`].
pub fn markdown_style(color: Hsla) -> TextViewStyle {
    let medium = FontWeight::MEDIUM;
    TextViewStyle {
        inline_code: HighlightStyle {
            color: Some(color),
            font_weight: Some(FontWeight::BOLD),
            ..HighlightStyle::default()
        },
        ..TextViewStyle::default()
    }
    .heading_font_size(|_, _| Type::Headline.size())
    .code_block(StyleRefinement::default().font_weight(medium))
}

/// The ring around inline code in [`markdown_style`] text.
pub fn inline_code_ring(color: Hsla) -> StyleRefinement {
    StyleRefinement::default()
        .border_1()
        .border_color(color.opacity(0.24))
}

/// Centres a leading glyph on the first line of top-aligned `style` text beside it.
pub fn first_line(style: Type, glyph: impl IntoElement) -> Div {
    div()
        .flex_shrink_0()
        .h(style.line_height())
        .flex()
        .items_center()
        .child(glyph)
}

/// The first non-empty line of user text, cut to 120 characters. Rows get this rather
/// than the whole text, which GPUI would shape every frame even when it is truncated.
pub fn single_line(text: &str) -> SharedString {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    clip(line, 120)
}

/// `text` cut to `max` characters, for multi-line previews under a line clamp.
pub fn clip(text: &str, max: usize) -> SharedString {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", text.get(..end).unwrap_or(text).trim_end()).into(),
        None => SharedString::from(text.to_owned()),
    }
}
