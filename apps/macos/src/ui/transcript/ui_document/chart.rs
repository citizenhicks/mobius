//! Charts in an interface document, drawn with gpui-component's charts: bars, lines and
//! soft-filled areas on a round value scale, and a donut beside its legend. Data marks take
//! the accent, later series a step further toward the card; axes are neutral Caption text.

use super::*;
use crate::numeric::{Saturating, ToFloat};
use gpui_component::chart::{AreaChart, BarChart, PieChart};
use mobius::protocol::{UiChartKind, UiSeries};

/// The plot's height; gridlines sit half a caption line in from its edges so the axis
/// labels centre on them.
const PLOT: Pixels = px(176.);
const DONUT: Pixels = px(160.);
/// A donut legend's share column, wide enough for "100%".
const SHARE: Pixels = px(40.);
/// Category labels shown before they thin to every n-th.
const MAX_LABELS: usize = 12;
/// The gap between neighbouring bars, as a share of a band.
const BAR_GAP: f32 = 0.6;
/// The space `BarChart` keeps clear above its tallest bar when bars carry no labels.
const BAR_HEADROOM: Pixels = px(10.);

/// Series colour `index` of `count`: the accent, then mixed toward the card in even steps.
fn mark(p: &Palette, index: usize, count: usize) -> Hsla {
    if index == 0 {
        p.accent
    } else {
        theme::mix(p.accent, p.card, 0.7 * index.to_f32() / count.to_f32())
    }
}

/// "1.2K", "40", "2.5", with the unit after a space unless it is a symbol like `%`.
fn figure(value: f64, unit: Option<&str>) -> String {
    let number = if value.fract() == 0. && (1000.0..1e15).contains(&value.abs()) {
        compact_count(value.saturating_i64())
    } else if value.abs() >= 1e15 || (value != 0. && value.abs() < 0.01) {
        format!("{value:.2e}")
    } else if value.fract() == 0. {
        format!("{value:.0}")
    } else {
        let text = format!("{value:.2}");
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    };
    match unit.filter(|unit| !unit.is_empty()) {
        Some(unit) if unit.chars().all(|c| !c.is_alphanumeric()) => format!("{number}{unit}"),
        Some(unit) => format!("{number} {unit}"),
        None => number,
    }
}

/// A 1-2-5 step that splits `span` into about three intervals, and the ticks covering
/// `low..=high` with it.
fn scale(low: f64, high: f64) -> Vec<f64> {
    if high <= low {
        return vec![0., 0.5, 1.];
    }
    // Divide first: a valid range can span both ends of the finite f64 domain.
    let rough = (high / 3. - low / 3.).max(f64::MIN_POSITIVE);
    let magnitude = 10f64.powf(rough.log10().floor());
    let step = [1., 2., 2.5, 5., 10.]
        .into_iter()
        .map(|factor| factor * magnitude)
        .find(|step| step.is_finite() && *step >= rough)
        .unwrap_or(magnitude);
    let first = (low / step).floor().saturating_i64();
    let last = (high / step).ceil().saturating_i64();
    let mut ticks: Vec<_> = (first..=last.max(first + 1))
        .map(|tick| tick.to_f64() * step)
        .filter(|tick| tick.is_finite())
        .collect();
    if ticks.first().is_none_or(|tick| *tick > low) {
        ticks.insert(0, low);
    }
    if ticks.last().is_none_or(|tick| *tick < high) {
        ticks.push(high);
    }
    ticks
}

fn values(series: &UiSeries) -> Vec<f64> {
    series
        .values
        .iter()
        .map(|value| value.as_f64().unwrap_or(0.))
        .collect()
}

fn legend<'a>(p: &Palette, names: impl Iterator<Item = (Hsla, &'a str)>) -> Div {
    div()
        .flex()
        .flex_wrap()
        .gap_x(space::L)
        .gap_y(space::XS)
        .children(names.map(|(color, name)| {
            div()
                .flex()
                .items_center()
                .gap(space::S)
                .typography(Type::Caption)
                .text_color(p.muted)
                .child(div().size(glyph::DOT).pill_corners().bg(color))
                .child(name.to_owned())
        }))
}

pub(super) fn chart(
    id: &str,
    p: &Palette,
    kind: UiChartKind,
    title: Option<&str>,
    labels: &[String],
    series: &[UiSeries],
    unit: Option<&str>,
) -> AnyElement {
    let body = if kind == UiChartKind::Pie {
        donut(p, labels, series.first(), unit)
    } else {
        cartesian(p, kind, labels, series, unit)
    };
    let description = series
        .iter()
        .map(|series| {
            let points = labels
                .iter()
                .zip(&series.values)
                .map(|(label, value)| format!("{label}: {value} {}", unit.unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}: {points}", series.name)
        })
        .collect::<Vec<_>>()
        .join("; ");
    div()
        .id(SharedString::from(id.to_owned()))
        .role(gpui::accesskit::Role::Image)
        .aria_label(description)
        .flex()
        .flex_col()
        .gap(space::S)
        .children(title.map(|title| {
            div()
                .typography(Type::Subhead)
                .text_color(p.text)
                .child(title.to_owned())
        }))
        .child(body)
        .into_any_element()
}

/// One category's values across the series.
struct Row {
    label: SharedString,
    values: Vec<f64>,
}

/// One bar, or an empty band between groups of several series.
struct Bar {
    band: usize,
    value: f64,
    color: Hsla,
}

/// The chart draws the marks; the value scale, its gridlines and the category labels are
/// ours, so they keep the app's caption type.
fn cartesian(
    p: &Palette,
    kind: UiChartKind,
    labels: &[String],
    series: &[UiSeries],
    unit: Option<&str>,
) -> Div {
    let data: Vec<Vec<f64>> = series.iter().map(values).collect();
    let (low, high) = data
        .iter()
        .flatten()
        .fold((0f64, 0f64), |(low, high), value| {
            (low.min(*value), high.max(*value))
        });
    let ticks = scale(low, high);
    let (bottom, top) = match (ticks.first(), ticks.last()) {
        (Some(&bottom), Some(&top)) => (bottom, top),
        _ => (0., 1.),
    };
    let colors: Vec<Hsla> = (0..data.len()).map(|i| mark(p, i, data.len())).collect();
    let inset = Type::Caption.line_height() / 2.;
    let key = (series.len() > 1).then(|| {
        legend(
            p,
            colors
                .iter()
                .copied()
                .zip(series.iter().map(|series| series.name.as_str())),
        )
    });
    let unit_owned = unit.map(str::to_owned);
    let (marks, names) = if kind == UiChartKind::Bar {
        let (marks, edge) = bar_marks(labels, &data, &colors, (bottom, top), unit_owned);
        (marks, category_axis(p, labels, relative(edge)))
    } else {
        let marks = point_marks(
            kind,
            labels,
            series,
            &data,
            &colors,
            (bottom, top),
            unit_owned,
        );
        (marks, category_axis(p, labels, space::L))
    };
    let grid = div()
        .absolute()
        .inset_0()
        .flex()
        .flex_col()
        .justify_between()
        .children(ticks.iter().map(|_| hairline(p)));
    let plot = div()
        .h(PLOT)
        .py(inset)
        .child(div().relative().size_full().child(grid).child(marks));
    div().flex().flex_col().gap(space::S).children(key).child(
        div()
            .flex()
            .gap(space::S)
            .child(value_axis(p, &ticks, unit))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(space::XS)
                    .child(plot)
                    .child(names),
            ),
    )
}

/// Bars of every series, grouped by label and scaled so `bottom..=top` spans the plot, and
/// how far in from either side the first and last label sit, as a share of the width.
fn bar_marks(
    labels: &[String],
    data: &[Vec<f64>],
    colors: &[Hsla],
    (bottom, top): (f64, f64),
    tooltip_unit: Option<String>,
) -> (Div, f32) {
    let inset = Type::Caption.line_height() / 2.;
    let groups = data.len().max(1);
    // Several series group their bars, an empty band apart.
    let stride = if groups > 1 { groups + 1 } else { 1 };
    let bars: Vec<Bar> = (0..labels.len() * stride)
        .map(|band| Bar {
            band,
            value: data
                .get(band % stride)
                .and_then(|values| values.get(band / stride))
                .copied()
                .unwrap_or(0.),
            color: colors
                .get(band % stride)
                .copied()
                .unwrap_or(gpui::transparent_black()),
        })
        .take((labels.len() * stride).saturating_sub(stride - groups))
        .collect();
    let names: Vec<SharedString> = labels.iter().map(|label| label.as_str().into()).collect();
    let count = bars.len().max(2).to_f32();
    // Bars fill the plot edge to edge, each `1 - BAR_GAP` of a band wide; a group's
    // label centres under its middle bar.
    let edge = ((1. - BAR_GAP) / 2.
        + (groups - 1).to_f32() / 2. * (count - 1. + BAR_GAP) / (count - 1.))
        / count;
    let (lo, hi) = bars.iter().fold((0f64, 0f64), |(lo, hi), bar| {
        (lo.min(bar.value), hi.max(bar.value))
    });
    let height = PLOT - inset * 2.;
    let y = |value: f64| height * ((top / 2. - value / 2.) / (top / 2. - bottom / 2.)).to_f32();
    let chart = BarChart::new(bars)
        .band(|bar: &Bar| bar.band.to_string())
        .value(|bar: &Bar| bar.value)
        .fill(|bar: &Bar, _, _, _| bar.color)
        .label_axis(false)
        .grid(false)
        .appear(false)
        .interactive(groups == 1)
        .padding_inner(BAR_GAP)
        .padding_outer(0.)
        .max_band_width(px(f32::MAX))
        .corner_radii(Corners {
            top_left: radius::CELL,
            top_right: radius::CELL,
            ..Corners::default()
        })
        // The tooltip hands back the label it shows; SharedString clones are a refcount.
        .tooltip_title(move |bar: &Bar| {
            if let Some(name) = names.get(bar.band / stride) {
                SharedString::clone(name)
            } else {
                SharedString::default()
            }
        })
        .tooltip_value(move |_, value| figure(value, tooltip_unit.as_deref()).into());
    let marks = div()
        .absolute()
        .left_0()
        .right_0()
        .top(y(hi) - BAR_HEADROOM)
        .h(y(lo) - y(hi) + BAR_HEADROOM)
        .child(chart);
    (marks, edge)
}

/// Lines, or areas filled soft toward the floor, pinned to `bottom..=top`.
fn point_marks(
    kind: UiChartKind,
    labels: &[String],
    series: &[UiSeries],
    data: &[Vec<f64>],
    colors: &[Hsla],
    (bottom, top): (f64, f64),
    tooltip_unit: Option<String>,
) -> Div {
    let rows = labels.iter().enumerate().map(|(index, label)| Row {
        label: label.as_str().into(),
        values: data
            .iter()
            .map(|values| values.get(index).copied().unwrap_or(0.))
            .collect(),
    });
    let mut chart = AreaChart::new(rows)
        // The tooltip titles each point with its label; SharedString clones are a refcount.
        .x(|row: &Row| SharedString::clone(&row.label))
        .x_axis(false)
        .grid(false)
        .appear(false)
        .y_domain(bottom, top)
        .y_padding(0., 0.)
        .tooltip_value(move |_, _, value| figure(value, tooltip_unit.as_deref()).into());
    for (index, (series, color)) in series.iter().zip(colors).enumerate() {
        let fill: Background = if kind == UiChartKind::Area {
            linear_gradient(
                180.,
                linear_color_stop(color.opacity(0.28), 0.),
                linear_color_stop(color.opacity(0.04), 1.),
            )
        } else {
            gpui::transparent_black().into()
        };
        chart = chart
            .y(move |row: &Row| row.values.get(index).copied().unwrap_or(0.))
            .stroke(*color)
            .fill(fill)
            .linear()
            .name(series.name.as_str());
    }
    div().absolute().inset_0().px(space::L).child(chart)
}

/// The value scale, top to bottom, each tick centred on its gridline.
fn value_axis(p: &Palette, ticks: &[f64], unit: Option<&str>) -> Div {
    div()
        .h(PLOT)
        .flex()
        .flex_col()
        .justify_between()
        .items_end()
        .flex_shrink_0()
        .typography(Type::Caption)
        .text_color(p.muted)
        .children(
            ticks
                .iter()
                .rev()
                .map(|tick| div().child(figure(*tick, unit))),
        )
}

/// Category labels centred on evenly spaced marks `edge` in from either side, thinned to
/// every n-th when crowded.
fn category_axis(p: &Palette, labels: &[String], edge: impl Into<DefiniteLength>) -> Div {
    let edge = edge.into();
    let every = labels.len().div_ceil(MAX_LABELS).max(1);
    div()
        .flex()
        .justify_between()
        .when(labels.len() == 1, gpui::Styled::justify_center)
        .pl(edge)
        .pr(edge)
        .typography(Type::Caption)
        .text_color(p.muted)
        .children(labels.iter().enumerate().map(|(index, label)| {
            div()
                .w(px(0.))
                .flex()
                .justify_center()
                .whitespace_nowrap()
                .when(index % every == 0, |cell| cell.child(label.to_owned()))
        }))
}

/// A donut of the first series beside a legend of each slice's value and share.
fn donut(p: &Palette, labels: &[String], series: Option<&UiSeries>, unit: Option<&str>) -> Div {
    let data: Vec<f64> = series
        .map(values)
        .unwrap_or_default()
        .into_iter()
        .map(|value| value.max(0.))
        .collect();
    let largest = data
        .iter()
        .copied()
        .fold(0f64, f64::max)
        .max(f64::MIN_POSITIVE);
    let total: f64 = data.iter().map(|value| value / largest).sum();
    let share = |value: f64| {
        if total > 0. {
            value / largest / total * 100.
        } else {
            0.
        }
    };
    let colors: Vec<Hsla> = (0..data.len()).map(|i| mark(p, i, data.len())).collect();
    let slices: Vec<(SharedString, f64, Hsla)> = labels
        .iter()
        .zip(&data)
        .zip(&colors)
        .map(|((label, value), color)| (label.as_str().into(), *value, *color))
        .collect();
    let tooltip_unit = unit.map(str::to_owned);
    // The hovered slice lifts out by 6px, so the ring leaves room for it.
    let outer = DONUT.as_f32() / 2. - 6.;
    let ring = PieChart::new(slices)
        .value(move |slice: &(SharedString, f64, Hsla)| (slice.1 / largest).to_f32())
        .color(|slice: &(SharedString, f64, Hsla)| slice.2)
        .outer_radius(outer)
        .inner_radius(outer * 0.62)
        .pad_angle(0.02)
        .appear(false)
        // The tooltip names each slice by its label; SharedString clones are a refcount.
        .tooltip_name(|slice| SharedString::clone(&slice.0))
        .tooltip_value(move |slice, _, _| figure(slice.1, tooltip_unit.as_deref()).into());
    let percent = unit == Some("%");
    let rows = labels
        .iter()
        .zip(&data)
        .zip(&colors)
        .map(|((label, value), color)| {
            div()
                .flex()
                .items_center()
                .gap(space::S)
                .py(space::XS)
                .border_b_1()
                .border_color(p.separator)
                .child(div().size(glyph::DOT).pill_corners().bg(*color))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .typography(Type::Body)
                        .text_color(p.text)
                        .child(label.to_owned()),
                )
                .child(
                    div()
                        .typography(Type::Mono)
                        .text_color(p.text)
                        .child(figure(*value, unit)),
                )
                // A value already in percent is its own share.
                .when(!percent, |row| {
                    row.child(
                        div()
                            .w(SHARE)
                            .flex()
                            .justify_end()
                            .typography(Type::Mono)
                            .text_color(p.muted)
                            .child(format!("{:.0}%", share(*value))),
                    )
                })
        });
    div()
        .flex()
        .items_center()
        .gap(space::L)
        .child(div().size(DONUT).flex_shrink_0().child(ring))
        .child(div().flex_1().min_w_0().flex().flex_col().children(rows))
}

#[cfg(test)]
mod tests {
    use super::{figure, scale};

    #[test]
    fn scale_covers_values_in_round_steps() {
        assert_eq!(scale(0., 47.), [0., 20., 40., 60.]);
        assert_eq!(scale(0., 11.), [0., 5., 10., 15.]);
        assert_eq!(figure(2500., Some("m")), "2.5K m");
        assert_eq!(figure(40., Some("%")), "40%");
        assert_eq!(figure(0.04, None), "0.04");
        assert!(
            scale(-f64::MAX, f64::MAX)
                .iter()
                .all(|tick| tick.is_finite())
        );
        assert!(figure(1e300, None).contains("e300"));
        assert!(super::super::numeric("$1,999"));
        assert!(!super::super::numeric("M5 Pro"));
    }
}
