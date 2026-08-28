import SwiftUI

struct SwarmView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    private static let messagePageSize = 25

    @State private var confirmsDisband = false
    @State private var visibleMessages = messagePageSize
    @State private var showsActivity = true
    @State private var showsRoster = true
    @State private var showsBoard = true
    let swarmID: String

    var body: some View {
        Group {
            if let swarm {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: MobiusSpace.l) {
                        SwarmStatsSection(
                            stats: stats(for: swarm),
                            isExpanded: $showsActivity
                        )
                        roster(swarm)
                        board(swarm)
                    }
                    .padding(MobiusSpace.l)
                }
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
                    optionsMenu(for: swarm)
                }
            }
        }
        .alert("Disband this swarm?", isPresented: $confirmsDisband) {
            if let swarm {
                Button("Disband Swarm", role: .destructive) {
                    model.disbandSwarm(swarm, leaderSessionID: swarm.leaderSessionId)
                }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This permanently deletes the shared swarm board.")
        }
        .background { palette.canvas.ignoresSafeArea() }
    }

    private var swarm: SwarmRecord? {
        model.swarms.first { $0.id == swarmID }
    }

    private func subtitle(for swarm: SwarmRecord) -> String {
        let members = orderedMembers(in: swarm)
        let coworkers = members.count == 1 ? "1 coworker" : "\(members.count) coworkers"
        return "\(coworkers) \u{2022} \(activeCount(in: swarm)) active"
    }

    private func stats(for swarm: SwarmRecord) -> SwarmStats {
        SwarmStats.make(messages: swarm.messages)
    }

    private func optionsMenu(for swarm: SwarmRecord) -> some View {
        let additions = model.sessions.filter { session in
            model.availableSwarms(for: session).contains { $0.id == swarm.id }
        }
        let removals = orderedMembers(in: swarm).filter {
            $0.sessionId != swarm.leaderSessionId
        }

        return HeaderOptionsMenu(label: "Swarm options") {
            Section("Members") {
                if !additions.isEmpty {
                    Menu {
                        ForEach(additions) { session in
                            Button {
                                model.addSwarmMember(session, to: swarm)
                            } label: {
                                MobiusLabel(
                                    verbatim: model.displayedTitle(for: session),
                                    glyph: .chatCircle
                                )
                            }
                        }
                    } label: {
                        MobiusLabel(title: "Add Coworker", glyph: .swarm)
                    }
                }
                if !removals.isEmpty {
                    Menu {
                        ForEach(removals) { member in
                            Button(role: .destructive) {
                                model.leaveSwarm(swarm, sessionID: member.sessionId)
                            } label: {
                                MobiusLabel(verbatim: member.handle, glyph: .x)
                            }
                        }
                    } label: {
                        MobiusLabel(title: "Remove Coworker", glyph: .swarm)
                    }
                }
            }
            .disabled(!model.canMutateSwarm)
            Section {
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
        VStack(alignment: .leading, spacing: MobiusSpace.xs) {
            SwarmSectionHeading(
                title: "Roster",
                trailing: "\(orderedMembers(in: swarm).count)",
                isExpanded: $showsRoster
            )
            if showsRoster {
                ForEach(orderedMembers(in: swarm)) { member in
                    if let session = session(member.sessionId) {
                        SessionCatalogRow(
                            session: session,
                            showsControls: false,
                            detail: rosterDetail(member, swarm: swarm)
                        )
                    } else {
                        unavailableMember(member, swarm: swarm)
                    }
                }
            }
        }
    }

    private func board(_ swarm: SwarmRecord) -> some View {
        let ordered = swarm.messages.sorted { $0.sequence < $1.sequence }
        let windowed = Array(ordered.suffix(visibleMessages))
        let hidden = ordered.count - windowed.count
        let roster = Set(swarm.members.map(\.handle))

        return VStack(alignment: .leading, spacing: MobiusSpace.s) {
            SwarmSectionHeading(
                title: "Message board",
                trailing: ordered.isEmpty ? nil : "\(ordered.count) posts",
                isExpanded: $showsBoard
            )
            if !showsBoard {
                EmptyView()
            } else if ordered.isEmpty {
                VStack(spacing: MobiusSpace.s) {
                    MobiusIcon(.chatDots, size: 24, foreground: palette.muted, gutter: false)
                    Text("No swarm messages yet")
                        .font(MobiusStyle.bodyFont)
                        .foregroundStyle(palette.muted)
                }
                .frame(maxWidth: .infinity, minHeight: 132)
                .background(palette.panel, in: MobiusStyle.cardShape)
            } else {
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
                            isLeader: message.authorSessionId == swarm.leaderSessionId,
                            isLast: index == windowed.count - 1
                        )
                    }
                }
                .swarmCard()
            }
        }
    }

    private func unavailableMember(
        _ member: SwarmMemberRecord,
        swarm: SwarmRecord
    ) -> some View {
        HStack(spacing: MobiusSpace.s) {
            MobiusIcon(.robot, foreground: palette.muted)
            VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                Text(verbatim: member.handle)
                    .font(MobiusStyle.controlFont)
                Text(member.sessionId == swarm.leaderSessionId ? "Leader, unavailable" : "Unavailable")
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
            if first.sessionId == swarm.leaderSessionId { return true }
            if second.sessionId == swarm.leaderSessionId { return false }
            return first.handle.localizedCaseInsensitiveCompare(second.handle) == .orderedAscending
        }
    }

    private func rosterDetail(_ member: SwarmMemberRecord, swarm: SwarmRecord) -> String {
        member.sessionId == swarm.leaderSessionId
            ? "\(member.handle) \u{2022} Leader"
            : member.handle
    }

    private func session(_ id: String) -> SessionRecord? {
        model.sessions.first { $0.sessionId == id }
    }

    private func activeCount(in swarm: SwarmRecord) -> Int {
        Set(orderedMembers(in: swarm).map(\.sessionId)).filter { sessionID in
            guard let session = session(sessionID) else { return false }
            return session.activity.state != .idle
        }.count
    }

}

/// One board post: a single header line naming who spoke and when, then what they said.
private struct SwarmMessageRow: View {
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
                CollapsibleText(
                    text: swarmHighlightedBody(message.body, roster: roster),
                    rendersMarkdown: true,
                    collapsedLineLimit: 6
                )
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
                .userFocus,
                size: MobiusStyle.glyphInline,
                foreground: .primary,
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
