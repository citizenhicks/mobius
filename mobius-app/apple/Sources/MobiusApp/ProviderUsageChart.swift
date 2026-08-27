import Charts
import Foundation
import SwiftUI

enum UsageAggregation: String, CaseIterable, Identifiable {
    case daily
    case weekly
    case cumulative

    var id: Self { self }

    var title: String {
        rawValue.capitalized
    }
}

struct ProviderUsagePoint: Equatable, Identifiable {
    var id: String { "\(provider):\(unixDay)" }
    let unixDay: UInt64
    let provider: String
    let providerLabel: String
    let totalTokens: Int

    var date: Date {
        Date(timeIntervalSince1970: TimeInterval(unixDay) * 86_400)
    }
}

struct UsageActivitySnapshot: Equatable {
    let values: [Int]
    let maximum: Int
    let activeDays: Int
    let totalTokens: Int
}

enum UsageActivitySeries {
    static func snapshot(
        from usage: [DailyUsage],
        endingOn endDay: UInt64,
        weekCount: Int,
        aggregation: UsageAggregation
    ) -> UsageActivitySnapshot {
        guard weekCount > 0 else {
            return UsageActivitySnapshot(values: [], maximum: 1, activeDays: 0, totalTokens: 0)
        }
        let totals = usage.reduce(into: [UInt64: Int]()) { result, day in
            guard !day.provider.isEmpty else { return }
            result[day.unixDay, default: 0] += day.usage.totalTokens
        }
        let dayCount = weekCount * 7
        let startDay = endDay - min(endDay, UInt64(dayCount - 1))
        let dailyValues = (0..<dayCount).map { offset in
            let day = startDay + UInt64(offset)
            return day <= endDay ? totals[day] ?? 0 : 0
        }

        let values: [Int]
        switch aggregation {
        case .daily:
            values = dailyValues
        case .weekly:
            values = dailyValues.enumerated().map { index, _ in
                let weekStart = (index / 7) * 7
                let weekEnd = min(weekStart + 7, dailyValues.count)
                return dailyValues[weekStart..<weekEnd].reduce(0, +)
            }
        case .cumulative:
            var runningTotal = 0
            values = dailyValues.enumerated().map { index, value in
                guard startDay + UInt64(index) <= endDay else { return 0 }
                runningTotal += value
                return runningTotal
            }
        }

        return UsageActivitySnapshot(
            values: values,
            maximum: max(values.max() ?? 0, 1),
            activeDays: dailyValues.filter { $0 > 0 }.count,
            totalTokens: dailyValues.reduce(0, +)
        )
    }
}

enum ProviderUsageSeries {
    static func points(
        from usage: [DailyUsage],
        endingOn endDay: UInt64,
        dayCount: Int,
        providerLabels: [String: String],
        aggregation: UsageAggregation = .daily
    ) -> [ProviderUsagePoint] {
        guard dayCount > 0 else { return [] }
        let requestedSpan = UInt64(dayCount - 1)
        let startDay = endDay - min(endDay, requestedSpan)
        let inRange = usage.filter {
            $0.unixDay >= startDay && $0.unixDay <= endDay && !$0.provider.isEmpty
        }
        let providers = Set(inRange.map(\.provider)).sorted {
            let left = providerLabels[$0] ?? $0
            let right = providerLabels[$1] ?? $1
            let comparison = left.localizedStandardCompare(right)
            return comparison == .orderedSame ? $0 < $1 : comparison == .orderedAscending
        }
        let totals = inRange.reduce(into: [ProviderDay: Int]()) { result, day in
            result[ProviderDay(provider: day.provider, unixDay: day.unixDay), default: 0]
                += day.usage.totalTokens
        }
        let actualDayCount = Int(endDay - startDay) + 1
        let buckets: [(unixDay: UInt64, offsets: Range<Int>)] = switch aggregation {
        case .daily:
            (0..<actualDayCount).map { offset in
                (startDay + UInt64(offset), offset..<(offset + 1))
            }
        case .weekly:
            stride(from: 0, to: actualDayCount, by: 7).map { offset in
                let end = min(offset + 7, actualDayCount)
                return (startDay + UInt64(offset), offset..<end)
            }
        case .cumulative:
            [(endDay, 0..<actualDayCount)]
        }

        return providers.flatMap { provider in
            buckets.map { bucket in
                let totalTokens = bucket.offsets.reduce(into: 0) { result, offset in
                    let unixDay = startDay + UInt64(offset)
                    result += totals[ProviderDay(provider: provider, unixDay: unixDay)] ?? 0
                }
                return ProviderUsagePoint(
                    unixDay: bucket.unixDay,
                    provider: provider,
                    providerLabel: providerLabels[provider] ?? provider,
                    totalTokens: totalTokens
                )
            }
        }
    }

    private struct ProviderDay: Hashable {
        let provider: String
        let unixDay: UInt64
    }
}

struct ProviderUsageChart: View {
    @Environment(\.mobiusPalette) private var palette
    let usage: [DailyUsage]
    let providerLabels: [String: String]
    let providerTints: [String: AccentTint]
    var weekCount = 25
    var aggregation: UsageAggregation = .daily

    var body: some View {
        let points = ProviderUsageSeries.points(
            from: usage,
            endingOn: UInt64(Date.now.timeIntervalSince1970 / 86_400),
            dayCount: weekCount * 7,
            providerLabels: providerLabels,
            aggregation: aggregation
        )
        if points.contains(where: { $0.totalTokens > 0 }) {
            chart(points)
        } else {
            ContentUnavailableView(
                "No usage yet",
                systemImage: "chart.bar.xaxis",
                description: Text("Provider activity will appear after the first model call.")
            )
            .frame(maxWidth: .infinity, minHeight: 190)
        }
    }

    @ViewBuilder
    private func chart(_ points: [ProviderUsagePoint]) -> some View {
        if aggregation == .cumulative {
            cumulativeChart(points)
        } else {
            timelineChart(points)
        }
    }

    private func timelineChart(_ points: [ProviderUsagePoint]) -> some View {
        let scale = providerStyleScale(points)
        return Chart(points) { point in
            BarMark(
                x: .value("Date", point.date),
                y: .value("Tokens", point.totalTokens)
            )
            .foregroundStyle(by: .value("Provider", point.providerLabel))
        }
        .chartForegroundStyleScale(domain: scale.domain, range: scale.range)
        .chartXAxis {
            AxisMarks(values: .stride(by: aggregation == .daily ? .month : .weekOfYear)) { _ in
                AxisGridLine().foregroundStyle(palette.line.opacity(0.3))
                AxisTick().foregroundStyle(palette.line)
                AxisValueLabel(format: .dateTime.month(.abbreviated).day())
                    .foregroundStyle(palette.muted)
            }
        }
        .chartYAxis {
            AxisMarks(position: .leading, values: .automatic(desiredCount: 4)) { value in
                AxisGridLine().foregroundStyle(palette.line.opacity(0.3))
                AxisValueLabel {
                    if let tokens = value.as(Int.self) {
                        Text(chartCompact(tokens))
                    }
                }
                .foregroundStyle(palette.muted)
            }
        }
        .chartLegend(position: .bottom, alignment: .leading, spacing: MobiusSpace.m)
        .chartPlotStyle { plot in
            plot
                .background(palette.line.opacity(0.08))
                .clipShape(MobiusStyle.controlShape)
        }
        .frame(height: 220)
        .accessibilityLabel("\(aggregation.title) token usage by provider")
    }

    private func cumulativeChart(_ points: [ProviderUsagePoint]) -> some View {
        let scale = providerStyleScale(points)
        return Chart(points) { point in
            BarMark(
                x: .value("Tokens", point.totalTokens),
                y: .value("Provider", point.providerLabel)
            )
            .foregroundStyle(by: .value("Provider", point.providerLabel))
        }
        .chartForegroundStyleScale(domain: scale.domain, range: scale.range)
        .chartXAxis {
            AxisMarks(position: .bottom, values: .automatic(desiredCount: 4)) { value in
                AxisGridLine().foregroundStyle(palette.line.opacity(0.3))
                AxisTick().foregroundStyle(palette.line)
                AxisValueLabel {
                    if let tokens = value.as(Int.self) {
                        Text(chartCompact(tokens))
                    }
                }
                .foregroundStyle(palette.muted)
            }
        }
        .chartYAxis {
            AxisMarks(position: .leading) { _ in
                AxisGridLine().foregroundStyle(palette.line.opacity(0.3))
                AxisValueLabel().foregroundStyle(palette.muted)
            }
        }
        .chartLegend(.hidden)
        .chartPlotStyle { plot in
            plot
                .background(palette.line.opacity(0.08))
                .clipShape(MobiusStyle.controlShape)
        }
        .frame(height: max(180, CGFloat(points.count) * 42))
        .accessibilityLabel("Cumulative token usage by provider")
    }

    private func providerStyleScale(
        _ points: [ProviderUsagePoint]
    ) -> (domain: [String], range: [Color]) {
        var seen = Set<String>()
        let providers = points.filter { seen.insert($0.providerLabel).inserted }
        return (
            providers.map(\.providerLabel),
            providers.map { (providerTints[$0.provider] ?? .appDefault).color }
        )
    }
}

private func chartCompact(_ value: Int) -> String {
    value.formatted(.number.notation(.compactName).precision(.fractionLength(0 ... 1)))
}
