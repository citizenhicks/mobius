//! Empty states, transcript notices and loading placeholders (spec §1.6).

use super::Shape;
use super::Spinner;
use super::controls::palette;
use super::tokens::{glyph, height, space, width};
use super::{Type, Typography, heading, icon};
use crate::ui::theme::{Palette, mix};
use gpui::prelude::*;
use gpui::{AnyElement, App, Div, Hsla, SharedString, Window, div, px, relative};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Success,
    Warning,
    Error,
}

impl Tone {
    pub fn color(self, p: &Palette) -> Hsla {
        match self {
            Self::Info => p.accent,
            Self::Success => p.signal,
            Self::Warning => p.warning,
            Self::Error => p.danger,
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Info => "hi.info",
            Self::Success => "hi.checkCircle",
            Self::Warning => "hi.warning",
            Self::Error => "hi.xCircle",
        }
    }
}

/// A centred placeholder for pages and panes with nothing to show (MobiusUnavailable).
#[derive(IntoElement)]
pub struct EmptyState {
    glyph: SharedString,
    title: SharedString,
    detail: Option<SharedString>,
    action: Option<AnyElement>,
    loading: bool,
}

impl EmptyState {
    pub fn new(glyph: impl Into<SharedString>, title: impl Into<SharedString>) -> Self {
        Self {
            glyph: glyph.into(),
            title: title.into(),
            detail: None,
            action: None,
            loading: false,
        }
    }

    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.action = Some(action.into_any_element());
        self
    }

    /// Swaps the glyph for a spinner.
    pub fn loading(mut self) -> Self {
        self.loading = true;
        self
    }
}

impl RenderOnce for EmptyState {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let glyph = if self.loading {
            Spinner::new(glyph::ICON).into_any_element()
        } else {
            icon(&self.glyph, &palette(cx))
                .size(glyph::HERO)
                .into_any_element()
        };
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(space::S)
            .p(space::L)
            .child(glyph)
            .child(heading(self.title))
            .children(self.detail.map(|detail| {
                div()
                    .max_w(width::NOTE)
                    .text_center()
                    .typography(Type::Body)
                    .text_color(p.muted)
                    .child(detail)
            }))
            .children(self.action)
    }
}

/// The sidebar's quiet empty note.
#[derive(IntoElement)]
pub struct SidebarNote(SharedString);

pub fn sidebar_note(text: impl Into<SharedString>) -> SidebarNote {
    SidebarNote(text.into())
}

impl RenderOnce for SidebarNote {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .px(space::S)
            .py(space::L)
            .typography(Type::Caption)
            .text_color(palette(cx).muted)
            .child(self.0)
    }
}

/// A softly tinted message in a conversation: an error recorded in the transcript.
/// Everything that happens elsewhere arrives as a toast.
#[derive(IntoElement)]
pub struct Notice {
    tone: Tone,
    title: SharedString,
}

impl Notice {
    pub fn new(tone: Tone, title: impl Into<SharedString>) -> Self {
        Self {
            tone,
            title: title.into(),
        }
    }
}

impl RenderOnce for Notice {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = palette(cx);
        let color = self.tone.color(&p);
        div()
            .w_full()
            .px(space::L)
            .py(space::S)
            .tile_corners()
            .border_1()
            .border_color(color.opacity(0.25))
            .bg(color.opacity(0.06))
            .typography(Type::Caption)
            .text_color(mix(color, p.text, 0.15))
            .whitespace_normal()
            .child(self.title)
    }
}

/// gpui-component's `Skeleton`, its breathing drawn as a GPU motion: the same theme fill
/// and two-second bounce from full to half opacity, without rendering the window.
#[derive(IntoElement)]
pub(crate) struct Skeleton {
    style: gpui::StyleRefinement,
}

impl Skeleton {
    pub(crate) fn new() -> Self {
        Self {
            style: gpui::StyleRefinement::default(),
        }
    }
}

impl Styled for Skeleton {
    fn style(&mut self) -> &mut gpui::StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Skeleton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        use gpui::MotionExt as _;
        use gpui_component::{ActiveTheme as _, StyledExt as _};
        static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let epoch = *EPOCH.get_or_init(std::time::Instant::now);
        let breathe = gpui::bounce(gpui::ease_in_out);
        let body = div()
            .w_full()
            .h_4()
            .bg(cx.theme().skeleton)
            .refine_style(&self.style);
        if !super::motion::is_animating(window, cx) {
            return body.into_any_element();
        }
        body.with_motion(
            gpui::Motion::new(move |at| {
                let elapsed = super::motion::sampled(at)
                    .saturating_duration_since(epoch)
                    .as_secs_f32();
                gpui::MotionFrame {
                    opacity: 1. - breathe(elapsed % 2. / 2.) * 0.5,
                    ..gpui::MotionFrame::default()
                }
            })
            .max_fps(super::motion::AMBIENT_FPS),
        )
        .into_any_element()
    }
}

/// Skeleton rows while a list loads.
pub fn loading_rows(count: usize) -> Div {
    const WIDTHS: [f32; 3] = [0.6, 0.85, 0.7];
    div()
        .flex()
        .flex_col()
        .children(WIDTHS.iter().cycle().take(count).map(|width| {
            div()
                .h(height::REGULAR)
                .px(space::S)
                .flex()
                .items_center()
                .child(
                    Skeleton::new()
                        .h(px(12.))
                        .pill_corners()
                        .w(relative(*width)),
                )
        }))
}
