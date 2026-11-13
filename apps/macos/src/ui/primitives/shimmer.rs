//! Status text with gpui-component's progress shimmer: a soft band sweeping across the
//! glyphs every shared shimmer period. The band is a GPU motion sampled at presentation, so a
//! shimmering line costs no renders between status changes; it matches
//! `gpui_component::shimmer::ShimmerText` (12 nested bands, 30% spread, a highlight
//! mixed a fifth of the way toward the theme's far end).

use super::motion;
use crate::numeric::ToFloat;
use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, Hsla, InspectorElementId, IntoElement,
    LayoutId, Motion, MotionFrame, MotionHighlight, Pixels, RenderOnce, SharedString, StyledText,
    Window, div, prelude::*, px,
};
use gpui_component::{ActiveTheme as _, Colorize as _};

/// Nested bands; the innermost is the brightest.
const LAYERS: u32 = 12;
/// Half the band's width, as a share of the text's width.
const SPREAD: f32 = 0.3;
/// The highlight sits this far from the text's lightness or it would not show.
const MIN_LIGHTNESS_GAP: f32 = 0.1;

/// `text` shimmering while motion runs; plain text in inactive windows or Reduce Motion.
#[derive(IntoElement)]
pub(crate) struct Shimmer {
    text: SharedString,
}

impl Shimmer {
    pub(crate) fn new(text: impl Into<SharedString>) -> Self {
        Self { text: text.into() }
    }
}

impl RenderOnce for Shimmer {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let text = StyledText::new(self.text);
        if !motion::is_animating(window, cx) {
            return div().min_w_0().child(text).into_any_element();
        }
        let tokens = cx.theme().semantic_tokens();
        div()
            .min_w_0()
            .child(Sweep {
                text,
                background: tokens.colors.background,
                foreground: tokens.colors.foreground,
                dark: cx.theme().is_dark(),
            })
            .into_any_element()
    }
}

/// Text laid out by `StyledText` and painted under the sweeping band.
struct Sweep {
    text: StyledText,
    background: Hsla,
    foreground: Hsla,
    dark: bool,
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

/// The band over text laid out at `bounds`, its phase shared by every shimmer.
fn sweep(bounds: Bounds<Pixels>, color: Hsla) -> Motion {
    let width = f32::from(bounds.size.width);
    let radius = width * SPREAD;
    let padding = SPREAD + 0.05;
    let period = motion::SHIMMER_PERIOD.as_secs_f64();
    Motion::new(move |at| {
        let elapsed = motion::seconds(at);
        let phase = (elapsed % period / period).to_f32();
        MotionFrame {
            highlight: Some(MotionHighlight {
                center: bounds.origin.x + px(phase.mul_add(1. + padding * 2., -padding) * width),
                radius: px(radius),
                color,
                layers: LAYERS,
            }),
            ..MotionFrame::default()
        }
    })
    .max_fps(motion::SMOOTH_FPS)
}

impl IntoElement for Sweep {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
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
        let motion = motion::in_view(bounds, window)
            .then(|| sweep(bounds, self.highlight(window.text_style().color)));
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
