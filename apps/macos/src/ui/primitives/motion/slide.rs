//! Positions that follow layout on the spring: when a row moves because others were added,
//! removed, reordered or opened above it, it slides from where it was drawn instead of
//! appearing in its new place. Scrolling never counts as a move.

use gpui::{
    AnyElement, App, Bounds, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, SharedString, Window, point, px,
};
use std::time::Instant;

use super::spring;

/// `child`, sliding from its last drawn position whenever layout moves it.
pub fn slide(id: impl Into<SharedString>, child: impl IntoElement) -> Slide {
    Slide {
        id: id.into(),
        child: child.into_any_element(),
    }
}

pub struct Slide {
    id: SharedString,
    child: AnyElement,
}

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
        let offset =
            window.with_optional_element_state(id, |state: Option<Option<State>>, window| {
                let next = match state.flatten() {
                    Some(last) if !still => {
                        // Keep it where it was drawn, then let the spring bring it home.
                        let moved = last.offset + last.top - top;
                        let (offset, velocity) =
                            spring::step(moved, last.velocity, 0., now - last.at);
                        if offset != 0. {
                            window.request_animation_frame();
                        }
                        State {
                            top,
                            offset,
                            velocity,
                            at: now,
                        }
                    }
                    _ => State {
                        top,
                        offset: 0.,
                        velocity: 0.,
                        at: now,
                    },
                };
                (next.offset, Some(next))
            });
        window.with_element_offset(point(px(0.), px(offset)), |window| {
            self.child.prepaint(window, cx)
        });
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        (): &mut (),
        (): &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}
