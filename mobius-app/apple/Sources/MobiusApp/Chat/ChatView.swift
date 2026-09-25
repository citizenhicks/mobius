import Foundation
import SwiftUI
import SwiftStreamingMarkdown
import CoreText
@preconcurrency import AVFoundation
@preconcurrency import Speech
import UIKit
import AudioToolbox

extension MountedWidget {
    var glyph: MobiusGlyph {
        widget.symbol.map { MobiusSymbol.glyph(for: $0) } ?? .squaresFour
    }

}
struct ChatView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusHasVerticalToolbar) private var hasVerticalToolbar
    @Environment(\.mobiusPalette) private var palette
    @State private var composerHeight: CGFloat = 0
    @State private var isAtBottom = true
    @State private var scrollToBottomRequest = 0
    @State private var presentedWidget: MountedWidget?
    @State private var presentedBotSettings: BotRecord?
    @State private var showsFolderAttachmentBrowser = false
    @State private var transcriptPresentationID = UUID()
    @State private var voiceOrbOffset = CGSize.zero
    @GestureState private var voiceOrbDrag = CGSize.zero

    var body: some View {
        @Bindable var model = model
        @Bindable var chat = model.chat
        ZStack(alignment: .bottom) {
            TranscriptView(
                bottomInset: bottomInset,
                isAtBottom: $isAtBottom,
                scrollToBottomRequest: scrollToBottomRequest
            )
            .id(transcriptPresentationID)
            if model.selectedSessionIsHidden, let approval = model.chat.pendingApproval {
                ApprovalView(approval: approval)
                    .frame(maxWidth: MobiusStyle.transcriptWidth)
                    .frame(maxWidth: .infinity)
                    .padding(.horizontal, MobiusSpace.l)
                    .padding(.bottom, MobiusSpace.m)
                    .onGeometryChange(for: CGFloat.self) { geometry in
                        geometry.size.height
                    } action: { height in
                        composerHeight = height
                    }
                    .zIndex(1)
            } else if !model.selectedSessionIsHidden {
                ComposerView(showBotSettings: presentSelectedBotSettings)
                    .onGeometryChange(for: CGFloat.self) { geometry in
                        geometry.size.height
                    } action: { height in
                        composerHeight = height
                    }
                    .zIndex(1)
            }
            if !isAtBottom {
                Button("Scroll to latest", glyph: .arrowDown) {
                    scrollToBottomRequest += 1
                }
                .mobiusIconButton()
                .padding(.bottom, bottomInset + 12)
                .help("Scroll to latest")
                .zIndex(2)
            }
            if hasVerticalToolbar, model.chat.realtimeVoiceCall != nil {
                GeometryReader { geometry in
                    let proposed = CGSize(
                        width: voiceOrbOffset.width + voiceOrbDrag.width,
                        height: voiceOrbOffset.height + voiceOrbDrag.height
                    )
                    DuoVoiceOrb()
                        .offset(clampedVoiceOrbOffset(proposed, in: geometry.size))
                        .simultaneousGesture(
                            DragGesture()
                                .updating($voiceOrbDrag) { value, drag, _ in
                                    drag = value.translation
                                }
                                .onEnded { value in
                                    voiceOrbOffset = clampedVoiceOrbOffset(
                                        CGSize(
                                            width: voiceOrbOffset.width + value.translation.width,
                                            height: voiceOrbOffset.height + value.translation.height
                                        ),
                                        in: geometry.size
                                    )
                                }
                        )
                        .frame(
                            maxWidth: .infinity,
                            maxHeight: .infinity,
                            alignment: .bottomTrailing
                        )
                        .padding(.trailing, MobiusSpace.l)
                        .padding(.bottom, bottomInset + MobiusSpace.m)
                        .onChange(of: geometry.size) { _, size in
                            voiceOrbOffset = clampedVoiceOrbOffset(voiceOrbOffset, in: size)
                        }
                }
                .transition(.scale.combined(with: .opacity))
                .zIndex(3)
            }
        }
        .onAppear {
            resetTranscriptPresentation()
        }
        .onChange(of: model.chat.chatPresentationRevision) {
            // SwiftUI can retain a popped navigation destination, so `onAppear` is not a
            // reliable signal when the same active chat is opened again.
            resetTranscriptPresentation()
        }
        .onChange(of: model.chat.selectedSessionID) {
            resetTranscriptPresentation()
        }
        .onChange(of: model.chat.composer) {
            model.chat.dictation.stopIfDraftChanged(model.chat.composer)
        }
        .onChange(of: model.chat.composerBlurRequest) { model.chat.dictation.stop() }
        .onDisappear { model.chat.dictation.stop() }
        .navigationTitle(chatTitle)
        .navigationSubtitle(navigationSubtitle)
        .toolbarTitleDisplayMode(.inline)
        .toolbarRole(.editor)
        .toolbar {
            ToolbarItem(placement: .title) {
                MobiusTitleText(verbatim: chatTitle)
                    .font(MobiusStyle.titleFont)
                    .lineLimit(1)
            }
            if model.chat.selectedSessionID != nil, !model.selectedSessionIsHidden {
                MobiusToolbarItem(placement: .primaryAction) {
                    ChatOptionsMenu(
                        presentedWidget: $presentedWidget,
                        presentedBotSettings: $presentedBotSettings,
                        showsFolderAttachmentBrowser: $showsFolderAttachmentBrowser
                    )
                }
                .sharedBackgroundVisibility(.hidden)
            }
            if hasVerticalToolbar,
                !model.selectedSessionIsHidden,
                model.chat.pendingApproval == nil,
                !model.chat.dictation.isActive
            {
                ToolbarSpacer(.flexible, placement: .bottomBar)
                if model.selectedRouteSupportsRealtimeVoice,
                    model.chat.realtimeVoiceCall == nil
                {
                    MobiusToolbarItem(placement: .bottomBar) {
                        MobiusToolbarIconButton(
                            glyph: .audioWave01, label: "Start voice chat"
                        ) {
                            model.startRealtimeVoice()
                        }
                        .buttonStyle(MobiusIconButtonStyle(bare: true))
                        .disabled(!model.canStartRealtimeVoice)
                    }
                    .sharedBackgroundVisibility(.hidden)
                }
                MobiusToolbarItem(placement: .bottomBar) {
                    railTrailingAction
                        .mobiusProminentIconButton(surfaceSize: 32, flat: true)
                }
                .sharedBackgroundVisibility(.hidden)
            }
        }
        .sheet(item: $chat.presentedPreview, content: PreviewTranscriptSheet.init)
        .sheet(item: $presentedWidget, content: FrontendWidgetSheet.init)
        .sheet(isPresented: $showsFolderAttachmentBrowser) {
            WorkspaceBrowserView(title: "Attach a folder for agent tools") { path in
                model.attachFolder(path)
            }
            .frame(idealWidth: 520, idealHeight: 620)
            .mobiusSheet()
        }
        .sheet(item: $presentedBotSettings) { bot in
            NavigationStack {
                AgentSettingsView(scope: .bot(bot.id))
                    .toolbar {
                        MobiusToolbarItem(placement: .cancellationAction) {
                            MobiusToolbarIconButton(glyph: .x, label: "Cancel") {
                                presentedBotSettings = nil
                            }
                        }
                    }
            }
            .mobiusSheet(detents: [.large])
        }
    }

    private func resetTranscriptPresentation() {
        transcriptPresentationID = UUID()
        isAtBottom = true
    }

    @ViewBuilder
    private var railTrailingAction: some View {
        if model.chat.realtimeVoiceCall != nil {
            MobiusToolbarIconButton(glyph: .stopFill, label: "End voice chat") {
                model.chat.stopRealtimeVoice()
            }
            .tint(palette.danger)
        } else if !model.composerRailShowsSendAction {
            MobiusToolbarIconButton(glyph: .mic01, label: "Dictate") {
                model.toggleComposerDictation()
            }
        } else {
            ComposerSendButton(send: sendFromRail)
                .mobiusToolbarIcon()
                .buttonStyle(.glass(.regular.tint(palette.accentFill)))
                .tint(palette.accentFill)
                .foregroundStyle(palette.onAccent)
        }
    }

    private func sendFromRail(_ delivery: ActiveMessageDelivery?) {
        model.chat.dictation.stop()
        _ = model.sendMessage(delivery: delivery)
    }

    private func clampedVoiceOrbOffset(_ offset: CGSize, in size: CGSize) -> CGSize {
        let diameter = 56 + MobiusSpace.m * 2
        return CGSize(
            width: min(0, max(diameter + MobiusSpace.l * 2 - size.width, offset.width)),
            height: min(
                0,
                max(diameter + bottomInset + MobiusSpace.m * 2 - size.height, offset.height)
            )
        )
    }

    private var chatTitle: String {
        model.currentSessionTitle
    }

    private var bottomInset: CGFloat {
        model.selectedSessionIsHidden && model.chat.pendingApproval == nil ? 0 : composerHeight
    }

    private var workspaceName: String {
        guard let path = model.workspace?.path else { return "" }
        let name = URL(fileURLWithPath: path).lastPathComponent
        return name.isEmpty ? path : name
    }

    private var navigationSubtitle: Text {
        guard !chatTitle.isEmpty else { return Text("") }
        let subtitle = Text(verbatim: chatSubtitle)
        guard let folders = model.chat.attachedFolders, !folders.isEmpty else { return subtitle }
        let folderCount = Text("Folders: \(folders.count)")
        return chatSubtitle.isEmpty ? folderCount : Text("\(subtitle) · \(folderCount)")
    }

    private var chatSubtitle: String {
        if model.selectedSessionIsHidden { return model.gateway.gatewayMachineName }
        return [workspaceName, model.gateway.gatewayMachineName]
            .filter { !$0.isEmpty }
            .joined(separator: " • ")
    }

    private func presentSelectedBotSettings() {
        guard let bot = model.selectedBot else { return }
        model.beginEditingBot(bot)
        presentedBotSettings = bot
    }

}

private struct DuoVoiceOrb: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette

    var body: some View {
        let voice = model.chat.realtimeVoice
        Button {
            model.chat.setRealtimeVoiceMuted(!voice.isMuted)
        } label: {
            AudioLevelEqualizer(
                amplitude: sqrt(voice.audioLevels.displayLevel),
                flare: voice.levelFlare,
                playbackColor: voice.audioLevels.isPlaybackActive
                    ? model.selectedBot?.tint.color ?? palette.accent : nil
            )
            .frame(width: 56, height: 56)
            .padding(MobiusSpace.m)
        }
        .buttonStyle(.glass)
        .buttonBorderShape(.circle)
        .tint(voice.isMuted ? palette.danger : palette.accent)
        .accessibilityLabel(voice.isMuted ? "Unmute voice chat" : "Mute voice chat")
        .accessibilityValue(voice.isMuted ? "Microphone muted" : "Microphone on")
        .help(voice.isMuted ? "Unmute voice chat" : "Mute voice chat")
    }
}

@MainActor
@Observable
final class ComposerDictation {
    private(set) var isRecording = false
    private(set) var isStarting = false
    private(set) var audioLevels: [Double] = []
    @ObservationIgnored private var meteredDuration = 0.0
    @ObservationIgnored private var meteredPeak = 0.0
    var isActive: Bool { isRecording || isStarting }
    @ObservationIgnored private let makeEngine: () -> AVAudioEngine
    @ObservationIgnored private var engine: AVAudioEngine?
    @ObservationIgnored private var request: SFSpeechAudioBufferRecognitionRequest?
    @ObservationIgnored private var task: SFSpeechRecognitionTask?
    @ObservationIgnored private var hasInputTap = false
    @ObservationIgnored private var ownsAudioSession = false
    @ObservationIgnored private var audioSessionTransition: Task<Void, Error>?
    @ObservationIgnored private var generation = UUID()
    @ObservationIgnored private var draft: DictationDraft?

    init(engine: @autoclosure @escaping () -> AVAudioEngine = AVAudioEngine()) {
        makeEngine = engine
    }

    static func supports(_ locale: Locale) -> Bool {
        guard let language = locale.language.languageCode?.identifier else { return false }
        return language == "en" || language == "fr"
    }

    func toggle(
        locale: Locale,
        currentText: @escaping @MainActor @Sendable () -> String,
        update: @escaping @MainActor @Sendable (String) -> Void,
        fail: @escaping @MainActor @Sendable (LocalizedStringResource) -> Void
    ) async {
        if isActive {
            stop()
            return
        }
        guard Self.supports(locale) else {
            fail("Dictation currently supports English and French.")
            return
        }
        let generation = UUID()
        self.generation = generation
        draft = DictationDraft(text: currentText())
        isStarting = true
        defer {
            if self.generation == generation { isStarting = false }
        }
        let authorization = await dictationAuthorization()
        guard self.generation == generation else { return }
        guard authorization == .authorized else {
            fail("Allow Speech Recognition in Settings to use dictation.")
            return
        }
        let canRecord = await AVAudioApplication.requestRecordPermission()
        guard self.generation == generation else { return }
        guard canRecord else {
            fail("Allow microphone access in Settings to use dictation.")
            return
        }
        guard let recognizer = SFSpeechRecognizer(locale: locale),
            recognizer.isAvailable, recognizer.supportsOnDeviceRecognition
        else {
            fail("On-device dictation is unavailable for this language.")
            return
        }

        let request = SFSpeechAudioBufferRecognitionRequest()
        request.shouldReportPartialResults = true
        request.addsPunctuation = true
        request.taskHint = .dictation
        request.requiresOnDeviceRecognition = true

        do {
            ownsAudioSession = true
            let previousTransition = audioSessionTransition
            let activation = Task {
                _ = try? await previousTransition?.value
                try await Self.setAudioSessionActive(true)
            }
            audioSessionTransition = activation
            try await activation.value
            guard self.generation == generation else { return }
            let engine = makeEngine()
            self.engine = engine
            let input = engine.inputNode
            let format = input.outputFormat(forBus: 0)
            guard format.sampleRate > 0, format.channelCount > 0 else {
                throw NSError(
                    domain: NSOSStatusErrorDomain, code: Int(kAudioFormatUnsupportedDataFormatError)
                )
            }
            input.installTap(onBus: 0, bufferSize: 1_024, format: format) {
                @Sendable [weak self, weak request] buffer, _ in
                request?.append(buffer)
                let level = Self.recordingLevel(in: buffer)
                let duration = Double(buffer.frameLength) / buffer.format.sampleRate
                Task { @MainActor [weak self] in
                    guard self?.generation == generation else { return }
                    self?.recordAudioLevel(level, duration: duration)
                }
            }
            hasInputTap = true
            engine.prepare()
            try engine.start()
            self.request = request
            isRecording = true
            Self.playCue("DictationStart")
            task = recognizer.recognitionTask(with: request) {
                @Sendable [weak self] result, error in
                let spoken = result?.bestTranscription.formattedString
                let isFinal = result?.isFinal == true
                let failed = error != nil
                Task { @MainActor [weak self] in
                    guard let self, self.generation == generation else { return }
                    if let spoken {
                        guard let text = draft?.update(spoken, currentText: currentText()) else {
                            stop()
                            return
                        }
                        update(text)
                    }
                    if isFinal || failed {
                        stop()
                        if spoken == nil, failed {
                            fail("Dictation stopped unexpectedly. Try again.")
                        }
                    }
                }
            }
        } catch {
            guard self.generation == generation else { return }
            stop()
            fail("Dictation could not start. Try again.")
        }
    }

    /// One bar per 100 ms of microphone audio; retain enough history for the widest composer.
    func recordAudioLevel(_ level: Double, duration: TimeInterval) {
        meteredPeak = max(meteredPeak, level)
        meteredDuration += duration
        guard meteredDuration >= 0.1 else { return }
        audioLevels.append(meteredPeak)
        if audioLevels.count > 128 { audioLevels.removeFirst(audioLevels.count - 128) }
        meteredDuration.formTruncatingRemainder(dividingBy: 0.1)
        meteredPeak = 0
    }

    nonisolated static func recordingLevel(in buffer: AVAudioPCMBuffer) -> Double {
        guard let samples = buffer.floatChannelData?.pointee else { return 0 }
        let count = Int(buffer.frameLength)
        guard count > 0 else { return 0 }
        let power = UnsafeBufferPointer(start: samples, count: count).reduce(0.0) {
            $0 + Double($1 * $1)
        }
        guard power.isFinite, power > 0 else { return 0 }
        // Map speech loudness in decibels; linear PCM amplitude makes normal speech invisible.
        let decibels = 10 * log10(power / Double(count))
        return min(1, max(0, (decibels + 60) / 50))
    }

    func stopIfDraftChanged(_ text: String) {
        if let draft, draft.text != text { stop() }
    }

    func cancel(currentText: String) -> String? {
        let original = draft?.originalText(ifCurrentText: currentText)
        stop()
        return original
    }

    func stop() {
        generation = UUID()
        draft = nil
        isStarting = false
        if let engine, engine.isRunning { engine.stop() }
        if hasInputTap {
            engine?.inputNode.removeTap(onBus: 0)
            hasInputTap = false
        }
        engine = nil
        request?.endAudio()
        task?.cancel()
        request = nil
        task = nil
        if isRecording { Self.playCue("DictationStop") }
        isRecording = false
        audioLevels.removeAll(keepingCapacity: true)
        meteredDuration = 0
        meteredPeak = 0
        if ownsAudioSession {
            ownsAudioSession = false
            let previousTransition = audioSessionTransition
            audioSessionTransition = Task {
                _ = try? await previousTransition?.value
                try await Self.setAudioSessionActive(false)
            }
        }
    }

    func stopAndReleaseAudioSession() async {
        stop()
        _ = try? await audioSessionTransition?.value
    }

    static func playCue(_ name: String) {
        guard let url = Bundle.main.url(forResource: name, withExtension: "wav") else { return }
        var sound: SystemSoundID = 0
        guard AudioServicesCreateSystemSoundID(url as CFURL, &sound) == kAudioServicesNoError
        else { return }
        let id = sound
        AudioServicesPlaySystemSoundWithCompletion(id) { @Sendable in
            AudioServicesDisposeSystemSoundID(id)
        }
    }

    nonisolated static func setAudioSessionActive(_ active: Bool) async throws {
        if #available(iOS 27.0, *) {
            let session = AVAudioSession.sharedInstance()
            if active {
                try session.setCategory(.record, mode: .measurement)
                try session.setAllowHapticsAndSystemSoundsDuringRecording(true)
                guard try await session.activate() else { throw CancellationError() }
            } else {
                _ = try await session.deactivate(options: .notifyOthersOnDeactivation)
            }
        } else {
            try await Task.detached {
                let session = AVAudioSession.sharedInstance()
                if active {
                    try session.setCategory(.record, mode: .measurement)
                    try session.setAllowHapticsAndSystemSoundsDuringRecording(true)
                }
                try session.setActive(active, options: active ? [] : .notifyOthersOnDeactivation)
            }.value
        }
    }
}

nonisolated func dictationAuthorization(
    request:
        @Sendable (@escaping @Sendable (SFSpeechRecognizerAuthorizationStatus) -> Void) -> Void =
        SFSpeechRecognizer.requestAuthorization
) async -> SFSpeechRecognizerAuthorizationStatus {
    await withCheckedContinuation { continuation in
        request { @Sendable status in continuation.resume(returning: status) }
    }
}

struct DictationDraft {
    private let original: String
    private(set) var text: String

    init(text: String) {
        original = text
        self.text = text
    }

    mutating func update(_ spoken: String, currentText: String) -> String? {
        guard currentText == text else { return nil }
        let separator =
            original.isEmpty || spoken.isEmpty || original.last?.isWhitespace == true
            ? "" : " "
        text = original + separator + spoken
        return text
    }

    func originalText(ifCurrentText currentText: String) -> String? {
        currentText == text ? original : nil
    }
}

private struct ChatOptionsMenu: View {
    @Environment(AppModel.self) private var model
    @Binding var presentedWidget: MountedWidget?
    @Binding var presentedBotSettings: BotRecord?
    @Binding var showsFolderAttachmentBrowser: Bool
    @State private var showsChatInfo = false

    var body: some View {
        HeaderOptionsMenu(label: "Chat options") {
            Button("Chat info", glyph: .info) {
                showsChatInfo = true
            }
            Section("Workspace") {
                if let git = model.gitStatus, !git.currentBranch.isEmpty {
                    Menu {
                        ForEach(git.branches, id: \.self) { branch in
                            Button {
                                model.switchGitBranch(to: branch)
                            } label: {
                                MobiusLabel(
                                    verbatim: branch,
                                    glyph: branch == git.currentBranch ? .check : .gitBranch
                                )
                            }
                            .disabled(branch == git.currentBranch)
                        }
                    } label: {
                        MobiusLabel(
                            verbatim: git.currentBranch,
                            glyph: .gitBranch
                        )
                    }
                    .disabled(model.isSwitchingGitBranch || !model.canModifySelectedSession)
                }
                Button {
                    model.showFiles()
                } label: {
                    MobiusLabel(
                        title: "Files",
                        glyph: .fileMagnifyingGlass
                    )
                }
                .disabled(
                    model.chat.selectedSessionID == nil || !model.gateway.connectionState.isReady)
                Button {
                    model.loadDirectory(
                        model.workspace?.path ?? (model.selectedGatewayIsMobiusCloud ? "." : "/")
                    )
                    showsFolderAttachmentBrowser = true
                } label: {
                    MobiusLabel(title: "Attach folder…", glyph: .folderPlus)
                }
                .disabled(!model.canModifySelectedSession)
                if let path = model.workspace?.path {
                    Button {
                        copyToPasteboard(path)
                    } label: {
                        MobiusLabel(
                            title: "Copy workspace path",
                            glyph: .copy
                        )
                    }
                }
            }
            Section("Actions") {
                Button {
                    guard let bot = model.selectedBot else { return }
                    model.beginEditingBot(bot)
                    presentedBotSettings = bot
                } label: {
                    MobiusLabel(title: "Bot agent settings", glyph: .slidersHorizontal)
                }
                .disabled(model.selectedBot == nil)
                if let session = model.selectedSession {
                    Button("Reassign Bot", glyph: .bot) {
                        model.chat.sessionToReassign = session
                    }
                    .disabled(!model.canReassignSession(session))
                }
                ForEach(model.chat.chatMenuWidgets) { widget in
                    Button {
                        activate(widget)
                    } label: {
                        MobiusLabel(
                            title: frontendPresentationText(widget.widget.text),
                            glyph: widget.glyph
                        )
                    }
                    .disabled(widget.widget.content == nil && widget.widget.action == nil)
                }
            }
            if let session = model.selectedSession {
                Section("Manage") {
                    Button {
                        model.chat.setSessionPinned(session, pinned: !session.pinned)
                    } label: {
                        MobiusLabel(
                            title: session.pinned ? "Unpin chat" : "Pin chat",
                            glyph: session.pinned ? .pushPinSlash : .pushPin
                        )
                    }
                    .disabled(!model.canRenameSession)
                    Button {
                        model.beginRenamingSession(session)
                    } label: {
                        MobiusLabel(title: "Rename chat", glyph: .pencilSimple)
                    }
                    .disabled(!model.canRenameSession)
                    Button(role: .destructive) {
                        model.beginDeletingSession(session)
                    } label: {
                        MobiusLabel(title: "Delete chat", glyph: .trash)
                    }
                    .disabled(!model.canRenameSession)
                }
            }
        } icon: {
            ChatBotFace(size: MobiusStyle.rowTouch)
        }
        .buttonStyle(.plain)
        .popover(isPresented: $showsChatInfo) {
            ChatInfoView()
                .presentationCompactAdaptation(.popover)
        }
    }

    private func activate(_ widget: MountedWidget) {
        if widget.widget.action != nil {
            model.submitWidget(widget)
        }
        if widget.widget.content != nil {
            presentedWidget = widget
        }
    }
}

struct ChatInfoView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: MobiusSpace.l) {
                Text("Chat info")
                    .font(MobiusStyle.titleFont)
                detail("Workspace", value: model.workspace?.path)
                if let bot = model.selectedBot {
                    detail("Bot", value: "\(bot.name) (@\(bot.handle))")
                }
                detail("Model", value: model.chatModelLabel)
                detail("Gateway", value: model.gateway.gatewayMachineName)
                Divider()
                VStack(alignment: .leading, spacing: MobiusSpace.s) {
                    Text("Attached folders")
                        .font(MobiusStyle.captionFont)
                        .foregroundStyle(.secondary)
                    if let folders = model.chat.attachedFolders {
                        if folders.isEmpty {
                            Text("No attached folders")
                                .foregroundStyle(.secondary)
                        }
                        ForEach(folders, id: \.self) { folder in
                            HStack(alignment: .top, spacing: MobiusSpace.s) {
                                MobiusIcon(.folderPlus)
                                Text(verbatim: folder)
                                    .textSelection(.enabled)
                            }
                        }
                    } else {
                        Text("Connect to load attached folders.")
                            .foregroundStyle(.secondary)
                    }
                }
            }
            .font(MobiusStyle.bodyFont)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(MobiusSpace.l)
        }
        .frame(width: 300, height: 440)
    }

    private func detail(_ title: LocalizedStringKey, value: String?) -> some View {
        VStack(alignment: .leading, spacing: MobiusSpace.xs) {
            Text(title)
                .font(MobiusStyle.captionFont)
                .foregroundStyle(.secondary)
            if let value, !value.isEmpty {
                Text(verbatim: value)
                    .textSelection(.enabled)
            } else {
                Text("None")
                    .foregroundStyle(.secondary)
            }
        }
    }
}

struct ReassignChatSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let session: SessionRecord
    @State private var submittedBotID: String?

    var body: some View {
        NavigationStack {
            List(model.bots) { bot in
                Button {
                    if model.reassignSession(session, to: bot.id) != nil {
                        submittedBotID = bot.id
                    }
                } label: {
                    HStack {
                        SettingsRowLabel(
                            title: .verbatim(bot.name), detail: .verbatim("@\(bot.handle)")
                        ) {
                            MobiusIcon(
                                .bot, size: MobiusStyle.glyphLead, foreground: bot.tint.color)
                        }
                        if bot.id == currentBotID {
                            MobiusIcon(.check)
                                .accessibilityLabel("Current Bot")
                        }
                    }
                }
                .disabled(bot.id == currentBotID || !model.canReassignSession(session))
            }
            .mobiusNavigationTitle("Reassign Bot")
            .toolbar {
                MobiusToolbarItem(placement: .cancellationAction) {
                    MobiusToolbarIconButton(glyph: .x, label: "Cancel") { dismiss() }
                }
            }
        }
        .onChange(of: currentBotID) { _, newValue in
            if newValue == submittedBotID { dismiss() }
        }
    }

    private var currentBotID: String {
        model.chat.sessions.first { $0.sessionId == session.sessionId }?.sessionContext.ownerId
            ?? session.sessionContext.ownerId
    }
}
