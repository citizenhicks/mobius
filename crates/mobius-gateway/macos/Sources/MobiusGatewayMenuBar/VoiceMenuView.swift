import AppKit
import SwiftUI

struct VoiceMenuView: View {
    @Bindable var model: MenuBarModel
    @Bindable var presentation: VoicePanelController
    var isPinned = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.accessibilityVoiceOverEnabled) private var voiceOverEnabled
    @Environment(\.colorScheme) private var colorScheme
    @State private var isVisible = false
    @State private var isHovered = false
    @State private var isTrackingMenu = false

    private var palette: MobiusPalette { MobiusPalette(colorScheme) }
    private var showsControls: Bool {
        isHovered || isTrackingMenu || presentation.controlsRequested || voiceOverEnabled
            || model.approval != nil || model.message != nil
    }

    var body: some View {
        Group {
            if isMini { miniSurface } else { fullSurface }
        }
        .contentShape(Rectangle())
        .tint(palette.accent)
        .background {
            VoiceHoverArea { hovering in
                isHovered = hovering
            }
        }
        .focusable()
        .focusEffectDisabled()
        .onKeyPress(.tab) {
            presentation.showControls()
            return .ignored
        }
        .onKeyPress(.escape) {
            presentation.controlsRequested = false
            return .handled
        }
        .onReceive(NotificationCenter.default.publisher(for: NSWindow.didResignKeyNotification)) {
            _ in
            presentation.controlsRequested = false
        }
        .accessibilityAction(named: "Show voice controls", presentation.showControls)
        .onReceive(NotificationCenter.default.publisher(for: NSMenu.didBeginTrackingNotification)) {
            _ in
            isTrackingMenu = true
        }
        .onReceive(NotificationCenter.default.publisher(for: NSMenu.didEndTrackingNotification)) {
            _ in
            isTrackingMenu = false
        }
        .animation(reduceMotion ? nil : .easeInOut(duration: 0.25), value: showsControls)
        .onGeometryChange(for: CGSize.self) {
            $0.size
        } action: { size in
            if isPinned { presentation.resize(to: size) }
        }
        .onAppear {
            isVisible = true
            model.refreshChats()
        }
        .onDisappear { isVisible = false }
    }

    private var isMini: Bool {
        isPinned && presentation.isMini && model.approval == nil && model.message == nil
    }

    private var fullSurface: some View {
        VStack(spacing: 0) {
            VoiceSelectionMenus(model: model, presentation: presentation)
                .padding([.horizontal, .top], MobiusSpace.m)
                .voiceControlsVisible(showsControls)
            voiceSurface
            if let approval = model.approval {
                VoiceApprovalView(
                    approval: approval, isSubmitting: model.approvalSubmissionID != nil,
                    approve: { model.resolveApproval(approve: true) },
                    decline: { model.resolveApproval(approve: false) }
                )
                .disabled(!model.chatIsReady)
                .padding(MobiusSpace.l)
            }
            if let message = model.message {
                Text(verbatim: message)
                    .font(.caption)
                    .foregroundStyle(palette.muted)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding([.horizontal, .bottom], MobiusSpace.l)
            }
        }
        .frame(width: 400)
        .fixedSize(horizontal: false, vertical: true)
        .background {
            if isPinned {
                VoiceGlassSurface(
                    shape: MobiusStyle.cardShape,
                    isActive: model.voice.isConnected && !model.voice.isMuted,
                    color: model.selectedBot?.color ?? palette.accent)
            }
        }
        .padding(isPinned ? 8 : 0)
    }

    private var miniSurface: some View {
        waveform
            .frame(width: 80, height: 80)
            .clipShape(.circle)
            .background {
                VoiceGlassSurface(
                    shape: Circle(),
                    isActive: model.voice.isConnected && !model.voice.isMuted,
                    color: model.selectedBot?.color ?? palette.accent)
            }
            .contentShape(.circle)
            .contextMenu {
                Button("Exit Mini mode", action: presentation.showControls)
                Button("Keyboard shortcuts…", action: presentation.showKeyboardShortcuts)
                Button("Unpin voice window", action: presentation.unpin)
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("Mini voice")
            .accessibilityValue(model.status)
            .help("Right-click to show voice controls.")
            .padding(8)
    }

    private var voiceSurface: some View {
        VStack(spacing: 0) {
            waveform
                .frame(height: 80)
                .padding(.horizontal, MobiusSpace.l)
            HStack(spacing: MobiusSpace.s) {
                Button(action: model.toggleMicrophone) {
                    VoiceIcon(
                        model.voice.isMuted ? "micOff01" : "mic01", size: MobiusStyle.iconSize
                    )
                    .foregroundStyle(microphoneColor)
                    .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
                }
                .buttonStyle(.plain)
                .disabled(model.voiceCall == nil)
                .accessibilityLabel(microphoneLabel)
                Spacer(minLength: 0)
                if !model.isReady, !model.isConnecting {
                    Button("Reconnect") { model.connect() }
                        .font(.caption)
                        .buttonStyle(.plain)
                } else {
                    Text(model.status)
                        .font(MobiusStyle.captionFont)
                        .foregroundStyle(palette.muted)
                        .lineLimit(2)
                        .multilineTextAlignment(.center)
                }
                Spacer(minLength: 0)
                Button(action: model.toggleVoice) {
                    VoiceIcon(
                        model.voiceCall == nil ? "playFill" : "stopFill", size: MobiusStyle.iconSize
                    )
                    .foregroundStyle(
                        model.voiceCall == nil && !model.canStartVoice ? palette.muted : .primary
                    )
                    .frame(
                        width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
                }
                .buttonStyle(.plain)
                .disabled(model.voiceCall == nil && !model.canStartVoice)
                .accessibilityLabel(model.voiceCall == nil ? "Start voice" : "End voice chat")
            }
            .padding(.horizontal, MobiusSpace.s)
            .padding(.bottom, MobiusSpace.s)
            .voiceControlsVisible(showsControls)
        }
    }

    private var waveform: some View {
        AudioLevelEqualizer(
            amplitude: isVisible ? sqrt(model.voice.audioLevels.displayLevel) : 0,
            flare: model.voice.levelFlare,
            playbackColor: model.voice.audioLevels.isPlaybackActive ? playbackColor : nil
        )
        .animation(
            reduceMotion ? nil : .smooth(duration: 0.09),
            value: [model.voice.audioLevels.displayLevel, model.voice.levelFlare]
        )
        .accessibilityHidden(true)
    }

    private var microphoneLabel: String {
        model.voice.isMuted ? "Unmute microphone" : "Mute microphone"
    }

    private var playbackColor: Color { model.selectedBot?.color ?? .primary }

    private var microphoneColor: Color {
        if model.voiceCall == nil { return palette.muted }
        return model.voice.isMuted ? palette.accent : .primary
    }
}

private struct VoiceSelectionMenus: View {
    @Bindable var model: MenuBarModel
    let presentation: VoicePanelController
    @Environment(\.dismiss) private var dismiss
    @Environment(\.colorScheme) private var colorScheme

    private var palette: MobiusPalette { MobiusPalette(colorScheme) }

    var body: some View {
        HStack(spacing: MobiusSpace.s) {
            Menu {
                Picker(
                    "Bot",
                    selection: Binding(
                        get: { model.selectedBot?.id },
                        set: { if let id = $0 { model.beginNewChat(botID: id) } }
                    )
                ) {
                    ForEach(model.bots) { bot in
                        Label {
                            Text(verbatim: bot.name)
                        } icon: {
                            VoiceIcon.menuImage("aiScan", color: bot.color, scheme: colorScheme)
                        }
                        .tag(Optional(bot.id))
                    }
                }
                .pickerStyle(.inline)
            } label: {
                HStack(spacing: MobiusSpace.xs) {
                    VoiceIcon.menuImage(
                        "aiScan", color: model.selectedBot?.color ?? palette.muted,
                        scheme: colorScheme)
                    Text(verbatim: model.selectedBot?.name ?? "Bot")
                }
            }
            .pillSurface(palette)
            .accessibilityLabel("Choose Bot")
            .accessibilityValue(model.selectedBot?.name ?? "None selected")
            .help(model.selectedBot.map { "Choose Bot · @\($0.handle)" } ?? "Choose Bot")
            .disabled(!model.canChooseChat)
            Menu {
                Picker(
                    "Folder",
                    selection: Binding(
                        get: { model.workspacePath },
                        set: { model.beginNewChat(workspace: $0) }
                    )
                ) {
                    ForEach(model.workspacePaths, id: \.self) { path in
                        Label {
                            Text(verbatim: folderName(path))
                        } icon: {
                            VoiceIcon.menuImage("folder", color: .primary, scheme: colorScheme)
                        }
                        .tag(path)
                        .help(path)
                    }
                }
                .pickerStyle(.inline)
                Divider()
                Button(action: chooseFolder) {
                    Label {
                        Text("Add new folder")
                    } icon: {
                        VoiceIcon.menuImage("plus", color: .primary, scheme: colorScheme)
                    }
                }
            } label: {
                HStack(spacing: MobiusSpace.xs) {
                    VoiceIcon.menuImage("folder", color: palette.muted, scheme: colorScheme)
                    Text(verbatim: model.workspaceName)
                }
            }
            .pillSurface(palette)
            .accessibilityLabel("Choose workspace")
            .accessibilityValue(model.workspacePath)
            .help(model.workspacePath)
            .disabled(!model.canChooseChat)
            Menu {
                Picker(
                    "Chat",
                    selection: Binding(
                        get: { model.selectedChatID },
                        set: { id in
                            if let chat = model.chats.first(where: { $0.id == id }) {
                                model.openChat(chat)
                            } else {
                                model.beginNewChat()
                            }
                        }
                    )
                ) {
                    Label {
                        Text("New chat")
                    } icon: {
                        VoiceIcon.menuImage("plus", color: .primary, scheme: colorScheme)
                    }
                    .tag(nil as String?)
                    ForEach(model.chatGroups, id: \.workspace) { group in
                        Section {
                            ForEach(group.chats) { chat in
                                Text(verbatim: chat.name).tag(Optional(chat.id))
                            }
                        } header: {
                            Text(verbatim: folderName(group.workspace)).help(group.workspace)
                        }
                    }
                }
                .pickerStyle(.inline)
            } label: {
                HStack(spacing: MobiusSpace.xs) {
                    VoiceIcon.menuImage("chatCircle", color: palette.muted, scheme: colorScheme)
                    Text(verbatim: model.selectedChat?.name ?? "New chat")
                }
            }
            .pillSurface(palette)
            .accessibilityLabel("Choose chat")
            .accessibilityValue(model.selectedChat?.name ?? "New chat")
            .help(model.selectedChat?.name ?? "Choose chat")
            .disabled(!model.canChooseChat)
            options
        }
        .font(MobiusStyle.captionFont.weight(.medium))
        .lineLimit(1)
        .truncationMode(.middle)
        .buttonStyle(.plain)
        .tint(.primary)
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
    }

    private var options: some View {
        Menu {
            if presentation.corner != nil {
                Button("Mini mode") { presentation.isMini = true }
                Button("Unpin voice window") { presentation.unpin() }
            }
            Menu("Pin to corner") {
                ForEach(VoiceCorner.allCases) { corner in
                    Button(corner.title) { pin(to: corner) }
                }
            }
            Divider()
            Button("Keyboard shortcuts…", action: presentation.showKeyboardShortcuts)
            Button("Refresh chats") { model.refreshChats() }
            Button("Reconnect gateway") { model.connect() }
                .disabled(model.isConnecting)
            Divider()
            Button("Quit voice menu") { NSApplication.shared.terminate(nil) }
                .keyboardShortcut("q")
        } label: {
            VoiceIcon.menuImage(
                "dotsThree", color: .primary, size: MobiusStyle.iconSize, scheme: colorScheme)
        }
        .menuStyle(.button)
        .buttonStyle(.glass)
        .buttonBorderShape(.circle)
        .controlSize(.small)
        .menuIndicator(.hidden)
        .fixedSize()
        .accessibilityLabel("Gateway voice options")
    }

    private func pin(to corner: VoiceCorner) {
        let wasPinned = presentation.corner != nil
        presentation.pin(to: corner)
        if !wasPinned { dismiss() }
    }

    private func folderName(_ path: String) -> String {
        path == "." ? "Default folder" : URL(fileURLWithPath: path).lastPathComponent
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Choose folder"
        NSApplication.shared.activate()
        panel.begin { response in
            if response == .OK, let url = panel.url {
                model.beginNewChat(workspace: url.path)
            }
        }
    }

}

extension View {
    func voiceControlsVisible(_ visible: Bool) -> some View {
        opacity(visible ? 1 : 0)
            .allowsHitTesting(visible)
            .accessibilityHidden(!visible)
    }

    fileprivate func pillSurface(_ palette: MobiusPalette) -> some View {
        frame(maxWidth: .infinity, minHeight: MobiusStyle.controlHeight)
            .padding(.horizontal, MobiusSpace.s)
            .background(palette.panel, in: .capsule)
            .overlay { Capsule().strokeBorder(palette.line, lineWidth: MobiusStyle.borderWidth) }
            .contentShape(.capsule)
    }
}

private struct VoiceGlassSurface<Surface: InsettableShape>: View {
    let shape: Surface
    let isActive: Bool
    let color: Color
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        Color.clear
            .glassEffect(.regular, in: shape)
            .opacity(0.82)
            .overlay {
                TimelineView(
                    .animation(minimumInterval: 1.0 / 30, paused: !isActive || reduceMotion)
                ) { timeline in
                    let phase = timeline.date.timeIntervalSinceReferenceDate * .pi
                    let glow = reduceMotion ? 0.65 : 0.45 + 0.25 * sin(phase)
                    shape
                        .strokeBorder(
                            LinearGradient(
                                colors: [.white.opacity(0.7), color, color.opacity(0.6)],
                                startPoint: .topLeading, endPoint: .bottomTrailing),
                            lineWidth: 1.5
                        )
                        .shadow(color: color.opacity(glow), radius: 6)
                        .opacity(isActive ? glow : 0)
                }
            }
            .allowsHitTesting(false)
            .accessibilityHidden(true)
    }
}
