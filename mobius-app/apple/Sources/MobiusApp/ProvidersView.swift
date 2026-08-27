import SwiftUI

struct ProvidersView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var isAdding = false
    @State private var removing: ProviderInstance?

    var body: some View {
        let status = catalogStatus
        PageScaffold(
            title: "Providers",
            detail: pageDetail,
            sharesHeaderBackground: true,
            headerAccessory: {
                HeaderActionGroup {
                    Button {
                        isAdding = true
                    } label: {
                        MobiusIcon(.plus, gutter: false)
                    }
                    .groupedHeaderAction(prominent: true)
                    .disabled(!model.connectionState.isReady)
                    .accessibilityLabel("Add provider")
                    .accessibilityHint("Opens the provider setup")
                    .help("Add provider")
                    SettingsStatusButton(
                        subject: "Providers",
                        statusLabel: status.label,
                        statusDetail: status.detail,
                        statusColor: status.color
                    )
                    .groupedHeaderAction()
                }
            }
        ) {
            Section("Configured") {
                if showsLoadingCatalog {
                    loadingCatalog
                } else if model.providerInstances.isEmpty {
                    SettingsCaption("No provider configured yet.")
                } else {
                    ForEach(model.providerInstances) { instance in
                        configuredRow(instance)
                    }
                }
            }
        }
        .sheet(isPresented: $isAdding) { AddProviderSheet() }
        .confirmationDialog(
            removing.map { "Remove \($0.label)?" } ?? "Remove provider?",
            isPresented: Binding(
                get: { removing != nil },
                set: { if !$0 { removing = nil } }
            ),
            titleVisibility: .visible,
            presenting: removing
        ) { instance in
            Button("Remove provider", role: .destructive) {
                model.removeProvider(instance.instance)
                removing = nil
            }
            Button("Cancel", role: .cancel) { removing = nil }
        } message: { _ in
            Text("This removes the provider setup from the gateway. This cannot be undone.")
        }
    }

    private var showsLoadingCatalog: Bool {
        model.connectionState.isLoading && model.providerInstances.isEmpty
    }

    /// The rows this page is about to show, drawn from the same primitive so the wait
    /// looks like the page rather than like a spinner.
    private var loadingCatalog: some View {
        SettingsLoadingRows(label: "Loading providers") {
            ForEach(["Provider account", "Another account"], id: \.self) { name in
                SettingsRowLabel(title: name, detail: "Model service") {
                    MobiusIcon(.hardDrives, size: MobiusStyle.glyphLead, foreground: palette.muted)
                        .unredacted()
                }
            }
        }
    }

    private func providerLabel(_ instance: ProviderInstance) -> some View {
        let definition = model.providerStatuses.first { $0.provider == instance.provider }
        return SettingsRowLabel(
            title: instance.label,
            detail: definition?.label ?? instance.provider
        ) {
            ProviderMark(symbol: definition?.symbol, tint: instance.tint)
        }
    }

    private var pageDetail: String {
        model.connectionState.isReady
            ? "Model services this gateway can reach. One setup per account or endpoint."
            : "Connect to a gateway to manage providers."
    }

    private var catalogStatus: (label: String, detail: String, color: Color) {
        switch model.connectionState {
        case .ready:
            let ready = model.providerInstances.filter(\.configured).count
            return (
                "Catalog up to date",
                "\(model.providerInstances.count) configured · \(ready) ready",
                palette.signal
            )
        case .failed(let message):
            return ("Needs attention", message, palette.danger)
        default:
            return (
                model.connectionState.label,
                "Connect to a gateway to manage its providers.",
                palette.warning
            )
        }
    }

    private func configuredRow(_ instance: ProviderInstance) -> some View {
        SettingsNavigationRow(
            hint: "Shows provider settings",
            open: {
                model.editProviderInstance(instance)
                model.navigationPath = [.settings(.provider(instance.instance))]
            },
            marks: {
                if !instance.configured {
                    MobiusIcon(.key, size: MobiusStyle.glyphMark, foreground: palette.warning)
                        .accessibilityLabel("\(instance.label) needs a credential")
                }
            }
        ) {
            providerLabel(instance)
        }
        .swipeActions(edge: .trailing) {
            Button {
                removing = instance
            } label: {
                MobiusIcon(.trash, foreground: palette.danger)
            }
            .tint(palette.panel)
            .disabled(model.isApplyingConfiguration || !model.connectionState.isReady)
            .accessibilityLabel("Remove \(instance.label)")
        }
    }
}

/// The provider's glyph drawn in the setup's chosen accent.
struct ProviderMark: View {
    let symbol: String?
    let tint: AccentTint

    private var glyph: MobiusGlyph {
        guard let symbol, let known = MobiusSymbol.knownGlyph(for: symbol) else { return .hardDrives }
        return known
    }

    var body: some View {
        MobiusIcon(glyph, size: MobiusStyle.glyphLead, foreground: tint.color)
        .accessibilityHidden(true)
    }
}

// MARK: - Add

private struct AddProviderSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var provider: String?

    var body: some View {
        NavigationStack {
            Form {
                if let provider, model.providerDraft != nil {
                    ProviderFormSections(provider: provider, isNew: true)
                } else {
                    Section {
                        providerRows(auth: .deviceCode)
                    } header: {
                        HStack(spacing: MobiusSpace.xs) {
                            Text("Subscription providers")
                            SettingsInfoButton(
                                title: "Provider setups",
                                detail: "Pick a service, then name this setup. Adding a second setup of the same service keeps both, each with its own credential.",
                                compact: true
                            )
                        }
                    }
                    Section("API providers") {
                        providerRows(auth: .apiKey)
                    }
                }
            }
            .navigationTitle(provider == nil ? "Add provider" : "New setup")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                if provider != nil {
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Save", action: model.registerProvider)
                            .disabled(model.isApplyingConfiguration)
                    }
                }
            }
            .onChange(of: model.providerInstances.map(\.instance)) { _, instances in
                guard let instance = model.providerDraft?.instance,
                      instances.contains(instance)
                else { return }
                dismiss()
            }
        }
        .mobiusSheet(detents: [.large])
    }

    private func providerRows(auth: ProviderAuthKind) -> some View {
        ForEach(model.providerStatuses.filter { $0.auth == auth }) { status in
            Button {
                model.addProviderInstance(status.provider)
                provider = status.provider
            } label: {
                SettingsRowLabel(
                    title: status.label,
                    detail: status.description
                )
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
        }
    }
}

// MARK: - Detail

struct ProviderDetailView: View {
    @Environment(AppModel.self) private var model
    @State private var confirmsRemoval = false
    let instance: String

    var body: some View {
        if let record = model.providerInstances.first(where: { $0.instance == instance }) {
            PageScaffold(
                title: record.label,
                detail: "",
                sharesHeaderBackground: true,
                headerAccessory: {
                    HeaderActionGroup {
                        Button {
                            model.registerProvider()
                        } label: {
                            MobiusIcon(.floppyDisk, gutter: false)
                        }
                        .groupedHeaderAction(prominent: true)
                        .disabled(model.isApplyingConfiguration)
                        .accessibilityLabel("Save to gateway")
                        .help("Save to gateway")
                        Button {
                            confirmsRemoval = true
                        } label: {
                            MobiusIcon(.trash, gutter: false)
                        }
                        .groupedHeaderAction()
                        .disabled(
                            model.isApplyingConfiguration || !model.connectionState.isReady
                        )
                        .accessibilityLabel("Remove provider")
                        .help("Remove provider")
                    }
                }
            ) {
                ProviderFormSections(provider: record.provider, isNew: false)
            }
            .toolbarRole(.editor)
            .confirmationDialog(
                "Remove \(record.label)?",
                isPresented: $confirmsRemoval,
                titleVisibility: .visible
            ) {
                Button("Remove provider", role: .destructive) {
                    model.removeProvider(record.instance)
                }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("This removes the provider setup from the gateway. This cannot be undone.")
            }
        } else {
            MobiusUnavailable(
                title: "Provider unavailable",
                glyph: AppDestination.providers.glyph,
                detail: "It is no longer configured on this gateway."
            )
            .navigationTitle("Provider")
            .toolbarRole(.editor)
            .background(MobiusBackdrop())
        }
    }
}

// MARK: - Shared form

/// The editable body of one setup. Every field, credential included, can be replaced.
private struct ProviderFormSections: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let provider: String
    let isNew: Bool

    var body: some View {
        @Bindable var model = model
        if let status = definition {
            Section("Setup") {
                LabeledContent {
                    TextField(status.label, text: $model.providerLabelDraft)
                        .settingsField()
                } label: {
                    HStack(spacing: MobiusSpace.xs) {
                        Text("Name")
                        SettingsInfoButton(
                            title: "Name",
                            detail: "Shown in model pickers and usage. Name setups for what they are, like Work or Personal."
                        )
                    }
                }
                // The service a setup talks to is fixed; only its presentation is editable.
                LabeledContent("Provider") {
                    HStack(spacing: MobiusSpace.xs) {
                        ProviderMark(symbol: status.symbol, tint: model.providerTintDraft)
                        Text(verbatim: status.label)
                    }
                    .font(MobiusStyle.controlFont)
                }
                LabeledContent("Colour") {
                    AccentTintPicker(selection: $model.providerTintDraft)
                }
            }

            Section("Model") {
                SettingsStackedField(
                    title: "Model ID(s)",
                    info: status.modelIdsConfigurable
                        ? "Enter one or more exact provider model IDs separated by commas. Whitespace, empty entries, and duplicates are ignored. Pick one per chat in the composer."
                        : "Every model above is available. Pick one per chat in the composer."
                ) {
                    if status.modelIdsConfigurable {
                        TextField(
                            "model-a, model-b",
                            text: providerModelIDs,
                            axis: .vertical
                        )
                        .lineLimit(1...4)
                    } else {
                        readOnlyValue(status.models.map(\.id).joined(separator: ", "))
                    }
                }
                SettingsStackedField(
                    title: "Reasoning effort(s)",
                    info: status.modelIdsConfigurable
                        ? "Enter the exact reasoning efforts supported by these models, separated by commas. Leave empty to use the provider default."
                        : "Available reasoning efforts across the listed models."
                ) {
                    if status.modelIdsConfigurable {
                        TextField(
                            "low, medium, high",
                            text: providerReasoningEfforts,
                            axis: .vertical
                        )
                        .lineLimit(1...4)
                    } else {
                        readOnlyValue(reasoningEfforts(for: status))
                    }
                }
                SettingsStackedField(title: "Base URL") {
                    if status.defaultBaseUrl != nil {
                        TextField(
                            "Provider endpoint",
                            text: providerBaseURL,
                            axis: .vertical
                        )
                        .textContentType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .lineLimit(1...4)
                    } else {
                        readOnlyValue("Provider managed")
                    }
                }

                Picker("Hosted web search", selection: providerWebSearch) {
                    ForEach(status.webSearch) { search in
                        if let value = HostedWebSearch(rawValue: search.value) {
                            Text(search.label).tag(value)
                        }
                    }
                }
                .settingsPickerStyle()
                .sensoryFeedback(.selection, trigger: providerWebSearch.wrappedValue)
                .disabled(status.webSearch.count == 1)
                if let search = status.webSearch.first(where: {
                    $0.value == providerWebSearch.wrappedValue.rawValue
                }) {
                    Text(search.description)
                        .font(MobiusStyle.captionFont)
                        .foregroundStyle(palette.muted)
                }
            }

            Section("Credential") {
                credentialControls(status)
            }

            providerActionStatus
            credentialAction(status)
        }
    }

    private var definition: ProviderStatus? {
        model.providerStatuses.first { $0.provider == provider }
    }

    private func reasoningEfforts(for status: ProviderStatus) -> String {
        let values = status.models
            .flatMap(\.reasoning)
            .map(\.id)
            .reduce(into: [String]()) { values, value in
                if !values.contains(value) { values.append(value) }
            }
        return values.isEmpty ? "Provider default" : values.joined(separator: ", ")
    }

    /// Under a stacked label, so a long list wraps down the row rather than truncating in
    /// a trailing column. Font and colour come from the field.
    private func readOnlyValue(_ value: String) -> some View {
        Text(value)
            .fixedSize(horizontal: false, vertical: true)
    }

    @ViewBuilder
    private func credentialControls(_ status: ProviderStatus) -> some View {
        @Bindable var model = model
        if !isNew {
            LabeledContent("Status") {
                Text(isConfigured ? "Configured on gateway" : "Not configured")
                    .font(MobiusStyle.controlFont)
                    .foregroundStyle(isConfigured ? palette.signal : palette.warning)
            }
        }
        if status.auth == .apiKey {
            if let instance = model.providerDraft?.instance,
               let hint = model.providerInstances.first(where: { $0.instance == instance })?.credentialHint {
                LabeledContent("Saved key") {
                    Text("••••\(hint)")
                        .foregroundStyle(palette.muted)
                        .accessibilityLabel("Saved key ending in \(hint)")
                }
            }
            SettingsStackedField(
                title: "API key",
                info: "Sent once to the gateway and never returned to this app. Sending a new one replaces the stored key for this setup."
            ) {
                SecureField("API key", text: $model.providerAPIKey)
                    .textContentType(.password)
                    .privacySensitive()
            }
        }
    }

    /// The credential action stands under the form rather than inside the Credential
    /// card, so the page ends on one full-width accent button.
    @ViewBuilder
    private func credentialAction(_ status: ProviderStatus) -> some View {
        if status.auth == .apiKey {
            MobiusActionRow {
                Button("Send key to gateway", glyph: .key) {
                    model.saveProviderCredential()
                }
                .mobiusProminentButton()
                .disabled(model.providerAPIKey.isEmpty)
            }
            .settingsStandaloneRow()
        } else if status.auth == .deviceCode {
            MobiusActionRow {
                Button("Start device sign-in", glyph: .signIn) {
                    model.startProviderLogin()
                }
                .mobiusProminentButton()
            }
            .settingsStandaloneRow()
        }
    }

    private var isConfigured: Bool {
        guard let instance = model.providerDraft?.instance else { return false }
        return model.providerInstances.first { $0.instance == instance }?.configured ?? false
    }

    @ViewBuilder
    private var providerActionStatus: some View {
        switch model.providerActionState {
        case .idle:
            EmptyView()
        case .savingCredential:
            StatusBanner(tone: .neutral, title: "Sending credential", detail: "The value is not persisted by this app.", progress: true)
        case .credentialSaved(let provider):
            StatusBanner(tone: .success, title: "Credential updated", detail: "\(model.providerLabel(for: provider)) is configured on the gateway.")
        case .startingLogin(let provider):
            StatusBanner(tone: .neutral, title: "Starting \(model.providerLabel(for: provider)) sign-in", detail: "Waiting for a device code.", progress: true)
        case .deviceCode(let provider, let url, let code):
            VStack(alignment: .leading, spacing: MobiusSpace.m) {
                Text("Finish \(model.providerLabel(for: provider)) sign-in")
                    .font(MobiusStyle.titleFont)
                Text("Open the verification page and enter this code.")
                    .font(MobiusStyle.bodyFont)
                    .foregroundStyle(palette.muted)
                Text(code)
                    .font(MobiusStyle.codeFont)
                    .tracking(3)
                    .textSelection(.enabled)
                    .padding(MobiusSpace.m)
                    .background(palette.raised, in: MobiusStyle.controlShape)
                MobiusActionRow {
                    if let destination = URL(string: url) {
                        Link("Open verification page", destination: destination)
                    }
                    ShareLink("Copy or share code", item: code)
                }
            }
        case .loginFinished(let provider):
            StatusBanner(tone: .success, title: "Sign-in complete", detail: "\(model.providerLabel(for: provider)) is ready on the gateway.")
        case .failed(let message):
            StatusBanner(tone: .error, title: "Provider action failed", detail: message)
        }
    }

    private var providerBaseURL: Binding<String> {
        Binding(
            get: { model.providerDraft?.baseUrl ?? "" },
            set: { model.providerDraft?.baseUrl = $0.nonEmpty }
        )
    }

    private var providerModelIDs: Binding<String> {
        Binding(
            get: { model.providerModelIDsText },
            set: { model.updateProviderModelIDs($0) }
        )
    }

    private var providerReasoningEfforts: Binding<String> {
        Binding(
            get: { model.providerReasoningEffortsText },
            set: { model.updateProviderReasoningEfforts($0) }
        )
    }

    private var providerWebSearch: Binding<HostedWebSearch> {
        Binding(
            get: { model.providerDraft?.webSearch ?? .off },
            set: { model.providerDraft?.webSearch = $0 }
        )
    }
}

/// The shared Nord accent swatches.
struct AccentTintPicker: View {
    @Environment(\.mobiusPalette) private var palette
    @Binding var selection: AccentTint

    var body: some View {
        ScrollView(.horizontal) {
            HStack(spacing: 0) {
                ForEach(AccentTint.allCases) { tint in
                    Button {
                        selection = tint
                    } label: {
                        Circle()
                            .fill(tint.color)
                            .frame(width: 18, height: 18)
                            .overlay(
                                Circle().strokeBorder(
                                    palette.onAccent,
                                    lineWidth: selection == tint ? 2 : 0
                                )
                            )
                            .frame(width: 44, height: 44)
                            .contentShape(.rect)
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel(tint.label)
                    .accessibilityAddTraits(selection == tint ? [.isSelected] : [])
                }
            }
        }
        .scrollIndicators(.hidden)
        .defaultScrollAnchor(.center, for: .alignment)
        .sensoryFeedback(.selection, trigger: selection)
    }
}
