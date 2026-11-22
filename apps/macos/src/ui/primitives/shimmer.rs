//! Status text with gpui-component's progress shimmer: a soft band sweeping across the
//! glyphs every shared shimmer period. The band is a GPU motion sampled at presentation, so a
//! shimmering line costs no renders between status changes; it matches
//! `gpui_component::shimmer::ShimmerText` (12 nested bands, 30% spread, a highlight
//! mixed a fifth of the way toward the theme's far end).
//!
//! A shimmer given a swap id also changes its text the way the iOS live line does: the
//! old text dims, the new text wipes in behind a soft edge, lands at the band's full
//! brightness and settles into the sweep. Both are GPU motions over laid-out text.

use super::motion;
use crate::numeric::ToFloat;
use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, Hsla, InspectorElementId, IntoElement,
    LayoutId, Motion, MotionFrame, MotionHighlight, MotionReveal, Pixels, RenderOnce, SharedString,
    StyledText, Window, div, prelude::*, px,
};
use gpui_component::{ActiveTheme as _, Colorize as _};
use std::time::Instant;

/// Nested bands; the innermost is the brightest.
const LAYERS: u32 = 12;
/// Half the band's width, as a share of the text's width.
const SPREAD: f32 = 0.3;
/// The highlight sits this far from the text's lightness or it would not show.
const MIN_LIGHTNESS_GAP: f32 = 0.1;
/// Width of the swap's soft edge.
const FEATHER: f32 = 18.;
/// How far the old text dims before the new text wipes over it.
const DIMMED: f32 = 0.35;

/// `text` shimmering while motion runs; plain text in inactive windows or Reduce Motion.
#[derive(IntoElement)]
pub(crate) struct Shimmer {
    text: SharedString,
    swap: Option<ElementId>,
}

impl Shimmer {
    pub(crate) fn new(text: impl Into<SharedString>) -> Self {
        Self {
            text: text.into(),
            swap: None,
        }
    }

    /// Wipe in each new text instead of replacing it; `id` keeps the outgoing text.
    pub(crate) fn swaps(mut self, id: impl Into<ElementId>) -> Self {
        self.swap = Some(id.into());
        self
    }
}

/// The text a swapping shimmer last showed, and the one it is replacing.
struct Swap {
    text: SharedString,
    previous: Option<(SharedString, Instant)>,
}

fn swap_end(since: Instant) -> Instant {
    since + motion::SWAP_DIM + motion::SWAP_WIPE + motion::SWAP_SETTLE
}

impl RenderOnce for Shimmer {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        if !motion::is_animating(window, cx) {
            return div()
                .min_w_0()
                .child(StyledText::new(self.text))
                .into_any_element();
        }
        let now = motion::now();
        let previous = self.swap.and_then(|id| {
            let swap = window.use_keyed_state(id, cx, |_, _| Swap {
                text: SharedString::clone(&self.text),
                previous: None,
            });
            swap.update(cx, |swap, _| {
                if swap.text != self.text {
                    let old = std::mem::replace(&mut swap.text, SharedString::clone(&self.text));
                    swap.previous = Some((old, now));
                }
                if swap
                    .previous
                    .as_ref()
                    .is_some_and(|(_, since)| now >= swap_end(*since))
                {
                    swap.previous = None;
                }
                swap.previous
                    .as_ref()
                    .map(|(text, since)| (SharedString::clone(text), *since))
            })
        });
        let tokens = cx.theme().semantic_tokens();
        let sweep = |text, swap| Sweep {
            text: StyledText::new(text),
            background: tokens.colors.background,
            foreground: tokens.colors.foreground,
            dark: cx.theme().is_dark(),
            swap,
        };
        let since = previous.as_ref().map(|(_, since)| *since);
        let incoming = sweep(self.text, since.map(|since| (since, Role::Incoming)));
        let outgoing = previous.map(|(text, since)| {
            div()
                .absolute()
                .top_0()
                .left_0()
                .whitespace_nowrap()
                .child(sweep(text, Some((since, Role::Outgoing))))
        });
        div()
            .relative()
            .min_w_0()
            .child(incoming)
            .children(outgoing)
            .into_any_element()
    }
}

#[derive(Clone, Copy)]
enum Role {
    Incoming,
    Outgoing,
}

/// Text laid out by `StyledText` and painted under the sweeping band; while it swaps,
/// the side of the swap it is on and when the swap began.
struct Sweep {
    text: StyledText,
    background: Hsla,
    foreground: Hsla,
    dark: bool,
    swap: Option<(Instant, Role)>,
}

impl Sweep {
    /// The band's colour over `text`: a fifth of the way toward the end of the theme
    /// that `text` is not already near, at the opacity of one band.
    fn highlight(&self, text: Hsla) -> Hsla {
        let (target, opposite) = if self.dark {
            (self.foreground, self.background)
        } else {
            (self.background, self.foreground)
        };
        let target = if (target.l - text.l).abs() < MIN_LIGHTNESS_GAP {
            opposite
        } else {
            target
        };
        let peak: f32 = if self.dark { 0.6 } else { 0.75 };
        let layer = 1. - (1. - peak).powf(1. / LAYERS.to_f32());
        text.mix_oklab(target, 0.2).opacity(layer)
    }
}

/// The band over text laid out at `bounds` at `at`, its phase shared by every shimmer.
fn band(bounds: Bounds<Pixels>, color: Hsla, at: Instant) -> MotionHighlight {
    let width = f32::from(bounds.size.width);
    let padding = SPREAD + 0.05;
    let period = motion::SHIMMER_PERIOD.as_secs_f64();
    let phase = (motion::seconds(at) % period / period).to_f32();
    MotionHighlight {
        center: bounds.origin.x + px(phase.mul_add(1. + padding * 2., -padding) * width),
        radius: px(width * SPREAD),
        color,
        layers: LAYERS,
    }
}

/// The sweeping band.
fn sweep(bounds: Bounds<Pixels>, color: Hsla) -> Motion {
    Motion::new(move |at| MotionFrame {
        highlight: Some(band(bounds, color, at)),
        ..MotionFrame::default()
    })
    .max_fps(motion::SMOOTH_FPS)
}

/// How far a swap that began at `since` has dimmed, wiped and settled at `at`, each 0 to 1.
fn phases(since: Instant, at: Instant) -> (f32, f32, f32) {
    let elapsed = motion::sampled(at)
        .saturating_duration_since(since)
        .as_secs_f32();
    let (dim, wipe, settle) = (
        motion::SWAP_DIM.as_secs_f32(),
        motion::SWAP_WIPE.as_secs_f32(),
        motion::SWAP_SETTLE.as_secs_f32(),
    );
    let wiped = ((elapsed - dim) / wipe).clamp(0., 1.);
    (
        (elapsed / dim).min(1.),
        1. - (1. - wiped).powi(3),
        ((elapsed - dim - wipe) / settle).clamp(0., 1.),
    )
}

/// The edge sweeping across `bounds` as the wipe runs from 0 to 1.
fn edge(bounds: Bounds<Pixels>, wiped: f32) -> Pixels {
    let width = f32::from(bounds.size.width);
    bounds.origin.x + px(wiped.mul_add(width + FEATHER, -FEATHER / 2.))
}

/// The new text: hidden while the old one dims, wiped in, lit end to end by the band's
/// colour as it lands, then the ordinary sweep.
fn incoming(bounds: Bounds<Pixels>, color: Hsla, since: Instant) -> Motion {
    let width = f32::from(bounds.size.width).max(1.);
    Motion::new(move |at| {
        let (_, wiped, settled) = phases(since, at);
        if settled >= 1. {
            return MotionFrame {
                highlight: Some(band(bounds, color, at)),
                ..MotionFrame::default()
            };
        }
        MotionFrame {
            reveal: Some(MotionReveal {
                edge: edge(bounds, wiped),
                feather: px(FEATHER),
            }),
            highlight: Some(MotionHighlight {
                center: bounds.center().x,
                radius: px(width * 4.),
                color: color.opacity(color.a * (1. - settled)),
                layers: LAYERS,
            }),
            ..MotionFrame::default()
        }
    })
    .max_fps(motion::SMOOTH_FPS)
}

/// The old text: dimmed, then hidden behind the edge as the new text wipes over it.
fn outgoing(bounds: Bounds<Pixels>, since: Instant) -> Motion {
    Motion::new(move |at| {
        let (dimmed, wiped, _) = phases(since, at);
        MotionFrame {
            opacity: (1. - (1. - DIMMED) * dimmed) * (1. - wiped),
            reveal: Some(MotionReveal {
                edge: edge(bounds, wiped),
                feather: px(-FEATHER),
            }),
            ..MotionFrame::default()
        }
    })
    .until(swap_end(since))
    .max_fps(motion::SMOOTH_FPS)
}

impl Element for Sweep {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.text.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text
            .prepaint(id, inspector_id, bounds, &mut (), window, cx);
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut (),
        (): &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let color = self.highlight(window.text_style().color);
        let motion = motion::in_view(bounds, window).then(|| match self.swap {
            None => sweep(bounds, color),
            Some((since, Role::Incoming)) => incoming(bounds, color, since),
            Some((since, Role::Outgoing)) => outgoing(bounds, since),
        });
        let mut paint = |window: &mut Window| {
            self.text
                .paint(id, inspector_id, bounds, &mut (), &mut (), window, cx);
        };
        match motion {
            Some(motion) => window.with_motion(motion, bounds.center(), paint),
            None => paint(window),
        }
    }
}
