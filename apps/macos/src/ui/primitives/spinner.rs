//! MobiusSpinner (MobiusStyle.swift): a bright head chasing a fading tail around an
//! accent track. The angle comes off the clock, so a re-render never restarts it; each
//! frame asks only the view that painted it for the next one, at most 30 per second.

use super::controls::palette;
use super::motion;
use crate::numeric::ToFloat;
use gpui::prelude::*;
use gpui::{
    App, Bounds, EntityId, Hsla, PathBuilder, Pixels, Window, canvas, div, point, px, size,
};
use std::cell::RefCell;
use std::collections::HashSet;
use std::f32::consts::TAU;
use std::time::{SystemTime, UNIX_EPOCH};

const PERIOD_MS: u128 = 900;
const TRIM: (f32, f32) = (0.06, 0.86);
/// The track's angular gradient: (location, opacity).
const STOPS: [(f32, f32); 5] = [
    (0.06, 0.),
    (0.3, 0.04),
    (0.52, 0.14),
    (0.72, 0.38),
    (0.86, 0.82),
];
const SEGMENTS: usize = 48;

thread_local! {
    /// Views with a frame already scheduled, so several spinners share one tick.
    static SCHEDULED: RefCell<HashSet<EntityId>> = RefCell::new(HashSet::new());
}

/// The shared loading indicator; `color` defaults to the accent.
#[derive(IntoElement)]
pub(crate) struct Spinner {
    size: Pixels,
    color: Option<Hsla>,
}

impl Spinner {
    pub(crate) fn new(size: Pixels) -> Self {
        Self { size, color: None }
    }

    pub(crate) fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }
}

fn opacity_at(location: f32) -> f32 {
    let [.., (_, last)] = STOPS;
    STOPS
        .windows(2)
        .find_map(|pair| match *pair {
            [(l0, o0), (l1, o1)] if location <= l1 => {
                Some(o0 + (o1 - o0) * ((location - l0) / (l1 - l0)).clamp(0., 1.))
            }
            _ => None,
        })
        .unwrap_or(last)
}

/// The fraction of a turn the spinner has made, off the wall clock.
fn turn() -> f32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    (now % PERIOD_MS).to_f32() / PERIOD_MS.to_f32()
}

pub(super) fn schedule_frame(window: &Window, cx: &App) {
    let view = window.current_view();
    if !SCHEDULED.with(|views| views.borrow_mut().insert(view)) {
        return;
    }
    window
        .spawn(cx, async move |cx| {
            cx.background_executor().timer(motion::FRAME).await;
            SCHEDULED.with(|views| views.borrow_mut().remove(&view));
            if let Err(error) = cx.update(|_, cx| cx.notify(view)) {
                tracing::debug!(%error, "spinner window closed before its next frame");
            }
        })
        .detach();
}

fn paint_spinner(bounds: Bounds<Pixels>, extent: f32, tint: Hsla, turn: f32, window: &mut Window) {
    let ring = extent * 1.04;
    let line = (extent * 0.14).max(1.8);
    let head = (extent * 0.19).max(2.4);
    let radius = ((ring - line) / 2.).max(0.);
    let center = bounds.center();
    let at = |fraction: f32, r: f32| {
        let angle = (fraction + turn) * TAU - TAU / 4.;
        point(
            center.x + px(r * angle.cos()),
            center.y + px(r * angle.sin()),
        )
    };
    let (inner, outer) = (radius - line / 2., radius + line / 2.);
    let step = (TRIM.1 - TRIM.0) / SEGMENTS.to_f32();
    for index in 0..SEGMENTS {
        let (start, end) = (
            TRIM.0 + step * index.to_f32(),
            TRIM.0 + step * (index + 1).to_f32(),
        );
        let alpha = opacity_at(f32::midpoint(start, end));
        if alpha <= 0.005 {
            continue;
        }
        let mut path = PathBuilder::fill();
        path.add_polygon(
            &[
                at(start, outer),
                at(end, outer),
                at(end, inner),
                at(start, inner),
            ],
            true,
        );
        if let Ok(path) = path.build() {
            window.paint_path(path, tint.opacity(alpha));
        }
    }
    let dot = at(TRIM.1, radius);
    let half = px(head / 2.);
    window.paint_quad(super::pill_quad(
        Bounds::new(point(dot.x - half, dot.y - half), size(px(head), px(head))),
        tint,
    ));
}

impl RenderOnce for Spinner {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let tint = self.color.unwrap_or_else(|| palette(cx).accent);
        let extent = self.size;
        div().size(extent).flex_shrink_0().child(
            canvas(
                |_, _, _| {},
                move |bounds, (), window, cx| {
                    if bounds.intersect(&window.content_mask().bounds).is_empty() {
                        return;
                    }
                    let still = !motion::is_animating(window, cx);
                    paint_spinner(
                        bounds,
                        f32::from(extent),
                        tint,
                        if still { 0. } else { turn() },
                        window,
                    );
                    if !still {
                        schedule_frame(window, cx);
                    }
                },
            )
            .size_full(),
        )
    }
}
