use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_FRAME_MILLIS: u128 = 80;
/// A 1×4 ordered-dither threshold row: neighbouring cells light at well-spread intensities.
const BAYER: [f32; 4] = [0.125, 0.625, 0.375, 0.875];

pub(super) fn line(text: &str, base: Color, highlight: Color) -> Line<'static> {
    line_at(text, elapsed_since_start(), base, highlight)
}

/// The spinner glyph for now, on the same clock as the shimmer so every spinner turns together.
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

fn elapsed_since_start() -> Duration {
    PROCESS_START.get_or_init(Instant::now).elapsed()
}

fn line_at(text: &str, elapsed: Duration, base: Color, highlight: Color) -> Line<'static> {
    let characters = text.chars().collect::<Vec<_>>();
    if characters.is_empty() {
        return Line::default();
    }

    let padding = 10;
    let period = characters.len() + padding * 2;
    let sweep_millis = 2000;
    let sweep_offset = usize::try_from(elapsed.as_millis() % sweep_millis).unwrap_or_default();
    let position = sweep_offset * period / 2000;
    let band_half_width = 5;

    Line::from(
        characters
            .into_iter()
            .enumerate()
            .map(|(index, character)| {
                let distance = (index + padding).abs_diff(position);
                let intensity = match u8::try_from(distance) {
                    Ok(distance) if distance <= band_half_width => {
                        let angle = std::f32::consts::PI
                            * (f32::from(distance) / f32::from(band_half_width));
                        f32::midpoint(1.0, angle.cos())
                    }
                    Ok(_) | Err(_) => 0.0,
                };
                // Ordered dither: each cell switches fully on or off against its Bayer threshold,
                // so the band reads as a stipple that thins toward its edges.
                let threshold = BAYER.get(index % BAYER.len()).copied().unwrap_or(0.5);
                let color = if intensity > threshold {
                    highlight
                } else {
                    base
                };
                Span::styled(
                    character.to_string(),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )
            })
            .collect::<Vec<_>>(),
    )
}
