import SwiftUI

/// Shared model sheet and reasoning slider for composer and configuration drafts.
struct ModelRoutePicker: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let label: MobiusText
    let detail: MobiusText
    let choices: [ModelChoice]
    var unsetLabel: String?
    var isEnabled = true
    @Binding var route: String?

    init(
        label: LocalizedStringResource,
        detail: LocalizedStringResource,
        choices: [ModelChoice],
        unsetLabel: String? = nil,
        isEnabled: Bool = true,
        route: Binding<String?>,
        voices: [String] = [],
        voice: Binding<String?>? = nil,
        onSelectModel: (() -> Void)? = nil
    ) {
        self.label = .localized(label)
        self.detail = .localized(detail)
        self.choices = choices
        self.unsetLabel = unsetLabel
        self.isEnabled = isEnabled
        _route = route
        self.voices = voices
        self.voice = voice
        self.onSelectModel = onSelectModel
    }

    init(
        verbatimLabel label: String,
        detail: String,
        choices: [ModelChoice],
        unsetLabel: String? = nil,
        isEnabled: Bool = true,
        route: Binding<String?>
    ) {
        self.label = .verbatim(label)
        self.detail = .verbatim(detail)
        self.choices = choices
        self.unsetLabel = unsetLabel
        self.isEnabled = isEnabled
        _route = route
    }

    @State private var showsModels = false
    @State private var reasoningPreview: Double?
    var voices: [String] = []
    var voice: Binding<String?>?
    var onSelectModel: (() -> Void)?

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.m) {
            HStack(spacing: MobiusSpace.s) {
                Button {
                    if let onSelectModel { onSelectModel() } else { showsModels = true }
                } label: {
                    MobiusMenuLabel(
                        text: selectedModelLabel,
                        glyph: selectedGlyph,
                        detail: selected.map { _ in .verbatim("• \(previewEffortLabel)") },
                        glyphColor: selectedTint,
                        font: MobiusStyle.bodyFont
                    )
                }
                .buttonStyle(.mobiusPlain)
                .accessibilityLabel(label.text)
                .accessibilityValue(selectedModelLabel.text)
                SettingsInfoButton(title: label, detail: detail)
            }
            .frame(maxWidth: .infinity, alignment: .center)
            if reasoningChoices.count > 1 {
                Slider(
                    value: Binding(
                        get: { Double(previewReasoningIndex) },
                        set: { reasoningPreview = $0 }
                    ),
                    in: 0...Double(reasoningChoices.count - 1),
                    step: 1
                ) {
                    Text("Reasoning effort")
                } onEditingChanged: { editing in
                    if !editing, reasoningPreview != nil {
                        route = reasoningChoices[previewReasoningIndex].route
                        self.reasoningPreview = nil
                    }
                }
                .labelsHidden()
                .accessibilityValue(Text(verbatim: previewEffortLabel))
                .accessibilityAdjustableAction { direction in
                    let delta = direction == .increment ? 1 : -1
                    let index = min(
                        max(selectedReasoningIndex + delta, 0), reasoningChoices.count - 1)
                    route = reasoningChoices[index].route
                }
                .tint(palette.accent)
            }
        }
        .alignmentGuide(.listRowSeparatorLeading) { $0[.leading] }
        .disabled(!isEnabled)
        .sensoryFeedback(.selection, trigger: reasoningPreview)
        .onChange(of: route) { reasoningPreview = nil }
        .onChange(of: choices) { reasoningPreview = nil }
        .sheet(isPresented: $showsModels) {
            ModelSelectionSheet(
                choices: choices, unsetLabel: unsetLabel, isEnabled: isEnabled,
                route: $route, voices: voices, voice: voice
            )
        }
    }

    private var selected: ModelChoice? { choices.first { $0.route == route } }

    private var selectedModelLabel: MobiusText {
        if let selected { return .verbatim(model.modelLabel(for: selected)) }
        if let unsetLabel { return .verbatim(unsetLabel) }
        return .localized("Select model")
    }

    private var selectedGlyph: MobiusGlyph {
        selected.flatMap { model.providerSymbol(for: $0) }
            .flatMap { MobiusSymbol.knownGlyph(for: $0) } ?? .aiScan
    }

    private var selectedTint: Color {
        selected.map { model.providerTint(for: $0).color } ?? palette.accent
    }

    private var reasoningChoices: [ModelChoice] {
        guard let selected else { return [] }
        return model.modelChoices(matching: selected, in: choices)
    }

    private var selectedReasoningIndex: Int {
        reasoningChoices.firstIndex { $0.route == route } ?? 0
    }

    private var previewEffortLabel: String {
        model.reasoningLabel(for: reasoningChoices[previewReasoningIndex])
    }

    private var previewReasoningIndex: Int {
        min(
            max(Int(reasoningPreview ?? Double(selectedReasoningIndex)), 0),
            reasoningChoices.count - 1)
    }

}

struct ModelSelectionSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let choices: [ModelChoice]
    var unsetLabel: String?
    var isEnabled = true
    @Binding var route: String?
    var voices: [String] = []
    var voice: Binding<String?>?

    var body: some View {
        NavigationStack {
            Form {
                Section("Model") {
                    if let unsetLabel {
                        Button {
                            route = nil
                        } label: {
                            HStack {
                                Text(verbatim: unsetLabel)
                                Spacer()
                                if route == nil { MobiusIcon(.check) }
                            }
                        }
                        .foregroundStyle(.primary)
                    }
                    ForEach(model.distinctModels(in: choices), id: \.route) { choice in
                        modelRow(choice)
                    }
                }
                if !voices.isEmpty, let voice {
                    Section {
                        Picker(
                            "Voice",
                            selection: Binding(
                                get: { voice.wrappedValue ?? voices[0] },
                                set: { voice.wrappedValue = $0 }
                            )
                        ) {
                            ForEach(voices, id: \.self) { value in
                                Text(verbatim: value.capitalized).tag(value)
                            }
                        }
                    }
                }
            }
            .disabled(!isEnabled)
            .navigationTitle("Configure")
            .toolbarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
        }
        .mobiusSheet()
    }

    private func modelRow(_ choice: ModelChoice) -> some View {
        let isSelected = selected.map { model.sameModel($0, choice) } == true
        return Button {
            route = model.modelRoute(selecting: choice, preserving: selected, in: choices)
        } label: {
            HStack(spacing: MobiusSpace.s) {
                MobiusIcon(
                    model.providerSymbol(for: choice).flatMap(MobiusSymbol.knownGlyph(for:))
                        ?? .aiScan,
                    foreground: model.providerTint(for: choice).color
                )
                Text(
                    verbatim:
                        "\(model.modelProviders[choice.route].map { model.providerLabel(for: $0) } ?? choice.group) • \(model.modelLabel(for: choice))"
                )
                .frame(maxWidth: .infinity, alignment: .leading)
                if isSelected { MobiusIcon(.check) }
            }
            .contentShape(Rectangle())
        }
        .foregroundStyle(.primary)
        .accessibilityAddTraits(isSelected ? .isSelected : [])
    }

    private var selected: ModelChoice? { choices.first { $0.route == route } }
}
