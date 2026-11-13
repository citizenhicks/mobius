//! One monotonic motion clock, including deterministic snapshot sampling.

use gpui::{App, Bounds, Pixels, Window};
use std::cell::Cell;
use std::sync::OnceLock;
use std::time::Instant;

static EPOCH: OnceLock<Instant> = OnceLock::new();

thread_local! {
    /// The snapshot harness's frame clock, while it records a frame strip.
    static CLOCK: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// The time motion reads: the real clock, or a frame clock the snapshot harness steps
/// exactly one frame per capture, however long each capture takes.
pub fn now() -> Instant {
    CLOCK.with(Cell::get).unwrap_or_else(Instant::now)
}

/// The instant a GPU motion shows when presented at `at`: the snapshot harness's frame
/// clock while it records, so captured strips step exactly as rendered motion does.
pub fn sampled(at: Instant) -> Instant {
    CLOCK.with(Cell::get).unwrap_or(at)
}

#[cfg(feature = "snapshot")]
pub fn set_clock(at: Option<Instant>) {
    CLOCK.with(|clock| clock.set(at));
}

/// Decorative motion runs only in active windows, except in the snapshot harness.
pub fn is_animating(window: &Window, cx: &App) -> bool {
    !cx.reduce_motion() && (window.is_window_active() || cfg!(feature = "snapshot"))
}

/// Seconds from the shared origin, sampled through the snapshot clock.
pub fn seconds(at: Instant) -> f64 {
    let at = sampled(at);
    at.saturating_duration_since(*EPOCH.get_or_init(|| at))
        .as_secs_f64()
}

/// Whether the laid-out decoration intersects the current paint mask.
pub fn in_view(bounds: Bounds<Pixels>, window: &Window) -> bool {
    !bounds.intersect(&window.content_mask().bounds).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ambient_time_uses_one_monotonic_origin_and_the_snapshot_clock() {
        let start = *EPOCH.get_or_init(Instant::now) + Duration::from_secs(1);
        let first = seconds(start);
        assert!((seconds(start + Duration::from_secs(1)) - first - 1.).abs() < 1e-9);
        #[cfg(feature = "snapshot")]
        {
            set_clock(Some(start));
            let frozen = (now(), seconds(start + Duration::from_secs(20)));
            set_clock(None);
            assert_eq!(frozen, (start, first));
        }
    }
}
