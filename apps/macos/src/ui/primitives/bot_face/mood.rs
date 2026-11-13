//! Mood glyphs, beat ranges, brows and blush.

use std::ops::Range;

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
    pub(super) fn beat(self) -> Range<f32> {
        match self {
            Self::Dizzy => 0.18..0.24,
            Self::Listening | Self::Happy | Self::Wink => 0.45..0.6,
            Self::Sleepy => 2.5..3.5,
            Self::Idle | Self::Watching | Self::Thinking => 1.2..2.6,
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
