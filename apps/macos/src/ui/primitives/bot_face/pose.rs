//! Poses and how a face moves between them: SwiftUI's springs, glides, and the course a
//! drawn face follows, still or live.

use super::*;

/// Offsets are fractions of the face size so every size moves alike.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Pose {
    pub(super) head: Point<f32>,
    pub(super) gaze: Point<f32>,
    pub(super) brows: Brows,
}

impl Pose {
    pub(super) fn resting(mood: BotMood) -> Self {
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
    pub(super) fn random(mood: BotMood) -> Self {
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

    pub(super) fn lerp(self, to: Self, t: f32) -> Self {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        Self {
            head: point(mix(self.head.x, to.head.x), mix(self.head.y, to.head.y)),
            gaze: point(mix(self.gaze.x, to.gaze.x), mix(self.gaze.y, to.gaze.y)),
            brows: self.brows.lerp(to.brows, t),
        }
    }
}

pub(super) fn between(low: f32, high: f32) -> f32 {
    // ponytail: std's randomly keyed hasher is enough jitter for a face.
    let bits = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    low + (high - low) * (bits >> 40).to_f32() / (1u64 << 24).to_f32()
}

/// SwiftUI's `.spring(duration:bounce:)` step response at `t` seconds: damping ratio
/// `1 - bounce` at a natural period of `duration`, so it overshoots and settles.
pub(super) fn spring(t: f32, duration: f32, bounce: f32) -> f32 {
    let omega = std::f32::consts::TAU / duration;
    let zeta = 1. - bounce;
    let damped = omega * (1. - zeta * zeta).sqrt();
    1. - (-zeta * omega * t).exp()
        * ((damped * t).cos() + zeta * omega / damped * (damped * t).sin())
}

/// A pose change: `.spring(duration: 0.45, bounce: 0.4)`.
pub(super) const POSE_SPRING: (f32, f32) = (0.45, 0.4);
/// A mood change: `.spring(duration: 0.35, bounce: 0.5)`.
pub(super) const MOOD_SPRING: (f32, f32) = (0.35, 0.5);

/// A thinking cog's turn after `clock` seconds: one tooth (an eighth of a turn) per
/// tick, snapping with a small mechanical overshoot.
pub(super) fn cog_turn(clock: f32) -> f32 {
    let ticks = (clock / COG_TICK).floor();
    let (duration, bounce) = COG_SPRING;
    (ticks + spring(clock - ticks * COG_TICK, duration, bounce)) * std::f32::consts::FRAC_PI_4
}

/// A pose change in progress: `from` springs to `to` from `start`, as SwiftUI's
/// `.spring(duration:bounce:)`, and rests at `to` from 1.3 durations on.
#[derive(Clone, Copy, Debug)]
pub(super) struct Glide {
    pub(super) from: Pose,
    pub(super) to: Pose,
    pub(super) start: Instant,
    pub(super) spring: (f32, f32),
}

impl Glide {
    pub(super) fn still(pose: Pose) -> Self {
        Self {
            from: pose,
            to: pose,
            start: Instant::now(),
            spring: POSE_SPRING,
        }
    }

    /// The pose shown at `at`.
    pub(super) fn at(&self, at: Instant) -> Pose {
        let (duration, bounce) = self.spring;
        let t = motion::sampled(at)
            .saturating_duration_since(self.start)
            .as_secs_f32();
        if t >= duration * 1.3 {
            self.to
        } else {
            self.from.lerp(self.to, spring(t, duration, bounce))
        }
    }

    pub(super) fn end(&self) -> Instant {
        self.start + Duration::from_secs_f32(self.spring.0 * 1.3)
    }
}

/// Where a drawn face's pose comes from: one still pose, or a glide its beat can turn
/// after the face is painted (see [`Motion::until_cell`]).
pub(super) enum Course {
    Still(Pose),
    Live { live: Live, layout: Pose },
}

/// A glide shared between a face's beat and the motions it painted.
pub(super) struct Live {
    pub(super) glide: Rc<Cell<Glide>>,
    pub(super) end: Rc<Cell<Instant>>,
    /// Glides present at this rate.
    pub(super) fps: f32,
}

impl Live {
    pub(super) fn new(glide: Glide) -> Self {
        Self {
            glide: Rc::new(Cell::new(glide)),
            end: Rc::new(Cell::new(glide.end())),
            fps: motion::SMOOTH_FPS,
        }
    }

    /// Small faces glide at the ambient rate they always have; larger ones smoothly.
    pub(super) fn for_size(&self, size: Pixels) -> Self {
        Self {
            fps: if size >= DETAIL {
                motion::SMOOTH_FPS
            } else {
                motion::AMBIENT_FPS
            },
            glide: Rc::clone(&self.glide),
            end: Rc::clone(&self.end),
        }
    }

    pub(super) fn set(&self, glide: Glide) {
        self.glide.set(glide);
        self.end.set(glide.end());
    }
}

impl Course {
    pub(super) fn live(live: Live) -> Self {
        let layout = live.glide.get().at(motion::now());
        Self::Live { live, layout }
    }

    /// The pose the face's parts are laid out at.
    pub(super) fn pose(&self) -> Pose {
        match self {
            Self::Still(pose) => *pose,
            Self::Live { layout, .. } => *layout,
        }
    }

    /// `element` painted where `sample` puts it at each instant, given the pose then and
    /// the pose it was laid out at; shown as laid out while the face is still.
    pub(super) fn moving<E: IntoElement + 'static>(
        &self,
        element: E,
        sample: impl Fn(Pose, Pose) -> MotionFrame + 'static,
    ) -> AnyElement {
        let Self::Live { live, layout } = self else {
            return element.into_any_element();
        };
        let (layout, glide) = (*layout, Rc::clone(&live.glide));
        element
            .with_motion(
                Motion::new(move |at| sample(glide.get().at(motion::sampled(at)), layout))
                    .until_cell(Rc::clone(&live.end))
                    .max_fps(live.fps),
            )
            .into_any_element()
    }
}

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
    pub(super) fn eyes(self) -> &'static str {
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
    pub(super) fn beat(self) -> (f32, f32) {
        match self {
            Self::Dizzy => (0.18, 0.24),
            Self::Listening | Self::Happy | Self::Wink => (0.45, 0.6),
            Self::Sleepy => (2.5, 3.5),
            Self::Idle | Self::Watching | Self::Thinking => (1.2, 2.6),
        }
    }

    pub(super) fn blinks(self) -> bool {
        matches!(self, Self::Idle | Self::Watching)
    }

    /// BotFace.swift `brows`: the same table on both platforms.
    pub(super) fn brows(self) -> Brows {
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
pub(super) struct Brows {
    pub(super) lift: f32,
    pub(super) slope: f32,
    pub(super) skew: f32,
    pub(super) blush: f32,
}

impl Brows {
    pub(super) fn lerp(self, to: Self, t: f32) -> Self {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        Self {
            lift: mix(self.lift, to.lift),
            slope: mix(self.slope, to.slope),
            skew: mix(self.skew, to.skew),
            blush: mix(self.blush, to.blush),
        }
    }
}
