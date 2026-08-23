import Foundation
import StoreKit
import SwiftUI

struct ProfileView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let usage = model.profile?.dailyUsage ?? []
        let providerLabels = model.providerInstances.reduce(into: [String: String]()) {
            $0[$1.instance] = $1.label
        }
        PageScaffold(
            title: "Settings",
            detail: "",
            headerAccessory: SettingsInformationButton.init
        ) {
            // Settings first, the dashboard last: this page is opened to change something,
            // and usage is the one section here nobody comes to act on.
            Section {
                CloudAccountSettings()
                    .task(id: model.cloudSession?.userID) {
                        await model.refreshCloudAccount()
                    }
            } header: {
                HStack(spacing: MobiusSpace.xs) {
                    MobiusCloudLabel(showsAccount: true)
                        .textCase(nil)
                    if model.isLoadingCloudAccount {
                        MobiusSpinner(size: MobiusStyle.glyphMark)
                    }
                }
            }
            .listRowSeparator(.hidden)
            Section("Appearance") {
                AppearanceSettings()
            }
            .listRowSeparator(.hidden)
            Section("Security") {
                AppLockSettings()
            }
            .listRowSeparator(.hidden)
            Section("Usage") {
                ProfileUsageSection(days: usage)
                DisclosureGroup("Usage history") {
                    ProfileUsageHistory(days: usage, providerLabels: providerLabels)
                }
                if let stats = model.profile?.runStats {
                    DisclosureGroup("Run activity") {
                        ProfileRunStatsSection(stats: stats)
                        ProfileRecentRuns(groups: model.profile?.recentRunGroups ?? [])
                    }
                }
            }
            .listRowSeparator(.hidden)
        }
        .task(id: model.connectionState.isReady) { model.refreshProfile() }
    }
}

private struct CloudAgentUsageLimit: View {
    @Environment(\.mobiusPalette) private var palette
    let limit: MobiusCloudUsageLimit

    private static let resetFormat = Date.FormatStyle(
        date: .abbreviated,
        time: .omitted,
        timeZone: .gmt
    )

    var body: some View {
        let percentage = limit.remainingFraction.formatted(
            .percent.precision(.fractionLength(0))
        )
        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            HStack(alignment: .firstTextBaseline) {
                Text("möbius cloud agent usage limits")
                Spacer(minLength: MobiusSpace.s)
                Text("\(percentage) remaining")
                    .monospacedDigit()
            }
            .font(MobiusStyle.controlFont)
            .accessibilityHidden(true)
            ProgressView(value: limit.remainingFraction)
                .progressViewStyle(.linear)
                .tint(palette.accent)
                .accessibilityLabel("möbius Cloud agent usage limits")
                .accessibilityValue("\(percentage) remaining")
            Text("Resets \(limit.resetsAt.formatted(Self.resetFormat))")
                .font(MobiusStyle.metadataFont)
                .foregroundStyle(palette.muted)
        }
    }
}

private struct SettingsInformationButton: View {
    @Environment(AppModel.self) private var model
    @State private var showsInformation = false

    var body: some View {
        Button {
            showsInformation = true
        } label: {
            MobiusIcon(.info, size: MobiusStyle.glyphInline)
        }
        .mobiusIconButton()
        .accessibilityLabel("About möbius")
        .accessibilityHint("Shows version, legal, and support information")
        .help("About möbius")
        .popover(
            isPresented: $showsInformation,
            attachmentAnchor: .rect(.bounds),
            arrowEdge: .top
        ) {
            VStack(alignment: .leading, spacing: 0) {
                Text(versionDescription)
                    .font(MobiusStyle.controlFont)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.bottom, MobiusSpace.l)
                Divider()
                SettingsInformationRow(
                    title: "Acceptable Use Policy",
                    glyph: .shieldCheck,
                    action: { showPlaceholder("Acceptable Use Policy") }
                )
                SettingsInformationRow(
                    title: "Terms of Service",
                    glyph: .doc,
                    action: { showPlaceholder("Terms of Service") }
                )
                SettingsInformationRow(
                    title: "Privacy Policy",
                    glyph: .shield02,
                    action: { showPlaceholder("Privacy Policy") }
                )
                SettingsInformationRow(
                    title: "Licenses",
                    glyph: .fileText,
                    action: { showPlaceholder("Licenses") }
                )
                Divider()
                SettingsInformationRow(
                    title: "Help & Support",
                    glyph: .question,
                    action: { showPlaceholder("Help & Support") }
                )
            }
            .padding(MobiusSpace.l)
            .frame(width: 320, alignment: .leading)
            .presentationCompactAdaptation(.popover)
        }
    }

    private var versionDescription: String {
        let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString")
            as? String ?? "—"
        let build = Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "—"
        return "möbius v\(version) (\(build))"
    }

    private func showPlaceholder(_ title: String) {
        showsInformation = false
        model.showToast("\(title) will be available before the cloud release.")
    }
}

private struct SettingsInformationRow: View {
    @Environment(\.mobiusPalette) private var palette
    let title: String
    let glyph: MobiusGlyph
    let action: @MainActor () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: MobiusSpace.m) {
                MobiusIcon(glyph, size: MobiusStyle.glyphLead, foreground: palette.muted)
                Text(title)
                Spacer(minLength: MobiusSpace.m)
                MobiusIcon(.arrowUpRight01, size: MobiusStyle.glyphMark, foreground: palette.muted)
            }
            .frame(maxWidth: .infinity, minHeight: MobiusStyle.iconButtonSize)
            .contentShape(Rectangle())
        }
        .buttonStyle(.mobiusPlain)
    }
}

private struct CloudAccountSettings: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var showsSubscriptionManagement = false
    @State private var confirmsSignOut = false
    @State private var confirmsAccountDeletion = false
    @State private var showsActiveSubscriptionWarning = false
    @State private var showsAccountDeletionAuthentication = false

    /// Signed out, this is an offer; signed in, it is an account. Account actions stay absent
    /// until there is a Cloud session to authenticate them.
    var body: some View {
        if model.isLoadingCloudAccount {
            SettingsLoadingRows(label: "Loading Cloud account") {
                LabeledContent("Email") { Text("account@example.com") }
                Toggle("Help improve möbius", isOn: .constant(false))
                LabeledContent("Subscriber since") { Text("August 2026") }
            }
        } else if model.hasCloudAccount {
            if let cloudError = model.cloudError {
                StatusBanner(
                    tone: .error,
                    title: "Cloud account unavailable",
                    detail: cloudError,
                    action: ("Retry", { Task { await model.refreshCloudAccount() } })
                )
            }
            LabeledContent("Email") {
                if let email = model.cloudAccount?.email {
                    Text(verbatim: email)
                } else {
                    Text("Unavailable")
                }
            }
            if model.cloudAccount != nil {
                // The info button sits beside the toggle rather than inside its label,
                // which would hand its taps to the switch.
                HStack(spacing: MobiusSpace.xs) {
                    Toggle("Help improve möbius", isOn: Binding(
                        get: { model.cloudAccount?.sharesDiagnostics ?? false },
                        set: { sharesDiagnostics in
                            Task { await model.setCloudSharesDiagnostics(sharesDiagnostics) }
                        }
                    ))
                    .toggleStyle(.switch)
                    .disabled(model.isUpdatingCloudDiagnostics)
                    SettingsInfoButton(
                        title: "Help improve möbius",
                        detail: "Off by default. Saved to your Cloud account."
                    )
                }
            }
            if let startedAt = model.cloudAccount?.subscriptionStartedAt {
                LabeledContent("Subscriber since") {
                    Text(startedAt, format: .dateTime.month(.wide).day().year())
                }
            }
            if let limit = model.cloudAccount?.luna {
                CloudAgentUsageLimit(limit: limit)
            }
            VStack(spacing: MobiusSpace.s) {
                if model.cloudAccount?.subscribed == false {
                    MobiusCloudOfferButton()
                } else if model.cloudAccount?.subscribed == true, model.mobiusCloudGateway == nil {
                    Button("Connect Cloud gateway", glyph: .cloudServer) {
                        Task { _ = await model.connectCloudGateway() }
                    }
                    .mobiusProminentButton()
                }
                Button("Manage subscription", glyph: .sealCheck) {
                    showsSubscriptionManagement = true
                }
                .buttonStyle(.mobiusGlass)
                .tint(palette.accent)
                .accessibilityHint("Opens App Store subscription management, where you can unsubscribe")
                .manageSubscriptionsSheet(isPresented: $showsSubscriptionManagement)
                Button(
                    model.cloudAction == .restoring ? "Restoring purchases…" : "Restore purchases",
                    glyph: .arrowClockwise
                ) {
                    Task { _ = await model.restoreCloudPurchases() }
                }
                .buttonStyle(.mobiusGlass)
                .tint(palette.signal)
                .disabled(model.cloudAction.isRunning)
                Button("Sign out", glyph: .lockOpen, role: .destructive) {
                    confirmsSignOut = true
                }
                .buttonStyle(.mobiusGlassProminent)
                .tint(palette.danger)
                .foregroundStyle(.white)
                .disabled(model.cloudAction.isRunning)
                .accessibilityHint("Forgets this Cloud sign-in and its paired gateway")
                Button("Delete account", glyph: .trash, role: .destructive) {
                    if model.cloudAccount?.subscribed == true {
                        showsActiveSubscriptionWarning = true
                    } else {
                        confirmsAccountDeletion = true
                    }
                }
                .buttonStyle(.mobiusGlassProminent)
                .tint(palette.danger)
                .foregroundStyle(.white)
                .disabled(model.cloudAction.isRunning || model.cloudAccount == nil)
                .accessibilityHint("Permanently deletes your Cloud account and its data")
            }
            .buttonBorderShape(.capsule)
            .buttonSizing(.flexible)
            .controlSize(.large)
            .alert("Sign out of möbius Cloud?", isPresented: $confirmsSignOut) {
                Button("Sign out", role: .destructive) {
                    Task { await model.signOutOfCloud() }
                }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("This device forgets the Cloud sign-in and removes its paired gateway. Your subscription is unaffected.")
            }
            .alert("Cancel your subscription first", isPresented: $showsActiveSubscriptionWarning) {
                Button("Manage subscription") {
                    showsSubscriptionManagement = true
                }
                Button("OK", role: .cancel) {}
            } message: {
                Text("An active möbius Cloud subscription cannot be deleted. Cancel it in the App Store, then delete your account after the subscription expires.")
            }
            .alert("Delete your möbius Cloud account?", isPresented: $confirmsAccountDeletion) {
                Button("Continue", role: .destructive) {
                    showsAccountDeletionAuthentication = true
                }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("This permanently deletes your Cloud account, gateway, chats, credentials, and other Cloud data. It cannot be undone.")
            }
            .sheet(isPresented: $showsAccountDeletionAuthentication) {
                MobiusCloudAccountDeletionSheet()
                    .presentationDragIndicator(.visible)
            }
            .onChange(of: showsSubscriptionManagement) { wasPresented, isPresented in
                guard wasPresented, !isPresented else { return }
                Task { await model.refreshCloudAccount() }
            }
        } else {
            Text("möbius works on its own with a gateway you run. Connect möbius Cloud to have one provisioned and managed for you.")
                .font(MobiusStyle.bodyFont)
                .foregroundStyle(palette.muted)
                .fixedSize(horizontal: false, vertical: true)
            MobiusCloudOfferButton()
        }
    }
}

private struct MobiusCloudAccountDeletionSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.mobiusPalette) private var palette
    @State private var didAttemptDeletion = false

    var body: some View {
        NavigationStack {
            ZStack {
                ScrollView {
                    VStack(alignment: .leading, spacing: MobiusSpace.l) {
                        MobiusIcon(
                            .trash,
                            size: 48,
                            foreground: palette.danger,
                            gutter: false
                        )
                        .frame(maxWidth: .infinity)
                        .accessibilityHidden(true)
                        Text("Confirm with Apple")
                            .font(.title.bold())
                            .frame(maxWidth: .infinity, alignment: .center)
                        Text("Sign in again to confirm permanent account deletion. This verifies that the request belongs to you.")
                            .font(MobiusStyle.bodyFont)
                            .foregroundStyle(palette.muted)
                            .fixedSize(horizontal: false, vertical: true)
                        if didAttemptDeletion, let cloudError = model.cloudError {
                            Text(cloudError)
                                .font(MobiusStyle.captionFont)
                                .foregroundStyle(palette.danger)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        if model.cloudAction == .deleting {
                            HStack(spacing: MobiusSpace.s) {
                                MobiusSpinner(size: MobiusStyle.glyphInline)
                                Text("Deleting account…")
                            }
                            .font(MobiusStyle.controlFont)
                            .frame(maxWidth: .infinity, minHeight: 50)
                            .accessibilityElement(children: .combine)
                        } else {
                            MobiusCloudAppleAuthorizationButton(label: .continue) {
                                authorizationCode, nonce in
                                didAttemptDeletion = true
                                Task {
                                    if await model.deleteCloudAccount(
                                        authorizationCode: authorizationCode,
                                        nonce: nonce
                                    ) {
                                        dismiss()
                                    }
                                }
                            } onFailure: {
                                didAttemptDeletion = true
                                model.reportCloudSignInFailure()
                            }
                            .frame(maxWidth: .infinity, alignment: .center)
                        }
                    }
                    .frame(maxWidth: 680, alignment: .leading)
                    .padding(MobiusSpace.l)
                    .frame(maxWidth: .infinity)
                }
                .scrollIndicators(.hidden)
            }
            .navigationTitle("Delete account")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                        .disabled(model.cloudAction == .deleting)
                }
            }
        }
        .interactiveDismissDisabled(model.cloudAction == .deleting)
        .presentationDetents([.medium, .large])
    }
}

private let profileUsageWeekCount = 25

private struct ProfileUsageSection: View {
    @Environment(\.mobiusPalette) private var palette
    let days: [DailyUsage]

    var body: some View {
        let total = days.reduce(into: TokenUsage()) { result, day in
            result.inputTokens += day.usage.inputTokens
            result.cachedInputTokens += day.usage.cachedInputTokens
            result.outputTokens += day.usage.outputTokens
            result.totalTokens += day.usage.totalTokens
        }
        // The section header already says "Usage", so the grid needs no heading of its own.
        UsageMetricGrid {
            UsageMetric(label: "Tokens", value: compact(total.totalTokens))
            UsageMetric(label: "Input", value: compact(total.inputTokens))
            UsageMetric(label: "Output", value: compact(total.outputTokens))
            UsageMetric(label: "Cached", value: cacheHit(total))
        }
    }
}

private struct ProfileUsageHistory: View {
    @Environment(\.mobiusPalette) private var palette
    @State private var aggregation: UsageAggregation = .daily
    let days: [DailyUsage]
    let providerLabels: [String: String]

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.l) {
            VStack(alignment: .leading, spacing: MobiusSpace.l) {
                Text("Token activity")
                    .font(MobiusStyle.controlFont)
                Picker("Usage grouping", selection: $aggregation) {
                    ForEach(UsageAggregation.allCases) { option in
                        Text(option.title).tag(option)
                    }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .sensoryFeedback(.selection, trigger: aggregation)
                UsageHeatmap(days: days, aggregation: aggregation)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(MobiusStyle.cardPadding)
            .background(palette.panel, in: MobiusStyle.cardShape)
            .overlay {
                MobiusStyle.cardShape.stroke(
                    palette.line.opacity(0.45),
                    lineWidth: MobiusStyle.borderWidth
                )
                .allowsHitTesting(false)
            }
            HStack(alignment: .firstTextBaseline) {
                Text("By provider")
                    .font(MobiusStyle.controlFont)
                Spacer()
                Text("Last \(profileUsageWeekCount) weeks")
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
            }
            ProviderUsageChart(
                usage: days,
                providerLabels: providerLabels,
                weekCount: profileUsageWeekCount,
                aggregation: aggregation
            )
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// Four fixed columns: an adaptive grid drops to three and orphans the last metric.
/// Every metric block shares this so their columns line up down the page.
private struct UsageMetricGrid<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        LazyVGrid(
            columns: Array(
                repeating: GridItem(.flexible(), spacing: MobiusSpace.s, alignment: .topLeading),
                count: 4
            ),
            alignment: .leading,
            spacing: MobiusSpace.l
        ) {
            content
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct UsageMetric: View {
    @Environment(\.mobiusPalette) private var palette
    let label: String
    let value: String

    var body: some View {
        // Value first: a label that wraps to two lines would otherwise push its number
        // off the baseline its neighbours sit on, which is what made the grid look ragged.
        VStack(alignment: .leading, spacing: MobiusSpace.xs) {
            Text(value)
                .font(MobiusStyle.titleFont)
                .monospacedDigit()
                .lineLimit(1)
                .minimumScaleFactor(0.7)
            // Sentence case, not tracked-out monospace caps: the value is the number, and a
            // shouted label competes with it at the same size the number is set in.
            Text(label)
                .font(MobiusStyle.captionFont)
                .foregroundStyle(palette.muted)
                .lineLimit(2)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct ProfileRunStatsSection: View {
    let stats: RunStats

    var body: some View {
        // Two independent HStacks sized their columns separately, so the rows never
        // lined up with each other or with the usage grid above.
        UsageMetricGrid {
            UsageMetric(label: "Runs", value: compact(stats.runCount))
            UsageMetric(label: "Failed", value: compact(stats.failedRunCount))
            UsageMetric(label: "Aborted", value: compact(stats.abortedRunCount))
            UsageMetric(label: "Elapsed", value: formatMilliseconds(stats.elapsedMs))
            UsageMetric(label: "Model calls", value: compact(stats.modelCalls))
            UsageMetric(label: "Tool calls", value: compact(stats.toolCalls))
            UsageMetric(label: "Tool errors", value: compact(stats.failedToolCalls))
            UsageMetric(label: "Run tokens", value: compact(stats.usage.totalTokens))
        }
    }
}

private struct ProfileRecentRuns: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var collapsedGroupIDs: Set<String> = []
    let groups: [SessionRunGroup]

    var body: some View {
        if groups.isEmpty {
            Text("No completed runs yet.")
                .font(MobiusStyle.bodyFont)
                .foregroundStyle(palette.muted)
        } else {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(groups) { group in
                        DisclosureGroup(isExpanded: expansion(for: group.id)) {
                            ForEach(group.runs) { run in
                                Button {
                                    model.openChat(group.sessionId)
                                } label: {
                                    HStack(spacing: MobiusSpace.m) {
                                        MobiusIcon(runGlyph(run), foreground: runColor(run))
                                        VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                                            HStack(spacing: MobiusSpace.s) {
                                                Text(run.sessionId == group.sessionId ? "Run" : "Sub-run")
                                                    .font(MobiusStyle.metadataFont.weight(.semibold))
                                                Text(
                                                    runDate(run),
                                                    format: .dateTime.month(.abbreviated).day().hour().minute()
                                                )
                                                .font(MobiusStyle.metadataFont)
                                                .foregroundStyle(palette.muted)
                                            }
                                            Text(runDetail(run))
                                                .font(MobiusStyle.metadataFont)
                                                .foregroundStyle(palette.muted)
                                                .lineLimit(1)
                                        }
                                        Spacer(minLength: MobiusSpace.xs)
                                        MobiusIcon(.caretRight, size: MobiusStyle.glyphMark, foreground: palette.muted)
                                    }
                                    .frame(maxWidth: .infinity, minHeight: MobiusStyle.iconButtonSize)
                                    .contentShape(Rectangle())
                                }
                                .buttonStyle(.mobiusPlain)
                                .disabled(
                                    !model.canOpenSession
                                        && group.sessionId != model.selectedSessionID
                                )
                                .accessibilityLabel("\(runOutcome(run)), \(group.title)")
                                .accessibilityValue(runDetail(run))
                                .accessibilityHint("Opens the chat for this run")
                            }
                        } label: {
                            HStack(spacing: MobiusSpace.s) {
                                Text(group.title)
                                    .font(MobiusStyle.controlFont)
                                    .lineLimit(1)
                                Text(group.runs.count, format: .number)
                                    .font(MobiusStyle.metadataFont)
                                    .foregroundStyle(palette.muted)
                            }
                            .frame(maxWidth: .infinity, minHeight: MobiusStyle.iconButtonSize)
                        }
                        .tint(palette.accent)
                    }
                }
            }
            .frame(height: CGFloat(min(visibleRowCount, 20)) * MobiusStyle.iconButtonSize)
            .scrollBounceBehavior(.basedOnSize)
        }
    }

    private var visibleRowCount: Int {
        groups.reduce(0) { count, group in
            count + 1 + (collapsedGroupIDs.contains(group.id) ? 0 : group.runs.count)
        }
    }

    private func expansion(for groupID: String) -> Binding<Bool> {
        Binding(
            get: { !collapsedGroupIDs.contains(groupID) },
            set: { expanded in
                if expanded { collapsedGroupIDs.remove(groupID) }
                else { collapsedGroupIDs.insert(groupID) }
            }
        )
    }

    private func runDetail(_ run: RunSummary) -> String {
        "\(formatMilliseconds(run.elapsedMs)) · \(run.modelCalls) model · \(run.toolCalls) tools · \(compact(run.usage.totalTokens)) tokens"
    }

    private func runDate(_ run: RunSummary) -> Date {
        Date(timeIntervalSince1970: TimeInterval(run.startedAtMs) / 1_000)
    }

    private func runOutcome(_ run: RunSummary) -> String {
        switch run.outcome {
        case .completed: "Completed"
        case .aborted: "Aborted"
        case .failed: "Failed"
        case nil: "Running"
        }
    }

    private func runGlyph(_ run: RunSummary) -> MobiusGlyph {
        switch run.outcome {
        case .completed: .checkCircle
        case .aborted: .stopFill
        case .failed: .xCircle
        case nil: .arrowClockwise
        }
    }

    private func runColor(_ run: RunSummary) -> Color {
        switch run.outcome {
        case .completed: palette.signal
        case .aborted: palette.warning
        case .failed: palette.danger
        case nil: palette.accent
        }
    }
}

private struct UsageHeatmap: View {
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityDifferentiateWithoutColor) private var differentiatesWithoutColor
    let days: [DailyUsage]
    let aggregation: UsageAggregation

    var body: some View {
        let chart = chartData
        VStack(alignment: .leading, spacing: MobiusSpace.xs) {
            Canvas { context, size in
                let gapRatio: CGFloat = 0.28
                let cell = size.width / (
                    CGFloat(profileUsageWeekCount)
                        + gapRatio * CGFloat(profileUsageWeekCount - 1)
                )
                let spacing = cell * gapRatio

                for index in chart.values.indices {
                    let week = index / 7
                    let weekday = index % 7
                    guard week < profileUsageWeekCount else { continue }
                    let value = chart.values[index]
                    let rect = CGRect(
                        x: CGFloat(week) * (cell + spacing),
                        y: CGFloat(weekday) * (cell + spacing),
                        width: cell,
                        height: cell
                    )
                    let path = Path(roundedRect: rect, cornerRadius: min(4, cell * 0.3))
                    context.fill(path, with: .color(heatColor(value: value, maximum: chart.maximum)))
                    if differentiatesWithoutColor, value > 0 {
                        context.stroke(path, with: .color(palette.canvas), lineWidth: 1)
                    }
                }
            }
            .aspectRatio(heatmapAspectRatio, contentMode: .fit)

            GeometryReader { geometry in
                let gapRatio: CGFloat = 0.28
                let cell = geometry.size.width / (
                    CGFloat(profileUsageWeekCount)
                        + gapRatio * CGFloat(profileUsageWeekCount - 1)
                )
                let spacing = cell * gapRatio
                ZStack(alignment: .topLeading) {
                    ForEach(monthLabels) { label in
                        Text(label.title)
                            .font(MobiusStyle.metadataFont)
                            .foregroundStyle(palette.muted)
                            .position(
                                x: CGFloat(label.week) * (cell + spacing) + 12,
                                y: 8
                            )
                    }
                }
            }
            .frame(height: 16)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(aggregation.title) token activity")
        .accessibilityValue("\(chart.activeDays) active days, \(chart.totalTokens) total tokens")
    }

    private var heatmapAspectRatio: CGFloat {
        let gapRatio: CGFloat = 0.28
        return (
            CGFloat(profileUsageWeekCount)
                + gapRatio * CGFloat(profileUsageWeekCount - 1)
        ) / (7 + gapRatio * 6)
    }

    private var monthLabels: [UsageMonthLabel] {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(secondsFromGMT: 0)!
        let today = UInt64(Date.now.timeIntervalSince1970 / 86_400)
        let dayCount = profileUsageWeekCount * 7
        let start = today - min(today, UInt64(dayCount - 1))
        var labels: [UsageMonthLabel] = []
        var previousMonth: Int?
        for week in 0..<profileUsageWeekCount {
            let date = Date(timeIntervalSince1970: TimeInterval(start + UInt64(week * 7)) * 86_400)
            let month = calendar.component(.month, from: date)
            guard month != previousMonth else { continue }
            previousMonth = month
            labels.append(UsageMonthLabel(
                week: week,
                title: date.formatted(.dateTime.month(.narrow))
            ))
        }
        return labels
    }

    private var chartData: UsageActivitySnapshot {
        UsageActivitySeries.snapshot(
            from: days,
            endingOn: UInt64(Date.now.timeIntervalSince1970 / 86_400),
            weekCount: profileUsageWeekCount,
            aggregation: aggregation
        )
    }

    private func heatColor(value: Int, maximum: Int) -> Color {
        guard value > 0 else { return palette.line.opacity(0.35) }
        let ratio = Double(value) / Double(maximum)
        return palette.accent.opacity(0.25 + 0.75 * ratio.squareRoot())
    }
}

private struct UsageMonthLabel: Identifiable {
    let week: Int
    let title: String

    var id: Int { week }
}

private struct AppearanceSettings: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Picker("Theme", selection: Binding(
            get: { model.theme },
            set: { model.setTheme($0) }
        )) {
            ForEach(ThemePreference.allCases) { Text($0.rawValue.capitalized).tag($0) }
        }
        .pickerStyle(.segmented)
        .sensoryFeedback(.selection, trigger: model.theme)
    }
}

private struct AppLockSettings: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette

    var body: some View {
        HStack(spacing: MobiusSpace.xs) {
            Toggle(model.appLockAuthenticationMethod.settingTitle, isOn: Binding(
                get: { model.appLockEnabled },
                set: { enabled in
                    Task { await model.setAppLockEnabled(enabled) }
                }
            ))
            .toggleStyle(.switch)
            .disabled(
                model.isAppLockAuthenticating
                    || !model.appLockEnabled && !model.appLockAuthenticationMethod.isAvailable
            )
            SettingsInfoButton(
                title: model.appLockAuthenticationMethod.settingTitle,
                detail: description
            )
        }
        .onAppear { model.refreshAppLockAuthenticationMethod() }
        if model.isAppLockAuthenticating {
            ProgressView("Authenticating")
        }
        if let error = model.appLockError {
            Text(error)
                .foregroundStyle(palette.danger)
                .accessibilityLabel("App lock status: \(error)")
        }
    }

    private var description: String {
        if model.appLockAuthenticationMethod.isAvailable {
            return "Locks möbius when it enters the background. This setting stays on this device."
        }
        return "Set up Face ID or Touch ID in Settings before enabling app lock."
    }
}

private func compact(_ value: Int) -> String {
    value.formatted(.number.notation(.compactName).precision(.fractionLength(0 ... 1)))
}

private func compact(_ value: UInt64) -> String {
    value.formatted(.number.notation(.compactName).precision(.fractionLength(0 ... 1)))
}

private func formatMilliseconds(_ milliseconds: UInt64) -> String {
    let seconds = Int(clamping: milliseconds / 1_000)
    return Duration.seconds(seconds).formatted(
        .time(pattern: .minuteSecond(padMinuteToLength: 1))
    )
}
