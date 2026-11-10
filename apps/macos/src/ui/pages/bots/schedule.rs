//! Routine schedules and times as people read them (RoutineViews.swift).

use crate::ui::counted;
use chrono::{DateTime, Local, TimeZone, Timelike};
use mobius_gateway::wire::{
    HookSelector, Routine, RoutineAction, RoutineBinding, RoutineRunStatus, RoutineSchedule,
    RoutineScheduleKind,
};

/// The schedule editor changes one timer that starts this routine; other bindings remain intact.
pub(super) fn scheduled_start(
    bindings: &[RoutineBinding],
) -> Option<(usize, &RoutineSchedule, Option<i64>)> {
    bindings
        .iter()
        .enumerate()
        .find_map(|(index, binding)| match (&binding.on, &binding.action) {
            (HookSelector::Schedule { schedule, ends_at }, RoutineAction::Start) => {
                Some((index, schedule, *ends_at))
            }
            _ => None,
        })
}

/// Names the editable start timer and accounts for every other saved trigger.
pub(super) fn routine_summary(routine: &Routine) -> String {
    match scheduled_start(&routine.bindings) {
        None if routine.bindings.is_empty() => "Manual".into(),
        Some((_, schedule, _)) if routine.bindings.len() == 1 => schedule_summary(schedule),
        Some((_, schedule, _)) => format!(
            "{} · {}",
            schedule_summary(schedule),
            counted(
                routine.bindings.len() - 1,
                "other trigger",
                "other triggers"
            )
        ),
        None => {
            let events = routine
                .bindings
                .iter()
                .all(|binding| matches!(binding.on, HookSelector::Event { .. }));
            let kind = if events { "event" } else { "saved" };
            counted(
                routine.bindings.len(),
                &format!("{kind} trigger"),
                &format!("{kind} triggers"),
            )
        }
    }
}

/// A `m h * * [d]` cron line: minute, hour and an optional weekday (0 is Sunday).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SimpleCron {
    pub minute: u32,
    pub hour: u32,
    pub weekday: Option<u32>,
}

impl SimpleCron {
    pub fn parse(expression: &str) -> Option<Self> {
        let fields: Vec<_> = expression.split_whitespace().collect();
        let [minute, hour, "*", "*", day] = fields.as_slice() else {
            return None;
        };
        let minute = minute.parse().ok().filter(|m| *m < 60)?;
        let hour = hour.parse().ok().filter(|h| *h < 24)?;
        let weekday = match *day {
            "*" => None,
            day => Some(day.parse::<u32>().ok().filter(|d| *d <= 7)? % 7),
        };
        Some(Self {
            minute,
            hour,
            weekday,
        })
    }

    pub fn expression(self) -> String {
        let day = self
            .weekday
            .map_or_else(|| "*".into(), |day| day.to_string());
        format!("{} {} * * {day}", self.minute, self.hour)
    }
}

pub(super) const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// "2:00 AM".
pub(super) fn clock(hour: u32, minute: u32) -> String {
    let (suffix, hour12) = match hour {
        0 => ("AM", 12),
        1..=11 => ("AM", hour),
        12 => ("PM", 12),
        _ => ("PM", hour - 12),
    };
    format!("{hour12}:{minute:02} {suffix}")
}

fn date_time(at: i64) -> String {
    Local
        .timestamp_opt(at, 0)
        .single()
        .map_or_else(String::new, |date: DateTime<Local>| {
            format!(
                "{} {}",
                date.format("%b %-d,"),
                clock(date.hour(), date.minute())
            )
        })
}

fn every(seconds: u64) -> String {
    let (value, unit) = if seconds.is_multiple_of(3_600) {
        (seconds / 3_600, "hour")
    } else if seconds.is_multiple_of(60) {
        (seconds / 60, "minute")
    } else {
        (seconds, "second")
    };
    format!("Every {}", counted(value, unit, &format!("{unit}s")))
}

/// `routineScheduleSummary`: "Daily at 2:00 AM", "Every 10 minutes", "Custom schedule".
pub(super) fn schedule_summary(schedule: &RoutineSchedule) -> String {
    match schedule.kind {
        RoutineScheduleKind::Once => schedule
            .at
            .map_or_else(|| "Once".into(), |at| format!("Once · {}", date_time(at))),
        RoutineScheduleKind::Interval => every(schedule.every_seconds.unwrap_or(0)),
        RoutineScheduleKind::Cron => {
            match schedule.expression.as_deref().and_then(SimpleCron::parse) {
                Some(SimpleCron {
                    minute,
                    hour,
                    weekday: None,
                }) => format!("Daily at {}", clock(hour, minute)),
                Some(SimpleCron {
                    minute,
                    hour,
                    weekday: Some(day),
                }) => format!(
                    "Every {} at {}",
                    WEEKDAYS.get(day as usize).copied().unwrap_or_default(),
                    clock(hour, minute)
                ),
                None => "Custom schedule".into(),
            }
        }
    }
}

/// "9:05" or "09:05" as hour and minute.
pub(super) fn parse_clock(text: &str) -> Option<(u32, u32)> {
    let (hour, minute) = text.trim().split_once(':')?;
    let (hour, minute) = (hour.parse().ok()?, minute.parse().ok()?);
    (hour < 24 && minute < 60).then_some((hour, minute))
}

const LOCAL_FORMAT: &str = "%Y-%m-%d %H:%M";

/// "2026-09-28 09:00" in this Mac's time zone, as a Unix timestamp.
pub(super) fn parse_local(text: &str) -> Option<i64> {
    let naive = chrono::NaiveDateTime::parse_from_str(text.trim(), LOCAL_FORMAT).ok()?;
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|date| date.timestamp())
}

pub(super) fn format_local(at: i64) -> String {
    Local
        .timestamp_opt(at, 0)
        .single()
        .map_or_else(String::new, |date| date.format(LOCAL_FORMAT).to_string())
}

/// This Mac's IANA time zone, or UTC.
pub(super) fn local_zone() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into())
}

pub(super) fn now() -> i64 {
    Local::now().timestamp()
}

pub(super) fn run_status_label(status: RoutineRunStatus) -> &'static str {
    match status {
        RoutineRunStatus::Running => "Running",
        RoutineRunStatus::Succeeded => "Succeeded",
        RoutineRunStatus::Failed => "Failed",
        RoutineRunStatus::Skipped => "Skipped",
        RoutineRunStatus::Cancelled => "Cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_cron_round_trips_daily_and_weekly_lines() {
        let daily = SimpleCron::parse("0 2 * * *").unwrap();
        assert_eq!(daily.weekday, None);
        assert_eq!(daily.expression(), "0 2 * * *");
        assert_eq!(SimpleCron::parse("30 9 * * 7").unwrap().weekday, Some(0));
        assert_eq!(SimpleCron::parse("0 9 * * 1-5"), None);
        assert_eq!(SimpleCron::parse("60 9 * * *"), None);
    }
}
