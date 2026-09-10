import AppKit
import SwiftUI

struct VoiceMenuView: View {
    @Bindable var model: MenuBarModel
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.colorScheme) private var colorScheme
    @State private var isVisible = false

    private var palette: MobiusPalette { MobiusPalette(colorScheme) }

    var body: some View {
        VStack(spacing: 0) {
            VoiceSelectionMenus(model: model).padding([.horizontal, .top], MobiusSpace.m)
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
        .background(palette.canvas)
        .tint(palette.accent)
        .onAppear {
            isVisible = true
            model.refreshChats()
        }
        .onDisappear { isVisible = false }
    }

    private var voiceSurface: some View {
        VStack(spacing: 0) {
            AudioLevelEqualizer(
                amplitude: isVisible ? sqrt(model.voice.audioLevels.displayLevel) : 0,
                flare: model.voice.levelFlare,
                playbackColor: model.voice.audioLevels.isPlaybackActive ? playbackColor : nil
            )
            .frame(height: 80)
            .padding(.horizontal, MobiusSpace.l)
            .animation(
                reduceMotion ? nil : .smooth(duration: 0.09),
                value: [model.voice.audioLevels.displayLevel, model.voice.levelFlare]
            )
            .accessibilityHidden(true)
            HStack(spacing: MobiusSpace.s) {
                Button {
                    model.voice.isMuted.toggle()
                } label: {
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
                Button {
                    if model.voiceCall == nil {
                        model.startVoice()
                    } else {
                        model.stopVoice()
                    }
                } label: {
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
        }
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
                    ForEach(model.matchingChats) { chat in
                        Text(verbatim: chat.name).tag(Optional(chat.id))
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
        .frame(width: MobiusStyle.rowCompact, height: MobiusStyle.rowRegular)
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
        .accessibilityLabel("Gateway voice options")
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

private extension View {
    func pillSurface(_ palette: MobiusPalette) -> some View {
        frame(maxWidth: .infinity, minHeight: MobiusStyle.controlHeight)
            .padding(.horizontal, MobiusSpace.s)
            .background(palette.panel, in: .capsule)
            .overlay { Capsule().strokeBorder(palette.line, lineWidth: MobiusStyle.borderWidth) }
            .contentShape(.capsule)
    }
}
