//! Reversible reveals and their actual completion deadlines.

use super::clock;
use super::timing::PANEL;
use gpui::Pixels;
use lilt::{Animated, Easing};
use std::collections::BTreeMap;
use std::time::Instant;

/// An open or closed state that eases between the two over `PANEL` and, changed
/// mid-way, turns around from wherever it is.
#[derive(Clone, Debug)]
pub struct Reveal {
    animation: Animated<bool, Instant>,
    until: Option<Instant>,
}

impl Reveal {
    pub fn new(open: bool) -> Self {
        Self {
            animation: Animated::new(open)
                .duration(PANEL.as_secs_f32() * 1000.)
                .easing(Easing::EaseOutCubic),
            until: None,
        }
    }

    /// Heads toward `open`; `still` (Reduce Motion) arrives at once.
    pub fn set(&mut self, open: bool, still: bool, now: Instant) {
        if still {
            // lilt ignores instantaneous transitions to an unchanged destination.
            *self = Self::new(open);
        } else {
            if self.animation.value != open {
                self.until = Some(now + PANEL);
            }
            self.animation.transition(open, now);
        }
    }

    /// How much shows at `now`, from 0 to 1.
    pub fn shown(&self, now: Instant) -> f32 {
        self.animation.animate_bool(0., 1., now)
    }

    /// `full` scaled by how much shows, while the reveal moves.
    pub fn sliding(&self, full: Pixels, now: Instant) -> Option<Pixels> {
        self.moving(now).then(|| full * self.shown(now))
    }

    pub fn moving(&self, now: Instant) -> bool {
        self.animation.in_progress(now)
    }

    /// How much shows at each instant, for a GPU motion to sample.
    pub fn sampler(&self) -> impl Fn(Instant) -> f32 + 'static {
        let reveal = self.animation.clone(); // rules-allow: the motion samples its own copy while the view keeps steering this one
        move |at| reveal.animate_bool(0., 1., clock::sampled(at))
    }

    /// When the reveal arrives where it is heading.
    pub fn end(&self, now: Instant) -> Instant {
        self.until.unwrap_or(now).max(now)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn repeated_sampling_and_same_target_keep_the_original_deadline() {
        let start = Instant::now();
        let mut reveal = Reveal::new(false);
        reveal.set(true, false, start);
        let midway = start + Duration::from_millis(50);
        reveal.set(true, false, midway);
        assert_eq!(reveal.end(midway), start + PANEL);
        reveal.set(false, false, midway);
        assert_eq!(
            reveal.end(midway + Duration::from_millis(20)),
            midway + PANEL
        );
        assert_eq!(reveal.shown(midway + PANEL), 0.);
        reveal.set(true, true, midway);
        assert_eq!(reveal.shown(midway), 1.);
        assert_eq!(reveal.end(midway), midway);
    }
}
