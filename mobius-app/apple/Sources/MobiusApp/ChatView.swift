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
    @State private var showsChatAgentSettings = false
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
            ComposerView()
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
            // SwiftUI can retain a navigation destination after it is popped. Give every
            // presentation a fresh scroll state even when the same session is reopened.
            transcriptPresentationID = UUID()
            withAnimation(reduceMotion ? .easeOut(duration: 0.12) : .smooth(duration: 0.28)) {
                hasEntered = true
            }
        }
        .onChange(of: model.selectedSessionID) {
            transcriptPresentationID = UUID()
            isAtBottom = true
        }
        .navigationTitle(chatTitle)
        .toolbarTitleDisplayMode(.inline)
        .toolbarRole(.editor)
        .toolbar {
            // Title changes animate glyphs, so the principal title must be a view the app
            // owns rather than the system's opaque navigation title.
            ToolbarItem(placement: .principal) {
                VStack(spacing: MobiusSpace.xxs) {
                    MobiusTitleText(title: chatTitle)
                        .font(MobiusStyle.titleFont)
                        .lineLimit(1)
                    if !chatSubtitle.isEmpty {
                        Text(chatSubtitle)
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
                HeaderActionGroup {
                    newChatButton
                    ChatOptionsMenu(
                        presentedWidget: $presentedWidget,
                        showsAgentSettings: $showsChatAgentSettings
                    )
                }
            }
        }
        .sheet(item: $model.presentedPreview, content: PreviewTranscriptSheet.init)
        .sheet(item: $presentedWidget, content: FrontendWidgetSheet.init)
        .sheet(isPresented: $showsChatAgentSettings) {
            NavigationStack {
                AgentSettingsView(scope: .currentChat)
                    .toolbar {
                        ToolbarItem(placement: .cancellationAction) {
                            Button("Done") { showsChatAgentSettings = false }
                        }
                    }
            }
            .mobiusSheet(detents: [.large])
        }
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

    private var workspaceName: String {
        guard let path = model.workspace?.path else { return "" }
        return path.split { $0 == "/" || $0 == "\\" }.last.map(String.init) ?? path
    }

    private var chatTitle: String {
        model.currentSessionTitle
    }

    private var chatSubtitle: String {
        [workspaceName, model.gatewayMachineName]
            .filter { !$0.isEmpty }
            .joined(separator: " • ")
    }
}

private struct ChatOptionsMenu: View {
    @Environment(AppModel.self) private var model
    @Binding var presentedWidget: MountedWidget?
    @Binding var showsAgentSettings: Bool

    var body: some View {
        HeaderOptionsMenu(label: "Chat options") {
            Section(model.workspace?.path ?? "No chat selected") {
                if let git = model.gitStatus, !git.currentBranch.isEmpty {
                    Menu {
                        ForEach(git.branches, id: \.self) { branch in
                            Button {
                                model.switchGitBranch(to: branch)
                            } label: {
                                MobiusLabel(
                                    title: branch,
                                    glyph: branch == git.currentBranch ? .check : .gitBranch
                                )
                            }
                            .disabled(branch == git.currentBranch)
                        }
                    } label: {
                        MobiusLabel(
                            title: git.currentBranch,
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
                    showsAgentSettings = true
                } label: {
                    MobiusLabel(
                        title: "Chat agent settings",
                        glyph: .slidersHorizontal
                    )
                }
                .disabled(model.selectedSessionID == nil || model.agentSnapshot == nil)
                ForEach(model.chatMenuWidgets) { widget in
                    Button {
                        activate(widget)
                    } label: {
                        MobiusLabel(
                            title: widget.widget.text,
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

    private func activate(_ widget: MountedWidget) {
        if widget.widget.action != nil {
            model.submitWidget(widget)
        }
        if widget.widget.content != nil {
            presentedWidget = widget
        }
    }
}
