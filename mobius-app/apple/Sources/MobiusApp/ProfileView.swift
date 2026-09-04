import Foundation
import SwiftUI

struct ProfileView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let usage = model.profile?.dailyUsage ?? []
        let providerLabels = model.providerInstances.reduce(into: [String: String]()) {
            $0[$1.instance] = $1.label
        }
        let providerTints = model.providerInstances.reduce(into: [String: AccentTint]()) {
            $0[$1.instance] = $1.tint
        }
        PageScaffold(
            title: .localized("Settings"),
            detail: .verbatim(""),
            headerAccessory: SettingsInformationButton.init
        ) {
            // Settings first, the dashboard last: this page is opened to change something,
            // and usage is the one section here nobody comes to act on.
            Section {
                CloudAccountSettings()
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
            Section("Security and Data") {
                AppLockSettings()
                RemoteNotificationSettings()
                LocalDataSettings()
            }
            .listRowSeparator(.hidden)
            Section("Usage") {
                ProfileUsageSection(days: usage)
                ProfileUsageHistory(
                    days: usage,
                    providerLabels: providerLabels,
                    providerTints: providerTints
                )
            }
            .listRowSeparator(.hidden)
        }
        .task(id: model.cloudSession?.credentialID) {
            await model.refreshCloudAccount()
        }
        .task(id: model.connectionState.isReady) { model.refreshProfile() }
    }
}

private struct CloudAgentUsageLimit: View {
    @Environment(\.mobiusPalette) private var palette
    let limit: MobiusCloudUsageLimit?

    private static let resetFormat = Date.FormatStyle(
        date: .abbreviated,
        time: .omitted,
        timeZone: .gmt
    )

    var body: some View {
        let percentage = (limit?.remainingFraction ?? 0).formatted(
            .percent.precision(.fractionLength(0))
        )
        let remaining = limit == nil
            ? Text("Unavailable")
            : Text("\(percentage) remaining")
        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            HStack(alignment: .firstTextBaseline) {
                Text("möbius cloud agent usage limits")
                Spacer(minLength: MobiusSpace.s)
                remaining
                    .monospacedDigit()
            }
            .font(MobiusStyle.controlFont)
            .accessibilityHidden(true)
            ProgressView(value: limit?.remainingFraction ?? 0)
                .progressViewStyle(.linear)
                .tint(palette.accent)
                .accessibilityLabel("möbius Cloud agent usage limits")
                .accessibilityValue(remaining)
            if let limit {
                Text("Resets \(limit.resetsAt.formatted(Self.resetFormat))")
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
            } else {
                Text("Unavailable")
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
            }
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

    private var versionDescription: LocalizedStringResource {
        let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString")
            as? String ?? "—"
        let build = Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "—"
        return "möbius v\(version) (\(build))"
    }

    private func showPlaceholder(_ title: LocalizedStringResource) {
        showsInformation = false
        model.showToast("\(title) will be available before the cloud release.")
    }
}

private struct SettingsInformationRow: View {
    @Environment(\.mobiusPalette) private var palette
    let title: LocalizedStringResource
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
    @State private var confirmsSignOut = false
    @State private var confirmsAccountDeletion = false
    @State private var showsAccountDeletionAuthentication = false

    /// Signed out, this is an offer; signed in, it is an account. Account actions stay absent
    /// until there is a Cloud session to authenticate them.
    var body: some View {
        let subscriptionExpired = model.cloudIssue == .subscriptionExpired
        if model.isLoadingCloudAccount, !subscriptionExpired {
            SettingsLoadingRows(label: "Loading Cloud account") {
                LabeledContent("Email") { Text("account@example.com") }
                HStack(spacing: MobiusSpace.xs) {
                    Toggle("Help improve möbius", isOn: .constant(false))
                    SettingsInfoButton(
                        title: "Help improve möbius",
                        detail: "Off by default. Saved to your Cloud account."
                    )
                }
                LabeledContent("Subscriber since") { Text("August 2026") }
                CloudAgentUsageLimit(limit: MobiusCloudUsageLimit(
                    creditMicrousd: 1,
                    remainingMicrousd: 1,
                    resetsAt: Date(timeIntervalSince1970: 0)
                ))
                VStack(spacing: MobiusSpace.s) {
                    ForEach(0..<(model.mobiusCloudGateway == nil ? 5 : 4), id: \.self) { _ in
                        Button("Manage subscription", glyph: .sealCheck) {}
                    }
                }
                .buttonStyle(.mobiusGlass)
                .buttonBorderShape(.capsule)
                .buttonSizing(.flexible)
                .controlSize(.large)
            }
        } else if model.hasCloudAccount {
            if subscriptionExpired {
                StatusBanner(
                    tone: .warning,
                    title: "Subscription expired",
                    detail: "Renew or restore your subscription to reconnect to möbius Cloud."
                )
            } else if let cloudError = model.cloudError {
                StatusBanner(
                    tone: .error,
                    title: .localized("Cloud account unavailable"),
                    detail: .verbatim(cloudError),
                    action: (.localized("Retry"), { Task { await model.refreshCloudAccount() } })
                )
            }
            if !subscriptionExpired || model.cloudAccount != nil {
                LabeledContent("Email") {
                    if let email = model.cloudAccount?.email {
                        Text(verbatim: email)
                    } else {
                        Text("Unavailable")
                    }
                }
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
                    .disabled(model.isUpdatingCloudDiagnostics || model.cloudAccount == nil)
                    SettingsInfoButton(
                        title: "Help improve möbius",
                        detail: "Off by default. Saved to your Cloud account."
                    )
                }
            }
            if !subscriptionExpired {
                LabeledContent("Subscriber since") {
                    if let startedAt = model.cloudAccount?.subscriptionStartedAt {
                        Text(startedAt, format: .dateTime.month(.wide).day().year())
                    } else {
                        Text("Unavailable")
                    }
                }
                CloudAgentUsageLimit(limit: model.cloudAccount?.luna)
            }
            VStack(spacing: MobiusSpace.s) {
                if subscriptionExpired || (model.cloudAccount != nil && model.mobiusCloudGateway == nil) {
                    MobiusCloudOfferButton()
                }
                Button("Manage subscription", glyph: .sealCheck) {
                    Task { await model.manageCloudSubscription() }
                }
                .buttonStyle(.mobiusGlass)
                .tint(palette.accent)
                .accessibilityHint("Opens App Store subscription management, where you can unsubscribe")
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
                .foregroundStyle(palette.onDanger)
                .disabled(model.cloudAction.isRunning)
                .accessibilityHint("Forgets this Cloud sign-in and its paired gateway")
                Button("Delete account", glyph: .trash, role: .destructive) {
                    confirmsAccountDeletion = true
                }
                .buttonStyle(.mobiusGlassProminent)
                .tint(palette.danger)
                .foregroundStyle(palette.onDanger)
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
            .alert("Delete your möbius Cloud account?", isPresented: $confirmsAccountDeletion) {
                Button("Delete now", role: .destructive) {
                    showsAccountDeletionAuthentication = true
                }
                Button("Manage subscription") {
                    Task { await model.manageCloudSubscription() }
                }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("This permanently deletes your Cloud account, gateway, chats, credentials, and other Cloud data. Access ends immediately; Cloud resources may take a short time to erase. App Store billing is separate and may continue until you cancel the subscription.")
            }
            .sheet(isPresented: $showsAccountDeletionAuthentication) {
                MobiusCloudAccountDeletionSheet()
                    .mobiusSheet(detents: [.large])
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
        .mobiusSheet()
    }
}

private let profileUsageWeekCount = 26

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
    let providerTints: [String: AccentTint]

    var body: some View {
        let endingOn = UInt64(Date.now.timeIntervalSince1970 / 86_400)
        let providers = ProviderUsageTotal.top(
            from: days,
            endingOn: endingOn,
            weekCount: profileUsageWeekCount
        )
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
            UsageHeatmap(
                days: days,
                aggregation: aggregation,
                endingOn: endingOn,
                tint: providers.first.flatMap { providerTints[$0.provider] }?.color
                    ?? palette.accent
            )
            if !providers.isEmpty {
                Divider()
                    .overlay(palette.line)
                HStack {
                    Text("Top providers")
                        .font(MobiusStyle.metadataFont.weight(.semibold))
                    Spacer()
                    Text("Last \(profileUsageWeekCount) weeks")
                        .font(MobiusStyle.metadataFont)
                        .foregroundStyle(palette.muted)
                }
                VStack(spacing: MobiusSpace.s) {
                    ForEach(providers) { provider in
                        let label = providerLabels[provider.provider] ?? provider.provider
                        HStack(spacing: MobiusSpace.s) {
                            Circle()
                                .fill((providerTints[provider.provider] ?? .appDefault).color)
                                .frame(width: 8, height: 8)
                            Text(label)
                                .font(MobiusStyle.metadataFont)
                                .lineLimit(1)
                            Spacer(minLength: MobiusSpace.m)
                            Text(compact(provider.totalTokens))
                                .font(MobiusStyle.metadataFont.monospacedDigit())
                                .foregroundStyle(palette.muted)
                        }
                        .accessibilityElement(children: .ignore)
                        .accessibilityLabel(
                            "\(label), \(provider.totalTokens.formatted()) tokens"
                        )
                    }
                }
            }
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
    let label: LocalizedStringResource
    let value: String

    var body: some View {
        // Value first: a label that wraps to two lines would otherwise push its number
        // off the baseline its neighbours sit on, which is what made the grid look ragged.
        VStack(alignment: .leading, spacing: MobiusSpace.xs) {
            Text(verbatim: value)
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

private struct UsageHeatmap: View {
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityDifferentiateWithoutColor) private var differentiatesWithoutColor
    let days: [DailyUsage]
    let aggregation: UsageAggregation
    let endingOn: UInt64
    let tint: Color

    var body: some View {
        let chart = UsageActivitySeries.snapshot(
            from: days,
            endingOn: endingOn,
            weekCount: profileUsageWeekCount,
            aggregation: aggregation
        )
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
                context.fill(path, with: .color(heatColor(level: chart.activityLevel(value))))
                if differentiatesWithoutColor, value > 0 {
                    context.stroke(path, with: .color(palette.canvas), lineWidth: 1)
                }
            }
        }
        .aspectRatio(heatmapAspectRatio, contentMode: .fit)
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

    private func heatColor(level: Int) -> Color {
        switch level {
        case 1: tint.opacity(0.28)
        case 2: tint.opacity(0.5)
        case 3: tint.opacity(0.72)
        case 4: tint
        default: palette.line.opacity(0.35)
        }
    }
}

private struct AppearanceSettings: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Picker("Theme", selection: Binding(
            get: { model.theme },
            set: { model.setTheme($0) }
        )) {
            ForEach(ThemePreference.allCases) { Text($0.label).tag($0) }
        }
        .pickerStyle(.segmented)
        .padding(.vertical, MobiusSpace.xs)
        .sensoryFeedback(.selection, trigger: model.theme)

        AccentTintPicker(selection: Binding(
            get: { model.accentTint },
            set: { model.setAccentTint($0) }
        ))

        Picker("Language", selection: Binding(
            get: { model.language },
            set: { model.setLanguage($0) }
        )) {
            ForEach(AppLanguage.allCases) { Text($0.label).tag($0) }
        }
        .settingsPickerStyle()
        .sensoryFeedback(.selection, trigger: model.language)
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

    private var description: LocalizedStringResource {
        if model.appLockAuthenticationMethod.isAvailable {
            return "Locks möbius when it enters the background. This setting stays on this device."
        }
        return "Set up Face ID or Touch ID in Settings before enabling app lock."
    }
}

private struct RemoteNotificationSettings: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette

    var body: some View {
        HStack(spacing: MobiusSpace.xs) {
            Toggle("Notifications", isOn: Binding(
                get: { model.notificationsEnabled },
                set: { enabled in
                    Task { await model.setNotificationsEnabled(enabled) }
                }
            ))
            .toggleStyle(.switch)
            .disabled(model.isUpdatingNotifications)
            SettingsInfoButton(
                title: "Notifications",
                detail: "Alerts you on this device when a Cloud chat needs approval or finishes, or a Swarm needs attention."
            )
        }
        if model.isUpdatingNotifications {
            ProgressView("Updating notifications")
        }
        if let error = model.notificationError {
            Text(error)
                .foregroundStyle(palette.danger)
                .accessibilityLabel("Notification status: \(error)")
        }
    }
}

private struct LocalDataSettings: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var confirmsCacheClear = false
    @State private var confirmsFullReset = false

    var body: some View {
        VStack(spacing: MobiusSpace.s) {
            Button("Clear cached data", glyph: .trash, role: .destructive) {
                confirmsCacheClear = true
            }
            .disabled(model.isClearingLocalData)
            .accessibilityHint("Removes cached chats, chat lists, and image thumbnails")
            .alert("Clear cached data?", isPresented: $confirmsCacheClear) {
                Button("Clear cached data", role: .destructive) {
                    Task { await model.clearCachedData() }
                }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text(
                    "This removes cached chats, chat lists, and image thumbnails from this device. Gateways, credentials, Cloud sign-in, settings, and drafts are kept."
                )
            }

            Button("Clear data and gateway information", glyph: .trash, role: .destructive) {
                confirmsFullReset = true
            }
            .disabled(model.isClearingLocalData)
            .accessibilityHint("Removes local data, gateway credentials, and the Cloud sign-in")
            .alert("Clear data and gateway information?", isPresented: $confirmsFullReset) {
                Button("Clear data and gateway information", role: .destructive) {
                    Task { await model.clearDataAndGatewayInformation() }
                }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text(
                    "This removes cached data, drafts, paired gateways, saved gateway credentials, and the Cloud sign-in from this device. You’ll need to pair or sign in again. Remote chats, your Cloud account, and your subscription are not deleted."
                )
            }
        }
        .buttonStyle(.mobiusGlassProminent)
        .tint(palette.danger)
        .foregroundStyle(palette.onDanger)
        .buttonBorderShape(.capsule)
        .buttonSizing(.flexible)
        .controlSize(.large)
    }
}

private func compact(_ value: Int) -> String {
    value.formatted(.number.notation(.compactName).precision(.fractionLength(0 ... 1)))
}

private func compact(_ value: UInt64) -> String {
    value.formatted(.number.notation(.compactName).precision(.fractionLength(0 ... 1)))
}
