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

/// How long a new title takes to decode into place.
const SCRAMBLE: Duration = Duration::from_millis(450);
/// Glyphs a title shows before each character settles.
const SCRAMBLE_GLYPHS: [char; 12] = ['⣿', '⢿', '⡿', '⣻', '⣽', '⣾', '⣷', '⣯', '░', '▒', '▓', '/'];

/// `title` part way through decoding `age` after it appeared: characters settle left to right
/// and the rest show dither glyphs. `None` once it has settled.
pub(super) fn scramble(title: &str, age: Duration) -> Option<String> {
    if age >= SCRAMBLE {
        return None;
    }
    let length = title.chars().count();
    let settled = usize::try_from(
        age.as_millis() * u128::try_from(length).unwrap_or_default() / SCRAMBLE.as_millis(),
    )
    .unwrap_or(length);
    let frame = usize::try_from(elapsed_since_start().as_millis() / 40).unwrap_or_default();
    Some(
        title
            .chars()
            .enumerate()
            .map(|(index, character)| {
                if index < settled || character == ' ' {
                    character
                } else {
                    SCRAMBLE_GLYPHS
                        .get((index * 7 + frame) % SCRAMBLE_GLYPHS.len())
                        .copied()
                        .unwrap_or(character)
                }
            })
            .collect(),
    )
}

/// Time on the shared animation clock, so every animated mark moves together.
pub(super) fn elapsed() -> Duration {
    elapsed_since_start()
}

fn elapsed_since_start() -> Duration {
    PROCESS_START.get_or_init(Instant::now).elapsed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_decodes_left_to_right_and_then_settles() {
        let title = "Bash · ls";
        let early = scramble(title, Duration::ZERO).expect("still decoding");
        assert_eq!(early.chars().count(), title.chars().count());
        assert_ne!(early, title);
        let late = scramble(title, SCRAMBLE * 9 / 10).expect("still decoding");
        assert!(late.starts_with("Bash · "), "{late}");
        assert_eq!(scramble(title, SCRAMBLE), None);
    }
}
