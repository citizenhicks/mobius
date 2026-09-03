import SwiftUI

struct SwarmView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette

    @State private var confirmsDisband = false
    @State private var showsRename = false
    @State private var renameDraft = ""
    @State private var showsActivity = true
    @State private var showsRoster = true
    @State private var showsScratchpad = true
    @State private var showsAddScratchpadNote = false
    let swarmID: String

    var body: some View {
        Group {
            if let swarm {
                Form {
                    SwarmStatsSection(
                        stats: stats(for: swarm),
                        isExpanded: $showsActivity
                    )
                    chat(swarm)
                    roster(swarm)
                    scratchpad(swarm)
                }
                .formStyle(.grouped)
                .listSectionSpacing(MobiusSpace.l)
                .contentMargins(.vertical, MobiusSpace.l, for: .scrollContent)
                .scrollContentBackground(.hidden)
                .scrollIndicators(.hidden)
                .navigationTitle(swarm.title)
                .navigationSubtitle(subtitle(for: swarm))
            } else {
                MobiusUnavailable(
                    title: "Swarm unavailable",
                    glyph: .swarm,
                    detail: "This swarm is no longer available on the gateway."
                )
                .navigationTitle("Swarm")
            }
        }
        .toolbarTitleDisplayMode(.inline)
        .toolbar {
            if let swarm {
                ToolbarItem(placement: .primaryAction) {
                    headerActions(for: swarm)
                }
            }
        }
        .alert("Disband this swarm?", isPresented: $confirmsDisband) {
            if let swarm {
                Button("Disband Swarm", role: .destructive) {
                    model.disbandSwarm(swarm)
                }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This permanently deletes the shared Swarm Chat and collective scratchpad.")
        }
        .alert("Rename swarm", isPresented: $showsRename) {
            TextField("Swarm name", text: $renameDraft)
            Button("Cancel", role: .cancel) {}
            Button("Rename") {
                if let swarm { model.renameSwarm(swarm, title: renameDraft) }
            }
            .disabled(renameDraft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
        .sheet(isPresented: $showsAddScratchpadNote) {
            AddSwarmScratchpadNoteSheet(swarmID: swarmID)
        }
        .task(id: "\(swarmID):\(model.connectionState.isReady)") {
            if model.connectionState.isReady {
                model.refreshScratchpad(scope: .swarm(id: swarmID))
            }
        }
        .background { palette.canvas.ignoresSafeArea() }
    }

    private var swarm: SwarmRecord? {
        model.swarms.first { $0.id == swarmID }
    }

    private func subtitle(for swarm: SwarmRecord) -> String {
        let members = orderedMembers(in: swarm)
        let coworkers = members.count == 1 ? "1 Bot" : "\(members.count) Bots"
        return "\(coworkers) \u{2022} \(activeCount(in: swarm)) active"
    }

    private func stats(for swarm: SwarmRecord) -> SwarmStats {
        SwarmStats.make(messages: swarm.messages)
    }

    private func headerActions(for swarm: SwarmRecord) -> some View {
        let additions = model.availableBotsForSwarm()

        return HeaderActionGroup {
            Menu {
                ForEach(additions) { bot in
                    Button {
                        model.addSwarmMember(bot, to: swarm)
                    } label: {
                        MobiusLabel(
                            verbatim: "\(bot.name) · @\(bot.handle)",
                            glyph: .aiScan,
                            iconColor: bot.tint.color
                        )
                    }
                }
            } label: {
                MobiusIcon(.swarm, foreground: .primary)
            }
            .menuIndicator(.hidden)
            .accessibilityLabel("Add Bot to Swarm")
            .help("Add Bot to Swarm")
            .disabled(additions.isEmpty || !model.canMutateSwarm)
            .groupedHeaderAction()
            optionsMenu(for: swarm)
                .groupedHeaderAction()
        }
    }

    private func optionsMenu(for swarm: SwarmRecord) -> some View {
        let removals = orderedMembers(in: swarm).filter {
            $0.botId != swarm.leaderBotId
        }

        return HeaderOptionsMenu(label: "Swarm options") {
            Section("Scratchpad") {
                Button {
                    showsAddScratchpadNote = true
                } label: {
                    MobiusLabel(title: "Add Collective Note", glyph: .plus)
                }
                .disabled(!model.connectionState.isReady)
            }
            if !removals.isEmpty {
                Section("Members") {
                    Menu {
                        ForEach(removals) { member in
                            Button(role: .destructive) {
                                model.leaveSwarm(swarm, botID: member.botId)
                            } label: {
                                MobiusLabel(verbatim: member.handle, glyph: .x)
                            }
                        }
                    } label: {
                        MobiusLabel(title: "Remove Coworker", glyph: .swarm)
                    }
                }
                .disabled(!model.canMutateSwarm)
            }
            Section {
                Button {
                    renameDraft = swarm.title
                    showsRename = true
                } label: {
                    MobiusLabel(title: "Rename Swarm", glyph: .pencilSimple)
                }
                Button(role: .destructive) {
                    confirmsDisband = true
                } label: {
                    MobiusLabel(title: "Disband Swarm", glyph: .trash)
                }
            }
            .disabled(!model.canMutateSwarm)
        }
    }

    private func roster(_ swarm: SwarmRecord) -> some View {
        Section {
            if showsRoster {
                ForEach(orderedMembers(in: swarm)) { member in
                    if let bot = model.bots.first(where: { $0.id == member.botId }) {
                        SwarmBotRow(
                            bot: bot,
                            isLeader: member.botId == swarm.leaderBotId,
                            isActive: botIsActive(bot.id)
                        )
                    } else {
                        unavailableMember(member, swarm: swarm)
                    }
                }
                .listRowInsets(
                    EdgeInsets(
                        top: 0,
                        leading: MobiusSpace.s,
                        bottom: 0,
                        trailing: MobiusSpace.s
                    )
                )
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
            }
        } header: {
            SwarmSectionHeading(
                title: "Roster",
                trailing: "\(orderedMembers(in: swarm).count)",
                isExpanded: $showsRoster
            )
            .textCase(nil)
        }
    }

    private func chat(_ swarm: SwarmRecord) -> some View {
        Section {
            SettingsNavigationRow(
                hint: "Opens the shared Swarm Chat",
                open: { model.openSwarmChat(swarm.id) },
                marks: {
                    if model.hasSwarmAttention(forSwarmID: swarm.id) {
                        MobiusIcon(
                            .bellDot,
                            size: MobiusStyle.glyphMark,
                            foreground: palette.warning
                        )
                        .accessibilityLabel("Swarm Chat needs attention")
                    }
                }
            ) {
                SettingsRowLabel(
                    title: "Swarm Chat",
                    detail: swarm.messages.isEmpty
                        ? "No messages yet"
                        : "\(swarm.messages.count) messages"
                ) {
                    MobiusIcon(
                        .chatDots,
                        size: MobiusStyle.glyphLead,
                        foreground: palette.accent
                    )
                    .accessibilityHidden(true)
                }
            }
        }
    }

    private func scratchpad(_ swarm: SwarmRecord) -> some View {
        let widget = model.swarmScratchpadWidget(swarmID: swarm.id)
        let count = model.swarmScratchpadContributions[swarm.id]?.count

        return Section {
            if showsScratchpad {
                if let widget, let content = widget.widget.content {
                    FrontendWidgetContentView(
                        content: content,
                        actionsEnabled: model.connectionState.isReady,
                        usesSwipeActions: true,
                        submitOperation: { operation in
                            model.submitScratchpadOperation(
                                operation,
                                scope: .swarm(id: swarm.id)
                            )
                        }
                    ) { _ in }
                } else {
                    HStack(spacing: MobiusSpace.s) {
                        MobiusSpinner(size: MobiusStyle.glyphInline, foreground: palette.muted)
                        Text("Loading collective scratchpad…")
                            .font(MobiusStyle.bodyFont)
                            .foregroundStyle(palette.muted)
                    }
                    .frame(maxWidth: .infinity, minHeight: MobiusStyle.rowTouch)
                }
            }
        } header: {
            SwarmSectionHeading(
                title: "Collective scratchpad",
                trailing: count.map { $0 == 1 ? "1 note" : "\($0) notes" },
                isExpanded: $showsScratchpad
            )
            .textCase(nil)
        }
    }

    private func unavailableMember(
        _ member: SwarmMemberRecord,
        swarm: SwarmRecord
    ) -> some View {
        HStack(spacing: MobiusSpace.s) {
            MobiusIcon(.aiScan, foreground: palette.muted)
            VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                Text(verbatim: member.handle)
                    .font(MobiusStyle.controlFont)
                Text(member.botId == swarm.leaderBotId ? "Leader, unavailable" : "Unavailable")
                    .font(MobiusStyle.captionFont)
                    .foregroundStyle(palette.muted)
            }
            Spacer(minLength: 0)
        }
        .frame(minHeight: MobiusStyle.iconButtonSize)
        .padding(.horizontal, MobiusSpace.s)
        .accessibilityElement(children: .combine)
    }

    private func orderedMembers(in swarm: SwarmRecord) -> [SwarmMemberRecord] {
        swarm.members.sorted { first, second in
            if first.botId == swarm.leaderBotId { return true }
            if second.botId == swarm.leaderBotId { return false }
            return first.handle.localizedStandardCompare(second.handle) == .orderedAscending
        }
    }

    private func botIsActive(_ id: String) -> Bool {
        model.sessions.contains {
            $0.sessionContext.botId == id && $0.activity.state != .idle
        }
    }

    private func activeCount(in swarm: SwarmRecord) -> Int {
        orderedMembers(in: swarm).count { botIsActive($0.botId) }
    }

}

struct SwarmChatView: View {
    private static let pageSize = 25

    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var draft = ""
    @State private var visibleMessages = pageSize
    @State private var composerHeight: CGFloat = 0
    @State private var transcriptScroll = TranscriptScrollState()
    @State private var pendingRequestID: String?
    @State private var pendingText: String?
    @FocusState private var isComposerFocused: Bool
    let swarmID: String

    var body: some View {
        Group {
            if let swarm {
                ZStack(alignment: .bottom) {
                    messages(swarm)
                    composer
                        .onGeometryChange(for: CGFloat.self) { geometry in
                            geometry.size.height
                        } action: { height in
                            composerHeight = height
                        }
                }
                .navigationTitle(swarm.title)
                .navigationSubtitle("Swarm Chat")
            } else {
                MobiusUnavailable(
                    title: "Swarm unavailable",
                    glyph: .swarm,
                    detail: "This swarm is no longer available on the gateway."
                )
                .navigationTitle("Swarm Chat")
            }
        }
        .toolbarTitleDisplayMode(.inline)
        .background { MobiusBackdrop() }
        .onChange(of: model.completedSwarmMessageRequestID) { _, requestID in
            guard requestID == pendingRequestID else { return }
            if draft.trimmingCharacters(in: .whitespacesAndNewlines) == pendingText {
                draft = ""
            }
            pendingRequestID = nil
            pendingText = nil
        }
    }

    private var swarm: SwarmRecord? {
        model.swarms.first { $0.id == swarmID }
    }

    private func messages(_ swarm: SwarmRecord) -> some View {
        let ordered = swarm.messages.sorted { $0.sequence < $1.sequence }
        let windowed = Array(ordered.suffix(visibleMessages))
        let entries = windowed.map(swarmTranscriptEntry)
        let projection = TranscriptProjection(entries: entries)
        let hasEarlier = ordered.count > windowed.count
        let loadEarlier = {
            loadEarlierHistory(
                hasEarlier: hasEarlier,
                projection: projection,
                boundaryID: entries.first?.presentationID
            )
        }

        return ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                if hasEarlier {
                    TranscriptPaginationButton(
                        isLoading: false,
                        isEnabled: true,
                        action: loadEarlier
                    )
                    .padding(.bottom, MobiusStyle.transcriptRowSpacing)
                }
                TranscriptRowsView(
                    projection: projection,
                    fileSessionID: nil,
                    rowSpacing: MobiusStyle.transcriptRowSpacing
                )
                Color.clear.frame(height: max(1, composerHeight))
            }
            .scrollTargetLayout()
            .frame(maxWidth: MobiusStyle.transcriptWidth)
            .frame(maxWidth: .infinity)
            .padding(MobiusSpace.l)
        }
        .transcriptScrollBehavior(
            $transcriptScroll,
            projection: projection,
            historyLoadCompletionRevision: visibleMessages,
            conversationID: swarm.id,
            loadEarlierHistory: loadEarlier
        )
        .overlay {
            if ordered.isEmpty {
                MobiusComposingOrb()
                    .frame(
                        width: MobiusStyle.transcriptOrbSize,
                        height: MobiusStyle.transcriptOrbSize
                    )
                    .padding(.bottom, composerHeight)
                    .accessibilityHidden(true)
            }
        }
    }

    private var composer: some View {
        VStack(spacing: 0) {
            TextField("Message the Swarm", text: $draft, axis: .vertical)
                .textFieldStyle(.plain)
                .focused($isComposerFocused)
                .lineLimit(1...8)
                .font(MobiusStyle.bodyFont)
                .accessibilityLabel("Swarm message")
                .onSubmit(send)
                .padding(.horizontal, MobiusSpace.l)
                .padding(.top, MobiusSpace.m)
                .padding(.bottom, MobiusSpace.xs)
            HStack(spacing: MobiusSpace.xs) {
                Spacer(minLength: 0)
                Button(action: send) {
                    Label {
                        Text("Send")
                    } icon: {
                        if isPosting {
                            MobiusSpinner(
                                size: MobiusStyle.iconSize,
                                foreground: palette.onAccent
                            )
                        } else {
                            MobiusIcon(.arrowUp02)
                        }
                    }
                }
                .mobiusProminentIconButton()
                .disabled(!canSend)
                .accessibilityHint("Posts to Swarm Chat")
            }
            .padding(.horizontal, MobiusStyle.iconRowPadding)
            .padding(.bottom, MobiusStyle.iconRowPadding)
        }
        .frame(maxWidth: MobiusStyle.transcriptWidth)
        .mobiusGlass(in: MobiusStyle.cardShape, interactive: true)
        .shadow(color: palette.shadow.opacity(0.18), radius: 12, y: 6)
        .frame(maxWidth: .infinity)
        .padding(.horizontal, MobiusSpace.l)
        .padding(.bottom, MobiusSpace.m)
    }

    private var canSend: Bool {
        !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && draft.utf8.count <= maximumComposerBytes
            && model.canPostSwarmMessage
    }

    private var isPosting: Bool {
        pendingRequestID != nil && model.swarmMessageRequestID == pendingRequestID
    }

    private func loadEarlierHistory(
        hasEarlier: Bool,
        projection: TranscriptProjection,
        boundaryID: TranscriptPresentationID?
    ) {
        guard hasEarlier else { return }
        transcriptScroll.beginHistoryRestore(
            projection: projection,
            boundaryID: boundaryID
        )
        visibleMessages += Self.pageSize
    }

    private func send() {
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canSend,
              let requestID = model.postSwarmMessage(
                to: swarmID,
                text: text
              )
        else { return }
        pendingRequestID = requestID
        pendingText = text
    }
}

private func swarmTranscriptEntry(_ message: SwarmMessageRecord) -> TranscriptEntry {
    let isUser = message.authorBotId == "user"
    let author: MessageAuthor = isUser
        ? .user
        : .peer(
            messageID: message.id,
            sessionID: message.sourceSessionId,
            handle: message.authorHandle
        )
    return TranscriptEntry(
        id: message.id,
        text: message.text,
        kind: isUser ? .user : .assistant,
        format: "plain_text",
        pending: false,
        sourceSequence: message.sequence,
        recordedAtMs: message.createdAtMs,
        messageMetadata: TranscriptMessageMetadata(author: author, delivery: .turn)
    )
}

private struct AddSwarmScratchpadNoteSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var note = ""
    let swarmID: String

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Note", text: $note, axis: .vertical)
                        .lineLimit(4...10)
                } footer: {
                    Text("This note becomes durable context for every Bot in the Swarm.")
                }
            }
            .scrollContentBackground(.hidden)
            .navigationTitle("Add collective note")
            .toolbarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: dismiss.callAsFunction)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Add", action: add)
                        .disabled(trimmedNote.isEmpty || !model.connectionState.isReady)
                }
            }
        }
        .mobiusSheet()
    }

    private var trimmedNote: String {
        note.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private func add() {
        guard !trimmedNote.isEmpty else { return }
        model.submitScratchpadOperation(
            .capabilityCommand(
                capability: "scratchpad",
                command: "scratchpad",
                arguments: "add",
                input: trimmedNote,
                target: nil
            ),
            scope: .swarm(id: swarmID)
        )
        dismiss()
    }
}

private struct SwarmBotRow: View {
    @Environment(\.mobiusPalette) private var palette
    let bot: BotRecord
    let isLeader: Bool
    let isActive: Bool

    var body: some View {
        HStack(spacing: MobiusSpace.s) {
            MobiusIcon(.aiScan, foreground: bot.tint.color)
            VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                MobiusTitleText(verbatim: bot.name)
                    .lineLimit(1)
                Text(verbatim: isLeader ? "@\(bot.handle) • Leader" : "@\(bot.handle)")
                    .font(MobiusStyle.captionFont)
                    .foregroundStyle(palette.muted)
            }
            Spacer(minLength: 0)
            if isActive {
                MobiusSpinner(size: MobiusStyle.glyphMark, foreground: palette.accent)
            }
        }
        .frame(minHeight: MobiusStyle.iconButtonSize)
        .padding(.horizontal, MobiusSpace.s)
        .accessibilityElement(children: .combine)
    }
}
