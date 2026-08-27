import SwiftUI

enum AgentSettingsScope: Equatable {
    case gatewayDefault
    case currentChat
}

struct AgentSettingsView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var editingCapability: MiddlewareFeature?
    let scope: AgentSettingsScope

    var body: some View {
        PageScaffold(
            title: pageTitle,
            detail: pageDetail,
            sharesHeaderBackground: true,
            headerAccessory: { agentStatusAccessory }
        ) {
            if draft != nil {
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
    /// rather than as an agent that is missing.
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
                    LabeledContent("Model", value: "Provider · model")
                    Text("Maximum model steps: 500")
                }
            }
            Section("Capabilities") {
                SettingsLoadingRows(label: "Loading capabilities") {
                    ForEach(["Web search", "File edits", "Shell commands"], id: \.self) { name in
                        SettingsRowLabel(title: name, detail: "Tools this agent may call.")
                    }
                }
            }
        }
    }

    private var agentStatusAccessory: some View {
        SettingsStatusAccessory(
            subject: "Agent",
            hasChanges: hasChanges,
            isSaving: model.isApplyingConfiguration,
            saveDisabled: model.isApplyingConfiguration,
            statusLabel: agentStatusLabel,
            statusDetail: agentStatusDetail,
            statusColor: agentStatusColor,
            saveLabel: applyTitle,
            secondaryActionLabel: reloadActionLabel,
            secondaryAction: reloadAction,
            save: applyConfiguration
        )
    }

    private var applyTitle: String {
        switch scope {
        case .currentChat: "Apply to this chat"
        case .gatewayDefault: "Save as gateway default"
        }
    }

    private func applyConfiguration() {
        switch scope {
        case .currentChat: model.changeAgentForCurrentChat()
        case .gatewayDefault: model.saveAgentAsDefault()
        }
    }

    private var agentStatusLabel: String {
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

    private var agentStatusDetail: String {
        guard draft != nil else { return unavailableDetail }
        return switch applyState {
        case .idle, .applied:
            hasChanges ? unsavedStatusDetail : savedStatusDetail
        case .applying:
            "The gateway is validating this revision."
        case .restarting:
            "The gateway accepted the configuration and is reopening the session."
        case .busy(let message), .conflict(let message), .invalid(let message), .failed(let message):
            message
        }
    }

    @ViewBuilder
    private func capabilityRow(_ feature: MiddlewareFeature) -> some View {
        if capabilityHasDetails(feature) {
            HStack(spacing: MobiusSpace.s) {
                Button {
                    editingCapability = feature
                } label: {
                    HStack(spacing: MobiusSpace.s) {
                        SettingsRowLabel(
                            title: feature.label,
                            detail: capabilitySummary(feature)
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
                .accessibilityValue(capabilitySummary(feature))
                .accessibilityHint("\(feature.description). Opens settings")
                .help("Edit \(feature.label) settings")

                Toggle(feature.label, isOn: middleware(feature))
                    .labelsHidden()
                    .disabled(feature.required)
                    .accessibilityHint(feature.description)
            }
        } else {
            Toggle(feature.label, isOn: middleware(feature))
                .disabled(feature.required)
                .accessibilityHint(feature.description)
                .help(feature.description)
        }
    }

    private func capabilityEditor(_ feature: MiddlewareFeature) -> some View {
        NavigationStack {
            Form {
                Section {
                    Toggle("Enabled", isOn: middleware(feature))
                        .disabled(feature.required)
                } footer: {
                    Text(feature.description)
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
            .navigationTitle(feature.label)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { editingCapability = nil }
                }
            }
        }
    }

    private func capabilityHasDetails(_ feature: MiddlewareFeature) -> Bool {
        !feature.settings.isEmpty || !extensions(for: feature).isEmpty
    }

    private func capabilitySummary(_ feature: MiddlewareFeature) -> String {
        let settings = feature.settings.map { settingSummary(feature, $0) }
        let availableExtensions = extensions(for: feature)
        guard !availableExtensions.isEmpty else { return settings.joined(separator: " · ") }
        let active = availableExtensions.filter { draft?.extensions.contains($0.id) == true }
        let extensionSummary = active.isEmpty
            ? "No extensions active"
            : active.map(\.name).joined(separator: ", ")
        return (settings + [extensionSummary]).joined(separator: " · ")
    }

    private func settingSummary(
        _ feature: MiddlewareFeature,
        _ setting: FrontendSetting
    ) -> String {
        switch setting.kind {
        case .integer(let minimum, let maximum, _):
            let value = integerSetting(
                feature,
                setting,
                minimum: minimum,
                maximum: maximum
            ).wrappedValue
            return "\(setting.label): \(value.formatted())"
        case .select(let options, let unsetLabel):
            guard let selected = selectSetting(feature, setting).wrappedValue else {
                return unsetLabel ?? "Not set"
            }
            if let choice = model.modelChoices.first(where: { $0.route == selected }) {
                return model.modelLabel(for: choice)
            }
            return options.first { $0.value == selected }?.label ?? selected
        }
    }

    private var reloadActionLabel: String? {
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
                Text(extensionRecord.name)
                Text(extensionMetadata(extensionRecord))
                    .font(MobiusStyle.captionFont)
                    .foregroundStyle(
                        extensionRecord.hooksTrusted ? palette.muted : palette.warning
                    )
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .accessibilityHidden(true)
            SettingsInfoButton(
                title: extensionRecord.name,
                detail: extensionDetail(extensionRecord)
            )
            Toggle(extensionRecord.name, isOn: selection)
                .labelsHidden()
                .accessibilityHint(extensionMetadata(extensionRecord))
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

    private func extensionMetadata(_ extensionRecord: ExtensionRecord) -> String {
        var metadata = [extensionRecord.kind == .plugin ? "Plugin" : "Skill"]
        if let version = extensionRecord.version { metadata.append(version) }
        if !extensionRecord.hooks.isEmpty && !extensionRecord.hooksTrusted {
            metadata.append("Hooks disabled until trusted")
        }
        return metadata.joined(separator: " · ")
    }

    private func extensionDetail(_ extensionRecord: ExtensionRecord) -> String {
        guard !extensionRecord.hooksTrusted else { return extensionRecord.description }
        return extensionRecord.description
            + " Its skills can be active now; executable hooks remain disabled until trusted on the Extensions page."
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
                SettingsInfoButton(title: setting.label, detail: setting.description)
            }
            .sensoryFeedback(.selection, trigger: value.wrappedValue)
        case .select(let options, let unsetLabel)
            where options.allSatisfy({ option in
                model.modelChoices.contains { $0.route == option.value }
            }) && !options.isEmpty:
            // The gateway advertises reviewer and subagent models as plain selects over
            // routes. They are model choices like any other, so they get the same split.
            ModelRoutePicker(
                label: setting.label,
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
            let selectedLabel = selection.wrappedValue.flatMap { selected in
                options.first { $0.value == selected }?.label ?? selected
            } ?? unsetLabel ?? "Select"
            LabeledContent {
                Menu {
                    Picker(setting.label, selection: selection) {
                        if let unsetLabel {
                            Text(unsetLabel).tag(String?.none)
                        }
                        ForEach(options) { option in
                            Text(option.label).tag(Optional(option.value))
                        }
                    }
                    .labelsHidden()
                } label: {
                    HStack(spacing: MobiusSpace.xs) {
                        Text(selectedLabel)
                        MobiusIcon(.caretUpDown, size: MobiusStyle.glyphMark, gutter: false)
                            .accessibilityHidden(true)
                    }
                    .foregroundStyle(palette.accent)
                }
                .menuIndicator(.hidden)
                .buttonStyle(.mobiusPlain)
                .disabled(!middlewareEnabled(feature))
                .accessibilityLabel(setting.label)
                .accessibilityValue(selectedLabel)
            } label: {
                HStack(spacing: MobiusSpace.xs) {
                    Text(setting.label)
                    SettingsInfoButton(
                        title: setting.label,
                        detail: selectedDescription ?? setting.description
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
        case .gatewayDefault: model.defaultAgentDraft
        case .currentChat: model.agentDraft
        }
    }

    private var snapshot: VersionedAgentConfig? {
        switch scope {
        case .gatewayDefault: model.defaultAgentSnapshot
        case .currentChat: model.agentSnapshot
        }
    }

    private var applyState: ApplyState {
        switch scope {
        case .gatewayDefault: model.defaultAgentApplyState
        case .currentChat: model.chatAgentApplyState
        }
    }

    private var selectedModelRoute: String? {
        switch scope {
        case .gatewayDefault: model.defaultAgentDraftModelRoute
        case .currentChat: model.agentDraftModelRoute
        }
    }

    private var hasChanges: Bool {
        guard let snapshot, let draft else { return false }
        return snapshot.config != draft
    }

    private func updateDraft(_ update: (inout AgentComposition) -> Void) {
        guard var draft else { return }
        update(&draft)
        switch scope {
        case .gatewayDefault: model.defaultAgentDraft = draft
        case .currentChat: model.agentDraft = draft
        }
    }

    private func selectModel(_ route: String) {
        switch scope {
        case .gatewayDefault: model.selectDefaultAgentDraftModel(route)
        case .currentChat: model.selectAgentDraftModel(route)
        }
    }

    private func reloadDraft() {
        switch scope {
        case .gatewayDefault: model.reloadDefaultAgentDraft()
        case .currentChat: model.reloadAgentDraft()
        }
    }

    private var pageTitle: String {
        scope == .gatewayDefault ? "Default agent" : "Chat agent"
    }

    private var pageDetail: String {
        switch scope {
        case .gatewayDefault:
            "The prompt, model, and capabilities new chats start from."
        case .currentChat:
            "The prompt, model, and capabilities for this chat only."
        }
    }

    private var modelSectionTitle: String {
        scope == .gatewayDefault ? "Default AI model" : "Chat AI model"
    }

    private var modelSectionDetail: String {
        switch scope {
        case .gatewayDefault:
            "Sets the provider, model, and reasoning inherited by new chats."
        case .currentChat:
            "Sets the provider, model, and reasoning used by this chat."
        }
    }

    private var unavailableTitle: String {
        scope == .gatewayDefault ? "Default agent unavailable" : "Chat agent unavailable"
    }

    private var unavailableDetail: String {
        guard model.connectionState.isReady else { return "Connect to a gateway first." }
        if scope == .currentChat, model.selectedSessionID == nil { return "Open a chat first." }
        return "Configure a provider first."
    }

    private var unsavedStatusDetail: String {
        scope == .gatewayDefault
            ? "Save this draft as the gateway default for new chats."
            : "Apply this draft to the current chat."
    }

    private var savedStatusDetail: String {
        scope == .gatewayDefault
            ? "The draft matches the gateway default."
            : "The draft matches this chat's saved agent configuration."
    }
}
