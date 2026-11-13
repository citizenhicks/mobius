//! Building a face's elements at a pose, with the GPU motions that carry each part along
//! its course: head, gaze, brows, blush, ground shadow, cogs and z's.

use super::*;

/// What the eyes show: glyphs, or two cogs turning from an instant (still without one).
#[derive(Clone, Copy)]
pub(super) enum Eyes {
    Glyphs(&'static str),
    Cogs(Option<Instant>),
}

/// Centres `element` on its parent's centre moved by `offset`, sized `extent`, both
/// in units of `unit`.
pub(super) fn place<E: Styled>(
    element: E,
    unit: Pixels,
    extent: (f32, f32),
    offset: (f32, f32),
) -> E {
    let ((width, height), (x, y)) = (extent, offset);
    element
        .absolute()
        .left(relative(0.5))
        .top(relative(0.5))
        .w(unit * width)
        .h(unit * height)
        .ml(unit * (x - width / 2.))
        .mt(unit * (y - height / 2.))
}

/// One eye glyph, with its brow, cheek and glint when the face has room for them.
/// `side` is -1 for the left eye and 1 for the right; offsets are in glyph sizes. Brows
/// and blush follow the face's course.
pub(super) fn eye(
    glyph: &'static str,
    side: f32,
    em: Pixels,
    course: &Course,
    detail: bool,
) -> Div {
    let glint = match glyph {
        "o" => Some((-0.07, 0.03, 0.05)),
        "O" => Some((-0.09, -0.09, 0.065)),
        _ => None,
    };
    let brows = course.pose().brows;
    div()
        .relative()
        .when(detail, |eye| {
            let blush = place(
                svg().path("icons/face.blush.svg"),
                em,
                (0.64, 0.4),
                (side * 0.22, 0.6),
            );
            // Gliding, the blush is drawn opaque and the motion sets its opacity.
            eye.child(if matches!(course, Course::Still(_)) {
                blush
                    .text_color(rgba(BLUSH).opacity(brows.blush))
                    .into_any_element()
            } else {
                course.moving(blush.text_color(rgba(BLUSH)), |pose, _| MotionFrame {
                    opacity: pose.brows.blush,
                    ..MotionFrame::default()
                })
            })
        })
        .child(glyph)
        .when(detail, |eye| {
            eye.children(glint.map(|(x, y, radius)| {
                place(
                    div().pill_corners().bg(rgba(GLINT)),
                    em,
                    (2. * radius, 2. * radius),
                    (x, y),
                )
            }))
            .child(
                course.moving(
                    place(
                        svg()
                            .path("icons/face.brow.svg")
                            .text_color(rgba(BROW_INK))
                            .with_transformation(Transformation::rotate(radians(
                                -side * brows.slope.atan(),
                            ))),
                        em,
                        (0.68, 0.26),
                        (0., -0.56 + brows.lift - side * brows.skew),
                    ),
                    move |pose, to| {
                        let (now, to) = (pose.brows, to.brows);
                        MotionFrame {
                            offset: point(
                                px(0.),
                                em * ((now.lift - to.lift) - side * (now.skew - to.skew)),
                            ),
                            rotation: radians(-side * (now.slope.atan() - to.slope.atan())),
                            ..MotionFrame::default()
                        }
                    },
                ),
            )
        })
}

/// Thinking cogs, turned tooth by tooth from `since`.
pub(super) fn cogs(size: Pixels, reach: f32, since: Option<Instant>) -> Div {
    let cog = |direction: f32| {
        let cog = svg()
            .path("icons/face.cog.svg")
            .size(size * 0.2 * reach)
            .text_color(rgba(EYE_INK));
        match since {
            Some(since) => cog
                .with_motion(
                    Motion::new(move |at| MotionFrame {
                        rotation: radians(direction * cog_turn(clock(since, at))),
                        ..MotionFrame::default()
                    })
                    .max_fps(motion::AMBIENT_FPS),
                )
                .into_any_element(),
            None => cog.into_any_element(),
        }
    };
    div()
        .flex()
        .gap(size * 0.04 * reach)
        .child(cog(1.))
        .child(cog(-1.))
}

/// Seconds since `since` at `at`.
pub(super) fn clock(since: Instant, at: Instant) -> f32 {
    motion::sampled(at)
        .saturating_duration_since(since)
        .as_secs_f32()
}

/// How far a pose lifts the head off its ground shadow.
pub(super) fn lift(pose: Pose) -> f32 {
    (-pose.head.y).max(0.) * 6.
}

/// The ground shadow's width at `pose`, as a fraction of the face.
pub(super) fn shadow_width(pose: Pose) -> f32 {
    0.72 * (1. - lift(pose) * 0.5)
}

/// The face laid out at its course's pose now; while it glides, every part moves with it.
pub(super) fn face(tint: Hsla, shape: BotShape, size: Pixels, eyes: Eyes, course: &Course) -> Div {
    let ([silhouette, light, shade], eye_row, eye_size) = outline(shape);
    // Smaller eyes look around less, so every shape keeps its eyes inside it.
    let reach = eye_size / 0.3;
    let detail = size >= DETAIL;
    let eyes = match eyes {
        Eyes::Glyphs(glyphs) => {
            let em = size * eye_size;
            let (left, right) = glyphs.split_once(' ').unwrap_or_default();
            div()
                .flex()
                .font_family(FontSet::current().lettering())
                .font_weight(FontWeight::BLACK)
                .text_size(size * eye_size)
                .text_color(rgba(EYE_INK))
                .child(eye(left, -1., em, course, detail))
                .child(" ")
                .child(eye(right, 1., em, course, detail))
        }
        Eyes::Cogs(since) => cogs(size, reach, since),
    };
    let layer = |path: &'static str, color: Hsla| {
        svg()
            .path(path)
            .absolute()
            .top_0()
            .left_0()
            .size(size)
            .text_color(color)
    };
    let pose = course.pose();
    // The ground shadow stays on the floor: it follows the head sideways only, and
    // narrows and fades as the head lifts.
    let shadow = course.moving(
        svg()
            .path("icons/face.shadow.svg")
            .absolute()
            .left(size * (0.5 - shadow_width(pose) / 2.))
            .top(size * (0.995 - pose.head.y))
            .w(size * shadow_width(pose))
            .h(size * 0.13)
            .text_color(black().opacity(1. - lift(pose) * 0.6)),
        move |now, to| MotionFrame {
            offset: point(px(0.), size * -(now.head.y - to.head.y)),
            scale: gpui::size(shadow_width(now) / shadow_width(to), 1.),
            opacity: (1. - lift(now) * 0.6) / (1. - lift(to) * 0.6),
            ..MotionFrame::default()
        },
    );
    let gaze = course.moving(
        div()
            .relative()
            .left(size * pose.gaze.x * reach)
            .top(size * (eye_row - 0.5 + pose.gaze.y * reach))
            .child(eyes),
        move |now, to| MotionFrame {
            offset: point(
                size * reach * (now.gaze.x - to.gaze.x),
                size * reach * (now.gaze.y - to.gaze.y),
            ),
            ..MotionFrame::default()
        },
    );
    div()
        .size(size)
        .flex_shrink_0()
        .relative()
        .left(size * pose.head.x)
        .top(size * pose.head.y)
        .flex()
        .items_center()
        .justify_center()
        .when(detail, |face| face.child(shadow))
        .child(layer(silhouette, tint))
        .child(layer(light, white()))
        .child(layer(shade, black()))
        .child(gaze)
}

/// `face` with its head following its course, which carries every other part.
pub(super) fn moving_face(
    face: impl IntoElement + 'static,
    size: Pixels,
    course: &Course,
) -> AnyElement {
    course.moving(face, move |now, to| MotionFrame {
        offset: point(
            size * (now.head.x - to.head.x),
            size * (now.head.y - to.head.y),
        ),
        ..MotionFrame::default()
    })
}

/// Two faces for the snapshot harness to compare: one still at the pose a glide shows
/// `elapsed` into its spring, and one laid out at the glide's end and moved there by its
/// motions, sampled at `start + elapsed` (the harness's frame clock).
#[cfg(feature = "snapshot")]
pub(crate) fn glide_parity(
    tint: Hsla,
    size: Pixels,
    start: Instant,
    elapsed: Duration,
) -> [AnyElement; 2] {
    let from = Pose {
        head: point(0.03, -0.04),
        gaze: point(-0.15, 0.05),
        brows: BotMood::Idle.brows(),
    };
    let to = Pose {
        head: point(-0.02, 0.),
        gaze: point(0.18, -0.06),
        brows: BotMood::Happy.brows(),
    };
    let glide = Glide {
        from,
        to,
        start,
        spring: POSE_SPRING,
    };
    let still = Course::Still(glide.at(start + elapsed));
    [
        still,
        Course::Live {
            live: Live::new(glide),
            layout: to,
        },
    ]
    .map(|course| {
        let eyes = Eyes::Glyphs(BotMood::Idle.eyes());
        moving_face(
            face(tint, BotShape::Circle, size, eyes, &course),
            size,
            &course,
        )
    })
}

/// Faces this large turn cogs and drift z's; smaller ones keep glyph eyes.
pub(super) const DETAIL: Pixels = px(40.);
/// Seconds between cog ticks; each tick turns one tooth on this spring.
pub(super) const COG_TICK: f32 = 0.6;
pub(super) const COG_SPRING: (f32, f32) = (0.3, 0.3);
/// Seconds a z drifts, and between one z and the next.
pub(super) const Z_LIFE: f32 = 2.4;
pub(super) const Z_GAP: f32 = 0.8;
const EYE_INK: u32 = 0x00_00_00_d1;
const BROW_INK: u32 = 0x00_00_00_c7;
const GLINT: u32 = 0xff_ff_ff_e6;
const BLUSH: u32 = 0xff_6e_8c_ff;

/// The shape's silhouette, light and shade layers, and where its eyes sit: the eye
/// row's centre and the eye size, both as fractions of the face (BotFace.swift's
/// shape table).
fn outline(shape: BotShape) -> ([&'static str; 3], f32, f32) {
    macro_rules! layers {
        ($name:literal) => {
            [
                concat!("icons/face.", $name, ".svg"),
                concat!("icons/face.", $name, ".light.svg"),
                concat!("icons/face.", $name, ".shade.svg"),
            ]
        };
    }
    match shape {
        BotShape::Circle => (layers!("circle"), 0.5, 0.3),
        BotShape::Squircle => (layers!("squircle"), 0.5, 0.3),
        BotShape::Triangle => (layers!("triangle"), 0.63, 0.25),
        BotShape::Diamond => (layers!("diamond"), 0.5, 0.25),
        BotShape::Hexagon => (layers!("hexagon"), 0.5, 0.28),
        BotShape::Star => (layers!("star"), 0.56, 0.23),
        BotShape::Flower => (layers!("flower"), 0.5, 0.29),
    }
}

/// The shape name shown by the native identity picker.
pub(crate) fn shape_label(shape: BotShape) -> &'static str {
    match shape {
        BotShape::Circle => "Circle",
        BotShape::Squircle => "Squircle",
        BotShape::Triangle => "Triangle",
        BotShape::Diamond => "Diamond",
        BotShape::Hexagon => "Hexagon",
        BotShape::Star => "Star",
        BotShape::Flower => "Flower",
    }
}

/// The static face used in lists, rows and pills.
#[derive(IntoElement)]
pub(crate) struct BotFace {
    tint: Hsla,
    shape: BotShape,
    size: Pixels,
}

impl BotFace {
    pub fn new(tint: Hsla, shape: BotShape, size: Pixels) -> Self {
        Self { tint, shape, size }
    }

    pub fn menu_item(self, label: impl Into<SharedString>) -> PopupMenuItem {
        crate::ui::primitives::controls::leading_item(label, move |_| {
            Self::new(self.tint, self.shape, self.size).into_any_element()
        })
    }
}

impl RenderOnce for BotFace {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let eyes = Eyes::Glyphs(BotMood::Idle.eyes());
        face(
            self.tint,
            self.shape,
            self.size,
            eyes,
            &Course::Still(Pose::resting(BotMood::Idle)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::outline;
    use crate::assets::Assets;
    use gpui::{AssetSource, SvgRenderer};
    use mobius_gateway::wire::BotShape;
    use std::sync::Arc;

    #[test]
    fn every_face_layer_is_embedded_and_renders() {
        let shapes = [
            BotShape::Circle,
            BotShape::Squircle,
            BotShape::Triangle,
            BotShape::Diamond,
            BotShape::Hexagon,
            BotShape::Star,
            BotShape::Flower,
        ];
        let renderer = SvgRenderer::new(Arc::new(Assets));
        let layers = shapes
            .into_iter()
            .flat_map(|shape| outline(shape).0)
            .chain([
                "icons/face.blush.svg",
                "icons/face.brow.svg",
                "icons/face.shadow.svg",
            ]);
        for path in layers {
            let bytes = Assets
                .load(path)
                .unwrap()
                .unwrap_or_else(|| panic!("{path} is missing"));
            renderer
                .render_single_frame(&bytes, 1.)
                .unwrap_or_else(|error| {
                    panic!("{path} cannot render: {error}");
                });
        }
    }
}
