//! Snapshot-only parity fixture for still and GPU-carried face poses.

use super::draw::{Eyes, face, moving_face};
use super::mood::BotMood;
use super::pose::{Course, Glide, Live, POSE_SPRING, Pose};
use gpui::{AnyElement, Hsla, Pixels, point};
use mobius_gateway::wire::BotShape;
use std::time::{Duration, Instant};

/// Two faces for the snapshot harness to compare: one still at the pose a glide shows
/// `elapsed` into its spring, and one laid out at the glide's end and moved there by its
/// motions, sampled at `start + elapsed` (the harness's frame clock).
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
