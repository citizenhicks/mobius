//! The shared animation clock and the spinner every running row turns with.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_FRAME_MILLIS: u128 = 80;
/// The spinner glyph for now, on the shared clock so every spinner turns together.
pub(super) fn spinner() -> &'static str {
    spinner_at(elapsed_since_start())
}

fn spinner_at(elapsed: Duration) -> &'static str {
    let frame = elapsed.as_millis() / SPINNER_FRAME_MILLIS % SPINNER.len() as u128;
    usize::try_from(frame)
        .ok()
        .and_then(|frame| SPINNER.get(frame))
        .copied()
        .unwrap_or("•")
}

/// Time on the shared animation clock, so every animated mark moves together.
pub(super) fn elapsed() -> Duration {
    elapsed_since_start()
}

fn elapsed_since_start() -> Duration {
    PROCESS_START.get_or_init(Instant::now).elapsed()
}
