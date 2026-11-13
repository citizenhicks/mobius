//! OrganicImageReveal (TranscriptContentViews.swift): while an image is on its way a field
//! of accent tiles pulses; once it arrives the tiles clear from the middle out, with a
//! little noise, and the picture is there. Reduce Motion shows pictures at once.
//!
//! Each tile is painted once and faded by a GPU motion, so neither the pulse nor the
//! reveal renders the window again while it runs.

use super::motion;
use crate::numeric::{Saturating, ToFloat};
use gpui::{App, Bounds, Hsla, IntoElement, Motion, Pixels, Styled, canvas, fill, point, px, size};
use std::time::{Duration, Instant};

const COLUMNS: usize = 24;
/// The tiles clear over 0.72 s; the completion render removes the overlay.
pub const REVEAL: Duration = Duration::from_millis(760);

fn noise(column: usize, row: usize) -> f32 {
    let value = (column.to_f64() * 12.9898 + row.to_f64() * 78.233).sin() * 43758.5453;
    (value - value.floor()).to_f32()
}

/// `bounds` cut into a grid of 24 columns: each tile with its centre (0–1) and noise.
fn tiles(bounds: Bounds<Pixels>, mut tile: impl FnMut(Bounds<Pixels>, f32, f32, f32)) {
    let width: f32 = bounds.size.width.into();
    let height: f32 = bounds.size.height.into();
    if width <= 0. || height <= 0. {
        return;
    }
    let columns = COLUMNS.to_f32();
    let rows = (height / width * columns).round().saturating_usize().max(1);
    let (tile_width, tile_height) = (width / columns, height / rows.to_f32());
    for row in 0..rows {
        for column in 0..COLUMNS {
            let rect = Bounds::new(
                point(
                    bounds.origin.x + px(column.to_f32() * tile_width),
                    bounds.origin.y + px(row.to_f32() * tile_height),
                ),
                size(px(tile_width), px(tile_height)),
            );
            tile(
                rect,
                (column.to_f32() + 0.5) / columns,
                (row.to_f32() + 0.5) / rows.to_f32(),
                noise(column, row),
            );
        }
    }
}

/// The field pulsing while an image is on its way; its phase runs off the clock, so a
/// row replaced mid-turn keeps the rhythm.
pub fn pulse(accent: Hsla, paused: bool) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, cx| {
            if !motion::in_view(bounds, window) {
                return;
            }
            let still = paused || !motion::is_animating(window, cx);
            tiles(bounds, |rect, _, _, noise| {
                let inset = rect.size.width.min(rect.size.height) * (0.10 + noise * 0.06);
                let rect = Bounds::new(
                    point(rect.origin.x + inset, rect.origin.y + inset),
                    size(rect.size.width - inset * 2., rect.size.height - inset * 2.),
                );
                if still {
                    window.paint_quad(fill(rect, accent.opacity(glow(0.5))));
                    return;
                }
                let beat = Motion::opacity(move |at| glow(beat_at(motion::seconds(at), noise)))
                    .max_fps(motion::AMBIENT_FPS);
                window.with_motion(beat, rect.center(), |window| {
                    window.paint_quad(fill(rect, accent));
                });
            });
        },
    )
    .absolute()
    .size_full()
}

/// A tile's opacity at `pulse`, from 0 to 1.
fn glow(pulse: f32) -> f32 {
    0.06 + 0.84 * pulse * pulse
}

/// A tile's pulse at monotonic `time` in seconds, so replaced rows keep the rhythm.
fn beat_at(time: f64, noise: f32) -> f32 {
    (time * (1.8 + noise as f64 * 1.4) + noise as f64 * 24.)
        .sin()
        .max(0.)
        .to_f32()
}

/// Tiles of `cover` over a picture that arrived at `arrived`, clearing from the middle out
/// until `REVEAL` has passed; nothing once it has, or under Reduce Motion.
pub fn reveal(cover: Hsla, arrived: Instant, cx: &App) -> Option<impl IntoElement> {
    if cx.reduce_motion() || motion::now().saturating_duration_since(arrived) >= REVEAL {
        return None;
    }
    Some(
        canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| {
                let animating = motion::is_animating(window, cx);
                let until = arrived + REVEAL;
                tiles(bounds, |rect, x, y, noise| {
                    let distance = ((x - 0.47).powi(2) + (y - 0.53).powi(2)).sqrt();
                    let order = (distance * 0.9 + noise * 0.3).min(1.);
                    // How much of the picture under the tile shows at `at`.
                    let shown = move |at: Instant| {
                        let elapsed = motion::sampled(at).saturating_duration_since(arrived);
                        let progress = (elapsed.as_secs_f32() / 0.72).min(1.);
                        ((progress - order * 0.68) / 0.24).clamp(0., 1.)
                    };
                    let visible = shown(motion::now());
                    if visible >= 1. {
                        return;
                    }
                    if !animating {
                        window.paint_quad(fill(rect, cover.opacity(1. - visible)));
                        return;
                    }
                    let clearing = Motion::opacity(move |at| 1. - shown(at))
                        .until(until)
                        .max_fps(motion::SMOOTH_FPS);
                    window.with_motion(clearing, rect.center(), |window| {
                        window.paint_quad(fill(rect, cover));
                    });
                });
            },
        )
        .absolute()
        .size_full(),
    )
}
