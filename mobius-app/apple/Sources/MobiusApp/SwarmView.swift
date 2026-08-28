import SwiftUI

struct SwarmView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let swarmID: String

    var body: some View {
        Group {
            if let swarm {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: MobiusSpace.l) {
                        summary(swarm)
                        roster(swarm)
                        board(swarm)
                    }
                    .padding(MobiusSpace.l)
                }
                .scrollIndicators(.hidden)
                .navigationTitle(swarm.title)
            } else {
                MobiusUnavailable(
                    title: "Swarm unavailable",
                    glyph: .group01,
                    detail: "This swarm is no longer available on the gateway."
                )
                .navigationTitle("Swarm")
            }
        }
        .toolbarTitleDisplayMode(.inline)
        .background { palette.canvas.ignoresSafeArea() }
    }

    private var swarm: SwarmRecord? {
        model.swarms.first { $0.id == swarmID }
    }

    private func summary(_ swarm: SwarmRecord) -> some View {
        HStack(spacing: MobiusSpace.m) {
            MobiusIcon(
                .group01,
                size: 28,
                foreground: palette.accent,
                gutter: false
            )
            VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                MobiusTitleText(verbatim: swarm.title)
                    .font(.title2.weight(.semibold))
                    .lineLimit(2)
                Text("\(activeCount(in: swarm)) active of \(orderedMembers(in: swarm).count)")
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
            }
            Spacer(minLength: 0)
        }
        .padding(MobiusSpace.l)
        .background(palette.panel, in: MobiusStyle.cardShape)
        .overlay {
            MobiusStyle.cardShape.stroke(palette.line, lineWidth: MobiusStyle.borderWidth)
        }
        .accessibilityElement(children: .combine)
    }

    private func roster(_ swarm: SwarmRecord) -> some View {
        VStack(alignment: .leading, spacing: MobiusSpace.xs) {
            Text("Roster")
                .font(.title3.weight(.semibold))
            ForEach(orderedMembers(in: swarm)) { member in
                if let session = session(member.sessionId) {
                    SessionCatalogRow(
                        session: session,
                        detail: rosterDetail(member, swarm: swarm)
                    )
                } else {
                    unavailableMember(member, swarm: swarm)
                }
            }
        }
    }

    private func board(_ swarm: SwarmRecord) -> some View {
        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            Text("Message board")
                .font(.title3.weight(.semibold))
            if swarm.messages.isEmpty {
                VStack(spacing: MobiusSpace.s) {
                    MobiusIcon(.chatDots, size: 24, foreground: palette.muted, gutter: false)
                    Text("No swarm messages yet")
                        .font(MobiusStyle.bodyFont)
                        .foregroundStyle(palette.muted)
                }
                .frame(maxWidth: .infinity, minHeight: 132)
                .background(palette.panel, in: MobiusStyle.cardShape)
            } else {
                ForEach(swarm.messages) { message in
                    swarmMessage(message)
                }
            }
        }
    }

    private func swarmMessage(_ message: SwarmMessageRecord) -> some View {
        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            HStack(spacing: MobiusSpace.s) {
                MobiusIcon(
                    .group01,
                    size: MobiusStyle.glyphInline,
                    foreground: palette.accent,
                    gutter: false
                )
                Text(verbatim: message.authorHandle)
                    .font(MobiusStyle.controlFont)
                    .lineLimit(1)
                Spacer(minLength: 0)
                Text(messageDate(message.createdAtMs), style: .relative)
                    .font(MobiusStyle.captionFont)
                    .foregroundStyle(palette.muted)
            }
            MobiusMarkdownText(message.body, streaming: false)
                .equatable()
        }
        .padding(MobiusSpace.l)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(palette.panel, in: MobiusStyle.cardShape)
        .overlay {
            MobiusStyle.cardShape.stroke(palette.line, lineWidth: MobiusStyle.borderWidth)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(Text("Message from agent \(message.authorHandle)"))
        .accessibilityValue(Text(verbatim: message.body))
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

    private func messageDate(_ milliseconds: Int64) -> Date {
        Date(timeIntervalSince1970: TimeInterval(milliseconds) / 1_000)
    }
}
