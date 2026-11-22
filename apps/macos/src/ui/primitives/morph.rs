//! MobiusMorphIcon (MobiusMorph.swift): a glyph that bends into the next one instead of
//! swapping. Every stroke of a stroked Hugeicons SVG is resampled to the same number of
//! points; strokes are paired by length and nearness, a stroke without a partner grows
//! from or shrinks to a point, and the points travel while the status line swaps its text.
//! Filled glyphs, Reduce Motion and inactive windows swap at once.

use super::{glyph_path, icon, motion};
use crate::numeric::ToFloat;
use gpui::{
    App, AppContext as _, Bounds, ContentMask, Context, Entity, Hsla, IntoElement, Motion,
    MotionFrame, Pixels, Render, RenderImage, RenderOnce, Rgba, SharedString, StyleRefinement,
    Styled as _, Task, Window, canvas, point, px, size,
};
use resvg::{tiny_skia, usvg};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

/// Points per stroke: enough for a 24-unit glyph's curves to stay round at any size.
const POINTS: usize = 40;
/// The Hugeicons view box and stroke width.
const VIEW_BOX: f32 = 24.;
const STROKE_WIDTH: f32 = 2.;

type Points = [[f32; 2]; POINTS];
/// Strokes leaving and arriving, point for point.
type Pairs = Box<[(Points, Points)]>;
/// 23 samples cover the 375 ms morph at display cadence.
const STEPS: u32 = 22;

struct Stroke {
    points: Points,
    length: f32,
    closed: bool,
    centre: [f32; 2],
}

thread_local! {
    /// Parsed glyphs by asset name; `None` for one that cannot morph.
    static STROKES: RefCell<HashMap<&'static str, Option<Rc<[Stroke]>>>> =
        RefCell::default();
}

/// `name` as an icon of `size` that morphs whenever the name under `id` changes.
pub(crate) fn morph_icon(
    id: impl Into<SharedString>,
    name: &'static str,
    size: Pixels,
    color: Hsla,
) -> MorphIcon {
    MorphIcon {
        id: id.into(),
        glyph: Glyph { name, size, color },
    }
}

#[derive(IntoElement)]
pub(crate) struct MorphIcon {
    id: SharedString,
    glyph: Glyph,
}

#[derive(Clone, Copy, PartialEq)]
struct Glyph {
    name: &'static str,
    size: Pixels,
    color: Hsla,
}

struct State {
    name: &'static str,
    flight: Option<Entity<Flight>>,
}

/// The renderer moves a single strip behind the glyph's clip. The UI wakes once at the end.
struct Flight {
    glyph: Glyph,
    scale: f32,
    since: Instant,
    pairs: Pairs,
    image: Option<Arc<RenderImage>>,
    _settle: Task<()>,
}

impl Flight {
    fn start(glyph: Glyph, state: &State, scale: f32, cx: &mut App) -> Option<Entity<Self>> {
        let since = motion::now();
        let from = departure(state, since, cx)?;
        let to = strokes(glyph.name, cx)?;
        let pairs = pair(&from, &to);
        let image = strip(&pairs, glyph.size, scale, glyph.color)?;
        Some(cx.new(|cx: &mut Context<Self>| {
            cx.on_release(Self::release).detach();
            let settle = cx.spawn(async move |view, cx| {
                cx.background_executor().timer(motion::MORPH).await;
                if let Err(error) = view.update(cx, |_, cx| cx.notify()) {
                    tracing::trace!(%error, "morph ended after its view closed");
                }
            });
            Self {
                glyph,
                scale,
                since,
                pairs,
                image: Some(image),
                _settle: settle,
            }
        }))
    }

    fn release(&mut self, cx: &mut App) {
        if let Some(image) = self.image.take() {
            cx.drop_image(image, None);
            self.pairs = Box::default();
        }
    }
}

impl Render for Flight {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let since = self.since;
        if motion::is_animating(window, cx)
            && motion::now() < since + motion::MORPH
            && let Some(image) = &self.image
        {
            let image = Arc::clone(image);
            let extent = self.glyph.size;
            return canvas(
                |_, _, _| {},
                move |bounds, (), window, _| {
                    if !motion::in_view(bounds, window) {
                        return;
                    }
                    let strip =
                        Bounds::new(bounds.origin, size(extent * (STEPS + 1).to_f32(), extent));
                    let slide = Motion::new(move |at| {
                        let elapsed = motion::sampled(at)
                            .saturating_duration_since(since)
                            .as_secs_f32();
                        let frame = ((elapsed / motion::MORPH.as_secs_f32()).clamp(0., 1.)
                            * STEPS.to_f32())
                        .round();
                        MotionFrame {
                            offset: point(-extent * frame, px(0.)),
                            ..MotionFrame::default()
                        }
                    })
                    .until(since + motion::MORPH)
                    .max_fps(motion::SMOOTH_FPS);
                    window.with_content_mask(Some(ContentMask { bounds }), |window| {
                        window.with_motion(slide, bounds.origin, |window| {
                            if let Err(error) = window.paint_image(
                                strip,
                                strip,
                                Default::default(),
                                image,
                                0,
                                false,
                            ) {
                                tracing::debug!(%error, "morph texture could not be painted");
                            }
                        });
                    });
                },
            )
            .size(extent)
            .into_any_element();
        }
        self.release(cx);
        icon(self.glyph.name, &super::palette(cx))
            .text_color(self.glyph.color)
            .size(self.glyph.size)
            .into_any_element()
    }
}

fn ease_in_out(t: f32) -> f32 {
    if t < 0.5 {
        4. * t * t * t
    } else {
        1. - (-2. * t + 2.).powi(3) / 2.
    }
}

impl RenderOnce for MorphIcon {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self { id, glyph } = self;
        window.with_global_id(id.into(), |id, window| {
            window.with_element_state(id, |state: Option<State>, window| {
                let mut state = state.unwrap_or(State {
                    name: glyph.name,
                    flight: None,
                });
                if !motion::is_animating(window, cx) {
                    state.flight = None;
                } else if state.name != glyph.name {
                    state.flight = Flight::start(glyph, &state, window.scale_factor(), cx);
                }
                state.name = glyph.name;
                if let Some(flight) = &state.flight
                    && {
                        let view = flight.read(cx);
                        view.image.is_some()
                            && view.glyph == glyph
                            && view.scale == window.scale_factor()
                    }
                {
                    let style = StyleRefinement::default().size(glyph.size).flex_shrink_0();
                    return (
                        Entity::clone(flight).cached(style).into_any_element(),
                        state,
                    );
                }
                state.flight = None;
                (
                    icon(glyph.name, &super::palette(cx))
                        .text_color(glyph.color)
                        .size(glyph.size)
                        .into_any_element(),
                    state,
                )
            })
        })
    }
}

/// Start an interrupted transition at the outlines currently on screen.
fn departure(state: &State, now: Instant, cx: &App) -> Option<Rc<[Stroke]>> {
    if let Some(flight) = &state.flight {
        let flight = flight.read(cx);
        let elapsed = now.saturating_duration_since(flight.since);
        if flight.image.is_some() && elapsed < motion::MORPH {
            let t = ease_in_out(elapsed.as_secs_f32() / motion::MORPH.as_secs_f32());
            return Some(
                flight
                    .pairs
                    .iter()
                    .map(|(from, to)| stroke(lerp(from, to, t)))
                    .filter(|stroke| stroke.length > 0.01)
                    .collect(),
            );
        }
    }
    strokes(state.name, cx)
}

fn strip(
    pairs: &[(Points, Points)],
    extent: Pixels,
    scale: f32,
    color: Hsla,
) -> Option<Arc<RenderImage>> {
    use tiny_skia::{LineCap, LineJoin, Paint, PathBuilder, Pixmap, Transform};
    let side = u32::try_from(size(extent, extent).to_device_pixels(scale).width.0).ok()?;
    let width = side.checked_mul(STEPS + 1)?;
    let mut pixels = Pixmap::new(width, side)?;
    let [r, g, b, a] = u32::from(Rgba::from(color)).to_be_bytes();
    let mut ink = Paint::default();
    ink.set_color_rgba8(255, 255, 255, a);
    let style = tiny_skia::Stroke {
        width: STROKE_WIDTH,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    };
    let scale = side.to_f32() / VIEW_BOX;
    for frame in 0..=STEPS {
        let t = ease_in_out(frame.to_f32() / STEPS.to_f32());
        let mut path = PathBuilder::new();
        for (from, to) in pairs {
            let [x, y] = mix(from[0], to[0], t);
            path.move_to(x, y);
            for (&a, &b) in from[1..].iter().zip(&to[1..]) {
                let [x, y] = mix(a, b, t);
                path.line_to(x, y);
            }
        }
        let transform =
            Transform::from_scale(scale, scale).post_translate((frame * side).to_f32(), 0.);
        pixels.stroke_path(&path.finish()?, &ink, &style, transform, None);
    }
    // GPUI's atlas takes straight BGRA. Tiny-skia supplied the antialiased coverage.
    for pixel in pixels.data_mut().as_chunks_mut::<4>().0 {
        *pixel = [b, g, r, pixel[3]];
    }
    let image = image::RgbaImage::from_raw(width, side, pixels.take())?;
    Some(Arc::new(RenderImage::new([image::Frame::new(image)])))
}

fn mix([ax, ay]: [f32; 2], [bx, by]: [f32; 2], t: f32) -> [f32; 2] {
    [ax + (bx - ax) * t, ay + (by - ay) * t]
}

fn lerp(from: &Points, to: &Points, t: f32) -> Points {
    let mut points = *from;
    for (point, target) in points.iter_mut().zip(to) {
        *point = mix(*point, *target, t);
    }
    points
}

/// `name`'s strokes, parsed once; `None` when the glyph has a fill or will not load.
fn strokes(name: &'static str, cx: &App) -> Option<Rc<[Stroke]>> {
    if let Some(cached) = STROKES.with(|cache| {
        cache
            .borrow()
            .get(name)
            .map(|strokes| strokes.as_ref().map(Rc::clone))
    }) {
        return cached;
    }
    let parsed = cx
        .asset_source()
        .load(&glyph_path(name))
        .ok()
        .flatten()
        .and_then(|bytes| parse(&bytes));
    STROKES.with(|cache| {
        cache
            .borrow_mut()
            .insert(name, parsed.as_ref().map(Rc::clone))
    });
    parsed
}

/// Every subpath of a stroked SVG as an evenly resampled stroke; `None` for any fill.
fn parse(svg: &[u8]) -> Option<Rc<[Stroke]>> {
    let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).ok()?;
    let mut polylines = Vec::new();
    collect(tree.root(), &mut polylines)?;
    let strokes: Rc<[Stroke]> = polylines
        .into_iter()
        .filter_map(|(line, closed)| {
            resample(&line).map(|points| Stroke {
                closed,
                ..stroke(points)
            })
        })
        .collect();
    (!strokes.is_empty()).then_some(strokes)
}

fn collect(group: &usvg::Group, out: &mut Vec<(Vec<[f32; 2]>, bool)>) -> Option<()> {
    use usvg::tiny_skia_path::PathSegment;
    for node in group.children() {
        match node {
            usvg::Node::Group(group) => collect(group, out)?,
            usvg::Node::Path(path) if path.fill().is_none() => {
                let transformed;
                let data = if path.abs_transform().is_identity() {
                    path.data()
                } else {
                    transformed = path.data().clone().transform(path.abs_transform())?; // rules-allow: transforming consumes the path the tree keeps
                    &transformed
                };
                let mut line: Vec<[f32; 2]> = Vec::new();
                let mut push = |line: &mut Vec<[f32; 2]>, closed| {
                    if line.len() > 1 {
                        out.push((std::mem::take(line), closed));
                    }
                    line.clear();
                };
                for segment in data.segments() {
                    let last = line.last().copied().unwrap_or_default();
                    match segment {
                        PathSegment::MoveTo(to) => {
                            push(&mut line, false);
                            line.push([to.x, to.y]);
                        }
                        PathSegment::LineTo(to) => line.push([to.x, to.y]),
                        PathSegment::QuadTo(c, to) => line.extend((1..=8).map(|k| {
                            let t = k.to_f32() / 8.;
                            let u = 1. - t;
                            [
                                u * u * last[0] + 2. * u * t * c.x + t * t * to.x,
                                u * u * last[1] + 2. * u * t * c.y + t * t * to.y,
                            ]
                        })),
                        PathSegment::CubicTo(c1, c2, to) => line.extend((1..=12).map(|k| {
                            let t = k.to_f32() / 12.;
                            let u = 1. - t;
                            let (a, b, c, d) =
                                (u * u * u, 3. * u * u * t, 3. * u * t * t, t * t * t);
                            [
                                a * last[0] + b * c1.x + c * c2.x + d * to.x,
                                a * last[1] + b * c1.y + c * c2.y + d * to.y,
                            ]
                        })),
                        PathSegment::Close => {
                            if let Some(&first) = line.first() {
                                line.push(first);
                            }
                            push(&mut line, true);
                        }
                    }
                }
                push(&mut line, false);
            }
            usvg::Node::Path(_) => return None,
            _ => {}
        }
    }
    Some(())
}

/// `line` as `POINTS` points spaced evenly along its length.
fn resample(line: &[[f32; 2]]) -> Option<Points> {
    let segments: Vec<([f32; 2], [f32; 2], f32)> = line
        .windows(2)
        .filter_map(|pair| match *pair {
            [a, b] => Some((a, b, distance(a, b))),
            _ => None,
        })
        .collect();
    let total: f32 = segments.iter().map(|&(.., length)| length).sum();
    if total < 0.01 {
        return None;
    }
    let mut points = [[0.; 2]; POINTS];
    let (mut walked, mut index) = (0., 0);
    for (k, slot) in points.iter_mut().enumerate() {
        let target = total * k.to_f32() / (POINTS - 1).to_f32();
        while let Some(&(_, _, length)) = segments.get(index)
            && index + 1 < segments.len()
            && walked + length < target
        {
            walked += length;
            index += 1;
        }
        let &(a, b, length) = segments.get(index)?;
        *slot = mix(
            a,
            b,
            ((target - walked) / length.max(f32::EPSILON)).clamp(0., 1.),
        );
    }
    Some(points)
}

/// A resampled stroke with its length and centre; closed when its ends meet.
fn stroke(points: Points) -> Stroke {
    let length = points
        .windows(2)
        .map(|pair| match *pair {
            [a, b] => distance(a, b),
            _ => 0.,
        })
        .sum();
    let n = POINTS.to_f32();
    let centre = points
        .iter()
        .fold([0., 0.], |sum, p| [sum[0] + p[0] / n, sum[1] + p[1] / n]);
    let [first, last] = [points[0], points[POINTS - 1]];
    Stroke {
        points,
        length,
        closed: distance(first, last) < 0.3,
        centre,
    }
}

fn travel(a: &Points, b: &Points) -> f32 {
    a.iter().zip(b).map(|(p, q)| distance(*p, *q)).sum()
}

/// `to` started wherever it travels least from `from`: either direction, and for two
/// closed strokes from any point around the ring.
fn aligned(from: &Stroke, to: &Stroke) -> Points {
    let mut best = to.points;
    let mut score = travel(&from.points, &best);
    if score == 0. {
        return best;
    }
    let ring = from.closed && to.closed;
    for reverse in [false, true] {
        let mut candidate = to.points;
        if reverse {
            candidate.reverse();
        }
        for offset in 0..if ring { POINTS - 1 } else { 1 } {
            if offset > 0 {
                candidate[..POINTS - 1].rotate_left(1);
                candidate[POINTS - 1] = candidate[0];
            } else if !reverse {
                continue;
            }
            let cost = travel(&from.points, &candidate);
            if cost < score {
                best = candidate;
                score = cost;
            }
            if score == 0. {
                return best;
            }
        }
    }
    best
}

fn dot(centre: [f32; 2]) -> Points {
    [centre; POINTS]
}

fn distance(a: [f32; 2], b: [f32; 2]) -> f32 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

/// Longest strokes choose first, each taking the nearest stroke left on the other side;
/// a stroke left without a partner collapses to, or grows from, the nearest centre.
fn pair(from: &[Stroke], to: &[Stroke]) -> Pairs {
    let mut from: Vec<_> = from.iter().collect();
    from.sort_by(|a, b| b.length.total_cmp(&a.length));
    let mut left: Vec<_> = to.iter().collect();
    let mut pairs = Vec::with_capacity(from.len().max(to.len()));
    for stroke in &from {
        let score = |s: &Stroke| distance(stroke.centre, s.centre) - 0.02 * s.length;
        let nearest = left
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| score(a).total_cmp(&score(b)))
            .map(|(index, _)| index);
        match nearest {
            Some(index) => {
                let partner = left.swap_remove(index);
                pairs.push((stroke.points, aligned(stroke, partner)));
            }
            None => pairs.push((stroke.points, dot(stroke.centre))),
        }
    }
    for stroke in left {
        let origin = from
            .iter()
            .map(|s| s.centre)
            .min_by(|a, b| distance(*a, stroke.centre).total_cmp(&distance(*b, stroke.centre)))
            .unwrap_or(stroke.centre);
        pairs.push((dot(origin), stroke.points));
    }
    pairs.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHECK: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor"><path d="M22 12C22 6.47715 17.5228 2 12 2C6.47715 2 2 6.47715 2 12C2 17.5228 6.47715 22 12 22C17.5228 22 22 17.5228 22 12Z" stroke-width="2"/><path d="M8 12.75C8 12.75 9.6 13.6625 10.4 15C10.4 15 12.8 9.75 16 8" stroke-width="2"/></svg>"#;
    const CROSS: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor"><path d="M22 12C22 6.47715 17.5228 2 12 2C6.47715 2 2 6.47715 2 12C2 17.5228 6.47715 22 12 22C17.5228 22 22 17.5228 22 12Z" stroke-width="2"/><path d="M14.9994 15L9 9M9.00064 15L15 9" stroke-width="2"/></svg>"#;

    #[test]
    fn a_shared_ring_stays_put_and_extra_strokes_grow_from_a_point() {
        let (check, cross) = (parse(CHECK).expect("check"), parse(CROSS).expect("cross"));
        assert_eq!((check.len(), cross.len()), (2, 3));
        assert_eq!(
            check.iter().map(|s| s.closed).collect::<Vec<_>>(),
            [true, false]
        );
        let pairs = pair(&check, &cross);
        let [ring, _, grown] = &*pairs else {
            panic!("three strokes travel");
        };
        assert!(
            travel(&ring.0, &ring.1) < 1.,
            "the circle both glyphs share should not move"
        );
        assert!(
            grown.0.iter().all(|p| *p == grown.0[0]),
            "the extra stroke starts as a point"
        );
        assert!(
            parse(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M2 2H22V22Z" fill="currentColor"/></svg>"#)
                .is_none(),
            "a filled glyph swaps rather than morphs"
        );
    }
}
