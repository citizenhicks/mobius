//! Heights that follow their content on a critically damped spring, so a row that arrives,
//! streams, opens, folds or loads an image slides instead of jumping. Tail-following lists
//! then scroll smoothly too, since they only ever see the height change a frame at a time.

use gpui::{
    AnyElement, App, Bounds, ContentMask, Display, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Length, Overflow, Pixels, SharedString, Style, Window, px, relative,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Instant;

use super::spring;

// ponytail: registries keep one float per row ever painted (bytes per chat); prune by
// scope if long sessions ever show it.
thread_local! {
    /// The last painted height of every smoothed element by id, and how far it has opened
    /// toward its content, for fading it in step.
    static PAINTED: RefCell<HashMap<SharedString, (f32, f32)>> = RefCell::new(HashMap::new());
    /// Heights an element starts from the next time it lays out.
    static HANDED: RefCell<HashMap<SharedString, f32>> = RefCell::new(HashMap::new());
}

/// How far `id` has opened toward its content, from 0 to 1, as last painted; 1 when settled
/// or never seen. Content fades with it.
pub fn progress(id: &str) -> f32 {
    PAINTED.with(|map| map.borrow().get(id).map_or(1., |(_, opened)| *opened))
}

/// Whether `id` was ever painted, so a row scrolled back into view does not grow in again.
fn seen(id: &str) -> bool {
    PAINTED.with(|map| map.borrow().contains_key(id))
}

/// Whether `id` still shows: open, about to start from handed height, or still closing.
pub fn visible(id: &str) -> bool {
    HANDED.with(|map| map.borrow().contains_key(id))
        || PAINTED.with(|map| map.borrow().get(id).is_some_and(|(height, _)| *height > 0.))
}

/// `to` starts at the combined height `from` last had, less `less`, as when a turn's steps
/// fold under a new header or a removed row's space passes to the row after it.
pub fn hand_off<'a>(from: impl IntoIterator<Item = &'a str>, to: SharedString, less: f32) {
    let height: f32 = PAINTED.with(|map| {
        from.into_iter()
            .filter_map(|id| map.borrow().get(id).map(|(height, _)| *height))
            .sum()
    });
    let base = PAINTED
        .with(|map| map.borrow().get(&to).map(|(height, _)| *height))
        .unwrap_or(0.);
    if height - less > 0. {
        HANDED.with(|map| map.borrow_mut().insert(to, base + height - less));
    }
}

/// `child` at a height that follows its content; `open(false)` closes it to nothing.
pub fn smooth(id: impl Into<SharedString>, child: impl IntoElement) -> Smooth {
    Smooth {
        id: id.into(),
        child: child.into_any_element(),
        grow: false,
        open: true,
    }
}

pub struct Smooth {
    id: SharedString,
    child: AnyElement,
    grow: bool,
    open: bool,
}

impl Smooth {
    /// Grows from nothing the first time it is ever painted.
    pub fn grow_in(mut self, grow: bool) -> Self {
        self.grow = grow && !seen(&self.id);
        self
    }

    pub fn open(mut self, open: bool) -> Self {
        self.open = open;
        self
    }
}

#[derive(Clone, Copy)]
struct State {
    height: f32,
    velocity: f32,
    at: Instant,
}

impl IntoElement for Smooth {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl gpui::Element for Smooth {
    /// The child's layout and the height this frame shows, when it is held.
    type RequestLayoutState = (LayoutId, Option<f32>);
    /// Whether the child is clipped this frame.
    type PrepaintState = bool;

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
    ) -> (LayoutId, Self::RequestLayoutState) {
        let child = self.child.request_layout(window, cx);
        let handed = HANDED.with(|map| map.borrow_mut().remove(&self.id));
        let held = id.and_then(|id| {
            window.with_element_state(id, |state: Option<Option<State>>, _| {
                let state = state.flatten();
                (state.map(|state| state.height), state)
            })
        });
        // A body that closed all the way reopens from nothing.
        let closed = PAINTED
            .with(|map| map.borrow().get(&self.id).map(|(height, _)| *height))
            .filter(|height| *height == 0.);
        let shown = handed
            .or(held)
            .or(closed)
            .or_else(|| (self.grow || !self.open).then_some(0.));
        let mut style = Style {
            display: Display::Block,
            ..Style::default()
        };
        style.size.width = relative(1.).into();
        if let Some(height) = shown {
            style.size.height = Length::Definite(px(height).into());
            style.overflow.y = Overflow::Hidden;
        }
        let layout = window.request_layout(style, [child], cx);
        (layout, (child, shown))
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        (child, shown): &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        self.child.prepaint(window, cx);
        let natural = f32::from(window.layout_bounds(*child).size.height);
        let target = if self.open { natural } else { 0. };
        let now = super::now();
        let still = cx.reduce_motion();
        let height = match id {
            Some(id) => window.with_element_state(id, |state: Option<Option<State>>, window| {
                let start = state.flatten().unwrap_or_else(|| State {
                    height: shown.unwrap_or(target),
                    velocity: 0.,
                    at: now,
                });
                let next = if still {
                    State {
                        height: target,
                        velocity: 0.,
                        at: now,
                    }
                } else {
                    let (height, velocity) =
                        spring::step(start.height, start.velocity, target, now - start.at);
                    State {
                        height,
                        velocity,
                        at: now,
                    }
                };
                if next.height != target {
                    window.request_animation_frame();
                }
                (next.height, Some(next))
            }),
            None => target,
        };
        let opened = if natural > 0. {
            (height / natural).min(1.)
        } else {
            1.
        };
        PAINTED.with(|map| {
            map.borrow_mut()
                .insert(SharedString::clone(&self.id), (height, opened))
        });
        shown.is_some_and(|shown| shown != natural)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        clipped: &mut bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        if *clipped {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                self.child.paint(window, cx);
            });
        } else {
            self.child.paint(window, cx);
        }
    }
}
