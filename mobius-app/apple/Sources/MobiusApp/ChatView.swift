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
    @State private var swarmCreation: SwarmCreationSelection?
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
                        swarmCreation: $swarmCreation,
                        showsAgentSettings: $showsChatAgentSettings
                    )
                }
            }
        }
        .sheet(item: $model.presentedPreview, content: PreviewTranscriptSheet.init)
        .sheet(item: $presentedWidget, content: FrontendWidgetSheet.init)
        .sheet(item: $swarmCreation) { selection in
            SwarmCreationPicker(selection: selection)
                .mobiusSheet()
        }
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
    @Binding var swarmCreation: SwarmCreationSelection?
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
                SwarmMenuSection(session: session, swarmCreation: $swarmCreation)
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

private struct SwarmMenuSection: View {
    @Environment(AppModel.self) private var model
    @State private var confirmsDisband = false
    let session: SessionRecord
    @Binding var swarmCreation: SwarmCreationSelection?

    @ViewBuilder
    var body: some View {
        if let swarm = model.swarm(containing: session.sessionId) {
            Section("Swarm") {
                if swarm.leaderSessionId == session.sessionId {
                    Button(role: .destructive) {
                        confirmsDisband = true
                    } label: {
                        MobiusLabel(title: "Disband Swarm", glyph: .trash)
                    }
                } else {
                    Button {
                        model.leaveSwarm(swarm, sessionID: session.sessionId)
                    } label: {
                        MobiusLabel(title: "Leave Swarm", glyph: .x)
                    }
                }
            }
            .disabled(!model.canMutateSwarm)
            .alert("Disband this swarm?", isPresented: $confirmsDisband) {
                Button("Disband Swarm", role: .destructive) {
                    model.disbandSwarm(swarm, leaderSessionID: session.sessionId)
                }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("This permanently deletes the shared swarm board.")
            }
        } else {
            availableActions
        }
    }

    @ViewBuilder
    private var availableActions: some View {
        let candidates = model.swarmCreationCandidates(for: session)
        let swarms = model.availableSwarms(for: session)
        if !candidates.isEmpty || !swarms.isEmpty {
            Section("Swarm") {
                if !candidates.isEmpty {
                    Button {
                        swarmCreation = SwarmCreationSelection(
                            leader: session,
                            candidates: candidates
                        )
                    } label: {
                        MobiusLabel(title: "Create Swarm…", glyph: .swarm)
                    }
                }
                if !swarms.isEmpty {
                    Menu {
                        ForEach(swarms) { swarm in
                            Button {
                                model.addSwarmMember(session, to: swarm)
                            } label: {
                                MobiusLabel(verbatim: swarm.title, glyph: .swarm)
                            }
                        }
                    } label: {
                        MobiusLabel(title: "Add to Swarm", glyph: .swarm)
                    }
                }
            }
            .disabled(!model.canMutateSwarm)
        }
    }
}

private struct SwarmCreationSelection: Identifiable {
    var id: String { leader.sessionId }

    let leader: SessionRecord
    let candidates: [SessionRecord]
}

private struct SwarmCreationPicker: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.mobiusPalette) private var palette
    @State private var selectedMemberIDs: Set<String> = []
    let selection: SwarmCreationSelection

    var body: some View {
        NavigationStack {
            List {
                Section("Leader") {
                    chatRow(selection.leader, selected: true)
                }
                Section {
                    ForEach(selection.candidates) { session in
                        Button {
                            if !selectedMemberIDs.insert(session.sessionId).inserted {
                                selectedMemberIDs.remove(session.sessionId)
                            }
                        } label: {
                            chatRow(
                                session,
                                selected: selectedMemberIDs.contains(session.sessionId)
                            )
                        }
                        .buttonStyle(.mobiusPlain)
                        .accessibilityValue(
                            selectedMemberIDs.contains(session.sessionId)
                                ? Text("Selected")
                                : Text("Not selected")
                        )
                        .accessibilityHint("Double-tap to toggle this coworker")
                    }
                } header: {
                    Text("Coworkers")
                } footer: {
                    Text("Choose one or more chats from this folder.")
                }
            }
            .scrollContentBackground(.hidden)
            .background(palette.canvas)
            .navigationTitle("Create Swarm")
            .toolbarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Create") {
                        model.createSwarm(
                            leaderSessionID: selection.leader.sessionId,
                            memberSessionIDs: selectedMemberIDs
                        )
                        dismiss()
                    }
                    .disabled(selectedMemberIDs.isEmpty || !model.canMutateSwarm)
                }
            }
        }
    }

    private func chatRow(_ session: SessionRecord, selected: Bool) -> some View {
        HStack(spacing: MobiusSpace.s) {
            MobiusIcon(
                .chatCircle,
                foreground: selected ? palette.accent : palette.muted
            )
            MobiusTitleText(verbatim: model.displayedTitle(for: session))
                .lineLimit(1)
                .frame(maxWidth: .infinity, alignment: .leading)
            SessionActivityIndicator(
                state: session.activity.state,
                isUnread: model.unreadSessionIDs.contains(session.sessionId)
            )
            if selected {
                MobiusIcon(.check, foreground: palette.accent)
                    .accessibilityHidden(true)
            }
        }
        .contentShape(Rectangle())
    }
}
