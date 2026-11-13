//! Motion: durations, and the eased open/closed state every sliding panel, card and
//! disclosure uses (lilt). Reduce Motion makes each change instant.

mod appear;
mod counter;
mod height;
mod slide;
pub mod spring;

pub use counter::counter;
pub use height::{hand_off, progress, smooth, visible};
pub use slide::slide;

use gpui::{AnyElement, App, ElementId, IntoElement, Pixels, Window, px};
use lilt::{Animated, Easing};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

// Shared timing tiers; named durations below keep call sites readable by purpose.
/// Glyph swaps, carets and small marks changing state.
pub const QUICK: Duration = Duration::from_millis(180);
/// Layout: rows arriving, panels, toasts, sections opening.
pub const STANDARD: Duration = Duration::from_millis(280);
/// A new transcript row settling in.
pub const REVEAL: Duration = STANDARD;
/// The sidebar and the side pane sliding open or closed.
pub const PANEL: Duration = QUICK;
/// A titlebar control appearing.
pub const APPEAR: Duration = QUICK;
pub const CROSSFADE: Duration = STANDARD;
pub const BLINK: Duration = Duration::from_millis(110);
pub const WINK: Duration = Duration::from_millis(800);
pub const REACTION: Duration = Duration::from_millis(1400);
pub const SHIMMER_PERIOD: Duration = Duration::from_millis(1600);
/// At most 30 frames per second.
pub const FRAME: Duration = Duration::from_millis(33);
/// A display frame, for motion driven from a task rather than the renderer.
pub const FRAME_60: Duration = Duration::from_millis(16);

thread_local! {
    /// The snapshot harness's frame clock, while it records a frame strip.
    static CLOCK: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// The time motion reads: the real clock, or a frame clock the snapshot harness steps
/// exactly one frame per capture, however long each capture takes.
pub fn now() -> Instant {
    CLOCK
        .with(std::cell::Cell::get)
        .unwrap_or_else(Instant::now)
}

/// GPU motion that should read as smooth movement: glides, sweeps, spins and reveals.
pub const SMOOTH_FPS: f32 = 60.;
/// GPU motion in the background of a scene: pulses, ticking cogs and drifting z's.
pub const AMBIENT_FPS: f32 = 30.;

/// The instant a GPU motion shows when presented at `at`: the snapshot harness's frame
/// clock while it records, so captured strips step exactly as rendered motion does.
pub fn sampled(at: Instant) -> Instant {
    CLOCK.with(std::cell::Cell::get).unwrap_or(at)
}

#[cfg(feature = "snapshot")]
pub fn set_clock(at: Option<Instant>) {
    CLOCK.with(|clock| clock.set(at));
}

/// Decorative motion runs only in active windows, except in the snapshot harness.
pub fn is_animating(window: &Window, cx: &App) -> bool {
    !cx.reduce_motion() && (window.is_window_active() || cfg!(feature = "snapshot"))
}

/// `element` fading in over `duration` when it first appears, and again whenever `id`
/// changes; Reduce Motion shows it at once.
pub fn fade_in(
    element: impl IntoElement + 'static,
    id: impl Into<ElementId>,
    duration: Duration,
    cx: &App,
) -> AnyElement {
    rise_in(element, id, duration, px(0.), cx)
}

/// `element` fading in while it rises the last `lift` points into place; Reduce Motion
/// shows it at once. Only its drawing moves: its layout and hit area stay put.
pub fn rise_in(
    element: impl IntoElement + 'static,
    id: impl Into<ElementId>,
    duration: Duration,
    lift: Pixels,
    cx: &App,
) -> AnyElement {
    if cx.reduce_motion() {
        return element.into_any_element();
    }
    appear::appear(element, &id.into(), duration, lift).into_any_element()
}

/// An open or closed state that eases between the two over `PANEL` and, changed
/// mid-way, turns around from wherever it is.
#[derive(Clone, Debug)]
pub struct Reveal(Animated<bool, Instant>);

impl Reveal {
    pub fn new(open: bool) -> Self {
        Self(
            Animated::new(open)
                .duration(PANEL.as_secs_f32() * 1000.)
                .easing(Easing::EaseOutCubic),
        )
    }

    /// Heads toward `open`; `still` (Reduce Motion) arrives at once.
    pub fn set(&mut self, open: bool, still: bool, now: Instant) {
        if still {
            // lilt ignores instantaneous transitions to an unchanged destination.
            *self = Self::new(open);
        } else {
            self.0.transition(open, now);
        }
    }

    /// How much shows at `now`, from 0 to 1.
    pub fn shown(&self, now: Instant) -> f32 {
        self.0.animate_bool(0., 1., now)
    }

    /// `full` scaled by how much shows, while the reveal moves.
    pub fn sliding(&self, full: Pixels, now: Instant) -> Option<Pixels> {
        self.moving(now).then(|| full * self.shown(now))
    }

    pub fn moving(&self, now: Instant) -> bool {
        self.0.in_progress(now)
    }

    /// How much shows at each instant, for a GPU motion to sample.
    pub fn sampler(&self) -> impl Fn(Instant) -> f32 + 'static {
        let reveal = self.0.clone(); // rules-allow: the motion samples its own copy while the view keeps steering this one
        move |at| reveal.animate_bool(0., 1., sampled(at))
    }

    /// When the reveal arrives where it is heading.
    pub fn end(&self, now: Instant) -> Instant {
        let remaining = if self.moving(now) {
            PANEL
        } else {
            Duration::ZERO
        };
        now + remaining
    }
}

/// Reveals keyed by item (folders, sidebar items, chats), kept only while they move.
#[derive(Clone, Debug, Default)]
pub struct Reveals(BTreeMap<String, Reveal>);

impl Reveals {
    /// Heads `key` toward `open` from where it shows now, or from fully `!open`.
    pub fn set(&mut self, key: &str, open: bool, still: bool, now: Instant) {
        if still {
            self.0.remove(key);
            return;
        }
        if let Some(reveal) = self.0.get_mut(key) {
            reveal.set(open, false, now);
        } else {
            let mut reveal = Reveal::new(!open);
            reveal.set(open, false, now);
            self.0.insert(key.to_owned(), reveal);
        }
    }

    /// How much of `key` shows: its moving reveal, else all of it when `open`.
    pub fn shown(&self, key: &str, open: bool, now: Instant) -> f32 {
        self.0
            .get(key)
            .map_or(if open { 1. } else { 0. }, |reveal| reveal.shown(now))
    }

    pub fn contains(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Drops reveals that arrived; true when any did.
    pub fn settle(&mut self, now: Instant) -> bool {
        let before = self.0.len();
        self.0.retain(|_, reveal| reveal.moving(now));
        self.0.len() != before
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}
