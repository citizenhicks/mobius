//! MobiusSpinner (MobiusStyle.swift): a bright head chasing a fading tail around an
//! accent track. The ring is painted once and turned by a GPU motion whose angle comes off
//! the clock, so a re-render never restarts it and turning costs no renders.

use super::controls::palette;
use super::motion;
use crate::numeric::ToFloat;
use gpui::prelude::*;
use gpui::{
    App, Bounds, Hsla, Motion, MotionFrame, PathBuilder, Pixels, Window, canvas, div, point, px,
    radians, size,
};
use std::f32::consts::TAU;
use std::ops::Range;

const TRIM: Range<f32> = 0.06..0.86;
/// The track's angular gradient: (location, opacity).
const STOPS: [(f32, f32); 5] = [
    (0.06, 0.),
    (0.3, 0.04),
    (0.52, 0.14),
    (0.72, 0.38),
    (0.86, 0.82),
];
const SEGMENTS: usize = 48;

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

/// The ring at rest, its head at the top; a motion turns it.
fn paint_spinner(bounds: Bounds<Pixels>, extent: f32, tint: Hsla, window: &mut Window) {
    let ring = extent * 1.04;
    let line = (extent * 0.14).max(1.8);
    let head = (extent * 0.19).max(2.4);
    let radius = ((ring - line) / 2.).max(0.);
    let center = bounds.center();
    let at = |fraction: f32, r: f32| {
        let angle = fraction * TAU - TAU / 4.;
        point(
            center.x + px(r * angle.cos()),
            center.y + px(r * angle.sin()),
        )
    };
    let (inner, outer) = (radius - line / 2., radius + line / 2.);
    let step = (TRIM.end - TRIM.start) / SEGMENTS.to_f32();
    for index in 0..SEGMENTS {
        let (start, end) = (
            TRIM.start + step * index.to_f32(),
            TRIM.start + step * (index + 1).to_f32(),
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
    let dot = at(TRIM.end, radius);
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
                    if !motion::in_view(bounds, window) {
                        return;
                    }
                    let extent = f32::from(extent);
                    if motion::is_animating(window, cx) {
                        let spin = Motion::new(|at| MotionFrame {
                            rotation: radians(
                                (motion::seconds(at) % motion::SPINNER_PERIOD.as_secs_f64()
                                    / motion::SPINNER_PERIOD.as_secs_f64())
                                .to_f32()
                                    * TAU,
                            ),
                            ..MotionFrame::default()
                        })
                        .max_fps(motion::SMOOTH_FPS);
                        window.with_motion(spin, bounds.center(), |window| {
                            paint_spinner(bounds, extent, tint, window);
                        });
                    } else {
                        paint_spinner(bounds, extent, tint, window);
                    }
                },
            )
            .size_full(),
        )
    }
}
