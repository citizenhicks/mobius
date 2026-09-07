import Foundation

enum UsageAggregation: String, CaseIterable, Identifiable {
    case daily
    case weekly
    case cumulative

    var id: Self { self }

    var title: LocalizedStringResource {
        switch self {
        case .daily: "Daily"
        case .weekly: "Weekly"
        case .cumulative: "Cumulative"
        }
    }
}

struct ProviderUsageTotal: Equatable, Identifiable {
    var id: String { provider }
    let provider: String
    let totalTokens: Int

    static func top(
        from usage: [DailyUsage],
        endingOn endDay: UInt64,
        weekCount: Int,
        limit: Int = 3
    ) -> [ProviderUsageTotal] {
        guard weekCount > 0, limit > 0 else { return [] }
        let startDay = UsageActivitySeries.startDay(endingOn: endDay, weekCount: weekCount)
        let totals = usage.reduce(into: [String: Int]()) { result, day in
            guard !day.provider.isEmpty,
                day.unixDay >= startDay,
                day.unixDay <= endDay
            else { return }
            result[day.provider, default: 0] += max(day.usage.totalTokens, 0)
        }
        var ranked = totals.map {
            ProviderUsageTotal(provider: $0.key, totalTokens: $0.value)
        }.filter { $0.totalTokens > 0 }
        ranked.sort {
            if $0.totalTokens == $1.totalTokens { return $0.provider < $1.provider }
            return $0.totalTokens > $1.totalTokens
        }
        return Array(ranked.prefix(limit))
    }
}

struct UsageActivitySnapshot: Equatable {
    let values: [Int]
    let maximum: Int
    let activeDays: Int
    let totalTokens: Int

    func activityLevel(_ value: Int) -> Int {
        guard value > 0 else { return 0 }
        return min(4, Int(ceil(Double(value) / Double(maximum) * 4)))
    }
}

enum UsageActivitySeries {
    static func startDay(endingOn endDay: UInt64, weekCount: Int) -> UInt64 {
        guard weekCount > 0 else { return endDay }
        let daysSinceMonday = (endDay % 7 + 3) % 7
        let currentWeekStart = endDay - daysSinceMonday
        let precedingDays = UInt64((weekCount - 1) * 7)
        return currentWeekStart - min(currentWeekStart, precedingDays)
    }

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
            result[day.unixDay, default: 0] += max(day.usage.totalTokens, 0)
        }
        let dayCount = weekCount * 7
        let startDay = startDay(endingOn: endDay, weekCount: weekCount)
        let dailyValues = (0..<dayCount).map { offset in
            let day = startDay + UInt64(offset)
            return day <= endDay ? totals[day] ?? 0 : 0
        }

        let weeklyTotals = stride(from: 0, to: dayCount, by: 7).map { weekStart in
            dailyValues[weekStart..<(weekStart + 7)].reduce(0, +)
        }

        let values: [Int]
        let maximum: Int
        switch aggregation {
        case .daily:
            values = dailyValues
            maximum = max(dailyValues.max() ?? 0, 1)
        case .weekly:
            maximum = max(weeklyTotals.max() ?? 0, 1)
            values = barValues(for: weeklyTotals, maximum: maximum)
        case .cumulative:
            var runningTotal = 0
            let cumulativeTotals = weeklyTotals.map { value in
                runningTotal += value
                return runningTotal
            }
            maximum = max(cumulativeTotals.last ?? 0, 1)
            values = barValues(for: cumulativeTotals, maximum: maximum)
        }

        return UsageActivitySnapshot(
            values: values,
            maximum: maximum,
            activeDays: dailyValues.filter { $0 > 0 }.count,
            totalTokens: dailyValues.reduce(0, +)
        )
    }

    private static func barValues(for totals: [Int], maximum: Int) -> [Int] {
        totals.flatMap { total in
            let height =
                total > 0
                ? min(7, Int(ceil(Double(total) / Double(maximum) * 7)))
                : 0
            return (0..<7).map { row in row >= 7 - height ? total : 0 }
        }
    }
}
