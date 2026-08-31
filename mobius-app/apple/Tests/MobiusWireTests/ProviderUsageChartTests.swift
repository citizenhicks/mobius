import XCTest

final class ProviderUsageChartTests: XCTestCase {
    func testBuildsTopThreeProviderTotalsForTheVisibleRange() {
        let totals = ProviderUsageTotal.top(
            from: [
                usage(day: 4, provider: "openai", tokens: 20),
                usage(day: 4, provider: "openai", tokens: 5),
                usage(day: 5, provider: "anthropic", tokens: 30),
                usage(day: 6, provider: "google", tokens: 10),
                usage(day: 7, provider: "xai", tokens: 5),
                usage(day: 1, provider: "ignored-past", tokens: 999),
                usage(day: 12, provider: "ignored-future", tokens: 999),
            ],
            endingOn: 11,
            weekCount: 2
        )

        XCTAssertEqual(totals.map(\.provider), [
            "anthropic", "openai", "google"])
        XCTAssertEqual(totals.map(\.totalTokens), [30, 25, 10])
    }

    func testAlignsDailyActivityToMondayAndLeavesFutureDaysEmpty() {
        let usage = [
            usage(day: 4, provider: "openai", tokens: 2),
            usage(day: 10, provider: "openai", tokens: 5),
            usage(day: 11, provider: "openai", tokens: 3),
            usage(day: 12, provider: "openai", tokens: 99),
        ]

        let daily = UsageActivitySeries.snapshot(
            from: usage,
            endingOn: 11,
            weekCount: 2,
            aggregation: .daily
        )

        XCTAssertEqual(daily.values, [2, 0, 0, 0, 0, 0, 5, 3, 0, 0, 0, 0, 0, 0])
        XCTAssertEqual(daily.maximum, 5)
        XCTAssertEqual(daily.activeDays, 3)
        XCTAssertEqual(daily.totalTokens, 10)
    }

    func testBuildsReferenceWeeklyAndCumulativeBars() {
        let usage = [
            usage(day: 4, provider: "openai", tokens: 10),
            usage(day: 11, provider: "openai", tokens: 25),
            usage(day: 18, provider: "openai", tokens: 50),
            usage(day: 25, provider: "openai", tokens: 100),
        ]

        let weekly = UsageActivitySeries.snapshot(
            from: usage,
            endingOn: 31,
            weekCount: 4,
            aggregation: .weekly
        )
        XCTAssertEqual(weekly.values,
            [
                0, 0, 0, 0, 0, 0, 10,
                0, 0, 0, 0, 0, 25, 25,
                0, 0, 0, 50, 50, 50, 50,
                100, 100, 100, 100, 100, 100, 100,
            ])
        XCTAssertEqual(weekly.maximum, 100)
        XCTAssertEqual(
            [0, 1, 2, 3, 4],
            [0, 10, 50, 75, 100].map(weekly.activityLevel)
        )

        let cumulative = UsageActivitySeries.snapshot(
            from: usage,
            endingOn: 31,
            weekCount: 4,
            aggregation: .cumulative
        )
        XCTAssertEqual(cumulative.values, [
                0, 0, 0, 0, 0, 0, 10,
                0, 0, 0, 0, 0, 35, 35,
                0, 0, 0, 85, 85, 85, 85,
                185, 185, 185, 185, 185, 185, 185,
            ])
        XCTAssertEqual(cumulative.maximum, 185)
        XCTAssertEqual(cumulative.activeDays, 4)
        XCTAssertEqual(cumulative.totalTokens, 185)
    }

    private func usage(day: UInt64, provider: String, tokens: Int) -> DailyUsage {
        var usage = TokenUsage()
        usage.totalTokens = tokens
        return DailyUsage(unixDay: day, provider: provider, usage: usage)
    }
}
