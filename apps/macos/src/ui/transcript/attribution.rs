//! Compact message dates and an attribution that gives up its date before its name.

use crate::ui::primitives::{FontSet, Type, Typography as _, glyph, icon, space};
use crate::ui::theme::Palette;
use chrono::{DateTime, Datelike, TimeZone};

use gpui::{
    App, Div, IntoElement, ParentElement as _, RenderOnce, SharedString, Styled as _, TextRun,
    Window, container_query, div, font,
};

#[derive(IntoElement)]
pub(super) struct Label {
    pub(super) name: SharedString,
    pub(super) recorded_at_ms: i64,
}

impl RenderOnce for Label {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let name = if self.name.contains(['\r', '\n']) {
            self.name.replace(['\r', '\n'], " ").into()
        } else {
            self.name
        };
        let Some(date) = compact_timestamp(self.recorded_at_ms, &chrono::Local::now()) else {
            return div()
                .min_w_0()
                .truncate()
                .typography(Type::Caption)
                .child(name)
                .into_any_element();
        };
        let text: SharedString = format!("{name} · {date}").into();
        let line = window.text_system().shape_line(
            text,
            Type::Caption.size(),
            &[TextRun {
                len: name.len() + " · ".len() + date.len(),
                font: font(FontSet::current().regular()),
                ..Default::default()
            }],
            None,
        );
        let width = line.width.ceil();
        container_query(move |size, _, _| {
            if width <= size.width {
                div()
                    .typography(Type::Caption)
                    .whitespace_nowrap()
                    .child(line.text)
                    .into_any_element()
            } else {
                div()
                    .w_full()
                    .min_w_0()
                    .truncate()
                    .typography(Type::Caption)
                    .child(name)
                    .into_any_element()
            }
        })
        .w(width)
        .h(Type::Caption.line_height())
        .min_w_0()
        .max_w_full()
        .flex_shrink_1()
        .into_any_element()
    }
}

/// The heading of a resumed chat's divider: "Today", a weekday this week, or the date.
pub(super) fn divider_timestamp<Tz: TimeZone>(
    milliseconds: i64,
    now: &DateTime<Tz>,
) -> Option<String>
where
    Tz::Offset: std::fmt::Display,
{
    if milliseconds <= 0 {
        return None;
    }
    let time = now.timezone().timestamp_millis_opt(milliseconds).single()?;
    let days = now
        .date_naive()
        .signed_duration_since(time.date_naive())
        .num_days();
    let format = match days {
        0 => "Today %H:%M",
        1 => "Yesterday %H:%M",
        2..=6 => "%A %H:%M",
        _ if time.year() == now.year() => "%b %-d %H:%M",
        _ => "%b %-d, %Y %H:%M",
    };
    Some(time.format(format).to_string())
}

fn compact_timestamp<Tz: TimeZone>(milliseconds: i64, now: &DateTime<Tz>) -> Option<String>
where
    Tz::Offset: std::fmt::Display,
{
    if milliseconds <= 0 {
        return None;
    }
    let time = now.timezone().timestamp_millis_opt(milliseconds).single()?;
    let days = now
        .date_naive()
        .signed_duration_since(time.date_naive())
        .num_days();
    let format = match days {
        0 => "%H:%M",
        1..=6 => "%a, %H:%M",
        _ => "%b %-d, %H:%M",
    };
    Some(time.format(format).to_string())
}

/// "you", "steer · you" or "queued · you" under a bubble.
pub(super) fn message_meta(
    p: &Palette,
    glyph: &'static str,
    delivery: Option<&'static str>,
    recorded_at_ms: i64,
) -> Div {
    div()
        .max_w_full()
        .min_w_0()
        .flex()
        .items_center()
        .gap(space::XS)
        .typography(Type::Caption)
        .text_color(p.muted)
        .child(icon(glyph, p).size(glyph::MARK).flex_shrink_0())
        .children(delivery.map(|label| div().flex_shrink_0().child(format!("{label} ·"))))
        .child(Label {
            name: "you".into(),
            recorded_at_ms,
        })
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    #[test]
    fn a_divider_names_today_a_weekday_or_the_date() {
        let now = Utc.with_ymd_and_hms(2026, 10, 25, 18, 0, 0).unwrap();
        let at = |y, m, d| {
            Utc.with_ymd_and_hms(y, m, d, 9, 5, 0)
                .unwrap()
                .timestamp_millis()
        };
        let heading = |ms| super::divider_timestamp(ms, &now).unwrap();
        assert_eq!(heading(at(2026, 10, 25)), "Today 09:05");
        assert_eq!(heading(at(2026, 10, 24)), "Yesterday 09:05");
        assert_eq!(heading(at(2026, 10, 22)), "Thursday 09:05");
        assert_eq!(heading(at(2026, 10, 5)), "Oct 5 09:05");
        assert_eq!(heading(at(2025, 10, 5)), "Oct 5, 2025 09:05");
        assert_eq!(super::divider_timestamp(0, &now), None);
    }
}
