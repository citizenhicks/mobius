import SwiftUI

enum AgentSettingsScope: Equatable {
    case botDefaults
    case bot(String)
}

struct AgentSettingsView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.locale) private var locale
    @State private var editingCapability: MiddlewareFeature?
    let scope: AgentSettingsScope

    var body: some View {
        @Bindable var model = model
        PageScaffold(
            title: pageTitle,
            detail: pageDetail,
            sharesHeaderBackground: true,
            showsBackdrop: scope == .botDefaults,
            headerAccessory: { configurationStatusAccessory }
        ) {
            if draft != nil {
                if case .bot = scope {
                    Section("Identity") {
                        TextField("Bot name", text: $model.botNameDraft)
                            .textInputAutocapitalization(.words)
                            .font(MobiusStyle.bodyFont)
                            .textFieldStyle(.plain)
                            .labelsHidden()
                            .accessibilityLabel("Bot name")
                            .promptCard()
                        TextField(
                            "Operational description",
                            text: $model.botDescriptionDraft,
                            axis: .vertical
                        )
                        .font(MobiusStyle.bodyFont)
                        .lineLimit(3...6)
                        .textFieldStyle(.plain)
                        .labelsHidden()
                        .accessibilityLabel("Operational description")
                        .promptCard()
                        AccentTintPicker(selection: $model.botTintDraft)
                            .settingsBareRow()
                    }
                }
                Section("System prompt") {
                    TextField("System prompt", text: systemPrompt, axis: .vertical)
                        .font(MobiusStyle.bodyFont)
                        .lineLimit(3...8)
                        .textFieldStyle(.plain)
                        .labelsHidden()
                        .accessibilityLabel("System prompt")
                        .promptCard()
                }

                Section(modelSectionTitle) {
                    ModelRoutePicker(
                        label: "Model",
                        detail: modelSectionDetail,
                        choices: model.modelChoices,
                        isEnabled: !model.modelChoices.isEmpty,
                        route: Binding(
                            get: { selectedModelRoute },
                            set: { if let route = $0 { selectModel(route) } }
                        )
                    )

                    HStack(spacing: MobiusSpace.xs) {
                        // Hundreds: this ceiling is set in the thousands, and stepping by one
                        // makes the control useless for reaching any value someone wants.
                        Stepper(value: maxModelSteps, in: 1...42_000, step: 100) {
                            Text("Maximum model steps: \(maxModelSteps.wrappedValue.formatted())")
                        }
                        SettingsInfoButton(
                            title: "Maximum model steps",
                            detail: "Maximum primary model rounds allowed in one run before möbius stops it."
                        )
                    }
                    .sensoryFeedback(.selection, trigger: maxModelSteps.wrappedValue)
                }

                Section("Capabilities") {
                    ForEach(model.middlewareFeatures, id: \.id) { feature in
                        capabilityRow(feature)
                    }
                }
                .toggleStyle(.switch)
            } else if model.connectionState.isLoading {
                loadingDraft
            } else {
                MobiusUnavailable(
                    title: unavailableTitle,
                    glyph: .slidersHorizontal,
                    detail: unavailableDetail
                )
            }
        }
        .sheet(item: $editingCapability) { feature in
            capabilityEditor(feature)
        }
    }

    /// The page this is about to become, so waiting for the gateway reads as loading
    /// rather than as configuration that is missing.
    private var loadingDraft: some View {
        Group {
            // Per section rather than around all three: a placeholder over the whole group
            // redacts the headers too, and then the page cannot be read at all.
            Section("System prompt") {
                Text("A short brief the gateway sends with every turn of a chat.")
                    .font(MobiusStyle.bodyFont)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .promptCard()
                    .mobiusLoadingPlaceholder("Loading system prompt")
            }
            Section(modelSectionTitle) {
                SettingsLoadingRows(label: "Loading model") {
                    LabeledContent("Model") { Text("Provider · model") }
                    Text("Maximum model steps: 500")
                }
            }
            Section("Capabilities") {
                SettingsLoadingRows(label: "Loading capabilities") {
                    SettingsRowLabel(title: "Web search", detail: "Tools this agent may call.")
                    SettingsRowLabel(title: "File edits", detail: "Tools this agent may call.")
                    SettingsRowLabel(title: "Shell commands", detail: "Tools this agent may call.")
                }
            }
        }
    }

    private var configurationStatusAccessory: some View {
        SettingsStatusAccessory(
            subject: .localized(scope == .botDefaults ? "Bot defaults" : "Bot"),
            hasChanges: hasChanges,
            isSaving: model.isApplyingConfiguration,
            saveDisabled: model.isApplyingConfiguration || !canSave,
            statusLabel: .localized(agentStatusLabel),
            statusDetail: agentStatusDetail,
            statusColor: agentStatusColor,
            saveLabel: .localized(applyTitle),
            secondaryActionLabel: reloadActionLabel.map { .localized($0) },
            secondaryAction: reloadAction,
            save: applyConfiguration
        )
    }

    private var applyTitle: LocalizedStringResource {
        switch scope {
        case .bot: "Save Bot"
        case .botDefaults: "Save Bot defaults"
        }
    }

    private var canSave: Bool {
        switch scope {
        case .botDefaults: true
        case .bot(let id): model.canMutateBot(id)
        }
    }

    private func applyConfiguration() {
        switch scope {
        case .bot: model.saveBotDraft()
        case .botDefaults: model.saveBotDefaults()
        }
    }

    private var agentStatusLabel: LocalizedStringResource {
        guard draft != nil else { return "Unavailable" }
        return switch applyState {
        case .idle, .applied:
            hasChanges ? "Unsaved changes" : "Up to date"
        case .applying: "Applying configuration"
        case .restarting: "Restarting"
        case .busy: "Busy"
        case .conflict: "Changed elsewhere"
        case .invalid: "Configuration rejected"
        case .failed: "Failed"
        }
    }

    private var agentStatusColor: Color {
        guard draft != nil else { return palette.danger }
        return switch applyState {
        case .idle, .applied:
            hasChanges ? palette.warning : palette.signal
        case .applying:
            palette.accent
        case .restarting, .busy, .conflict:
            palette.warning
        case .invalid, .failed:
            palette.danger
        }
    }

    private var agentStatusDetail: MobiusText {
        guard draft != nil else { return .localized(unavailableDetail) }
        return switch applyState {
        case .idle, .applied:
            .localized(hasChanges ? unsavedStatusDetail : savedStatusDetail)
        case .applying:
            .localized("The gateway is validating this revision.")
        case .restarting:
            .localized("The gateway accepted the configuration and is reopening the session.")
        case .busy(let message), .conflict(let message), .invalid(let message), .failed(let message):
            .verbatim(message)
        }
    }

    @ViewBuilder
    private func capabilityRow(_ feature: MiddlewareFeature) -> some View {
        let summary = capabilitySummary(feature)
        if capabilityHasDetails(feature) {
            HStack(spacing: MobiusSpace.s) {
                Button {
                    editingCapability = feature
                } label: {
                    HStack(spacing: MobiusSpace.s) {
                        SettingsRowLabel(
                            title: .verbatim(feature.label),
                            detail: summary
                        )
                        MobiusIcon(
                            .caretRight,
                            size: MobiusStyle.glyphMark,
                            foreground: palette.muted
                        )
                        .accessibilityHidden(true)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("\(feature.label) settings")
                .accessibilityValue(summary.text)
                .accessibilityHint("\(feature.description). Opens settings")
                .help("Edit \(feature.label) settings")

                Toggle(isOn: middleware(feature)) {
                    Text(verbatim: feature.label)
                }
                    .labelsHidden()
                    .disabled(feature.required)
                    .accessibilityHint(Text(verbatim: feature.description))
            }
        } else {
            Toggle(isOn: middleware(feature)) {
                Text(verbatim: feature.label)
            }
                .disabled(feature.required)
                .accessibilityHint(Text(verbatim: feature.description))
                .help(Text(verbatim: feature.description))
        }
    }

    private func capabilityEditor(_ feature: MiddlewareFeature) -> some View {
        NavigationStack {
            Form {
                Section {
                    Toggle("Enabled", isOn: middleware(feature))
                        .disabled(feature.required)
                } footer: {
                    Text(verbatim: feature.description)
                }

                if !feature.settings.isEmpty {
                    Section("Settings") {
                        ForEach(feature.settings) { setting in
                            middlewareSetting(feature, setting)
                        }
                    }
                }

                let availableExtensions = extensions(for: feature)
                if !availableExtensions.isEmpty {
                    Section("Extensions") {
                        ForEach(availableExtensions) { extensionRecord in
                            extensionActivation(feature, extensionRecord)
                        }
                    }
                }
            }
            .navigationTitle(Text(verbatim: feature.label))
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { editingCapability = nil }
                }
            }
        }
        .mobiusSheet(detents: [.large])
    }

    private func capabilityHasDetails(_ feature: MiddlewareFeature) -> Bool {
        !feature.settings.isEmpty || !extensions(for: feature).isEmpty
    }

    private func capabilitySummary(_ feature: MiddlewareFeature) -> MobiusText {
        let settings = feature.settings.map { settingSummary(feature, $0) }
        let availableExtensions = extensions(for: feature)
        guard !availableExtensions.isEmpty else { return joined(settings) }
        let active = availableExtensions.filter { draft?.extensions.contains($0.id) == true }
        let extensionSummary: MobiusText = active.isEmpty
            ? .localized("No extensions active")
            : .verbatim(active.map(\.name).joined(separator: ", "))
        return joined(settings + [extensionSummary])
    }

    private func settingSummary(
        _ feature: MiddlewareFeature,
        _ setting: FrontendSetting
    ) -> MobiusText {
        switch setting.kind {
        case .integer(let minimum, let maximum, _):
            let value = integerSetting(
                feature,
                setting,
                minimum: minimum,
                maximum: maximum
            ).wrappedValue
            return .localized("\(setting.label): \(value.formatted())")
        case .select(let options, let unsetLabel):
            guard let selected = selectSetting(feature, setting).wrappedValue else {
                return unsetLabel.map(MobiusText.verbatim) ?? .localized("Not set")
            }
            if let choice = model.modelChoices.first(where: { $0.route == selected }) {
                return .verbatim(model.modelLabel(for: choice))
            }
            return .verbatim(options.first { $0.value == selected }?.label ?? selected)
        }
    }

    private func joined(_ values: [MobiusText]) -> MobiusText {
        guard let first = values.first else { return .verbatim("") }
        guard values.count > 1 else { return first }
        return .verbatim(values.map { $0.resolved(locale: locale) }.joined(separator: " · "))
    }

    private var reloadActionLabel: LocalizedStringResource? {
        if case .conflict = applyState { "Reload" } else { nil }
    }

    private var reloadAction: (() -> Void)? {
        guard reloadActionLabel != nil else { return nil }
        return { reloadDraft() }
    }

    private func extensions(for feature: MiddlewareFeature) -> [ExtensionRecord] {
        model.extensions.filter { $0.capability == feature.id }
    }

    private func extensionActivation(
        _ feature: MiddlewareFeature,
        _ extensionRecord: ExtensionRecord
    ) -> some View {
        let selection = extensionSelection(extensionRecord)
        return HStack(spacing: MobiusSpace.xs) {
            VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                Text(verbatim: extensionRecord.name)
                extensionMetadata(extensionRecord).text
                    .font(MobiusStyle.captionFont)
                    .foregroundStyle(
                        extensionRecord.hooksTrusted ? palette.muted : palette.warning
                    )
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .accessibilityHidden(true)
            SettingsInfoButton(
                title: .verbatim(extensionRecord.name),
                detail: extensionDetail(extensionRecord)
            )
            Toggle(isOn: selection) {
                Text(verbatim: extensionRecord.name)
            }
                .labelsHidden()
                .accessibilityHint(extensionMetadata(extensionRecord).text)
                .disabled(!middlewareEnabled(feature) && !selection.wrappedValue)
        }
    }

    private func extensionSelection(_ extensionRecord: ExtensionRecord) -> Binding<Bool> {
        Binding(
            get: { draft?.extensions.contains(extensionRecord.id) ?? false },
            set: { isEnabled in
                updateDraft { draft in
                    if isEnabled { draft.extensions.insert(extensionRecord.id) }
                    else { draft.extensions.remove(extensionRecord.id) }
                }
            }
        )
    }

    private func extensionMetadata(_ extensionRecord: ExtensionRecord) -> MobiusText {
        let hooksDisabled = !extensionRecord.hooks.isEmpty && !extensionRecord.hooksTrusted
        return switch (extensionRecord.kind, extensionRecord.version, hooksDisabled) {
        case (.plugin, .some(let version), true):
            .localized("Plugin · \(version) · Hooks disabled until trusted")
        case (.plugin, .some(let version), false): .localized("Plugin · \(version)")
        case (.plugin, .none, true): .localized("Plugin · Hooks disabled until trusted")
        case (.plugin, .none, false): .localized("Plugin")
        case (.skill, .some(let version), true):
            .localized("Skill · \(version) · Hooks disabled until trusted")
        case (.skill, .some(let version), false): .localized("Skill · \(version)")
        case (.skill, .none, true): .localized("Skill · Hooks disabled until trusted")
        case (.skill, .none, false): .localized("Skill")
        }
    }

    private func extensionDetail(_ extensionRecord: ExtensionRecord) -> MobiusText {
        guard !extensionRecord.hooksTrusted else {
            return .verbatim(extensionRecord.description)
        }
        return .localized("\(extensionRecord.description) Its skills can be active now; executable hooks remain disabled until trusted on the Extensions page.")
    }

    @ViewBuilder
    private func middlewareSetting(
        _ feature: MiddlewareFeature,
        _ setting: FrontendSetting
    ) -> some View {
        switch setting.kind {
        case .integer(let minimum, let maximum, let step):
            let value = integerSetting(
                feature,
                setting,
                minimum: minimum,
                maximum: maximum
            )
            let increment = Swift.max(Int(clamping: step), 1)
            HStack(spacing: MobiusSpace.xs) {
                if let maximum {
                    Stepper(
                        value: value,
                        in: minimum...maximum,
                        step: increment
                    ) {
                        Text("\(setting.label): \(value.wrappedValue.formatted())")
                    }
                    .disabled(!middlewareEnabled(feature))
                } else {
                    Stepper(value: value, step: increment) {
                        Text("\(setting.label): \(value.wrappedValue.formatted())")
                    }
                    .disabled(!middlewareEnabled(feature))
                }
                SettingsInfoButton(
                    title: .verbatim(setting.label),
                    detail: .verbatim(setting.description)
                )
            }
            .sensoryFeedback(.selection, trigger: value.wrappedValue)
        case .select(let options, let unsetLabel)
            where options.allSatisfy({ option in
                model.modelChoices.contains { $0.route == option.value }
            }) && !options.isEmpty:
            // The gateway advertises reviewer and subagent models as plain selects over
            // routes. They are model choices like any other, so they get the same split.
            ModelRoutePicker(
                verbatimLabel: setting.label,
                detail: setting.description,
                choices: options.compactMap { option in
                    model.modelChoices.first { $0.route == option.value }
                },
                unsetLabel: unsetLabel,
                isEnabled: middlewareEnabled(feature),
                route: selectSetting(feature, setting)
            )
        case .select(let options, let unsetLabel):
            let selection = selectSetting(feature, setting)
            let selectedDescription = selection.wrappedValue.flatMap { selected in
                options.first { $0.value == selected }?.description
            }
            let selectedLabel: MobiusText = selection.wrappedValue.map { selected in
                .verbatim(options.first { $0.value == selected }?.label ?? selected)
            } ?? unsetLabel.map(MobiusText.verbatim) ?? .localized("Select")
            LabeledContent {
                Menu {
                    Picker(selection: selection) {
                        if let unsetLabel {
                            Text(verbatim: unsetLabel).tag(String?.none)
                        }
                        ForEach(options) { option in
                            Text(verbatim: option.label).tag(Optional(option.value))
                        }
                    } label: { Text(verbatim: setting.label) }
                    .labelsHidden()
                } label: {
                    HStack(spacing: MobiusSpace.xs) {
                        selectedLabel.text
                        MobiusIcon(.caretUpDown, size: MobiusStyle.glyphMark, gutter: false)
                            .accessibilityHidden(true)
                    }
                    .foregroundStyle(palette.accent)
                }
                .menuIndicator(.hidden)
                .buttonStyle(.mobiusPlain)
                .disabled(!middlewareEnabled(feature))
                .accessibilityLabel(Text(verbatim: setting.label))
                .accessibilityValue(selectedLabel.text)
            } label: {
                HStack(spacing: MobiusSpace.xs) {
                    Text(verbatim: setting.label)
                    SettingsInfoButton(
                        title: .verbatim(setting.label),
                        detail: .verbatim(selectedDescription ?? setting.description)
                    )
                }
            }
            .sensoryFeedback(.selection, trigger: selection.wrappedValue)
        }
    }

    private var systemPrompt: Binding<String> {
        Binding(
            get: { draft?.systemPrompt ?? "" },
            set: { value in updateDraft { $0.systemPrompt = value } }
        )
    }

    private var maxModelSteps: Binding<UInt64> {
        Binding(
            get: { draft?.maxModelSteps ?? 1 },
            set: { value in updateDraft { $0.maxModelSteps = Swift.max(value, 1) } }
        )
    }

    private func middleware(_ feature: MiddlewareFeature) -> Binding<Bool> {
        Binding(
            get: { middlewareEnabled(feature) },
            set: { isEnabled in
                guard !feature.required, var enabled = draft?.middleware.enabled else { return }
                if isEnabled { enabled.insert(feature.id) }
                else { enabled.remove(feature.id) }
                updateDraft { $0.middleware.enabled = enabled }
            }
        )
    }

    private func middlewareEnabled(_ feature: MiddlewareFeature) -> Bool {
        feature.required || (draft?.middleware.enabled.contains(feature.id) ?? false)
    }

    private func integerSetting(
        _ feature: MiddlewareFeature,
        _ setting: FrontendSetting,
        minimum: Int64,
        maximum: Int64?
    ) -> Binding<Int64> {
        Binding(
            get: {
                guard let configured = draft?
                    .middleware.settings[feature.id]?[setting.id],
                    case .integer(let value) = configured
                else { return minimum }
                return value
            },
            set: { value in
                let bounded = maximum.map { Swift.min(Swift.max(value, minimum), $0) }
                    ?? Swift.max(value, minimum)
                updateDraft {
                    $0.middleware.setSetting(
                        .integer(bounded),
                        middleware: feature.id,
                        setting: setting.id
                    )
                }
            }
        )
    }

    private func selectSetting(
        _ feature: MiddlewareFeature,
        _ setting: FrontendSetting
    ) -> Binding<String?> {
        Binding(
            get: {
                guard let configured = draft?
                    .middleware.settings[feature.id]?[setting.id],
                    case .string(let value) = configured
                else { return nil }
                return value
            },
            set: { value in
                updateDraft {
                    $0.middleware.setSetting(
                        value.map(FrontendSettingValue.string),
                        middleware: feature.id,
                        setting: setting.id
                    )
                }
            }
        )
    }

    private var draft: AgentComposition? {
        switch scope {
        case .botDefaults: model.botDefaultsDraft
        case .bot: model.botDraft
        }
    }

    private var snapshot: VersionedAgentConfig? {
        switch scope {
        case .botDefaults: model.botDefaultsSnapshot
        case .bot(let id): model.bots.first { $0.id == id }?.config
        }
    }

    private var applyState: ApplyState {
        switch scope {
        case .botDefaults: model.botDefaultsApplyState
        case .bot: model.botApplyState
        }
    }

    private var selectedModelRoute: String? {
        switch scope {
        case .botDefaults: model.botDefaultsDraftModelRoute
        case .bot: model.botDraftModelRoute
        }
    }

    private var hasChanges: Bool {
        guard let snapshot, let draft else { return false }
        if case .bot(let id) = scope,
           let bot = model.bots.first(where: { $0.id == id }),
           bot.name != model.botNameDraft.trimmingCharacters(in: .whitespacesAndNewlines)
            || bot.description != model.botDescriptionDraft.trimmingCharacters(
                in: .whitespacesAndNewlines
            )
            || bot.tint != model.botTintDraft {
            return true
        }
        return snapshot.config != draft
    }

    private func updateDraft(_ update: (inout AgentComposition) -> Void) {
        guard var draft else { return }
        update(&draft)
        switch scope {
        case .botDefaults: model.botDefaultsDraft = draft
        case .bot: model.botDraft = draft
        }
    }

    private func selectModel(_ route: String) {
        switch scope {
        case .botDefaults: model.selectBotDefaultsDraftModel(route)
        case .bot: model.selectBotDraftModel(route)
        }
    }

    private func reloadDraft() {
        switch scope {
        case .botDefaults: model.reloadBotDefaultsDraft()
        case .bot: model.reloadBotDraft()
        }
    }

    private var pageTitle: LocalizedStringResource {
        scope == .botDefaults ? "Bot defaults" : "Bot settings"
    }

    private var pageDetail: LocalizedStringResource {
        switch scope {
        case .botDefaults:
            "The prompt, model, and capabilities new Bots start from."
        case .bot:
            "The durable prompt, model, and capabilities used by this Bot."
        }
    }

    private var modelSectionTitle: LocalizedStringResource {
        scope == .botDefaults ? "Bot defaults model" : "Bot AI model"
    }

    private var modelSectionDetail: LocalizedStringResource {
        switch scope {
        case .botDefaults:
            "Sets the provider, model, and reasoning inherited by new Bots."
        case .bot:
            "Sets the provider, model, and reasoning used by this Bot."
        }
    }

    private var unavailableTitle: LocalizedStringResource {
        scope == .botDefaults ? "Bot defaults unavailable" : "Bot unavailable"
    }

    private var unavailableDetail: LocalizedStringResource {
        guard model.connectionState.isReady else { return "Connect to a gateway first." }
        if case .bot(let id) = scope, !model.bots.contains(where: { $0.id == id }) {
            return "Choose a Bot first."
        }
        return "Configure a provider first."
    }

    private var unsavedStatusDetail: LocalizedStringResource {
        scope == .botDefaults
            ? "Save this draft as the defaults for new Bots."
            : "Save this durable Bot configuration."
    }

    private var savedStatusDetail: LocalizedStringResource {
        scope == .botDefaults
            ? "The draft matches the saved Bot defaults."
            : "The draft matches this Bot's saved configuration."
    }
}
