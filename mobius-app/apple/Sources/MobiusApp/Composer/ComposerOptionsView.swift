import Foundation
import SwiftUI
import UniformTypeIdentifiers
import CoreTransferable
import PhotosUI

private struct ImportedMediaFile: Transferable {
    let url: URL

    static var transferRepresentation: some TransferRepresentation {
        FileRepresentation(importedContentType: .image) { received in
            try copy(received.file)
        }
        FileRepresentation(importedContentType: .movie) { received in
            try copy(received.file)
        }
    }

    private static func copy(_ source: URL) throws -> Self {
        let directory = URL.temporaryDirectory.appending(
            path: UUID().uuidString,
            directoryHint: .isDirectory
        )
        do {
            try FileManager.default.createDirectory(
                at: directory, withIntermediateDirectories: true)
            let url = directory.appending(path: source.lastPathComponent)
            try FileManager.default.copyItem(at: source, to: url)
            return Self(url: url)
        } catch {
            try? FileManager.default.removeItem(at: directory)
            throw error
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
    @Environment(\.mobiusHasVerticalToolbar) private var hasVerticalToolbar
    let send: (ActiveMessageDelivery?) -> Void
    var isCompact = false
    var newChatEntry: ((ComposerEntryIntent) -> Void)? = nil
    @State private var showsModelSettings = false
    @State private var opensModelSelection = false
    @State private var showsModelSelection = false
    @State private var isFileImporterPresented = false
    @State private var isPhotoPickerPresented = false
    @State private var photoSelection: [PhotosPickerItem] = []

    var body: some View {
        Group {
            if model.gateway.connectionState.isLoading && model.modelChoices.isEmpty {
                loadingControls
            } else {
                controls
            }
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

    private var controls: some View {
        HStack(spacing: 0) {
            HStack(spacing: -MobiusSpace.s) {
                if newChatEntry != nil || model.attachmentsEnabled { addAttachmentControl }
                if !isCompact {
                    if model.selectedBot == nil {
                        unselectedBotPlaceholder
                    } else {
                        ForEach(composerSettings) { item in
                            ComposerSettingMenu(item: item)
                        }
                    }
                }
            }
            Spacer(minLength: MobiusSpace.s)
            HStack(spacing: showsVoiceButton ? -MobiusSpace.s : 0) {
                if !isCompact {
                    if model.selectedBot == nil {
                        unselectedBotPlaceholder
                    } else {
                        modelMenu
                    }
                }
                if !hasVerticalToolbar { actionButtons }
            }
        }
    }

    private var loadingControls: some View {
        HStack(spacing: 0) {
            HStack(spacing: -MobiusSpace.s) {
                if newChatEntry != nil || model.attachmentsEnabled { placeholderButton() }
                if !isCompact { placeholderButton() }
            }
            Spacer(minLength: MobiusSpace.s)
            if !isCompact { placeholderButton() }
            if !hasVerticalToolbar { placeholderButton(size: 32) }
        }
        .mobiusLoadingPlaceholder("Loading composer controls")
    }

    private var unselectedBotPlaceholder: some View {
        placeholderButton()
            .mobiusRunningShimmer(active: true)
            .allowsHitTesting(false)
            .accessibilityHidden(true)
    }

    private func placeholderButton(size: CGFloat = MobiusStyle.glyphLead) -> some View {
        Circle()
            .fill(palette.muted)
            .frame(width: size, height: size)
            .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
    }

    /// The photo library and the file browser are separate pickers, so the plus offers both
    /// rather than assuming every attachment lives in Files.
    @ViewBuilder
    private var addAttachmentControl: some View {
        if let newChatEntry {
            Button("New chat", glyph: .plus) { newChatEntry(.focus) }
                .labelStyle(.iconOnly)
                .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
                .buttonStyle(.mobiusPlain)
                .accessibilityLabel("New chat")
                .accessibilityHint("Open the composer to add an attachment")
        } else {
            Menu {
                Button("Photos", glyph: .image01) {
                    isPhotoPickerPresented = true
                }
                Button("Files", glyph: .fileText) {
                    isFileImporterPresented = true
                }
            } label: {
                MobiusLabel(
                    title: "Add attachment",
                    glyph: .plus,
                    iconColor: model.canImportAttachments ? nil : palette.muted,
                    iconSize: MobiusStyle.glyphLead
                )
                .labelStyle(.iconOnly)
                .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .labelStyle(.titleAndIcon)
            .menuIndicator(.hidden)
            .disabled(!model.canImportAttachments)
            .accessibilityLabel("Add attachment")
        }
    }

    private var modelMenu: some View {
        Button {
            showsModelSettings = true
        } label: {
            MobiusIcon(
                .reasoning(currentChoice.map { model.reasoningFraction(for: $0) } ?? 0),
                size: MobiusStyle.iconSize
            )
            .contentTransition(.opacity)
            .animation(reduceMotion ? nil : .easeInOut(duration: 0.18), value: currentChoice?.route)
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
            get: { model.selectedBotModelRoute },
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
        HStack(spacing: -MobiusSpace.xs) {
            if showsVoiceButton {
                voiceButton
                    .buttonStyle(MobiusIconButtonStyle(bare: true))
            }
            primaryAction
                .mobiusProminentIconButton(surfaceSize: 32, flat: true)
                .transaction { $0.animation = nil }
        }
    }

    private var showsVoiceButton: Bool {
        newChatEntry != nil
            ? model.newChatRouteSupportsRealtimeVoice
            : model.selectedRouteSupportsRealtimeVoice
    }

    private var voiceButton: some View {
        Button {
            if newChatEntry != nil {
                model.openNewVoiceChat()
            } else {
                model.startRealtimeVoice()
            }
        } label: {
            MobiusLabel(
                title: "Start voice chat", glyph: .audioWave01,
                iconSize: MobiusStyle.glyphLead
            )
        }
        .labelStyle(.iconOnly)
        .disabled(newChatEntry != nil ? !model.canCreateSession : !model.canStartRealtimeVoice)
        .help("Start voice chat")
        .accessibilityLabel("Start voice chat")
    }

    @ViewBuilder
    private var primaryAction: some View {
        if isCompact && (newChatEntry != nil || !model.composerShowsInterruptAction) {
            Button("Dictate", glyph: .mic01) {
                if let newChatEntry {
                    newChatEntry(.dictate)
                } else {
                    model.toggleComposerDictation()
                }
            }
            .help("Dictate")
            .accessibilityLabel("Dictate")
        } else {
            ComposerSendButton(send: send)
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

    private var currentChoice: ModelChoice? {
        guard let selectedBotModelRoute = model.selectedBotModelRoute else { return nil }
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

}

/// The inline composer and the system rail share the complete send/interrupt interaction.
struct ComposerSendButton: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let send: (ActiveMessageDelivery?) -> Void

    var body: some View {
        if model.composerShowsInterruptAction {
            Button {
                model.interrupt()
            } label: {
                MobiusLabel(
                    title: "Stop", glyph: .stopFill, iconSize: MobiusStyle.glyphLead)
            }
            .help("Stop")
            .accessibilityLabel("Stop")
        } else {
            Button(action: { send(nil) }) {
                Label {
                    Text(model.composerSendLabel)
                } icon: {
                    if model.gateway.connectionState.isLoading {
                        MobiusSpinner(
                            size: MobiusStyle.glyphLead,
                            foreground: palette.onAccent
                        )
                    } else {
                        MobiusIcon(model.composerSendGlyph, size: MobiusStyle.glyphLead)
                    }
                }
            }
            .disabled(!model.canSubmitComposer)
            .help(Text(model.composerSendLabel))
            .accessibilityLabel(Text(model.composerSendLabel))
            .accessibilityHint(Text(model.composerSendHint))
            .contextMenu {
                if model.chat.composerTargetTurnID != nil {
                    Button(
                        model.composerAlternateSendLabel,
                        glyph: model.composerAlternateSendGlyph
                    ) {
                        send(model.composerAlternateDelivery)
                    }
                }
            }
        }
    }
}

extension AppModel {
    var composerRailShowsSendAction: Bool {
        chat.realtimeVoiceCall == nil
            && (composerShowsInterruptAction
                || !chat.composerIsCompact
                || !chat.composer.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                || !chat.composerAttachments.isEmpty)
    }

    var composerShowsInterruptAction: Bool {
        chat.activeTurnID != nil && !canSubmitComposer
    }

    var composerSendLabel: LocalizedStringResource {
        guard chat.composerTargetTurnID != nil else { return "Send" }
        return activeMessageDelivery == .steer ? "Send as Steer" : "Send as Queue"
    }

    var composerSendHint: LocalizedStringResource {
        guard chat.composerTargetTurnID != nil else { return "Starts a new turn" }
        return activeMessageDelivery == .steer
            ? "Long press to send after this turn"
            : "Long press to steer the active turn"
    }

    var composerSendGlyph: MobiusGlyph {
        guard chat.composerTargetTurnID != nil else { return .arrowUp02 }
        return activeMessageDelivery == .steer ? .arrowUpRight01 : .queue01
    }

    var composerAlternateDelivery: ActiveMessageDelivery {
        activeMessageDelivery == .steer ? .queue : .steer
    }

    var composerAlternateSendLabel: LocalizedStringResource {
        composerAlternateDelivery == .steer ? "Send as Steer" : "Send as Queue"
    }

    var composerAlternateSendGlyph: MobiusGlyph {
        composerAlternateDelivery == .steer ? .arrowUpRight01 : .queue01
    }
}
