import SwiftUI

/// Model and reasoning as two rows, in the composer's glyph-led menu style.
///
/// The gateway advertises one route per model-and-effort pair, so one combined list
/// multiplies every model by every effort and buries the choice that matters. Split, each
/// list stays short and the effort reads as its own decision. The reasoning row appears only
/// when the chosen model actually offers more than one.
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
        route: Binding<String?>
    ) {
        self.label = .localized(label)
        self.detail = .localized(detail)
        self.choices = choices
        self.unsetLabel = unsetLabel
        self.isEnabled = isEnabled
        _route = route
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

    var body: some View {
        LabeledContent {
            Menu {
                Picker(selection: modelSelection) {
                    if let unsetLabel {
                        Text(verbatim: unsetLabel).tag(String?.none)
                    }
                    ForEach(distinctModels, id: \.route) { choice in
                        optionLabel(
                            model.modelLabel(for: choice),
                            symbol: model.providerSymbol(for: choice),
                            tint: model.providerTint(for: choice)
                        )
                        .tag(Optional(choice.route))
                    }
                } label: { label.text }
                .labelsHidden()
            } label: {
                menuLabel(selectedModelLabel, glyph: selectedGlyph)
            }
            .menuIndicator(.hidden)
            .buttonStyle(.mobiusPlain)
            .disabled(!isEnabled)
            .accessibilityLabel(label.text)
            .accessibilityValue(selectedModelLabel.text)
        } label: {
            HStack(spacing: MobiusSpace.xs) {
                label.text
                SettingsInfoButton(title: label, detail: detail)
            }
        }
        .sensoryFeedback(.selection, trigger: route)

        if reasoningChoices.count > 1 {
            LabeledContent("Reasoning") {
                Menu {
                    Picker("Reasoning", selection: reasoningSelection) {
                        ForEach(reasoningChoices, id: \.route) { choice in
                            effortLabel(choice).text.tag(choice.route)
                        }
                    }
                    .labelsHidden()
                } label: {
                    menuLabel(selectedEffortLabel, glyph: nil)
                }
                .menuIndicator(.hidden)
                .buttonStyle(.mobiusPlain)
                .disabled(!isEnabled)
                .accessibilityLabel("Reasoning")
                .accessibilityValue(selectedEffortLabel.text)
            }
        }
    }

    private func menuLabel(_ text: MobiusText, glyph: MobiusGlyph?) -> some View {
        MobiusMenuLabel(
            text: text,
            glyph: glyph,
            glyphColor: selectedTint?.color,
            font: MobiusStyle.bodyFont
        )
        .foregroundStyle(palette.accent)
    }

    private var selectedTint: AccentTint? {
        selected.map { model.providerTint(for: $0) }
    }

    @ViewBuilder
    private func optionLabel(_ title: String, symbol: String?, tint: AccentTint) -> some View {
        if let symbol,
           let glyph = MobiusSymbol.knownGlyph(for: symbol),
           let image = glyph.menuImage(tint.color) {
            Label { Text(verbatim: title) } icon: { image }
        } else {
            Text(verbatim: title)
        }
    }

    private var selected: ModelChoice? {
        choices.first { $0.route == route }
    }

    private var selectedModelLabel: MobiusText {
        if let selected { return .verbatim(model.modelLabel(for: selected)) }
        if let unsetLabel { return .verbatim(unsetLabel) }
        return .localized("Select")
    }

    private var selectedGlyph: MobiusGlyph? {
        selected
            .flatMap { model.providerSymbol(for: $0) }
            .flatMap { MobiusSymbol.knownGlyph(for: $0) }
    }

    private func effortLabel(_ choice: ModelChoice) -> MobiusText {
        guard let effort = choice.reasoningEffort else { return .localized("Default") }
        return .verbatim(effort.capitalized)
    }

    private var selectedEffortLabel: MobiusText {
        guard let effort = selected?.reasoningEffort else { return .localized("Default") }
        return .verbatim(effort.capitalized)
    }

    private var distinctModels: [ModelChoice] {
        model.distinctModels(in: choices)
    }

    private var reasoningChoices: [ModelChoice] {
        guard let selected else { return [] }
        return model.modelChoices(matching: selected, in: choices)
    }

    /// Switching model keeps the effort when the new model offers the same one, so changing
    /// model does not silently reset reasoning to the provider default.
    private var modelSelection: Binding<String?> {
        Binding {
            guard let selected else { return nil }
            return distinctModels.first { model.sameModel($0, selected) }?.route ?? selected.route
        } set: { newRoute in
            guard let newRoute, let choice = choices.first(where: { $0.route == newRoute }) else {
                route = nil
                return
            }
            let effort = selected?.reasoningEffort
            route = choices.first {
                model.sameModel($0, choice) && $0.reasoningEffort == effort
            }?.route ?? choice.route
        }
    }

    private var reasoningSelection: Binding<String> {
        Binding { route ?? "" } set: { route = $0 }
    }
}
