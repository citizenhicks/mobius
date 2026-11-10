//! Settings › Usage (WP5): limits, token totals and the activity heatmap.

mod series;

use super::*;
use crate::numeric::{Saturating, ToFloat};
use mobius_gateway::wire::{DailyUsage, ProviderUsage};
pub(in crate::ui) use series::Aggregation;
use series::WEEKS;

impl Workspace {
    pub(in crate::ui) fn usage_settings(&self, cx: &Context<Self>) -> Page {
        let Some(profile) = &self.client.state.profile else {
            let empty = if self.client.link.is_ready() {
                EmptyState::new("hi.chartBarDecreasing", "Loading usage").loading()
            } else {
                EmptyState::new("hi.chartBarDecreasing", "No usage yet")
                    .detail("Connect to a gateway to see its usage.")
            };
            return page("settings-usage", PageHeader::new("Usage")).child(empty);
        };
        let today = unix_day();
        let days = &profile.daily_usage;
        let chart = series::snapshot(days, today, WEEKS, Aggregation::Daily);
        let header = PageHeader::new("Usage").subtitle(format!(
            "{} tokens · {} active days in the last {WEEKS} weeks",
            compact_count(chart.total_tokens),
            chart.active_days
        ));
        let mut body = page("settings-usage", header);
        let limited: Vec<_> = profile
            .provider_usage
            .iter()
            .filter(|usage| usage.error.is_some() || usage.limits.iter().flatten().next().is_some())
            .collect();
        if !limited.is_empty() {
            let mut limits = Group::new();
            for usage in limited {
                limits = self.provider_limits(limits, usage);
            }
            body = body.child(Section::new("Limits").child(limits));
        }
        body.child(
            Section::new("Tokens")
                .child(Group::new().child(token_metrics(days, self.palette.muted))),
        )
        .child(self.activity_section(days, today, cx))
    }

    /// Today's tokens, each provider limit and the Cloud renewal, for the menu bar item.
    pub(in crate::ui) fn menu_bar_usage(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(profile) = &self.client.state.profile {
            let today = unix_day();
            let tokens: i64 = profile
                .daily_usage
                .iter()
                .filter(|day| day.unix_day == today)
                .map(|day| day.usage.total_tokens.max(0))
                .sum();
            lines.push(format!("Today: {} tokens", compact_count(tokens)));
            for usage in &profile.provider_usage {
                let (label, _) = self.provider_label(&usage.provider);
                for limit in usage.limits.iter().flatten() {
                    lines.push(format!(
                        "{label} · {}: {:.0}% left · {}",
                        limit.label,
                        limit.remaining_fraction.clamp(0., 1.) * 100.,
                        reset_label(limit.resets_at)
                    ));
                }
            }
        }
        let cloud = self.cloud_status();
        if let Some(date) = &cloud.allowance_renews_on {
            lines.push(format!("möbius Cloud renews {date}"));
        } else if let Some(date) = &cloud.access_ends_on {
            lines.push(format!("möbius Cloud ends {date}"));
        }
        lines
    }

    fn provider_label<'a>(&'a self, instance: &'a str) -> (&'a str, Hsla) {
        self.client
            .state
            .gateway
            .as_ref()
            .and_then(|gateway| {
                gateway
                    .provider_instances
                    .iter()
                    .find(|record| record.selection.instance == instance)
            })
            .map_or((instance, self.palette.accent), |record| {
                (record.label.as_str(), Accent::from(record.tint).color())
            })
    }

    fn provider_limits(&self, mut group: Group, usage: &ProviderUsage) -> Group {
        let p = self.palette;
        let (label, _) = self.provider_label(&usage.provider);
        if let Some(error) = &usage.error {
            group = group.child(
                Row::new(
                    SharedString::from(format!("usage-error:{}", usage.provider)),
                    label,
                )
                .leading(Leading::Glyph("hi.warning".into()))
                .subtitle(error.as_str()),
            );
        }
        for limit in usage.limits.iter().flatten() {
            let remaining = limit.remaining_fraction.clamp(0., 1.);
            let resets = reset_label(limit.resets_at);
            group = group.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S)
                    .px(space::L)
                    .py(inset::ROW)
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .gap(space::S)
                            .typography(Type::Body)
                            .child(SharedString::from(&limit.label))
                            .child(div().text_color(p.muted).child(motion::counter(
                                format!("usage-remaining:{}", limit.label),
                                remaining,
                                |share| format!("{} remaining", percent(share)).into_any_element(),
                            ))),
                    )
                    .child(
                        div().h(px(4.)).w_full().bg(p.inset).pill_corners().child(
                            div()
                                .h_full()
                                .w(relative(remaining.to_f32()))
                                .bg(p.accent)
                                .pill_corners(),
                        ),
                    )
                    .child(
                        div()
                            .typography(Type::Caption)
                            .text_color(p.muted)
                            .child(resets),
                    ),
            );
        }
        group
    }

    fn activity_section(&self, days: &[DailyUsage], today: u64, cx: &Context<Self>) -> Section {
        let p = self.palette;
        let aggregation = self.config.usage_aggregation;
        let chart = series::snapshot(days, today, WEEKS, aggregation);
        let top = series::top_providers(days, today, WEEKS, 3);
        let tint = top
            .first()
            .map_or(p.accent, |(provider, _)| self.provider_label(provider).1);
        // Full width above the chart, as on iOS.
        let picker = segmented(
            "usage-aggregation",
            Aggregation::ALL.map(Aggregation::label),
            Aggregation::ALL
                .iter()
                .position(|option| *option == aggregation),
            cx.listener(|this, index: &usize, _, cx| {
                if let Some(&aggregation) = Aggregation::ALL.get(*index) {
                    this.config.usage_aggregation = aggregation;
                    cx.notify();
                }
            }),
        )
        .full_width();
        let columns = chart.values.chunks(7).map(|week| {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(space::XS)
                .children(week.iter().map(|value| {
                    let fill = match chart.level(*value) {
                        0 => p.line.opacity(0.35),
                        1 => tint.opacity(0.28),
                        2 => tint.opacity(0.5),
                        3 => tint.opacity(0.72),
                        _ => tint,
                    };
                    div().w_full().aspect_ratio(1.).cell_corners().bg(fill)
                }))
        });
        let mut card = div()
            .flex()
            .flex_col()
            .gap(space::L)
            .p(space::L)
            .child(picker)
            .child(div().flex().gap(space::XS).children(columns));
        if !top.is_empty() {
            card = card
                .child(hairline(&p))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .typography(Type::Caption)
                        .child(div().typography(Type::Subhead).child("Top providers"))
                        .child(
                            div()
                                .text_color(p.muted)
                                .child(format!("Last {WEEKS} weeks")),
                        ),
                )
                .children(top.into_iter().map(|(provider, tokens)| {
                    let (label, tint) = self.provider_label(&provider);
                    div()
                        .flex()
                        .items_center()
                        .gap(space::S)
                        .typography(Type::Caption)
                        .child(div().size(glyph::DOT).pill_corners().bg(tint))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(label)),
                        )
                        .child(div().typography(Type::Mono).text_color(p.muted).child(
                            motion::counter(
                                format!("usage-provider:{provider}"),
                                tokens.to_f64(),
                                |tokens| {
                                    compact_count(tokens.round().saturating_i64())
                                        .into_any_element()
                                },
                            ),
                        ))
                }));
        }
        let detail = match aggregation {
            Aggregation::Daily => "Tokens per day, Monday first.",
            Aggregation::Weekly => "Tokens per week as bars.",
            Aggregation::Cumulative => "Running total by week.",
        };
        Section::new("Token activity")
            .description(detail)
            .child(Group::new().child(card))
    }

    pub(in crate::ui) fn refresh_profile(&mut self, cx: &mut Context<Self>) {
        self.send(
            ClientMessage::GetProfile {
                request_id: crate::gateway::request_id(),
                include_provider_usage: true,
            },
            cx,
        );
    }
}

/// Four equal columns: the value, then its label.
fn token_metrics(days: &[DailyUsage], muted: Hsla) -> Div {
    let (mut total, mut input, mut output, mut cached) = (0i64, 0i64, 0i64, 0i64);
    for day in days {
        total = total.saturating_add(day.usage.total_tokens.max(0));
        input = input.saturating_add(day.usage.input_tokens.max(0));
        output = output.saturating_add(day.usage.output_tokens.max(0));
        cached = cached.saturating_add(day.usage.cached_input_tokens.max(0));
    }
    let hit = if input > 0 {
        percent(cached.to_f64() / input.to_f64())
    } else {
        "—".into()
    };
    let metrics = [
        ("Tokens", compact_count(total)),
        ("Input", compact_count(input)),
        ("Output", compact_count(output)),
        ("Cache hit", hit),
    ];
    div()
        .flex()
        .gap(space::S)
        .p(space::L)
        .children(metrics.map(|(label, value)| {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(space::XS)
                .child(heading(value))
                .child(
                    div()
                        .typography(Type::Caption)
                        .text_color(muted)
                        .child(label),
                )
        }))
}

/// "Resets Nov 3, 4:10 PM" in local time.
fn reset_label(at: Option<i64>) -> String {
    at.map_or_else(
        || "Reset time unavailable".into(),
        |at| {
            chrono::DateTime::from_timestamp(at, 0).map_or(String::new(), |date| {
                let local = date.with_timezone(&chrono::Local);
                format!("Resets {}", local.format("%b %-d, %-I:%M %p"))
            })
        },
    )
}

fn unix_day() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400
}
