//! Numbers that count to a new value instead of changing in one frame: usage totals, the
//! context share and anything else read as a quantity.

use gpui::{
    AnyElement, App, Bounds, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, SharedString, Window,
};
use std::time::Instant;

/// `render` of a value that eases to `value` over `STANDARD` whenever it changes.
pub fn counter(
    id: impl Into<SharedString>,
    value: f64,
    render: impl Fn(f64) -> AnyElement + 'static,
) -> Counter {
    Counter {
        id: id.into(),
        value,
        render: Box::new(render),
        child: None,
    }
}

pub struct Counter {
    id: SharedString,
    value: f64,
    render: Box<dyn Fn(f64) -> AnyElement>,
    child: Option<AnyElement>,
}

#[derive(Clone, Copy)]
struct State {
    from: f64,
    to: f64,
    since: Instant,
}

impl State {
    fn shown(&self, now: Instant) -> (f64, bool) {
        let t = ((now - self.since).as_secs_f64() / super::STANDARD.as_secs_f64()).min(1.);
        // Ease out quint: quick to start, gentle to land.
        let eased = 1. - (1. - t).powi(5);
        (self.from + (self.to - self.from) * eased, t < 1.)
    }
}

impl IntoElement for Counter {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl gpui::Element for Counter {
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
        let now = super::now();
        let (value, still) = (self.value, cx.reduce_motion());
        let shown =
            window.with_optional_element_state(id, |state: Option<Option<State>>, window| {
                let state = match state.flatten() {
                    Some(state) if state.to != value && !still => State {
                        from: state.shown(now).0,
                        to: value,
                        since: now,
                    },
                    Some(state) if !still => state,
                    _ => State {
                        from: value,
                        to: value,
                        since: now,
                    },
                };
                let (shown, moving) = state.shown(now);
                if moving {
                    window.request_animation_frame();
                }
                (shown, Some(state))
            });
        let mut child = (self.render)(shown);
        let layout = child.request_layout(window, cx);
        self.child = Some(child);
        (layout, ())
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
        if let Some(child) = &mut self.child {
            child.prepaint(window, cx);
        }
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
        if let Some(child) = &mut self.child {
            child.paint(window, cx);
        }
    }
}
