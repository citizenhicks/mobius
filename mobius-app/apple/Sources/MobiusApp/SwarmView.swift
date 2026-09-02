import SwiftUI

struct SwarmView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    private static let messagePageSize = 25

    @State private var confirmsDisband = false
    @State private var showsRename = false
    @State private var renameDraft = ""
    @State private var visibleMessages = messagePageSize
    @State private var showsActivity = true
    @State private var showsRoster = true
    @State private var showsScratchpad = true
    @State private var showsBoard = true
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
                    roster(swarm)
                    scratchpad(swarm)
                    board(swarm)
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
            Text("This permanently deletes the shared swarm board.")
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

    private func board(_ swarm: SwarmRecord) -> some View {
        let ordered = swarm.messages.sorted { $0.sequence < $1.sequence }
        let windowed = Array(ordered.suffix(visibleMessages))
        let hidden = ordered.count - windowed.count
        let roster = Set(swarm.members.map(\.handle))

        return Section {
            if showsBoard, ordered.isEmpty {
                VStack(spacing: MobiusSpace.s) {
                    MobiusIcon(.chatDots, size: 24, foreground: palette.muted, gutter: false)
                    Text("No swarm messages yet")
                        .font(MobiusStyle.bodyFont)
                        .foregroundStyle(palette.muted)
                }
                .frame(maxWidth: .infinity, minHeight: 132)
                .background(palette.panel, in: MobiusStyle.cardShape)
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
            } else if showsBoard {
                // One card for the whole board, not one per post: a card per message is what
                // made a short exchange fill a screen.
                VStack(alignment: .leading, spacing: 0) {
                    if hidden > 0 {
                        Button {
                            visibleMessages += Self.messagePageSize
                        } label: {
                            MobiusLabel(
                                title: "Show \(hidden) earlier",
                                glyph: .arrowUp,
                                iconColor: palette.accent
                            )
                            .frame(maxWidth: .infinity, minHeight: MobiusStyle.iconButtonSize)
                        }
                        .buttonStyle(.mobiusPlain)
                        .foregroundStyle(palette.accent)
                        .padding(.bottom, MobiusSpace.s)
                    }
                    ForEach(windowed.enumerated(), id: \.element.id) { index, message in
                        SwarmMessageRow(
                            message: message,
                            roster: roster,
                            isLeader: message.authorBotId == swarm.leaderBotId,
                            isLast: index == windowed.count - 1
                        )
                    }
                }
                .swarmCard()
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
            }
        } header: {
            SwarmSectionHeading(
                title: "Message board",
                trailing: ordered.isEmpty ? nil : "\(ordered.count) posts",
                isExpanded: $showsBoard
            )
            .textCase(nil)
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
            MobiusIcon(.aiScan, foreground: isActive ? bot.tint.color : palette.muted)
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

/// One board post: a single header line naming who spoke and when, then what they said.
private struct SwarmMessageRow: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    /// The author mark and the name beside it centre in the same box, so the rail node stays
    /// level with the header at every Dynamic Type size instead of riding above it.
    @ScaledMetric(relativeTo: .body) private var headerHeight = MobiusStyle.rowRegular
    let message: SwarmMessageRecord
    let roster: Set<String>
    let isLeader: Bool
    let isLast: Bool

    var body: some View {
        HStack(alignment: .top, spacing: MobiusSpace.s) {
            rail
            VStack(alignment: .leading, spacing: MobiusSpace.xs) {
                header
                // Mentions are emphasised inside the body rather than repeated beside it, so
                // the post reads as the sentence the agent actually wrote.
                MobiusMarkdownText(
                    swarmHighlightedText(message.text, roster: roster),
                    streaming: false
                )
                .equatable()
            }
            .padding(.bottom, isLast ? 0 : MobiusSpace.l)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .contain)
        .accessibilityLabel(Text("Post from \(message.authorHandle)"))
    }

    /// The board's spine. The author mark is the node, so the header row still reads
    /// icon-then-name while the line carries the eye from one post to the next.
    private var rail: some View {
        // Stacked rather than overlaid: the line starts where the author mark ends, so no
        // stub of it shows above the node.
        VStack(spacing: 0) {
            MobiusIcon(
                .aiScan,
                size: MobiusStyle.glyphInline,
                foreground: model.bots.first(where: {
                    $0.id == message.authorBotId
                })?.tint.color ?? .primary,
                gutter: false
            )
            .frame(height: headerHeight)
            if !isLast {
                Rectangle()
                    .fill(palette.line)
                    .frame(width: 1)
                    .frame(maxHeight: .infinity)
            }
        }
        .frame(width: MobiusStyle.glyphInline)
        .accessibilityHidden(true)
    }

    private var header: some View {
        HStack(spacing: MobiusSpace.xs) {
            Text(verbatim: message.authorHandle)
                .font(MobiusStyle.controlFont)
                .lineLimit(1)
                .truncationMode(.middle)
            if isLeader {
                Text("Leader")
                    .font(MobiusStyle.captionFont)
                    .foregroundStyle(palette.muted)
                    .padding(.horizontal, MobiusSpace.s)
                    .padding(.vertical, MobiusSpace.xxs)
                    .background(palette.raised, in: Capsule())
            }
            separator
            Text(
                Date(timeIntervalSince1970: TimeInterval(message.createdAtMs) / 1_000),
                style: .relative
            )
            .font(MobiusStyle.captionFont)
            .foregroundStyle(palette.muted)
            .monospacedDigit()
            .lineLimit(1)
            Spacer(minLength: 0)
        }
        .frame(minHeight: headerHeight)
    }

    private var separator: some View {
        Text(verbatim: "\u{2022}")
            .font(MobiusStyle.captionFont)
            .foregroundStyle(palette.muted)
            .accessibilityHidden(true)
    }
}
