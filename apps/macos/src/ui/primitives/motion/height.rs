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

/// The fastest a height changes, in points a second: quick enough to keep up with streamed
/// text, slow enough that the eye follows the rows it moves.
const GLIDE: f32 = 2400.;

/// The most elements whose last painted height is kept. Past it, the half painted longest
/// ago is forgotten: such a row is treated as never seen, which only matters if it is
/// still waiting to grow in.
const KEPT: usize = 4096;

/// What a smoothed element last showed.
struct Painted {
    height: f32,
    /// How far it had opened toward its content, for fading it in step.
    opened: f32,
    at: Instant,
}

thread_local! {
    /// The last painted state of every smoothed element by id.
    static PAINTED: RefCell<HashMap<SharedString, Painted>> = RefCell::new(HashMap::new());
    /// Heights an element starts from the next time it lays out.
    static HANDED: RefCell<HashMap<SharedString, f32>> = RefCell::new(HashMap::new());
}

/// Records what `id` shows this frame, forgetting the stalest half once `KEPT` is reached.
fn remember(id: &SharedString, height: f32, opened: f32) {
    PAINTED.with(|map| {
        let mut map = map.borrow_mut();
        if map.len() >= KEPT && !map.contains_key(id) {
            let mut painted: Vec<Instant> = map.values().map(|painted| painted.at).collect();
            let (_, median, _) = painted.select_nth_unstable(KEPT / 2);
            let median = *median;
            map.retain(|_, painted| painted.at > median);
        }
        map.insert(
            SharedString::clone(id),
            Painted {
                height,
                opened,
                at: Instant::now(),
            },
        );
    });
}

/// How far `id` has opened toward its content, from 0 to 1, as last painted; 1 when settled
/// or never seen. Content fades with it.
pub fn progress(id: &str) -> f32 {
    PAINTED.with(|map| map.borrow().get(id).map_or(1., |painted| painted.opened))
}

/// Whether `id` was ever painted, so a row scrolled back into view does not grow in again.
fn seen(id: &str) -> bool {
    PAINTED.with(|map| map.borrow().contains_key(id))
}

/// Whether `id` still shows: open, about to start from handed height, or still closing.
pub fn visible(id: &str) -> bool {
    HANDED.with(|map| map.borrow().contains_key(id))
        || PAINTED.with(|map| {
            map.borrow()
                .get(id)
                .is_some_and(|painted| painted.height > 0.)
        })
}

/// `to` starts at the combined height `from` last had, less `less`, as when a turn's steps
/// fold under a new header or a removed row's space passes to the row after it.
pub fn hand_off<'a>(from: impl IntoIterator<Item = &'a str>, to: SharedString, less: f32) {
    let height: f32 = PAINTED.with(|map| {
        from.into_iter()
            .filter_map(|id| map.borrow().get(id).map(|painted| painted.height))
            .sum()
    });
    let base = PAINTED
        .with(|map| map.borrow().get(&to).map(|painted| painted.height))
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

impl gpui::Element for Smooth {
    /// The child's layout, the height this frame shows when it is held, and whether that
    /// height was handed over this frame.
    type RequestLayoutState = (LayoutId, Option<f32>, bool);
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
            .with(|map| map.borrow().get(&self.id).map(|painted| painted.height))
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
        (layout, (child, shown, handed.is_some()))
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        (child, shown, handed): &mut Self::RequestLayoutState,
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
                // A handed height restarts an element that was already at rest, such as the
                // row after a removed one, from that height.
                let start = state
                    .flatten()
                    .filter(|_| !*handed)
                    .unwrap_or_else(|| State {
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
                    let elapsed = now - start.at;
                    let (height, velocity) =
                        spring::step(start.height, start.velocity, target, elapsed);
                    // A spring covers a long way fast: a whole answer landing at once would
                    // fling the rows above it. Past this speed the height glides instead.
                    let reach = GLIDE * elapsed.as_secs_f32().min(0.05);
                    let step = height - start.height;
                    let (height, velocity) = if step.abs() > reach && reach > 0. {
                        (start.height + reach.copysign(step), GLIDE.copysign(step))
                    } else {
                        (height, velocity)
                    };
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
        remember(&self.id, height, opened);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn painted_heights_stay_bounded_and_keep_the_rows_painted_last() {
        for row in 0..KEPT * 3 {
            remember(&SharedString::from(format!("row:{row}")), 10., 1.);
        }
        assert!(PAINTED.with(|map| map.borrow().len()) <= KEPT);
        assert!(seen(&format!("row:{}", KEPT * 3 - 1)));
        assert!(!seen("row:0"));
    }
}
