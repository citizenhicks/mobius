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
                try FileManager.default.createDirectory(
                    at: directory, withIntermediateDirectories: true)
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
            } label: {
                Text(verbatim: item.setting.label)
            }
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
        guard
            let configured = model.selectedBot?.config.config.middleware
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
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Namespace private var actionTransition
    let send: (ActiveMessageDelivery?) -> Void
    var isCompact = false
    @State private var showsModelSettings = false
    @State private var opensModelSelection = false
    @State private var showsModelSelection = false
    @State private var isFileImporterPresented = false
    @State private var isPhotoPickerPresented = false
    @State private var photoSelection: [PhotosPickerItem] = []

    var body: some View {
        // ponytail: overlap 44pt targets by 4pt; split groups if boundary taps misfire.
        HStack(spacing: -MobiusSpace.xs) {
            if model.attachmentsEnabled { addAttachmentControl }
            if !isCompact && !model.selectedChatIsGroup {
                ForEach(composerSettings) { item in
                    ComposerSettingMenu(item: item)
                }
            }
            Spacer(minLength: MobiusSpace.s)
            if !isCompact && !model.selectedChatIsGroup { modelMenu }
            actionButtons
        }
        .sheet(isPresented: $showsModelSelection) {
            ModelSelectionSheet(
                choices: model.modelChoices,
                isEnabled: model.canMutateSelectedBot,
                route: selectedModelBinding,
                voices: model.realtimeVoices(for: model.selectedBot?.config.config),
                voice: selectedVoiceBinding
            )
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
            Button {
                isPhotoPickerPresented = true
            } label: {
                MobiusLabel(title: "Photos", glyph: .image01)
            }
            Button {
                isFileImporterPresented = true
            } label: {
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
        let progress = currentChoice.map { model.reasoningFraction(for: $0) } ?? 0
        let sweep = 250.0
        let start = 90 + (360 - sweep) / 2
        let endpoint = (start + sweep * progress) * .pi / 180
        return Button {
            showsModelSettings = true
        } label: {
            ZStack {
                Circle()
                    .trim(from: 0, to: sweep / 360)
                    .stroke(
                        palette.muted.opacity(0.2),
                        style: StrokeStyle(lineWidth: 2.5, lineCap: .round)
                    )
                    .rotationEffect(.degrees(start))
                Circle()
                    .trim(from: 0, to: progress * sweep / 360)
                    .stroke(
                        providerTint?.color ?? palette.accent,
                        style: StrokeStyle(lineWidth: 2.5, lineCap: .round)
                    )
                    .rotationEffect(.degrees(start))
                Circle()
                    .fill(providerTint?.color ?? palette.accent)
                    .frame(width: 4.5, height: 4.5)
                    .offset(
                        x: MobiusStyle.rowCompact / 2 * cos(endpoint),
                        y: MobiusStyle.rowCompact / 2 * sin(endpoint)
                    )
                MobiusIcon(
                    providerGlyph ?? .aiScan, size: MobiusStyle.glyphInline,
                    foreground: providerTint?.color)
            }
            .frame(width: MobiusStyle.rowCompact, height: MobiusStyle.rowCompact)
            .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
            .contentShape(Rectangle())
        }
        .buttonStyle(.mobiusPlain)
        .disabled(!model.canMutateSelectedBot)
        .accessibilityLabel("Model and reasoning")
        .accessibilityValue(modelLabel.text)
        .popover(isPresented: $showsModelSettings, arrowEdge: .bottom) {
            ModelRoutePicker(
                label: "Model and reasoning",
                detail: "Choose the model and reasoning effort used by this Bot.",
                choices: model.modelChoices,
                isEnabled: model.canMutateSelectedBot,
                route: selectedModelBinding,
                onSelectModel: {
                    opensModelSelection = true
                    showsModelSettings = false
                }
            )
            .padding(MobiusSpace.l)
            .frame(width: 300)
            .presentationCompactAdaptation(.popover)
            .onDisappear {
                if opensModelSelection {
                    opensModelSelection = false
                    showsModelSelection = true
                }
            }
        }
    }

    private var selectedModelBinding: Binding<String?> {
        Binding(
            get: { selectedBotModelRoute },
            set: { if let route = $0 { model.selectModelForSelectedBot(route) } }
        )
    }

    private var selectedVoiceBinding: Binding<String?> {
        Binding(
            get: { model.selectedBot?.config.config.realtimeVoice },
            set: { if let voice = $0 { model.setSelectedBotVoice(voice) } }
        )
    }

    private var actionButtons: some View {
        GlassEffectContainer(spacing: MobiusSpace.s) {
            HStack(spacing: -MobiusSpace.xs) {
                if model.selectedRouteSupportsRealtimeVoice && !model.composerUsesPrimaryVoice {
                    voiceButton
                        .buttonStyle(MobiusIconButtonStyle(surfaceSize: 32))
                        .glassEffectID("voice", in: actionTransition)
                        .glassEffectTransition(reduceMotion ? .identity : .matchedGeometry)
                }
                primaryAction
                    .mobiusProminentIconButton(surfaceSize: 32)
                    .glassEffectID("primary", in: actionTransition)
                    .glassEffectTransition(reduceMotion ? .identity : .matchedGeometry)
            }
        }
        .animation(
            reduceMotion ? nil : .smooth(duration: 0.3), value: model.composerUsesPrimaryVoice)
    }

    private var voiceButton: some View {
        Button {
            model.startRealtimeVoice()
        } label: {
            MobiusLabel(
                title: "Start voice chat", glyph: .audioWave01,
                iconSize: MobiusStyle.glyphLead
            )
        }
        .labelStyle(.iconOnly)
        .disabled(!model.canStartRealtimeVoice)
        .help("Start voice chat")
        .accessibilityLabel("Start voice chat")
    }

    @ViewBuilder
    private var primaryAction: some View {
        if model.composerUsesPrimaryVoice {
            voiceButton
        } else if model.chat.activeTurnID != nil && !canSend {
            Button {
                model.interrupt()
            } label: {
                MobiusLabel(
                    title: "Stop", glyph: .stopFill, iconSize: MobiusStyle.glyphLead)
            }
            .help("Stop")
        } else {
            Button(action: { send(nil) }) {
                Label {
                    Text(sendLabel)
                } icon: {
                    if isWaitingForGateway {
                        MobiusSpinner(
                            size: MobiusStyle.glyphLead,
                            foreground: palette.onAccent
                        )
                    } else {
                        MobiusIcon(sendGlyph, size: MobiusStyle.glyphLead)
                    }
                }
            }
            .disabled(!canSend)
            .help(Text(sendLabel))
            .accessibilityLabel(Text(sendLabel))
            .accessibilityHint(Text(sendHint))
            .contextMenu {
                if model.chat.composerTargetTurnID != nil {
                    Button(alternateSendLabel, glyph: alternateSendGlyph) {
                        send(alternateDelivery)
                    }
                }
            }
        }
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
            guard
                let id = model.reserveComposerAttachment(
                    named: mediaPlaceholderName(for: item)
                )
            else { break }
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
        let type =
            item.supportedContentTypes.first(where: {
                $0.conforms(to: .movie) || $0.conforms(to: .video)
            }) ?? item.supportedContentTypes.first
        let base =
            type?.conforms(to: .movie) == true || type?.conforms(to: .video) == true
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
        return .localized("\(modelName) · \(model.reasoningLabel(for: currentChoice))")
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
        model.gateway.connectionState.isReady && model.canSendComposer
            && (model.chat.composerTargetTurnID == nil || model.chat.composerAttachments.isEmpty)
    }

    private var isWaitingForGateway: Bool {
        switch model.gateway.connectionState {
        case .connecting, .authenticating, .loading: true
        case .disconnected, .ready, .failed: false
        }
    }

    private var sendLabel: LocalizedStringResource {
        guard model.chat.composerTargetTurnID != nil else { return "Send" }
        return model.activeMessageDelivery == .steer ? "Send as Steer" : "Send as Queue"
    }

    private var sendHint: LocalizedStringResource {
        if model.selectedChatIsGroup { return "Posts to the group chat" }
        guard model.chat.composerTargetTurnID != nil else { return "Starts a new turn" }
        return model.activeMessageDelivery == .steer
            ? "Long press to send after this turn"
            : "Long press to steer the active turn"
    }

    private var sendGlyph: MobiusGlyph {
        guard model.chat.composerTargetTurnID != nil else { return .arrowUp02 }
        return model.activeMessageDelivery == .steer ? .arrowUpRight01 : .queue01
    }

    private var alternateDelivery: ActiveMessageDelivery {
        model.activeMessageDelivery == .steer ? .queue : .steer
    }

    private var alternateSendLabel: LocalizedStringResource {
        alternateDelivery == .steer ? "Send as Steer" : "Send as Queue"
    }

    private var alternateSendGlyph: MobiusGlyph {
        alternateDelivery == .steer ? .arrowUpRight01 : .queue01
    }
}
