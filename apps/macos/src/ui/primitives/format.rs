//! Counts, sizes, durations and percentages as both apps write them.

use crate::numeric::ToFloat;

/// "1 file", "3 files": the count and the word that agrees with it.
pub fn counted<N>(count: N, one: &str, many: &str) -> String
where
    N: std::fmt::Display + PartialEq + From<u8> + Copy,
{
    let word = if count == N::from(1) { one } else { many };
    format!("{count} {word}")
}

/// A file size as Apple's `.byteCount(style: .file)` reads it, so both apps agree: decimal
/// units, whole kB, one decimal place for MB and two above, trailing zeros dropped.
pub fn byte_count(bytes: u64) -> String {
    const UNITS: [(&str, usize); 4] = [("kB", 0), ("MB", 1), ("GB", 2), ("TB", 2)];
    match bytes {
        0 => return "Zero kB".into(),
        1 => return "1 byte".into(),
        2..999 => return format!("{bytes} bytes"),
        _ => {}
    }
    let mut value = bytes.to_f64() / 1000.;
    let mut unit = 0;
    while value >= 999. && unit < UNITS.len() - 1 {
        value /= 1000.;
        unit += 1;
    }
    let [.., largest] = UNITS;
    let (name, decimals) = UNITS.get(unit).copied().unwrap_or(largest);
    let text = format!("{value:.decimals$}");
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        &text
    };
    format!("{text} {name}")
}

/// "m:ss", or "h:mm:ss" from an hour on, as SwiftUI's timer text.
pub fn clock_duration(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    let (hours, minutes) = (seconds / 3_600, seconds / 60 % 60);
    if hours == 0 {
        format!("{minutes}:{:02}", seconds % 60)
    } else {
        format!("{hours}:{minutes:02}:{:02}", seconds % 60)
    }
}

/// The largest whole unit, as Apple's abbreviated units read: "12 sec", "35 min", "2 hr",
/// "1 day", "3 days".
pub fn compact_duration(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    match seconds {
        0..60 => format!("{seconds} sec"),
        60..3_600 => format!("{} min", seconds / 60),
        3_600..86_400 => format!("{} hr", seconds / 3_600),
        86_400..172_800 => "1 day".into(),
        _ => format!("{} days", seconds / 86_400),
    }
}

/// "Just now" under a minute, else "3 min ago", from the seconds since.
pub fn time_ago(seconds: i64) -> String {
    match u64::try_from(seconds) {
        Ok(seconds) if seconds >= 60 => {
            format!("{} ago", compact_duration(seconds.saturating_mul(1000)))
        }
        _ => "Just now".into(),
    }
}

/// "3 min ago", "Just now", from Unix seconds.
pub fn ago(at: i64) -> String {
    time_ago(chrono::Utc::now().timestamp().saturating_sub(at))
}

/// "999", "1.2K", "12K", "-3.4M", in the style of SwiftUI's compact notation.
pub fn compact_count(value: i64) -> String {
    const UNITS: [(f64, &str); 3] = [(1e9, "B"), (1e6, "M"), (1e3, "K")];
    let magnitude = value.unsigned_abs().to_f64();
    for (scale, unit) in UNITS {
        if magnitude >= scale {
            let scaled = format!("{:.1}", value.to_f64() / scale);
            return format!("{}{unit}", scaled.trim_end_matches(".0"));
        }
    }
    value.to_string()
}

/// A fraction as a whole percent: "42%".
pub fn percent(fraction: f64) -> String {
    format!("{:.0}%", fraction * 100.)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_duration_rolls_minutes_into_hours() {
        assert_eq!(clock_duration(65_000), "1:05");
        assert_eq!(clock_duration(7_265_000), "2:01:05");
    }

    #[test]
    fn relative_times_handle_future_and_extreme_timestamps() {
        for (seconds, expected) in [(-1, "Just now"), (59, "Just now"), (60, "1 min ago")] {
            assert_eq!(time_ago(seconds), expected);
        }
        assert!(time_ago(i64::MAX).ends_with(" days ago"));
    }

    #[test]
    fn counted_agrees_with_one() {
        assert_eq!(counted(1u32, "file", "files"), "1 file");
        assert_eq!(counted(0usize, "file", "files"), "0 files");
        assert_eq!(counted(12u64, "run", "runs"), "12 runs");
    }

    #[test]
    fn byte_count_matches_apples_file_style() {
        // Read from Foundation's `.byteCount(style: .file)` in en_US.
        for (bytes, apple) in [
            (0, "Zero kB"),
            (1, "1 byte"),
            (998, "998 bytes"),
            (999, "1 kB"),
            (1_500, "2 kB"),
            (12_345, "12 kB"),
            (999_499, "1 MB"),
            (1_500_000, "1.5 MB"),
            (9_949_999, "9.9 MB"),
            (12_345_678, "12.3 MB"),
            (999_950_000, "1 GB"),
            (1_234_567_890, "1.23 GB"),
            (12_345_678_901, "12.35 GB"),
            (1_000_000_000_000, "1 TB"),
        ] {
            assert_eq!(byte_count(bytes), apple, "{bytes}");
        }
    }
}
