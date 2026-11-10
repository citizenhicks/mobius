//! Usage activity series (ProviderUsageChart.swift), as pure functions.

use crate::numeric::{Saturating, ToFloat};
use mobius_gateway::wire::DailyUsage;
use std::collections::BTreeMap;

pub(super) const WEEKS: usize = 26;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::ui) enum Aggregation {
    #[default]
    Daily,
    Weekly,
    Cumulative,
}

impl Aggregation {
    pub(super) const ALL: [Self; 3] = [Self::Daily, Self::Weekly, Self::Cumulative];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Daily => "Daily",
            Self::Weekly => "Weekly",
            Self::Cumulative => "Cumulative",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Snapshot {
    /// Column-major: seven weekdays per week, Monday first.
    pub values: Vec<i64>,
    pub maximum: i64,
    pub active_days: usize,
    pub total_tokens: i64,
}

impl Snapshot {
    /// 0 for an empty cell, then 1 to 4 by share of the maximum.
    pub fn level(&self, value: i64) -> usize {
        if value <= 0 {
            return 0;
        }
        (value.to_f64() / self.maximum.to_f64() * 4.)
            .ceil()
            .saturating_usize()
            .min(4)
    }
}

/// The Monday that starts the first of `weeks` weeks ending with `end_day`'s week.
pub(super) fn start_day(end_day: u64, weeks: usize) -> u64 {
    if weeks == 0 {
        return end_day;
    }
    let since_monday = (end_day % 7 + 3) % 7;
    let week_start = end_day.saturating_sub(since_monday);
    week_start.saturating_sub((weeks as u64 - 1).saturating_mul(7))
}

pub(super) fn snapshot(
    usage: &[DailyUsage],
    end_day: u64,
    weeks: usize,
    aggregation: Aggregation,
) -> Snapshot {
    let mut totals = BTreeMap::<u64, i64>::new();
    for day in usage.iter().filter(|day| !day.provider.is_empty()) {
        let total = totals.entry(day.unix_day).or_default();
        *total = total.saturating_add(day.usage.total_tokens.max(0));
    }
    let start = start_day(end_day, weeks);
    let daily: Vec<i64> = (0..weeks as u64 * 7)
        .map(|offset| start + offset)
        .map(|day| {
            if day <= end_day {
                totals.get(&day).copied().unwrap_or(0)
            } else {
                0
            }
        })
        .collect();
    let active_days = daily.iter().filter(|value| **value > 0).count();
    // Token counts are nonnegative; saturate display totals rather than wrap or panic.
    let total_tokens = daily.iter().copied().fold(0i64, i64::saturating_add);
    let weekly: Vec<i64> = daily
        .chunks(7)
        .map(|week| week.iter().copied().fold(0i64, i64::saturating_add))
        .collect();
    let (values, maximum) = match aggregation {
        Aggregation::Daily => {
            let maximum = daily.iter().copied().max().unwrap_or(0).max(1);
            (daily, maximum)
        }
        Aggregation::Weekly => {
            let maximum = weekly.iter().copied().max().unwrap_or(0).max(1);
            (bars(&weekly, maximum), maximum)
        }
        Aggregation::Cumulative => {
            let running: Vec<i64> = weekly
                .iter()
                .scan(0i64, |total, week| {
                    *total = total.saturating_add(*week);
                    Some(*total)
                })
                .collect();
            let maximum = running.last().copied().unwrap_or(0).max(1);
            (bars(&running, maximum), maximum)
        }
    };
    Snapshot {
        values,
        maximum,
        active_days,
        total_tokens,
    }
}

/// Each week becomes a bottom-aligned bar of up to seven cells.
fn bars(totals: &[i64], maximum: i64) -> Vec<i64> {
    totals
        .iter()
        .flat_map(|&total| {
            let height = if total > 0 {
                (total.to_f64() / maximum.to_f64() * 7.)
                    .ceil()
                    .saturating_usize()
                    .min(7)
            } else {
                0
            };
            (0..7).map(move |row| if row >= 7 - height { total } else { 0 })
        })
        .collect()
}

/// The providers with the most tokens in the visible range, largest first.
pub(super) fn top_providers(
    usage: &[DailyUsage],
    end_day: u64,
    weeks: usize,
    limit: usize,
) -> Vec<(String, i64)> {
    if weeks == 0 {
        return Vec::new();
    }
    let start = start_day(end_day, weeks);
    let mut totals = BTreeMap::<&str, i64>::new();
    for day in usage
        .iter()
        .filter(|day| !day.provider.is_empty() && (start..=end_day).contains(&day.unix_day))
    {
        let total = totals.entry(&day.provider).or_default();
        *total = total.saturating_add(day.usage.total_tokens.max(0));
    }
    let mut ranked: Vec<(String, i64)> = totals
        .into_iter()
        .filter(|(_, tokens)| *tokens > 0)
        .map(|(provider, tokens)| (provider.to_owned(), tokens))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(limit);
    ranked
}
