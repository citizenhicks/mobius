use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

pub(super) fn line(text: &str, base: Color, highlight: Color) -> Line<'static> {
    line_at(text, elapsed_since_start(), base, highlight)
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
                Span::styled(
                    character.to_string(),
                    Style::default()
                        .fg(blend(highlight, base, intensity * 0.9))
                        .add_modifier(Modifier::BOLD),
                )
            })
            .collect::<Vec<_>>(),
    )
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "an alpha mix of two u8 channels stays within 0..=255"
)]
fn blend(foreground: Color, background: Color, alpha: f32) -> Color {
    let (Color::Rgb(fr, fg, fb), Color::Rgb(br, bg, bb)) = (foreground, background) else {
        return if alpha >= 0.5 { foreground } else { background };
    };
    Color::Rgb(
        (f32::from(fr) * alpha + f32::from(br) * (1.0 - alpha)) as u8,
        (f32::from(fg) * alpha + f32::from(bg) * (1.0 - alpha)) as u8,
        (f32::from(fb) * alpha + f32::from(bb) * (1.0 - alpha)) as u8,
    )
}
