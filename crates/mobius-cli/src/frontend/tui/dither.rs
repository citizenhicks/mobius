//! Ordered (Bayer) dither drawn with braille dots. Only foreground dots are drawn, so rules,
//! card rims and glows sit on whatever background the terminal has.

use std::time::Duration;

const BAYER: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
/// The braille bit for each dot of a cell, by dot row then dot column.
const DOTS: [[u32; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

/// A dot position and the width of the field it sits in, all in dots.
#[derive(Clone, Copy)]
pub(super) struct Dot {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) width: f32,
}

/// `cols` by `rows` braille cells. `tone(dot)` gives each dot's ink density in `0..=1`; a dot is
/// drawn where the density beats its Bayer threshold.
pub(super) fn field(cols: u16, rows: u16, tone: impl Fn(Dot) -> f32) -> Vec<String> {
    let width = f32::from(cols) * 2.0;
    (0..rows)
        .map(|row| {
            (0..cols)
                .map(|col| {
                    let mut code = 0;
                    for (dy, (bits, thresholds)) in (0_u16..).zip(DOTS.iter().zip(BAYER.iter())) {
                        for (dx, bit) in (0_u16..).zip(bits) {
                            let (x, y) = (col * 2 + dx, row * 4 + dy);
                            let threshold = thresholds
                                .get(usize::from(x % 4))
                                .map_or(16.0, |level| f32::from(*level) + 0.5);
                            let dot = Dot {
                                x: f32::from(x),
                                y: f32::from(y),
                                width,
                            };
                            if tone(dot) * 16.0 > threshold {
                                code |= bit;
                            }
                        }
                    }
                    char::from_u32(0x2800 + code)
                        .filter(|_| code != 0)
                        .unwrap_or(' ')
                })
                .collect()
        })
        .collect()
}

/// A one-cell-high dotted rule on dot row `row` (0 at the top of the cell, 3 at the bottom), whose
/// density along its length is `tone(position)`, the position running `0..=1` from the left end.
pub(super) fn rule_on(cols: u16, row: u8, tone: impl Fn(f32) -> f32) -> String {
    let row = f32::from(row);
    field(cols, 1, |dot| {
        if dot.y == row {
            tone(dot.x / dot.width)
        } else {
            0.0
        }
    })
    .pop()
    .unwrap_or_default()
}

/// A mid-height rule that is densest in the middle and fades out at both ends.
pub(super) fn fading_rule(cols: u16) -> String {
    rule_on(cols, 2, |position| {
        1.05 - (position * 2.0 - 1.0).abs() * 1.1
    })
}

/// A mid-height rule that fades out toward its right end.
pub(super) fn trailing_rule(cols: u16) -> String {
    rule_on(cols, 2, |position| 1.0 - position * 1.05)
}

/// A card's top or bottom rim: solid near its left corner, fading out toward the right.
pub(super) fn rim(cols: u16, row: u8) -> String {
    rule_on(cols, row, |position| 1.4 - position * 1.3)
}

/// A glow `cols` wide that brightens toward its top-right corner, for card row `row` of `rows`.
pub(super) fn corner_glow(cols: u16, row: u16, rows: u16) -> String {
    let field_height = f32::from(rows) * 4.0;
    let top = f32::from(row) * 4.0;
    field(cols, 1, move |dot| {
        let across = (dot.width - dot.x) / dot.width;
        let down = (top + dot.y) / field_height;
        0.85 - across.hypot(down) * 1.05
    })
    .pop()
    .unwrap_or_default()
}

/// A loading band `cols` wide that sweeps left to right, `elapsed` into the animation clock.
pub(super) fn sweep(cols: u16, elapsed: Duration) -> String {
    let travel = elapsed.as_secs_f32() * 10.0;
    field(cols, 1, move |dot| {
        let span = dot.width * 1.6;
        let position = (dot.x - travel).rem_euclid(span);
        if position < dot.width {
            (1.0 - (position - dot.width * 0.5).abs() / (dot.width * 0.5)) * 1.2 - 0.05
        } else {
            0.04
        }
    })
    .pop()
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dots(text: &str) -> u32 {
        text.chars()
            .map(|c| u32::from(c).saturating_sub(0x2800).count_ones())
            .sum()
    }

    #[test]
    fn a_fading_rule_is_one_dot_row_densest_in_its_middle() {
        let rule = fading_rule(20);
        let cells: Vec<char> = rule.chars().collect();
        assert_eq!(cells.len(), 20);
        let middle: String = cells[8..12].iter().collect();
        let edge: String = cells[..4].iter().collect();
        assert!(dots(&middle) > dots(&edge), "{rule}");
        // Only the third dot row (bits 0x04 and 0x20) is drawn.
        assert!(
            cells
                .iter()
                .all(|c| *c == ' ' || (u32::from(*c) - 0x2800) & !0x24 == 0),
            "{rule}"
        );
    }
}
