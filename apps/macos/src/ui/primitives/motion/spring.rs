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
    at(
        value,
        velocity,
        target,
        elapsed.min(Duration::from_millis(50)),
    )
}

/// Where the spring is `elapsed` after leaving `value` at `velocity` for `target`, in one
/// step however long; exactly the target at rest.
pub fn at(value: f32, velocity: f32, target: f32, elapsed: Duration) -> (f32, f32) {
    let t = elapsed.as_secs_f32();
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

/// When the closed-form spring is guaranteed to stay at rest, including after reversal.
pub fn settling_time(value: f32, velocity: f32, target: f32) -> Duration {
    let offset = f64::from(value) - f64::from(target);
    let speed = f64::from(velocity) / f64::from(OMEGA);
    let amplitude = offset.abs().max(speed.abs());
    let carry = (offset + speed).abs();
    // This bounds both displacement and velocity / OMEGA, including a later peak.
    let envelope = |t: f64| (amplitude + carry * t) * (-t).exp();
    let mut low = if carry > 0. {
        (1. - amplitude / carry).max(0.)
    } else {
        0.
    };
    if envelope(low) < f64::from(REST) {
        return Duration::ZERO;
    }
    let mut high = low.max(1.);
    while envelope(high) >= f64::from(REST) {
        high *= 2.;
    }
    for _ in 0..16 {
        let mid = low.midpoint(high);
        if envelope(mid) >= f64::from(REST) {
            low = mid;
        } else {
            high = mid;
        }
    }
    Duration::from_secs_f64(high / f64::from(OMEGA))
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
        assert_eq!(settling_time(0., 0., 0.), Duration::ZERO);
        assert!(
            settling_time(400., 0., 0.) < Duration::from_millis(320),
            "ordinary rows should stop presenting promptly"
        );
        assert!(
            settling_time(10_000_000., 0., 0.) > Duration::from_millis(600),
            "long moves must not be cut short"
        );
        for (value, velocity) in [
            (400., 0.),
            (-20., 800.),
            (20., -800.),
            (1_000_000., -1_000_000.),
            (0., 100_000.),
        ] {
            let end = settling_time(value, velocity, 0.);
            for later in [
                Duration::ZERO,
                Duration::from_millis(16),
                Duration::from_secs(1),
            ] {
                assert_eq!(
                    at(value, velocity, 0., end + later),
                    (0., 0.),
                    "rest deadline for {value}/{velocity}"
                );
            }
        }
    }
}
