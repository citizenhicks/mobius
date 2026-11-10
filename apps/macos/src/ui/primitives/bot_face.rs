//! SwiftUI's BotFace: a tinted silhouette whose eyes carry the Bot's mood.
//!
//! `BotFace` is the static face for lists and rows. `lively_face` returns a
//! `BotFaceView` entity for hero and header faces: it animates only while it is
//! rendered, at no more than 30 frames per second, and stays idle between beats,
//! so a moving face never forces the transcript or catalog regions to re-render.
//! Faces of at least `DETAIL` size also think with ticking cogs for eyes and sleep
//! with drifting z's, on a slower ambient clock that runs only in those moods.
//!
//! Every face is lit by two fixed SVG layers over its silhouette, a light from the
//! top left and a shaded rim, so depth costs no extra frames. Faces of `DETAIL`
//! size also get brows and blush that follow the mood, glints in round eyes, and a
//! ground shadow that shrinks as the head lifts.

use super::Shape;
use super::controls::palette;
use super::motion;
use crate::numeric::{Saturating, ToFloat};
use gpui::prelude::*;
use gpui::{
    App, Div, Entity, FontWeight, Hsla, MouseButton, Pixels, Point, SharedString, Subscription,
    Task, Transformation, Window, black, div, point, px, radians, relative, rgba, svg, white,
};
use gpui_component::menu::PopupMenuItem;
use mobius_gateway::wire::BotShape;
use std::hash::{BuildHasher, Hasher};
use std::time::{Duration, Instant};

/// Faces this large turn cogs and drift z's; smaller ones keep glyph eyes.
const DETAIL: Pixels = px(40.);
/// The ambient clock for cogs and z's, slower than pose frames.
const AMBIENT_FRAME: Duration = Duration::from_millis(50);
/// Seconds between cog ticks; each tick turns one tooth on this spring.
const COG_TICK: f32 = 0.6;
const COG_SPRING: (f32, f32) = (0.3, 0.3);
/// Seconds a z drifts, and between one z and the next.
const Z_LIFE: f32 = 2.4;
const Z_GAP: f32 = 0.8;
const EYE_INK: u32 = 0x00_00_00_d1;
const BROW_INK: u32 = 0x00_00_00_c7;
const GLINT: u32 = 0xff_ff_ff_e6;
const BLUSH: u32 = 0xff_6e_8c_ff;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub(crate) enum BotMood {
    #[default]
    Idle,
    Watching,
    Listening,
    Thinking,
    Happy,
    Dizzy,
    Wink,
    Sleepy,
}

impl BotMood {
    fn eyes(self) -> &'static str {
        match self {
            Self::Idle | Self::Watching => "o o",
            Self::Listening => "O O",
            Self::Thinking => "- -",
            Self::Happy => "^ ^",
            Self::Dizzy => "+ +",
            Self::Wink => "o -",
            Self::Sleepy => "u u",
        }
    }

    /// Seconds between pose changes (BotFace.swift `beat`).
    fn beat(self) -> (f32, f32) {
        match self {
            Self::Dizzy => (0.18, 0.24),
            Self::Listening | Self::Happy | Self::Wink => (0.45, 0.6),
            Self::Sleepy => (2.5, 3.5),
            Self::Idle | Self::Watching | Self::Thinking => (1.2, 2.6),
        }
    }

    fn blinks(self) -> bool {
        matches!(self, Self::Idle | Self::Watching)
    }

    /// BotFace.swift `brows`: the same table on both platforms.
    fn brows(self) -> Brows {
        let (lift, slope, skew, blush) = match self {
            Self::Idle => (0., 0., 0., 0.22),
            Self::Watching => (-0.08, -0.05, 0., 0.18),
            Self::Listening => (-0.2, -0.1, 0., 0.2),
            Self::Thinking => (0.05, 0.3, 0., 0.12),
            Self::Happy => (-0.15, -0.15, 0., 0.55),
            Self::Dizzy => (-0.05, 0.1, -0.18, 0.3),
            Self::Wink => (-0.05, 0., -0.12, 0.5),
            Self::Sleepy => (0.1, -0.3, 0., 0.25),
        };
        Brows {
            lift,
            slope,
            skew,
            blush,
        }
    }
}

/// Brows and blush for a mood, in eye-glyph sizes: `lift` moves both brows down,
/// `slope` drops their inner ends, `skew` raises the left brow over the right, and
/// `blush` is the cheeks' opacity.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Brows {
    lift: f32,
    slope: f32,
    skew: f32,
    blush: f32,
}

impl Brows {
    fn lerp(self, to: Self, t: f32) -> Self {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        Self {
            lift: mix(self.lift, to.lift),
            slope: mix(self.slope, to.slope),
            skew: mix(self.skew, to.skew),
            blush: mix(self.blush, to.blush),
        }
    }
}

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

/// Offsets are fractions of the face size so every size moves alike.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Pose {
    head: Point<f32>,
    gaze: Point<f32>,
    brows: Brows,
}

impl Pose {
    fn resting(mood: BotMood) -> Self {
        let gaze = match mood {
            BotMood::Watching => 0.16,
            BotMood::Thinking => -0.16,
            BotMood::Sleepy => 0.12,
            BotMood::Idle
            | BotMood::Listening
            | BotMood::Happy
            | BotMood::Dizzy
            | BotMood::Wink => 0.,
        };
        Self {
            head: point(0., if mood == BotMood::Sleepy { 0.04 } else { 0. }),
            gaze: point(0., gaze),
            brows: mood.brows(),
        }
    }

    /// BotFace.swift `Pose.random(for:)`, without the head tilt GPUI text cannot rotate.
    fn random(mood: BotMood) -> Self {
        let mut pose = Self {
            head: point(between(-0.04, 0.04), between(-0.03, 0.03)),
            gaze: point(between(-0.2, 0.2), between(-0.08, 0.08)),
            brows: mood.brows(),
        };
        match mood {
            BotMood::Idle => {}
            BotMood::Watching => pose.gaze.y = 0.16,
            BotMood::Thinking => pose.gaze.y = -0.16,
            BotMood::Listening => {
                pose.gaze = point(0., 0.);
                pose.head.y = if between(0., 1.) < 0.5 { -0.04 } else { 0.04 };
            }
            BotMood::Happy => {
                pose.head.y = between(-0.1, -0.06);
                pose.gaze = point(0., 0.);
            }
            BotMood::Dizzy => pose.head.x = between(-0.06, 0.06),
            BotMood::Wink => {
                pose.head = point(0., 0.);
                pose.gaze = point(0., 0.);
            }
            BotMood::Sleepy => pose = Self::resting(mood),
        }
        pose
    }

    fn lerp(self, to: Self, t: f32) -> Self {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        Self {
            head: point(mix(self.head.x, to.head.x), mix(self.head.y, to.head.y)),
            gaze: point(mix(self.gaze.x, to.gaze.x), mix(self.gaze.y, to.gaze.y)),
            brows: self.brows.lerp(to.brows, t),
        }
    }
}

fn between(low: f32, high: f32) -> f32 {
    // ponytail: std's randomly keyed hasher is enough jitter for a face.
    let bits = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    low + (high - low) * (bits >> 40).to_f32() / (1u64 << 24).to_f32()
}

/// SwiftUI's `.spring(duration:bounce:)` step response at `t` seconds: damping ratio
/// `1 - bounce` at a natural period of `duration`, so it overshoots and settles.
fn spring(t: f32, duration: f32, bounce: f32) -> f32 {
    let omega = std::f32::consts::TAU / duration;
    let zeta = 1. - bounce;
    let damped = omega * (1. - zeta * zeta).sqrt();
    1. - (-zeta * omega * t).exp()
        * ((damped * t).cos() + zeta * omega / damped * (damped * t).sin())
}

/// A pose change: `.spring(duration: 0.45, bounce: 0.4)`.
const POSE_SPRING: (f32, f32) = (0.45, 0.4);
/// A mood change: `.spring(duration: 0.35, bounce: 0.5)`.
const MOOD_SPRING: (f32, f32) = (0.35, 0.5);

/// A thinking cog's turn after `clock` seconds: one tooth (an eighth of a turn) per
/// tick, snapping with a small mechanical overshoot.
fn cog_turn(clock: f32) -> f32 {
    let ticks = (clock / COG_TICK).floor();
    let (duration, bounce) = COG_SPRING;
    (ticks + spring(clock - ticks * COG_TICK, duration, bounce)) * std::f32::consts::FRAC_PI_4
}

/// What the eyes show: glyphs, or two cogs turned by an angle in radians.
#[derive(Clone, Copy)]
enum Eyes {
    Glyphs(&'static str),
    Cogs(f32),
}

/// Centres `element` on its parent's centre moved by `offset`, sized `extent`, both
/// in units of `unit`.
fn place<E: Styled>(element: E, unit: Pixels, extent: (f32, f32), offset: (f32, f32)) -> E {
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
/// `side` is -1 for the left eye and 1 for the right; offsets are in glyph sizes.
fn eye(glyph: &'static str, side: f32, em: Pixels, brows: Brows, detail: bool) -> Div {
    let glint = match glyph {
        "o" => Some((-0.07, 0.03, 0.05)),
        "O" => Some((-0.09, -0.09, 0.065)),
        _ => None,
    };
    div()
        .relative()
        .when(detail, |eye| {
            eye.child(place(
                svg()
                    .path("icons/face.blush.svg")
                    .text_color(rgba(BLUSH).opacity(brows.blush)),
                em,
                (0.64, 0.4),
                (side * 0.22, 0.6),
            ))
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
            .child(place(
                svg()
                    .path("icons/face.brow.svg")
                    .text_color(rgba(BROW_INK))
                    .with_transformation(Transformation::rotate(radians(
                        -side * brows.slope.atan(),
                    ))),
                em,
                (0.68, 0.26),
                (0., -0.56 + brows.lift - side * brows.skew),
            ))
        })
}

fn face(tint: Hsla, shape: BotShape, size: Pixels, eyes: Eyes, pose: Pose) -> Div {
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
                .font_family(super::FontSet::current().lettering())
                .font_weight(FontWeight::BLACK)
                .text_size(size * eye_size)
                .text_color(rgba(EYE_INK))
                .child(eye(left, -1., em, pose.brows, detail))
                .child(" ")
                .child(eye(right, 1., em, pose.brows, detail))
        }
        Eyes::Cogs(turn) => {
            let cog = |turn: f32| {
                svg()
                    .path("icons/face.cog.svg")
                    .size(size * 0.2 * reach)
                    .text_color(rgba(EYE_INK))
                    .with_transformation(Transformation::rotate(radians(turn)))
            };
            div()
                .flex()
                .gap(size * 0.04 * reach)
                .child(cog(turn))
                .child(cog(-turn))
        }
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
    // The ground shadow stays on the floor: it follows the head sideways only, and
    // narrows and fades as the head lifts.
    let lift = (-pose.head.y).max(0.) * 6.;
    let shadow_width = 0.72 * (1. - lift * 0.5);
    div()
        .size(size)
        .flex_shrink_0()
        .relative()
        .left(size * pose.head.x)
        .top(size * pose.head.y)
        .flex()
        .items_center()
        .justify_center()
        .when(detail, |face| {
            face.child(
                svg()
                    .path("icons/face.shadow.svg")
                    .absolute()
                    .left(size * (0.5 - shadow_width / 2.))
                    .top(size * (0.995 - pose.head.y))
                    .w(size * shadow_width)
                    .h(size * 0.13)
                    .text_color(black().opacity(1. - lift * 0.6)),
            )
        })
        .child(layer(silhouette, tint))
        .child(layer(light, white()))
        .child(layer(shade, black()))
        .child(
            div()
                .relative()
                .left(size * pose.gaze.x * reach)
                .top(size * (eye_row - 0.5 + pose.gaze.y * reach))
                .child(eyes),
        )
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
        super::controls::leading_item(label, move |_| {
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
            Pose::resting(BotMood::Idle),
        )
    }
}

/// A face that moves like SwiftUI's; see the module note on cost.
pub(crate) struct BotFaceView {
    tint: Hsla,
    shape: BotShape,
    size: Pixels,
    mood: BotMood,
    pose: Pose,
    blinking: bool,
    winking: bool,
    beat: Option<Task<()>>,
    motion: Option<Task<()>>,
    /// Redraws cogs and z's while the face thinks or sleeps, counting from `since`.
    ambient: Option<Task<()>>,
    since: Instant,
    _activation: Subscription,
}

/// Returns the lively face for `key`, updating its tint, shape and mood.
pub(crate) fn lively_face(
    key: impl Into<SharedString>,
    tint: Hsla,
    shape: BotShape,
    mood: BotMood,
    size: Pixels,
    window: &mut Window,
    cx: &mut App,
) -> Entity<BotFaceView> {
    let face = window.use_keyed_state(key.into(), cx, |window, cx| BotFaceView {
        tint,
        shape,
        size,
        mood,
        pose: Pose::resting(mood),
        blinking: false,
        winking: false,
        beat: None,
        motion: None,
        ambient: None,
        since: Instant::now(),
        _activation: cx.observe_window_activation(window, |_, _, cx| cx.notify()),
    });
    face.update(cx, |face, cx| face.set(tint, shape, mood, size, cx));
    face
}

impl BotFaceView {
    fn set(
        &mut self,
        tint: Hsla,
        shape: BotShape,
        mood: BotMood,
        size: Pixels,
        cx: &mut Context<Self>,
    ) {
        if (self.tint, self.shape, self.mood, self.size) != (tint, shape, mood, size) {
            let changed_mood = self.mood != mood;
            (self.tint, self.shape, self.mood, self.size) = (tint, shape, mood, size);
            if changed_mood {
                self.beat = None;
                self.glide(Pose::resting(mood), MOOD_SPRING, cx);
            }
            cx.notify();
        }
    }

    fn shown_mood(&self) -> BotMood {
        if self.winking {
            BotMood::Wink
        } else {
            self.mood
        }
    }

    /// Springs the pose toward `to` in frames of at most 30 per second, until settled.
    fn glide(&mut self, to: Pose, (duration, bounce): (f32, f32), cx: &Context<Self>) {
        if cx.reduce_motion() {
            self.motion = None;
            self.pose = to;
            return;
        }
        let from = self.pose;
        let frame = motion::FRAME.as_secs_f32();
        let frames = (duration * 1.3 / frame).ceil().saturating_u32();
        self.motion = Some(cx.spawn(async move |this, cx| {
            for index in 1..=frames {
                cx.background_executor().timer(motion::FRAME).await;
                let t = if index == frames {
                    1.
                } else {
                    spring(index.to_f32() * frame, duration, bounce)
                };
                let alive = this.update(cx, |face, cx| {
                    face.pose = from.lerp(to, t);
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        }));
    }

    /// One beat task per mood, as BotFace.swift's loop: spring to a new pose, hold for
    /// the mood's beat, and now and then blink before the next pose.
    fn ensure_beat(&mut self, window: &Window, cx: &Context<Self>) {
        if self.beat.is_some() || !motion::is_animating(window, cx) {
            return;
        }
        let mood = self.shown_mood();
        self.beat = Some(cx.spawn(async move |this, cx| {
            loop {
                let moved = this.update(cx, |face, cx| {
                    face.glide(Pose::random(mood), POSE_SPRING, cx);
                });
                if moved.is_err() {
                    return;
                }
                let (low, high) = mood.beat();
                cx.background_executor()
                    .timer(Duration::from_secs_f32(between(low, high)))
                    .await;
                if mood.blinks() && between(0., 4.) < 1. {
                    if let Err(error) = this.update(cx, |face, cx| {
                        face.blinking = true;
                        cx.notify();
                    }) {
                        tracing::debug!(%error, "bot face released before its blink");
                        return;
                    }
                    cx.background_executor().timer(motion::BLINK).await;
                    if let Err(error) = this.update(cx, |face, cx| {
                        face.blinking = false;
                        cx.notify();
                    }) {
                        tracing::debug!(%error, "bot face released during its blink");
                        return;
                    }
                }
            }
        }));
    }

    /// Thinking and sleepy faces with room for detail keep an ambient clock running.
    fn ensure_ambient(&mut self, animating: bool, cx: &Context<Self>) -> Option<f32> {
        let wanted = animating
            && self.size >= DETAIL
            && matches!(self.shown_mood(), BotMood::Thinking | BotMood::Sleepy);
        if !wanted {
            self.ambient = None;
            return None;
        }
        if self.ambient.is_none() {
            self.since = Instant::now();
            self.ambient = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(AMBIENT_FRAME).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        return;
                    }
                }
            }));
        }
        Some(self.since.elapsed().as_secs_f32())
    }

    /// Three z's rising from the crown, each growing and fading over its life.
    fn drifting_zs(&self, clock: f32, cx: &App) -> impl Iterator<Item = Div> + use<> {
        let (size, ink) = (self.size, palette(cx).muted);
        (0..3).map(move |index| {
            let life = (clock + index.to_f32() * Z_GAP) % Z_LIFE / Z_LIFE;
            div()
                .absolute()
                .left(size * (0.78 + 0.22 * life))
                .top(size * (0.18 - 0.4 * life))
                .font_family(super::FontSet::current().lettering())
                .font_weight(FontWeight::BLACK)
                .text_size(size * 0.2 * (0.6 + 0.5 * life))
                .text_color(ink)
                .opacity((life * std::f32::consts::PI).sin())
                .child("z")
        })
    }
}

impl Render for BotFaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let animating = motion::is_animating(window, cx);
        if animating {
            self.ensure_beat(window, cx);
        } else {
            self.beat = None;
            self.motion = None;
            self.blinking = false;
            self.pose = Pose::resting(self.shown_mood());
        }
        let clock = self.ensure_ambient(animating, cx);
        let mood = self.shown_mood();
        let eyes = match mood {
            _ if self.blinking => Eyes::Glyphs("- -"),
            BotMood::Thinking if self.size >= DETAIL => Eyes::Cogs(clock.map_or(0., cog_turn)),
            mood => Eyes::Glyphs(mood.eyes()),
        };
        let zs = clock
            .filter(|_| mood == BotMood::Sleepy)
            .map(|clock| self.drifting_zs(clock, cx));
        face(self.tint, self.shape, self.size, eyes, self.pose)
            .children(zs.into_iter().flatten())
            .id("bot-face")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|face, _, _, cx| {
                    face.winking = true;
                    face.beat = None;
                    cx.notify();
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(motion::WINK).await;
                        if let Err(error) = this.update(cx, |face, cx| {
                            face.winking = false;
                            face.beat = None;
                            cx.notify();
                        }) {
                            tracing::debug!(%error, "bot face released before its wink ended");
                        }
                    })
                    .detach();
                }),
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
