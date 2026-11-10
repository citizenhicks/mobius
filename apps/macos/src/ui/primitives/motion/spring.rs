//! The one spring every continuous motion uses: critically damped, so it settles as fast as
//! it can without overshoot, and solved in closed form, so a long frame never destabilises it.

use std::time::Duration;

/// Natural frequency: a 400 pt move settles in about `STANDARD`, a 20 pt line in about
/// `QUICK`.
const OMEGA: f32 = 30.;
/// Closer than this to rest is at rest.
const REST: f32 = 0.5;

/// `value` and `velocity` after `elapsed` heading for `target`; exactly the target at rest.
pub fn step(value: f32, velocity: f32, target: f32, elapsed: Duration) -> (f32, f32) {
    // Past a hitch, resume from where the last frame left off instead of leaping ahead.
    let t = elapsed.as_secs_f32().min(1. / 20.);
    let offset = value - target;
    let decay = (-OMEGA * t).exp();
    let carry = velocity + OMEGA * offset;
    let offset = (offset + carry * t) * decay;
    let velocity = (velocity - OMEGA * carry * t) * decay;
    if offset.abs() < REST && velocity.abs() < REST * OMEGA {
        (target, 0.)
    } else {
        (target + offset, velocity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settles_without_overshoot() {
        let (mut value, mut velocity) = (0., 0.);
        let mut frames = 0;
        while value != 400. {
            (value, velocity) = step(value, velocity, 400., Duration::from_millis(16));
            assert!(value <= 400., "overshot to {value}");
            frames += 1;
        }
        assert!((10..=24).contains(&frames), "settled in {frames} frames");
    }
}
