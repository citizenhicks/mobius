import Foundation
import SwiftUI
import UniformTypeIdentifiers
import CoreTransferable
import PhotosUI

private struct ImportedMediaFile: Transferable {
    let url: URL

    static var transferRepresentation: some TransferRepresentation {
        FileRepresentation(importedContentType: .item) { received in
            let directory = URL.temporaryDirectory.appending(
                path: UUID().uuidString,
                directoryHint: .isDirectory
            )
            do {
                try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
                let url = directory.appending(path: received.file.lastPathComponent)
                try FileManager.default.copyItem(at: received.file, to: url)
                return Self(url: url)
            } catch {
                try? FileManager.default.removeItem(at: directory)
                throw error
            }
        }
    }
}

private struct ComposerSettingItem: Identifiable {
    let feature: MiddlewareFeature
    let setting: FrontendSetting
    let options: [FrontendSettingOption]
    let unsetLabel: String?

    var id: String { "\(feature.id)\u{0}\(setting.id)" }
}

private struct ComposerSettingMenu: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var pendingDestructiveOption: FrontendSettingOption?
    let item: ComposerSettingItem

    var body: some View {
        Menu {
            Picker(selection: selection) {
                if let unsetLabel = item.unsetLabel {
                    Text(verbatim: unsetLabel).tag(String?.none)
                }
                ForEach(item.options) { option in
                    Text(verbatim: option.label).tag(Optional(option.value))
                }
            } label: { Text(verbatim: item.setting.label) }
            .labelsHidden()
        } label: {
            MobiusLabel(
                verbatim: selectedLabel,
                glyph: selectedGlyph ?? .slidersHorizontal,
                iconColor: palette.tone(selectedOption?.tone ?? "neutral"),
                iconSize: MobiusStyle.glyphLead
            )
            .labelStyle(.iconOnly)
            .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
            .contentShape(Rectangle())
        }
        .buttonStyle(.mobiusPlain)
        .sensoryFeedback(.selection, trigger: selectedValue)
        .disabled(!isEnabled)
        .help(Text(verbatim: selectedLabel))
        .accessibilityLabel(Text(verbatim: item.setting.label))
        .accessibilityValue(Text(verbatim: selectedLabel))
        .confirmationDialog(
            "Confirm setting",
            isPresented: destructiveConfirmationPresented,
            titleVisibility: .visible,
            presenting: pendingDestructiveOption
        ) { option in
            Button("Enable \(option.label)", role: .destructive) {
                apply(option.value)
            }
            Button("Cancel", role: .cancel) {}
        } message: { option in
            Text(verbatim: option.description)
        }
    }

    private var selection: Binding<String?> {
        Binding {
            selectedValue
        } set: { value in
            guard let value,
                  let option = item.options.first(where: { $0.value == value })
            else {
                apply(nil)
                return
            }
            if option.tone == "error", value != selectedValue {
                pendingDestructiveOption = option
            } else {
                apply(value)
            }
        }
    }

    private var selectedValue: String? {
        guard let configured = model.selectedBot?.config.config.middleware
            .settings[item.feature.id]?[item.setting.id],
              case .string(let value) = configured
        else { return nil }
        return value
    }

    private var selectedOption: FrontendSettingOption? {
        item.options.first { $0.value == selectedValue }
    }

    private var selectedLabel: String {
        selectedOption?.label ?? item.unsetLabel ?? item.setting.label
    }

    private var selectedGlyph: MobiusGlyph? {
        selectedOption?.symbol.flatMap(MobiusSymbol.knownGlyph(for:))
    }

    private var isEnabled: Bool {
        model.canMutateSelectedBot
            && (item.feature.required
                || model.selectedBot?.config.config.middleware.enabled.contains(item.feature.id)
                    == true)
    }

    private var destructiveConfirmationPresented: Binding<Bool> {
        Binding {
            pendingDestructiveOption != nil
        } set: { isPresented in
            if !isPresented { pendingDestructiveOption = nil }
        }
    }

    private func apply(_ value: String?) {
        pendingDestructiveOption = nil
        model.setSelectedBotSetting(
            value.map(FrontendSettingValue.string),
            middleware: item.feature.id,
            setting: item.setting.id
        )
    }
}

struct ComposerOptionsView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let dictation: ComposerDictation
    @Binding var selection: TextSelection?
    let send: (ActiveMessageDelivery?) -> Void
    @State private var isFileImporterPresented = false
    @State private var isPhotoPickerPresented = false
    @State private var photoSelection: [PhotosPickerItem] = []

    var body: some View {
        Group {
            if dictation.isActive {
                ComposerDictationControls(
                    dictation: dictation,
                    cancel: discardDictation,
                    stop: toggleDictation
                )
            } else {
                VStack(spacing: MobiusSpace.xs) {
                    if model.realtimeVoiceCall != nil { realtimeVoiceControls }
                    // ponytail: overlap 44pt targets by 4pt; split groups if boundary taps misfire.
                    HStack(spacing: -MobiusSpace.xs) {
                        if model.attachmentsEnabled { addAttachmentControl }
                        ForEach(composerSettings) { item in
                            ComposerSettingMenu(item: item)
                        }
                        Spacer(minLength: MobiusSpace.s)
                        modelMenu
                        actionButtons
                    }
                }
            }
        }
        .fileImporter(
            isPresented: $isFileImporterPresented,
            allowedContentTypes: [.data],
            allowsMultipleSelection: true,
            onCompletion: importFiles
        )
        // The picker runs out of process, so this needs no photo library permission.
        .photosPicker(
            isPresented: $isPhotoPickerPresented,
            selection: $photoSelection,
            maxSelectionCount: 16,
            matching: .any(of: [.images, .videos])
        )
        .onChange(of: photoSelection) { _, items in
            guard !items.isEmpty else { return }
            let imports = reserveMedia(items)
            photoSelection = []
            guard !imports.isEmpty else { return }
            Task { await importMedia(imports) }
        }
    }

    /// The photo library and the file browser are separate pickers, so the plus offers both
    /// rather than assuming every attachment lives in Files.
    @ViewBuilder
    private var addAttachmentControl: some View {
        Menu {
            Button { isPhotoPickerPresented = true } label: {
                MobiusLabel(title: "Photos", glyph: .image01)
            }
            Button { isFileImporterPresented = true } label: {
                MobiusLabel(title: "Files", glyph: .fileText)
            }
        } label: {
            MobiusLabel(
                title: "Add attachment",
                glyph: .plus,
                // A plain menu label gets no disabled treatment, so mute the glyph whenever
                // connection or composer state makes importing unavailable.
                iconColor: model.canImportAttachments ? nil : palette.muted,
                iconSize: MobiusStyle.glyphLead
            )
                .labelStyle(.iconOnly)
                .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
                .contentShape(Rectangle())
        }
        .buttonStyle(.mobiusPlain)
        .disabled(!model.canImportAttachments)
        .accessibilityLabel("Add attachment")
    }

    private var modelMenu: some View {
        Menu {
            Section("Model") { modelMenuContent }
            Section("Reasoning") { reasoningMenuContent }
        } label: {
            MobiusMenuLabel(
                text: currentChoice.map { .verbatim(model.modelLabel(for: $0)) }
                    ?? .localized("Model"),
                glyph: providerGlyph,
                detail: displayedReasoningLabel,
                glyphSize: MobiusStyle.glyphLead,
                glyphColor: providerTint?.color
            )
            .frame(minHeight: MobiusStyle.iconButtonSize)
            .contentShape(Rectangle())
        }
        .buttonStyle(.mobiusPlain)
        .sensoryFeedback(.selection, trigger: selectedBotModelRoute)
        .disabled(!model.canMutateSelectedBot)
        .accessibilityLabel("Model and reasoning")
        .accessibilityValue(modelLabel.text)
    }

    @ViewBuilder
    private var modelMenuContent: some View {
        Picker("Model", selection: modelPickerSelection) {
            ForEach(distinctModels, id: \.route) { choice in
                modelMenuOptionLabel(
                    model.modelLabel(for: choice),
                    providerSymbol: model.providerSymbol(for: choice),
                    tint: model.providerTint(for: choice)
                )
                .tag(choice.route)
            }
        }
        .labelsHidden()
    }

    @ViewBuilder
    private var reasoningMenuContent: some View {
        Picker("Reasoning", selection: reasoningPickerSelection) {
            ForEach(reasoningChoices, id: \.route) { choice in
                if let effort = choice.reasoningEffort {
                    Text(verbatim: effort.capitalized).tag(choice.route)
                } else {
                    Text("Default").tag(choice.route)
                }
            }
        }
        .labelsHidden()
    }

    private func modelMenuOptionLabel(
        _ title: String,
        providerSymbol: String?,
        tint: AccentTint
    ) -> some View {
        Group {
            if let providerSymbol,
               let glyph = MobiusSymbol.knownGlyph(for: providerSymbol),
               let image = glyph.menuImage(tint.color) {
                Label { Text(verbatim: title) } icon: { image }
            } else {
                Text(verbatim: title)
            }
        }
    }

    @ViewBuilder
    private var actionButtons: some View {
        if model.selectedRouteSupportsRealtimeVoice {
            Button {
                if model.realtimeVoiceCall == nil { model.startRealtimeVoice() }
                else { model.stopRealtimeVoice() }
            } label: {
                MobiusLabel(title: realtimeVoiceLabel, glyph: .audioWave01)
            }
            .buttonStyle(MobiusIconButtonStyle(prominent: model.realtimeVoiceCall != nil, bare: true))
            .labelStyle(.iconOnly)
            .disabled(model.realtimeVoiceCall == nil && !model.canStartRealtimeVoice)
            .help(Text(realtimeVoiceLabel))
        } else {
            Button(action: toggleDictation) {
                if dictation.isTransitioning {
                    ProgressView()
                        .controlSize(.small)
                } else {
                    MobiusLabel(
                        title: dictationLabel,
                        glyph: .mic01,
                        iconSize: MobiusStyle.glyphLead
                    )
                }
            }
            .labelStyle(.iconOnly)
            .buttonStyle(MobiusIconButtonStyle(prominent: dictation.isRecording, bare: true))
            .disabled(!canToggleDictation)
            .help(Text(dictationLabel))
            .accessibilityLabel(Text(dictationLabel))
            .accessibilityValue(Text(dictationValue))
        }

        Group {
            if model.activeTurnID != nil && !canSend {
                Button("Stop", glyph: .stopFill) { model.interrupt() }
                    .help("Stop")
            } else {
                Button(action: { send(nil) }) {
                    Label {
                        Text(sendLabel)
                    } icon: {
                        if isWaitingForGateway {
                            MobiusSpinner(
                                size: MobiusStyle.iconSize,
                                foreground: palette.onAccent
                            )
                        } else {
                            MobiusIcon(sendGlyph)
                        }
                    }
                }
                    .disabled(!canSend)
                    .help(Text(sendLabel))
                    .accessibilityLabel(Text(sendLabel))
                    .accessibilityHint(Text(sendHint))
                    .contextMenu {
                        if model.activeTurnID != nil {
                            Button(alternateSendLabel, glyph: alternateSendGlyph) {
                                send(alternateDelivery)
                            }
                        }
                    }
            }
        }
        .mobiusProminentIconButton()
    }

    private func importFiles(_ result: Result<[URL], Error>) {
        switch result {
        case .success(let urls):
            Task { await model.importAttachments(urls) }
        case .failure(let error):
            model.showToast(
                verbatim: model.localizedErrorDescription(error),
                tone: .error
            )
        }
    }

    /// Keep the filename supplied by Photos while taking the same import path and limits as Files.
    private func reserveMedia(
        _ items: [PhotosPickerItem]
    ) -> [(item: PhotosPickerItem, id: UUID)] {
        var imports: [(item: PhotosPickerItem, id: UUID)] = []
        for item in items {
            guard let id = model.reserveComposerAttachment(
                named: mediaPlaceholderName(for: item)
            ) else { break }
            imports.append((item, id))
        }
        return imports
    }

    private func importMedia(_ imports: [(item: PhotosPickerItem, id: UUID)]) async {
        var failed = false
        for (item, id) in imports {
            guard let media = try? await item.loadTransferable(type: ImportedMediaFile.self) else {
                failed = model.cancelComposerAttachmentImport(id) || failed
                continue
            }
            await model.completeComposerAttachmentImport(media.url, reservedID: id)
            try? FileManager.default.removeItem(at: media.url.deletingLastPathComponent())
        }
        if failed {
            model.showToast("Could not read the selected photos or videos.", tone: .error)
        }
    }

    private func mediaPlaceholderName(for item: PhotosPickerItem) -> String {
        let type = item.supportedContentTypes.first(where: {
            $0.conforms(to: .movie) || $0.conforms(to: .video)
        }) ?? item.supportedContentTypes.first
        let base = type?.conforms(to: .movie) == true || type?.conforms(to: .video) == true
            ? "video"
            : "image"
        guard let ext = type?.preferredFilenameExtension else { return base }
        return "\(base).\(ext)"
    }

    private var selectedBotModelRoute: String? {
        model.modelRoute(for: model.selectedBot?.config.config)
    }

    private var currentChoice: ModelChoice? {
        guard let selectedBotModelRoute else { return nil }
        return model.modelChoices.first { $0.route == selectedBotModelRoute }
    }

    private var modelPickerSelection: Binding<String> {
        Binding {
            guard let currentChoice else { return "" }
            return distinctModels.first { model.sameModel($0, currentChoice) }?.route
                ?? currentChoice.route
        } set: { route in
            guard let choice = distinctModels.first(where: { $0.route == route }) else { return }
            let effort = currentChoice?.reasoningEffort
            let target = model.modelChoices.first {
                model.sameModel($0, choice) && $0.reasoningEffort == effort
            } ?? choice
            model.selectModelForSelectedBot(target.route)
        }
    }

    private var reasoningPickerSelection: Binding<String> {
        Binding {
            selectedBotModelRoute ?? ""
        } set: { route in
            model.selectModelForSelectedBot(route)
        }
    }

    private var distinctModels: [ModelChoice] {
        model.distinctModels(in: model.modelChoices)
    }

    private var reasoningChoices: [ModelChoice] {
        guard let currentChoice else { return [] }
        return model.modelChoices(matching: currentChoice, in: model.modelChoices)
    }

    private var composerSettings: [ComposerSettingItem] {
        model.middlewareFeatures.flatMap { feature in
            feature.settings.compactMap { setting in
                guard setting.composer,
                      case .select(let options, let unsetLabel) = setting.kind
                else { return nil }
                return ComposerSettingItem(
                    feature: feature,
                    setting: setting,
                    options: options,
                    unsetLabel: unsetLabel
                )
            }
        }
    }

    private var modelLabel: MobiusText {
        guard let currentChoice else { return .localized("Model") }
        let modelName = model.modelLabel(for: currentChoice)
        if let effort = currentChoice.reasoningEffort {
            return .localized("\(modelName) · \(effort.capitalized)")
        }
        return .localized("\(modelName) · Default")
    }

    private var displayedReasoningLabel: MobiusText? {
        guard let currentChoice else { return nil }
        if let effort = currentChoice.reasoningEffort {
            return .verbatim("• \(effort.capitalized)")
        }
        return .localized("• Default")
    }

    private var providerTint: AccentTint? {
        currentChoice.map { model.providerTint(for: $0) }
    }

    private var providerGlyph: MobiusGlyph? {
        currentChoice
            .flatMap { model.providerSymbol(for: $0) }
            .flatMap { MobiusSymbol.knownGlyph(for: $0) }
    }

    private var canSend: Bool {
        guard model.connectionState.isReady,
              model.canSendComposer,
              model.activeTurnID == nil || model.composerAttachments.isEmpty
        else { return false }
        return !dictation.isActive
    }

    private var isWaitingForGateway: Bool {
        switch model.connectionState {
        case .connecting, .authenticating, .loading: true
        case .disconnected, .ready, .failed: false
        }
    }

    private var realtimeVoiceControls: some View {
        HStack(spacing: MobiusSpace.s) {
            MobiusLabel(
                title: model.realtimeVoice.isConnected ? "Voice chat" : "Connecting voice",
                glyph: .audioWave01
            )
            .font(MobiusStyle.badgeFont)
            .foregroundStyle(palette.muted)
            Spacer(minLength: MobiusSpace.s)
            Button {
                model.realtimeVoice.isMuted.toggle()
            } label: {
                Text(model.realtimeVoice.isMuted ? "Unmute" : "Mute")
            }
            .buttonStyle(.plain)
            .frame(minWidth: MobiusStyle.iconButtonSize, minHeight: MobiusStyle.iconButtonSize)
            .disabled(!model.realtimeVoice.isConnected)
            .accessibilityValue(model.realtimeVoice.isMuted ? "Microphone muted" : "Microphone on")
        }
    }

    private var realtimeVoiceLabel: LocalizedStringResource {
        model.realtimeVoiceCall == nil ? "Start voice chat" : "End voice chat"
    }

    private var canToggleDictation: Bool {
        guard !model.selectedRouteSupportsRealtimeVoice, model.realtimeVoiceCall == nil else { return false }
        return dictation.isRecording
            || dictation.canToggle
                && model.connectionState.isReady
                && model.selectedSessionID != nil
    }

    private var dictationLabel: LocalizedStringResource {
        switch dictation.state {
        case .idle: "Start dictation"
        case .preparing: "Preparing dictation"
        case .recording: "Stop dictation"
        case .stopping: "Finishing dictation"
        }
    }

    private var dictationValue: LocalizedStringResource {
        switch dictation.state {
        case .idle: "Not listening"
        case .preparing: "Preparing speech recognition"
        case .recording: "Listening"
        case .stopping: "Finishing transcription"
        }
    }

    private var sendLabel: LocalizedStringResource {
        guard model.activeTurnID != nil else { return "Send" }
        return model.activeMessageDelivery == .steer ? "Send as Steer" : "Send as Queue"
    }

    private var sendHint: LocalizedStringResource {
        guard model.activeTurnID != nil else { return "Starts a new turn" }
        return model.activeMessageDelivery == .steer
            ? "Long press to send after this turn"
            : "Long press to steer the active turn"
    }

    private var sendGlyph: MobiusGlyph {
        guard model.activeTurnID != nil else { return .arrowUp02 }
        return model.activeMessageDelivery == .steer ? .workflowSquare03 : .queue01
    }

    private var alternateDelivery: ActiveMessageDelivery {
        model.activeMessageDelivery == .steer ? .queue : .steer
    }

    private var alternateSendLabel: LocalizedStringResource {
        alternateDelivery == .steer ? "Send as Steer" : "Send as Queue"
    }

    private var alternateSendGlyph: MobiusGlyph {
        alternateDelivery == .steer ? .workflowSquare03 : .queue01
    }

    private func toggleDictation() {
        guard canToggleDictation else { return }
        model.messageSpeaker.stop()
        Task {
            do {
                if dictation.isRecording {
                    try await dictation.stop()
                } else {
                    let sessionID = model.selectedSessionID
                    try await dictation.start(
                        existingText: model.composer,
                        updateText: { text in
                            guard model.selectedSessionID == sessionID else { return }
                            selection = nil
                            model.composer = text
                        },
                        reportError: {
                            model.showToast($0.localizedDescriptionResource, tone: .error)
                        }
                    )
                }
            } catch is CancellationError {
                return
            } catch {
                model.showToast(
                    verbatim: model.localizedErrorDescription(error),
                    tone: .error
                )
            }
        }
    }

    private func discardDictation() {
        Task { await dictation.discard() }
    }
}

private struct ComposerDictationControls: View {
    @Environment(\.mobiusPalette) private var palette
    let dictation: ComposerDictation
    let cancel: () -> Void
    let stop: () -> Void

    var body: some View {
        HStack(spacing: MobiusSpace.s) {
            Button("Cancel dictation", glyph: .x, action: cancel)
                .mobiusIconButton()
                .disabled(dictation.state == .stopping)

            Group {
                if let languageCode = dictation.detectedLanguageCode {
                    Text(verbatim: languageCode)
                } else {
                    Text(verbatim: "—")
                }
            }
            .font(MobiusStyle.badgeFont)
            .foregroundStyle(palette.muted)
            .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
            .accessibilityLabel("Detected language")
            .accessibilityValue(
                dictation.detectedLanguageCode.map { Text(verbatim: $0) } ?? Text("Detecting")
            )

            ComposerDictationWaveform(samples: dictation.audioLevels)
                .frame(maxWidth: .infinity)
                .frame(height: MobiusStyle.rowCompact)

            Button(action: stop) {
                if dictation.isTransitioning {
                    ProgressView()
                        .controlSize(.small)
                } else {
                    MobiusLabel(title: "Stop dictation", glyph: .stopFill)
                }
            }
            .labelStyle(.iconOnly)
            .buttonStyle(MobiusIconButtonStyle(prominent: true))
            .disabled(!dictation.isRecording)
            .help("Stop dictation")
            .accessibilityLabel("Stop dictation")
        }
        .frame(minHeight: MobiusStyle.iconButtonSize)
    }
}

private struct ComposerDictationWaveform: View {
    @Environment(\.mobiusPalette) private var palette
    let samples: [Double]

    var body: some View {
        Canvas { context, size in
            let count = max(16, min(44, Int(size.width / 7)))
            let levels = Array(samples.suffix(count))
            let leadingEmpty = count - levels.count
            let step = size.width / Double(count)
            let barWidth = min(4, max(2, step * 0.48))
            let minimumHeight = 3.0

            for index in 0..<count {
                let level = index < leadingEmpty ? 0 : levels[index - leadingEmpty]
                let height = minimumHeight
                    + pow(min(1, max(0, level)), 0.7) * max(0, size.height - minimumHeight)
                let rect = CGRect(
                    x: (Double(index) + 0.5) * step - barWidth / 2,
                    y: (size.height - height) / 2,
                    width: barWidth,
                    height: height
                )
                context.fill(
                    Path(roundedRect: rect, cornerRadius: barWidth / 2),
                    with: .color(palette.muted.opacity(0.45 + level * 0.55))
                )
            }
        }
        .accessibilityHidden(true)
    }
}
