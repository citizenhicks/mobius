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
    @State private var hasEntered = false
    @State private var transcriptPresentationID = UUID()

    var body: some View {
        @Bindable var model = model
        ZStack(alignment: .bottom) {
            TranscriptView(
                bottomInset: composerHeight,
                isAtBottom: $isAtBottom,
                scrollToBottomRequest: scrollToBottomRequest
            )
            .id(transcriptPresentationID)
            ComposerView(showBotSettings: presentSelectedBotSettings)
                .onGeometryChange(for: CGFloat.self) { geometry in
                    geometry.size.height
                } action: { height in
                    composerHeight = height
                }
                .zIndex(1)
            if !isAtBottom {
                Button("Scroll to latest", glyph: .arrowDown) {
                    scrollToBottomRequest += 1
                }
                .mobiusIconButton()
                .padding(.bottom, composerHeight + 12)
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
        .onChange(of: model.chatPresentationRevision) {
            // SwiftUI can retain a popped navigation destination, so `onAppear` is not a
            // reliable signal when the same active chat is opened again.
            resetTranscriptPresentation()
        }
        .onChange(of: model.selectedSessionID) {
            resetTranscriptPresentation()
        }
        .navigationTitle(chatTitle)
        .toolbarTitleDisplayMode(.inline)
        .toolbarRole(.editor)
        .toolbar {
            // Title changes animate glyphs, so the principal title must be a view the app
            // owns rather than the system's opaque navigation title.
            ToolbarItem(placement: .principal) {
                VStack(spacing: MobiusSpace.xxs) {
                    MobiusTitleText(verbatim: chatTitle)
                        .font(MobiusStyle.titleFont)
                        .lineLimit(1)
                    if !chatSubtitle.isEmpty {
                        Text(verbatim: chatSubtitle)
                            .font(MobiusStyle.captionFont)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                }
                .accessibilityElement(children: .combine)
            }
            // One item holding both, so the spacing is this stack's rather than the bar's
            // between two items. The 44pt targets still touch; only the slack goes.
            ToolbarItem(placement: .primaryAction) {
                if model.selectedSessionID != nil {
                    HeaderActionGroup {
                        newChatButton
                        ChatOptionsMenu(
                            presentedWidget: $presentedWidget,
                            presentedBotSettings: $presentedBotSettings
                        )
                    }
                }
            }
        }
        .sheet(item: $model.presentedPreview, content: PreviewTranscriptSheet.init)
        .sheet(item: $presentedWidget, content: FrontendWidgetSheet.init)
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

    private var workspaceName: String {
        guard let path = model.workspace?.path else { return "" }
        let name = URL(fileURLWithPath: path).lastPathComponent
        return name.isEmpty ? path : name
    }

    private var chatSubtitle: String {
        [workspaceName, model.gatewayMachineName]
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

    var body: some View {
        HeaderOptionsMenu(label: "Chat options") {
            Section {
                Button {} label: {
                    Text(verbatim: model.workspace?.path ?? "No chat selected")
                }
                .disabled(true)
                if let session = model.selectedSession,
                   let bot = model.bot(for: session) {
                    Button {} label: {
                        botIdentityLabel(bot)
                    }
                    .disabled(true)
                }
                if let swarm = model.selectedBotSwarm {
                    Button {} label: {
                        MobiusLabel(verbatim: swarm.title, glyph: .swarm)
                    }
                    .disabled(true)
                }
            }
            Section {
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
                Button { model.showFiles() } label: {
                    MobiusLabel(
                        title: "Files",
                        glyph: .fileMagnifyingGlass
                    )
                }
                .disabled(model.selectedSessionID == nil || !model.connectionState.isReady)
                if let path = model.workspace?.path {
                    Button { copyToPasteboard(path) } label: {
                        MobiusLabel(
                            title: "Copy workspace path",
                            glyph: .copy
                        )
                    }
                }
            }
            Section {
                Button {
                    guard let bot = model.selectedBot else { return }
                    model.beginEditingBot(bot)
                    presentedBotSettings = bot
                } label: {
                    MobiusLabel(title: "Bot agent settings", glyph: .slidersHorizontal)
                }
                .disabled(model.selectedBot == nil)
                ForEach(model.chatMenuWidgets) { widget in
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
                Section {
                    Button {
                        model.setSessionPinned(session, pinned: !session.pinned)
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
    }

    @ViewBuilder
    private func botIdentityLabel(_ bot: BotRecord) -> some View {
        let title = "\(bot.name) (@\(bot.handle))"
        if let image = MobiusGlyph.aiScan.menuImage(bot.tint.color) {
            Label { Text(verbatim: title) } icon: { image }
        } else {
            MobiusLabel(
                verbatim: title,
                glyph: .aiScan,
                iconColor: bot.tint.color
            )
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
