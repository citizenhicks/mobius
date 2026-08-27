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
    let label: String
    let detail: String
    let choices: [ModelChoice]
    var unsetLabel: String?
    var isEnabled = true
    @Binding var route: String?

    var body: some View {
        LabeledContent {
            Menu {
                Picker(label, selection: modelSelection) {
                    if let unsetLabel {
                        Text(unsetLabel).tag(String?.none)
                    }
                    ForEach(distinctModels, id: \.route) { choice in
                        optionLabel(
                            model.modelGroupLabel(for: choice),
                            symbol: model.providerSymbol(for: choice),
                            tint: model.providerTint(for: choice)
                        )
                        .tag(Optional(choice.route))
                    }
                }
                .labelsHidden()
            } label: {
                menuLabel(selectedModelLabel, glyph: selectedGlyph)
            }
            .menuIndicator(.hidden)
            .buttonStyle(.mobiusPlain)
            .disabled(!isEnabled)
            .accessibilityLabel(label)
            .accessibilityValue(selectedModelLabel)
        } label: {
            HStack(spacing: MobiusSpace.xs) {
                Text(label)
                SettingsInfoButton(title: label, detail: detail)
            }
        }
        .sensoryFeedback(.selection, trigger: route)

        if reasoningChoices.count > 1 {
            LabeledContent("Reasoning") {
                Menu {
                    Picker("Reasoning", selection: reasoningSelection) {
                        ForEach(reasoningChoices, id: \.route) { choice in
                            Text(effortLabel(choice)).tag(choice.route)
                        }
                    }
                    .labelsHidden()
                } label: {
                    menuLabel(selected.map(effortLabel) ?? "Default", glyph: nil)
                }
                .menuIndicator(.hidden)
                .buttonStyle(.mobiusPlain)
                .disabled(!isEnabled)
                .accessibilityLabel("Reasoning")
                .accessibilityValue(selected.map(effortLabel) ?? "Default")
            }
        }
    }

    private func menuLabel(_ text: String, glyph: MobiusGlyph?) -> some View {
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
            Label { Text(title) } icon: { image }
        } else {
            Text(title)
        }
    }

    private var selected: ModelChoice? {
        choices.first { $0.route == route }
    }

    private var selectedModelLabel: String {
        guard let selected else { return unsetLabel ?? "Select" }
        return model.modelLabel(for: selected)
    }

    private var selectedGlyph: MobiusGlyph? {
        selected
            .flatMap { model.providerSymbol(for: $0) }
            .flatMap { MobiusSymbol.knownGlyph(for: $0) }
    }

    private func effortLabel(_ choice: ModelChoice) -> String {
        choice.reasoningEffort?.capitalized ?? "Default"
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
