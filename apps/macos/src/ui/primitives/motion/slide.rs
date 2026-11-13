//! Positions that follow layout on the spring: when a row moves because others were added,
//! removed, reordered or opened above it, it slides from where it was drawn instead of
//! appearing in its new place. Scrolling never counts as a move. The slide is a GPU
//! motion: layout and hit testing use the new place at once, and the window renders once.

use gpui::{
    AnyElement, App, Bounds, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Motion, MotionFrame, Pixels, SharedString, Window, point, px,
};
use std::time::{Duration, Instant};

use super::spring;

/// `child`, sliding from its last drawn position whenever layout moves it.
pub fn slide(id: impl Into<SharedString>, child: impl IntoElement) -> Slide {
    Slide {
        id: id.into(),
        child: child.into_any_element(),
        seed: State {
            top: 0.,
            offset: 0.,
            velocity: 0.,
            at: Instant::now(),
        },
    }
}

pub struct Slide {
    id: SharedString,
    child: AnyElement,
    /// Where the spring started from, read in paint.
    seed: State,
}

/// Longer than the spring takes to come home from any distance a row moves.
const SETTLE: Duration = Duration::from_millis(600);

#[derive(Clone, Copy)]
struct State {
    /// Where layout last placed it, ignoring scroll.
    top: f32,
    offset: f32,
    velocity: f32,
    at: Instant,
}

impl IntoElement for Slide {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl gpui::Element for Slide {
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
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let top = f32::from(bounds.top() - window.element_offset().y);
        let now = super::now();
        let still = cx.reduce_motion();
        self.seed = window.with_optional_element_state(id, |state: Option<Option<State>>, _| {
            let next = match state.flatten() {
                // Moved by layout: carry on from where it is drawn now, toward its new place.
                Some(last) if !still && last.top != top => {
                    let (offset, velocity) =
                        spring::at(last.offset, last.velocity, 0., now - last.at);
                    State {
                        top,
                        offset: offset + last.top - top,
                        velocity,
                        at: now,
                    }
                }
                Some(last) if !still => last,
                _ => State {
                    top,
                    offset: 0.,
                    velocity: 0.,
                    at: now,
                },
            };
            (next, Some(next))
        });
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
        let seed = self.seed;
        let settled =
            spring::at(seed.offset, seed.velocity, 0., super::now() - seed.at) == (0., 0.);
        if settled {
            self.child.paint(window, cx);
            return;
        }
        // The spring settles well within this; the motion stops presenting after it.
        let home = Motion::new(move |at| {
            let elapsed = super::sampled(at).saturating_duration_since(seed.at);
            MotionFrame {
                offset: point(
                    px(0.),
                    px(spring::at(seed.offset, seed.velocity, 0., elapsed).0),
                ),
                ..MotionFrame::default()
            }
        })
        .until(seed.at + SETTLE)
        .max_fps(super::SMOOTH_FPS);
        window.with_motion(home, bounds.center(), |window| self.child.paint(window, cx));
    }
}
