import Foundation
import SwiftUI
import SwiftStreamingMarkdown
import CoreText
@preconcurrency import AVFoundation
import UIKit

extension MountedWidget {
    var glyph: MobiusGlyph {
        widget.symbol.map { MobiusSymbol.glyph(for: $0) } ?? .squaresFour
    }

}
struct ChatView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var composerHeight: CGFloat = 0
    @State private var isAtBottom = true
    @State private var scrollToBottomRequest = 0
    @State private var presentedWidget: MountedWidget?
    @State private var presentedBotSettings: BotRecord?
    @State private var showsFolderAttachmentBrowser = false
    @State private var hasEntered = false
    @State private var transcriptPresentationID = UUID()

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
        }
        .scaleEffect(hasEntered || reduceMotion ? 1 : 0.985)
        .opacity(hasEntered ? 1 : 0)
        .onAppear {
            resetTranscriptPresentation()
            withAnimation(reduceMotion ? .easeOut(duration: 0.12) : .smooth(duration: 0.28)) {
                hasEntered = true
            }
        }
        .onChange(of: model.chat.chatPresentationRevision) {
            // SwiftUI can retain a popped navigation destination, so `onAppear` is not a
            // reliable signal when the same active chat is opened again.
            resetTranscriptPresentation()
        }
        .onChange(of: model.chat.selectedSessionID) {
            resetTranscriptPresentation()
        }
        .navigationTitle(chatTitle)
        .toolbarTitleDisplayMode(.inline)
        .toolbarRole(.editor)
        .toolbar {
            // Title changes animate glyphs, so the principal title must be a view the app
            // owns rather than the system's opaque navigation title.
            ToolbarItem(placement: .principal) {
                VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                    MobiusTitleText(verbatim: chatTitle)
                        .font(MobiusStyle.titleFont)
                        .lineLimit(1)
                    HStack(spacing: MobiusSpace.xs) {
                        if !chatSubtitle.isEmpty {
                            Text(verbatim: chatSubtitle)
                                .lineLimit(1)
                        }
                        if let folders = model.chat.attachedFolders, !folders.isEmpty {
                            HStack(spacing: MobiusSpace.xxs) {
                                MobiusIcon(.folderPlus, size: 12)
                                Text(folders.count, format: .number)
                            }
                            .fixedSize()
                            .accessibilityElement(children: .ignore)
                            .accessibilityLabel(Text("Folders: \(folders.count)"))
                        }
                    }
                    .font(MobiusStyle.captionFont)
                    .foregroundStyle(.secondary)
                }
                .accessibilityElement(children: .combine)
            }
            // One item holding both, so the spacing is this stack's rather than the bar's
            // between two items. The 44pt targets still touch; only the slack goes.
            ToolbarItem(placement: .primaryAction) {
                if model.chat.selectedSessionID != nil, !model.selectedSessionIsHidden {
                    HeaderActionGroup {
                        newChatButton
                        ChatOptionsMenu(
                            presentedWidget: $presentedWidget,
                            presentedBotSettings: $presentedBotSettings,
                            showsFolderAttachmentBrowser: $showsFolderAttachmentBrowser
                        )
                    }
                }
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
                        ToolbarItem(placement: .cancellationAction) {
                            Button("Done") { presentedBotSettings = nil }
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

    /// Starting a chat in the folder you are already in belongs with the other page-level
    /// actions, not in the composer beside the controls that shape the message being written.
    private var newChatButton: some View {
        Button(action: model.openNewSessionInCurrentWorkspace) {
            MobiusIcon(.notePencil, foreground: .primary)
        }
        .groupedHeaderAction()
        .disabled(model.workspace == nil || !model.canCreateSession)
        .accessibilityLabel("New chat in this folder")
        .help("New chat in this folder")
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
                    Button("Reassign Bot", glyph: .aiScan) {
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
                Button {
                    model.openNewSession()
                } label: {
                    MobiusLabel(
                        title: "New chat in another folder…",
                        glyph: .folderPlus
                    )
                }
                .disabled(!model.canCreateSession)
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
        }
        .groupedHeaderAction()
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
                detail("Swarm", value: model.selectedBotSwarm?.title)
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
                                .aiScan, size: MobiusStyle.glyphLead, foreground: bot.tint.color)
                        }
                        if bot.id == currentBotID {
                            MobiusIcon(.check)
                                .accessibilityLabel("Current Bot")
                        }
                    }
                }
                .disabled(bot.id == currentBotID || !model.canReassignSession(session))
            }
            .navigationTitle("Reassign Bot")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
            }
        }
        .onChange(of: currentBotID) { _, newValue in
            if newValue == submittedBotID { dismiss() }
        }
    }

    private var currentBotID: String {
        model.chat.sessions.first { $0.sessionId == session.sessionId }?.sessionContext.botId
            ?? session.sessionContext.botId
    }
}
