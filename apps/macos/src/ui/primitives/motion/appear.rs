//! Elements that fade in, and optionally rise the last few points into place, the first
//! time they appear and again whenever their id changes. The fade is a GPU motion, so
//! an appearing row or page renders the window once rather than every frame.

use super::{SMOOTH_FPS, now, sampled};
use gpui::{
    AnyElement, App, Bounds, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Motion, MotionFrame, Pixels, SharedString, Window, ease_out_quint, point, px,
};
use std::time::{Duration, Instant};

/// `child` fading in over `duration` from its first paint under `id`, risen by `lift`
/// at the start.
pub(super) fn appear(
    child: impl IntoElement,
    id: &ElementId,
    duration: Duration,
    lift: Pixels,
) -> Appear {
    Appear {
        id: SharedString::from(id.to_string()),
        child: child.into_any_element(),
        duration,
        lift,
        start: None,
    }
}

pub struct Appear {
    id: SharedString,
    child: AnyElement,
    duration: Duration,
    lift: Pixels,
    /// When this element first painted under its id.
    start: Option<Instant>,
}

impl IntoElement for Appear {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl gpui::Element for Appear {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name(SharedString::clone(&self.id)))
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        if let Some(id) = id {
            self.start = Some(window.with_element_state(id, |start: Option<Instant>, _| {
                let start = start.unwrap_or_else(now);
                (start, start)
            }));
        }
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        (): &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut (),
        (): &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(start) = self.start.filter(|start| now() < *start + self.duration) else {
            self.child.paint(window, cx);
            return;
        };
        let (duration, lift) = (self.duration.as_secs_f32(), self.lift);
        let ease = ease_out_quint();
        let fade = Motion::new(move |at| {
            let elapsed = sampled(at).saturating_duration_since(start).as_secs_f32();
            let shown = ease((elapsed / duration).min(1.));
            MotionFrame {
                offset: point(px(0.), lift * (1. - shown)),
                opacity: shown,
                ..MotionFrame::default()
            }
        })
        .until(start + self.duration)
        .max_fps(SMOOTH_FPS);
        window.with_motion(fade, bounds.center(), |window| self.child.paint(window, cx));
    }
}
