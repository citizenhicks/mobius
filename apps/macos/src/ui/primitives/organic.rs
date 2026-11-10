//! OrganicImageReveal (TranscriptContentViews.swift): while an image is on its way a field
//! of accent tiles pulses; once it arrives the tiles clear from the middle out, with a
//! little noise, and the picture is there. Reduce Motion shows pictures at once.

use super::motion;
use crate::numeric::{Saturating, ToFloat};
use gpui::{App, Bounds, Hsla, IntoElement, Pixels, Styled, canvas, fill, point, px, size};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const COLUMNS: usize = 24;
/// The tiles clear over 0.72 s; the overlay stays a frame past that.
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

fn in_view(bounds: Bounds<Pixels>, window: &gpui::Window) -> bool {
    !bounds.intersect(&window.content_mask().bounds).is_empty()
}

/// The field pulsing while an image is on its way; its phase runs off the clock, so a
/// row replaced mid-turn keeps the rhythm.
pub fn pulse(accent: Hsla, paused: bool) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, cx| {
            if !in_view(bounds, window) {
                return;
            }
            let still = paused || !motion::is_animating(window, cx);
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            tiles(bounds, |rect, _, _, noise| {
                let pulse = if still {
                    0.5
                } else {
                    (time * (1.8 + noise as f64 * 1.4) + noise as f64 * 24.)
                        .sin()
                        .max(0.)
                        .to_f32()
                };
                let inset = rect.size.width.min(rect.size.height) * (0.10 + noise * 0.06);
                let rect = Bounds::new(
                    point(rect.origin.x + inset, rect.origin.y + inset),
                    size(rect.size.width - inset * 2., rect.size.height - inset * 2.),
                );
                window.paint_quad(fill(rect, accent.opacity(0.06 + 0.84 * pulse * pulse)));
            });
            if !still {
                super::spinner::schedule_frame(window, cx);
            }
        },
    )
    .absolute()
    .size_full()
}

/// Tiles of `cover` over a picture that arrived at `arrived`, clearing from the middle out
/// until `REVEAL` has passed; nothing once it has, or under Reduce Motion.
pub fn reveal(cover: Hsla, arrived: Instant, cx: &App) -> Option<impl IntoElement> {
    if cx.reduce_motion() || arrived.elapsed() >= REVEAL {
        return None;
    }
    Some(
        canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| {
                let progress = (arrived.elapsed().as_secs_f32() / 0.72).min(1.);
                tiles(bounds, |rect, x, y, noise| {
                    let distance = ((x - 0.47).powi(2) + (y - 0.53).powi(2)).sqrt();
                    let order = (distance * 0.9 + noise * 0.3).min(1.);
                    let shown = ((progress - order * 0.68) / 0.24).clamp(0., 1.);
                    if shown < 1. {
                        window.paint_quad(fill(rect, cover.opacity(1. - shown)));
                    }
                });
                if progress < 1. && motion::is_animating(window, cx) && in_view(bounds, window) {
                    window.request_animation_frame();
                }
            },
        )
        .absolute()
        .size_full(),
    )
}
