import SwiftUI
import UIKit

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
                ReasoningSlider(
                    value: Binding(
                        get: { Double(previewReasoningIndex) },
                        set: { reasoningPreview = $0 }
                    ),
                    count: reasoningChoices.count,
                    valueLabel: previewEffortLabel
                ) {
                    if reasoningPreview != nil {
                        route = reasoningChoices[previewReasoningIndex].route
                        self.reasoningPreview = nil
                    }
                }
                .frame(height: MobiusStyle.rowTouch)
            }
        }
        .alignmentGuide(.listRowSeparatorLeading) { $0[.leading] }
        .disabled(!isEnabled)
        .sensoryFeedback(trigger: reasoningPreview) { _, preview in
            guard let preview else { return nil }
            return .impact(
                weight: .light,
                intensity: 0.4 + 0.6 * preview / Double(max(1, reasoningChoices.count - 1)))
        }
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

private struct ReasoningSlider: UIViewRepresentable {
    @Environment(\.isEnabled) private var isEnabled
    @Environment(\.locale) private var locale
    @Environment(\.mobiusPalette) private var palette
    @Binding var value: Double
    let count: Int
    let valueLabel: String
    let commit: () -> Void

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeUIView(context: Context) -> ThumbHeightSlider {
        let slider = ThumbHeightSlider()
        // Keep UIKit's live Liquid Glass thumb; custom thumb/track images opt out.
        slider.minimumTrackTintColor = .clear
        slider.maximumTrackTintColor = .clear
        slider.isAccessibilityElement = true
        slider.accessibilityTraits.insert(.adjustable)
        slider.addTarget(context.coordinator, action: #selector(Coordinator.began), for: .touchDown)
        slider.addTarget(
            context.coordinator, action: #selector(Coordinator.changed), for: .valueChanged)
        slider.addTarget(
            context.coordinator, action: #selector(Coordinator.ended),
            for: [.touchUpInside, .touchUpOutside, .touchCancel])
        return slider
    }

    func updateUIView(_ slider: ThumbHeightSlider, context: Context) {
        context.coordinator.owner = self
        slider.maximumValue = Float(count - 1)
        slider.trackConfiguration = .init(
            enabledRange: 0...slider.maximumValue, numberOfTicks: count)
        slider.value = Float(value)
        slider.isEnabled = isEnabled
        slider.tintColor = UIColor(palette.accent)
        slider.gradient.colors = [
            UIColor(palette.accentSoft).cgColor, UIColor(palette.accent).cgColor,
        ]
        slider.setNeedsLayout()
        slider.accessibilityLabel = String(localized: "Reasoning effort", locale: locale)
        slider.accessibilityValue = valueLabel
    }

    @MainActor final class Coordinator: NSObject {
        var owner: ReasoningSlider
        private var isEditing = false

        init(_ owner: ReasoningSlider) { self.owner = owner }

        @objc func began() { isEditing = true }

        @objc func changed(_ slider: UISlider) {
            owner.value = Double(slider.value.rounded())
            if !isEditing { owner.commit() }
        }

        @objc func ended() {
            isEditing = false
            owner.commit()
        }
    }

    final class ThumbHeightSlider: UISlider {
        private let thumbDiameter = MobiusStyle.iconButtonSize * 1.02
        let gradient = CAGradientLayer()
        private let trackBackground = CALayer()
        private let fillMask = CALayer()

        override init(frame: CGRect) {
            super.init(frame: frame)
            trackBackground.masksToBounds = true
            layer.insertSublayer(trackBackground, at: 0)
            gradient.startPoint = CGPoint(x: 0, y: 0.5)
            gradient.endPoint = CGPoint(x: 1, y: 0.5)
            fillMask.backgroundColor = UIColor.black.cgColor
            gradient.mask = fillMask
            trackBackground.addSublayer(gradient)
        }

        required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

        override func layoutSubviews() {
            super.layoutSubviews()
            let track = trackRect(forBounds: bounds)
            let thumb = thumbRect(forBounds: bounds, trackRect: track, value: value)
            let minimumThumb = thumbRect(forBounds: bounds, trackRect: track, value: minimumValue)
            let maximumThumb = thumbRect(forBounds: bounds, trackRect: track, value: maximumValue)
            let isReversed = minimumThumb.midX > maximumThumb.midX
            let edge = min(track.width, max(0, thumb.midX - track.minX))
            var fill = CGRect(
                x: isReversed ? edge : 0, y: 0,
                width: isReversed ? track.width - edge : edge, height: track.height)
            if value <= minimumValue { fill.size.width = 0 }
            if value >= maximumValue { fill = CGRect(origin: .zero, size: track.size) }
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            trackBackground.frame = track
            trackBackground.cornerRadius = track.height / 2
            trackBackground.backgroundColor = UIColor.tertiarySystemFill.cgColor
            trackBackground.opacity = isEnabled ? 1 : 0.4
            gradient.frame = trackBackground.bounds
            gradient.startPoint.x = isReversed ? 1 : 0
            gradient.endPoint.x = isReversed ? 0 : 1
            fillMask.frame = fill
            fillMask.cornerRadius = track.height / 2
            CATransaction.commit()
        }

        override func trackRect(forBounds bounds: CGRect) -> CGRect {
            let track = super.trackRect(forBounds: bounds)
            let height = MobiusStyle.iconButtonSize / 1.1
            return CGRect(
                x: track.minX, y: bounds.midY - height / 2, width: track.width, height: height)
        }

        override func thumbRect(forBounds bounds: CGRect, trackRect rect: CGRect, value: Float)
            -> CGRect
        {
            let minimum = super.thumbRect(forBounds: bounds, trackRect: rect, value: minimumValue)
            let maximum = super.thumbRect(forBounds: bounds, trackRect: rect, value: maximumValue)
            // Retain UIKit's direction and value mapping, but align the circular thumb's
            // edges with the rounded track instead of retaining the capsule thumb's inset.
            let range = maximumValue - minimumValue
            let fraction = range > 0 ? CGFloat((value - minimumValue) / range) : 0
            let progress = minimum.midX > maximum.midX ? 1 - fraction : fraction
            return CGRect(
                x: rect.minX + progress * (rect.width - thumbDiameter),
                y: rect.midY - thumbDiameter / 2,
                width: thumbDiameter, height: thumbDiameter)
        }

        override func accessibilityIncrement() { adjust(by: 1) }
        override func accessibilityDecrement() { adjust(by: -1) }

        private func adjust(by amount: Float) {
            guard isEnabled else { return }
            value = min(maximumValue, max(minimumValue, value + amount))
            sendActions(for: .valueChanged)
        }
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
            .mobiusNavigationTitle("Configure")
            .toolbar {
                MobiusToolbarItem(placement: .confirmationAction) {
                    MobiusToolbarIconButton(glyph: .check, label: "Done") { dismiss() }
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
