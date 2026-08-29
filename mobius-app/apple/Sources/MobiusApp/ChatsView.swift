import SwiftUI

private enum ChatOrganization: CaseIterable, Identifiable {
    case byProject
    case chronological

    var id: Self { self }

    var title: LocalizedStringResource {
        switch self {
        case .byProject: "By project"
        case .chronological: "Chronological list"
        }
    }

    var heading: LocalizedStringResource {
        switch self {
        case .byProject: "Projects"
        case .chronological: "Recent chats"
        }
    }

    var glyph: MobiusGlyph {
        switch self {
        case .byProject: .folder
        case .chronological: .clock
        }
    }
}

struct ChatsView: View {
    private static let sessionPageSize = 10

    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var collapsedSwarms: Set<String> = []
    @State private var collapsedWorkspaces: Set<String> = []
    @State private var visibleSessionCounts: [String: Int] = [:]
    @State private var showsAttentionOnly = false
    @State private var organization = ChatOrganization.byProject
    @State private var searchText = ""

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 0) {
                catalogHeading
                if showsLoadingCatalog {
                    loadingCatalog
                } else if displayedSessions.isEmpty && catalogSwarms.isEmpty {
                    emptyState
                } else {
                    catalog
                }
            }
            .padding(.horizontal, MobiusSpace.l)
            .padding(.bottom, MobiusSpace.xl)
        }
        .scrollIndicators(.hidden)
        .scrollDismissesKeyboard(.interactively)
        .background { palette.canvas.ignoresSafeArea() }
        .navigationTitle("Chats")
        .toolbarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .principal) {
                VStack(spacing: MobiusSpace.xxs) {
                    MobiusTitleText(title: "Chats")
                        .font(MobiusStyle.titleFont)
                    if let account = model.selectedAccount {
                        // A menu-style Picker draws its own button from the selected tag and
                        // drops the label view, so the dot and the caption step never survive:
                        // the choice goes inside a Menu whose label is ours to draw.
                        Menu {
                            Picker("Gateway", selection: Binding(
                                get: { model.selectedAccountID },
                                set: { model.selectAccount($0) }
                            )) {
                                ForEach(model.accounts) { account in
                                    Text(verbatim: account.machineName)
                                        .tag(Optional(account.id))
                                }
                            }
                            .labelsHidden()
                        } label: {
                            HStack(spacing: MobiusSpace.xs) {
                                Circle()
                                    .fill(model.connectionState.tone.color(in: palette))
                                    .frame(width: 6, height: 6)
                                Text(verbatim: account.machineName)
                                    .font(MobiusStyle.captionFont)
                                    .lineLimit(1)
                                    .truncationMode(.middle)
                                MobiusIcon(
                                    .caretUpDown,
                                    size: MobiusStyle.glyphMark,
                                    foreground: palette.muted,
                                    gutter: false
                                )
                            }
                            .foregroundStyle(palette.muted)
                        }
                        .menuIndicator(.hidden)
                        .buttonStyle(.mobiusPlain)
                        .sensoryFeedback(.selection, trigger: model.selectedAccountID)
                        .accessibilityLabel("Gateway")
                        .accessibilityValue(
                            Text("\(account.machineName), \(model.connectionState.label)")
                        )
                        .help("Switch gateway")
                    }
                }
            }
            if usesIPadLayout {
                ToolbarItem(placement: .primaryAction) {
                    organizationMenu
                }
                ToolbarItem(placement: .primaryAction) {
                    newChatButton
                }
                .sharedBackgroundVisibility(.hidden)
            } else {
                ToolbarItem(placement: .primaryAction) {
                    organizationMenu
                }
            }
            DefaultToolbarItem(kind: .search, placement: .bottomBar)
            if !usesIPadLayout {
                ToolbarSpacer(.fixed, placement: .bottomBar)
                ToolbarItem(placement: .bottomBar) {
                    newChatButton
                }
                .sharedBackgroundVisibility(.hidden)
            }
        }
        .searchable(
            text: $searchText,
            prompt: "Search chats"
        )
        .searchToolbarBehavior(.automatic)
        .searchPresentationToolbarBehavior(.avoidHidingContent)
    }

    private var usesIPadLayout: Bool {
        MobiusLayout.usesIPadLayout(platform: GatewayClientKind.currentApplePlatform)
    }

    private var catalogHeading: some View {
        HStack(spacing: MobiusSpace.s) {
            Text(organization.heading)
                .font(.title2.weight(.semibold))
            if showsLoadingCatalog {
                MobiusSpinner(size: MobiusStyle.glyphMark)
            }
            Spacer()
            Button {
                showsAttentionOnly.toggle()
            } label: {
                MobiusIcon(.notificationSquare)
            }
            .buttonStyle(MobiusIconButtonStyle(prominent: showsAttentionOnly, bare: true))
            .accessibilityLabel(
                "Filter chats needing attention"
            )
            .accessibilityValue(showsAttentionOnly ? Text("On") : Text("Off"))
            .accessibilityAddTraits(showsAttentionOnly ? .isSelected : [])
            .help(
                showsAttentionOnly
                    ? Text("Show all chats")
                    : Text("Show active and unread chats")
            )
            .disabled(showsLoadingCatalog)
        }
        .padding(.top, MobiusSpace.l)
        .padding(.bottom, MobiusSpace.s)
    }

    @ViewBuilder
    private var catalog: some View {
        switch organization {
        case .byProject:
            ForEach(sessionGroups) { group in
                workspaceGroup(group)
            }
        case .chronological:
            ForEach(chronologicalSessions) { session in
                SessionCatalogRow(session: session, showsWorkspace: true)
            }
        }
    }

    private var organizationMenu: some View {
        HeaderOptionsMenu(label: "Organize chats") {
            Section("Organize") {
                ForEach(ChatOrganization.allCases) { option in
                    Button {
                        organization = option
                    } label: {
                        MobiusLabel(
                            title: option.title,
                            glyph: option == organization ? .check : option.glyph
                        )
                    }
                }
            }
        }
        .accessibilityValue(Text(organization.title))
    }

    private var newChatButton: some View {
        Button("New chat", glyph: .notePencil) {
            model.openNewSession()
        }
        .mobiusProminentIconButton()
        .disabled(!model.canCreateSession)
        .accessibilityHint("Choose a workspace for the new chat")
        .help("New chat")
    }

    private var showsLoadingCatalog: Bool {
        model.connectionState.isLoading
            && model.sessions.isEmpty
            && model.swarms.isEmpty
            && !showsAttentionOnly
            && searchText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    private var loadingCatalog: some View {
        VStack(alignment: .leading, spacing: 0) {
            loadingWorkspace(
                "Current project",
                chats: ["Conversation title", "Another recent chat"]
            )
            loadingWorkspace(
                "Another project",
                chats: ["Planning notes", "Follow-up conversation", "Recent chat"]
            )
        }
        .mobiusLoadingPlaceholder("Loading chats")
    }

    private func loadingWorkspace(
        _ name: LocalizedStringResource,
        chats: [LocalizedStringResource]
    ) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 0) {
                HStack(spacing: MobiusSpace.s) {
                    MobiusIcon(.folderOpen, foreground: palette.muted)
                        .unredacted()
                    Text(name)
                        .font(MobiusStyle.controlFont)
                    MobiusIcon(.caretRight, size: 12, foreground: palette.muted)
                        .rotationEffect(.degrees(90))
                        .unredacted()
                }
                .frame(
                    maxWidth: .infinity,
                    minHeight: MobiusStyle.iconButtonSize,
                    alignment: .leading
                )
                Color.clear
                    .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
            }

            ForEach(chats, id: \.key) { title in
                Text(title)
                    .font(MobiusStyle.bodyFont)
                    .lineLimit(1)
                    .frame(
                        maxWidth: .infinity,
                        minHeight: MobiusStyle.iconButtonSize,
                        alignment: .leading
                    )
                    .padding(.horizontal, MobiusSpace.s)
            }
        }
    }

    private var displayedSessions: [SessionRecord] {
        var sessions = model.sessions
        if showsAttentionOnly {
            let attentionSessionIDs = model.attentionSessionIDs
            sessions = sessions.filter { attentionSessionIDs.contains($0.sessionId) }
        }
        let query = searchText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !query.isEmpty else { return sessions }
        return sessions.filter { session in
            model.displayedTitle(for: session).localizedCaseInsensitiveContains(query)
                || session.sessionId.localizedCaseInsensitiveContains(query)
                || (session.sessionContext.workspaceLabel ?? "")
                    .localizedCaseInsensitiveContains(query)
        }
    }

    private var emptySessionsMessage: LocalizedStringResource {
        if !searchText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return "No chats match your search"
        }
        if !model.connectionState.isReady && model.sessions.isEmpty {
            return model.connectionState.label
        }
        if showsAttentionOnly {
            return "No chats need attention"
        }
        return "No chats yet"
    }

    private var emptyState: some View {
        VStack(spacing: MobiusSpace.m) {
            MobiusIcon(
                AppDestination.chats.glyph,
                size: 32,
                foreground: palette.muted,
                gutter: false
            )
            Text(emptySessionsMessage)
                .font(MobiusStyle.bodyFont)
                .foregroundStyle(palette.muted)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity, minHeight: 220)
    }

    private var chronologicalSessions: [SessionRecord] {
        displayedSessions.sorted {
            if $0.updatedAt != $1.updatedAt { return $0.updatedAt > $1.updatedAt }
            return $0.sessionId < $1.sessionId
        }
    }

    private var sessionGroups: [WorkspaceSessions] {
        let claimedSessionIDs = Set(
            catalogSwarms.flatMap { $0.members.map(\.session.sessionId) }
        )
        let sessionsByWorkspace = Dictionary(
            grouping: displayedSessions.filter {
                !claimedSessionIDs.contains($0.sessionId)
            },
            by: workspaceID
        )
        let swarmsByWorkspace = Dictionary(grouping: catalogSwarms, by: \.workspaceID)
        return Set(sessionsByWorkspace.keys).union(swarmsByWorkspace.keys)
        .map { id in
            let sessions = sessionsByWorkspace[id, default: []]
            let swarms = swarmsByWorkspace[id, default: []]
            let reference = sessions.first ?? swarms.first?.members.first?.session
            let path = reference?.sessionContext.workspaceLabel ?? "Workspace"
            return WorkspaceSessions(
                id: id,
                name: workspaceName(path),
                path: path,
                sessions: sessions.sorted(by: pinnedThenRecent),
                swarms: swarms.sorted {
                    if $0.record.updatedAtMs != $1.record.updatedAtMs {
                        return $0.record.updatedAtMs > $1.record.updatedAtMs
                    }
                    return $0.record.id < $1.record.id
                }
            )
        }
        .sorted {
            if $0.id == model.workspace?.id { return true }
            if $1.id == model.workspace?.id { return false }
            let firstUpdated = $0.latestUpdatedAt
            let secondUpdated = $1.latestUpdatedAt
            if firstUpdated != secondUpdated { return firstUpdated > secondUpdated }
            return $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending
        }
    }

    private var catalogSwarms: [WorkspaceSwarm] {
        let sessionsByID = Dictionary(
            model.sessions.map { ($0.sessionId, $0) },
            uniquingKeysWith: { _, latest in latest }
        )
        let query = searchText.trimmingCharacters(in: .whitespacesAndNewlines)
        return model.swarms.compactMap { swarm in
            let members = orderedMemberIDs(for: swarm).compactMap { sessionID -> SwarmCatalogMember? in
                guard let session = sessionsByID[sessionID] else { return nil }
                let handle = swarm.members.first(where: { $0.sessionId == sessionID })?.handle
                    ?? sessionID
                return SwarmCatalogMember(session: session, handle: handle)
            }
            if showsAttentionOnly,
               !members.contains(where: {
                   model.attentionSessionIDs.contains($0.session.sessionId)
               }) {
                return nil
            }
            if !query.isEmpty,
               !swarm.title.localizedCaseInsensitiveContains(query),
               !swarm.id.localizedCaseInsensitiveContains(query),
               !members.contains(where: {
                   $0.handle.localizedCaseInsensitiveContains(query)
                       || model.displayedTitle(for: $0.session)
                           .localizedCaseInsensitiveContains(query)
               }) {
                return nil
            }
            let reference = members.first?.session
            return WorkspaceSwarm(
                record: swarm,
                workspaceID: reference.map(workspaceID) ?? "workspace",
                members: members
            )
        }
    }

    private func orderedMemberIDs(for swarm: SwarmRecord) -> [String] {
        var seen: Set<String> = []
        return ([swarm.leaderSessionId] + swarm.members.map(\.sessionId)).filter {
            seen.insert($0).inserted
        }
    }

    private func workspaceID(_ session: SessionRecord) -> String {
        session.sessionContext.workspaceId
            ?? session.sessionContext.workspaceLabel
            ?? "workspace"
    }

    private func pinnedThenRecent(_ first: SessionRecord, _ second: SessionRecord) -> Bool {
        if first.pinned != second.pinned { return first.pinned }
        if first.updatedAt != second.updatedAt { return first.updatedAt > second.updatedAt }
        return first.sessionId < second.sessionId
    }

    private func workspaceName(_ path: String) -> String {
        let name = URL(fileURLWithPath: path).lastPathComponent
        return name.isEmpty ? path : name
    }

    private func expansionBinding(for id: String) -> Binding<Bool> {
        Binding(
            get: { !collapsedWorkspaces.contains(id) },
            set: { expanded in
                if expanded { collapsedWorkspaces.remove(id) }
                else { collapsedWorkspaces.insert(id) }
            }
        )
    }

    private func workspaceGroup(_ group: WorkspaceSessions) -> some View {
        let visibleCount = min(
            visibleSessionCounts[group.id, default: Self.sessionPageSize],
            group.sessions.count
        )
        let isExpanded = !collapsedWorkspaces.contains(group.id)
        return VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 0) {
                Button {
                    withAnimation(reduceMotion ? nil : .snappy(duration: 0.22)) {
                        expansionBinding(for: group.id).wrappedValue.toggle()
                    }
                } label: {
                    HStack(spacing: MobiusSpace.s) {
                        // Two drawings rather than one glyph in two states, so the swap is an
                        // insertion the id drives, not a morph: HugeIcons ships no symbol
                        // effect to interpolate between them.
                        ZStack {
                            MobiusIcon(
                                isExpanded ? .folderOpen : .folder,
                                foreground: palette.muted
                            )
                            .id(isExpanded)
                            .transition(.opacity)
                        }
                        Text(verbatim: group.name)
                            .font(MobiusStyle.controlFont)
                            .lineLimit(1)
                            .truncationMode(.middle)
                        MobiusIcon(.caretRight, size: 12, foreground: palette.muted)
                            .rotationEffect(.degrees(isExpanded ? 90 : 0))
                    }
                    .frame(
                        maxWidth: .infinity,
                        minHeight: MobiusStyle.iconButtonSize,
                        alignment: .leading
                    )
                    .contentShape(Rectangle())
                }
                .buttonStyle(.mobiusPlain)
                .accessibilityValue(isExpanded ? Text("Expanded") : Text("Collapsed"))
                .help(Text(verbatim: group.path))

                Button {
                    model.chooseWorkspace(group.path)
                } label: {
                    MobiusLabel(
                        title: "New chat in \(group.name)",
                        glyph: .notePencil,
                        iconColor: palette.muted
                    )
                    .labelStyle(.iconOnly)
                    .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.mobiusPlain)
                .disabled(!model.canCreateSession)
                .help("New chat in \(group.path)")
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            if isExpanded {
                ForEach(group.swarms) { swarm in
                    swarmSection(swarm)
                }
                ForEach(group.sessions.prefix(visibleCount)) { session in
                    SessionCatalogRow(session: session)
                }
                if visibleCount < group.sessions.count {
                    Button {
                        visibleSessionCounts[group.id] = visibleCount + Self.sessionPageSize
                    } label: {
                        MobiusLabel(
                            title: "Load more",
                            glyph: .arrowDown,
                            iconColor: palette.muted,
                            iconSize: MobiusStyle.glyphInline
                        )
                        .font(MobiusStyle.metadataFont)
                        .foregroundStyle(palette.muted)
                        .frame(
                            maxWidth: .infinity,
                            minHeight: MobiusStyle.iconButtonSize,
                            alignment: .leading
                        )
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.mobiusPlain)
                    .padding(.horizontal, MobiusSpace.s)
                    .accessibilityLabel("Load more chats in \(group.name)")
                }
            }
        }
    }

    private func swarmSection(_ swarm: WorkspaceSwarm) -> some View {
        let isExpanded = !collapsedSwarms.contains(swarm.id)
        return VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 0) {
                Button {
                    model.openSwarm(swarm.id)
                } label: {
                    HStack(spacing: MobiusSpace.s) {
                        MobiusIcon(.swarm, foreground: .primary)
                        MobiusTitleText(verbatim: swarm.record.title)
                            .font(MobiusStyle.controlFont)
                            .lineLimit(1)
                        Text(verbatim: "\u{2022}")
                            .foregroundStyle(palette.muted)
                            .accessibilityHidden(true)
                        Text("\(swarm.activeCount) active")
                            .font(MobiusStyle.metadataFont)
                            .foregroundStyle(palette.muted)
                        Spacer(minLength: 0)
                    }
                    .frame(
                        maxWidth: .infinity,
                        minHeight: MobiusStyle.iconButtonSize,
                        alignment: .leading
                    )
                    .contentShape(Rectangle())
                }
                .buttonStyle(.mobiusPlain)
                .accessibilityLabel(Text("Swarm, \(swarm.record.title)"))
                .accessibilityValue(Text("\(swarm.activeCount) active agents"))
                .accessibilityHint("Show roster and message board")

                Button {
                    withAnimation(reduceMotion ? nil : .snappy(duration: 0.22)) {
                        if collapsedSwarms.remove(swarm.id) == nil {
                            collapsedSwarms.insert(swarm.id)
                        }
                    }
                } label: {
                    MobiusIcon(.caretRight, size: 12, foreground: palette.muted)
                        .rotationEffect(.degrees(isExpanded ? 90 : 0))
                        .frame(
                            width: MobiusStyle.iconButtonSize,
                            height: MobiusStyle.iconButtonSize
                        )
                        .contentShape(Rectangle())
                }
                .buttonStyle(.mobiusPlain)
                .accessibilityLabel(
                    isExpanded
                        ? Text("Collapse swarm \(swarm.record.title)")
                        : Text("Expand swarm \(swarm.record.title)")
                )
                .accessibilityValue(isExpanded ? Text("Expanded") : Text("Collapsed"))
            }
            .padding(.leading, MobiusSpace.s)

            if isExpanded {
                ForEach(swarm.members.enumerated(), id: \.element.id) { index, member in
                    SessionCatalogRow(
                        session: member.session,
                        detail: member.handle,
                        connector: .init(isLast: index == swarm.members.count - 1)
                    )
                }
            }
        }
    }

}

struct SessionCatalogRow: View {
    struct Connector {
        let isLast: Bool
    }

    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let session: SessionRecord
    var showsWorkspace = false
    var showsControls = true
    var detail: String?
    var connector: Connector?

    @ViewBuilder
    var body: some View {
        let isSelected = session.sessionId == model.selectedSessionID
        let isUnread = model.unreadSessionIDs.contains(session.sessionId)
        let row = HStack(spacing: MobiusSpace.xs) {
            if let connector {
                SwarmMemberConnector(isLast: connector.isLast)
            }
            Button {
                model.openChat(session.sessionId)
            } label: {
                HStack(spacing: MobiusSpace.s) {
                    VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                        MobiusTitleText(verbatim: model.displayedTitle(for: session))
                        .lineLimit(1)
                        if let secondaryText, !secondaryText.isEmpty {
                            Text(verbatim: secondaryText)
                                .font(MobiusStyle.captionFont)
                                .foregroundStyle(palette.muted)
                                .lineLimit(1)
                                .truncationMode(.middle)
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    if session.pinned {
                        MobiusIcon(
                            .pushPin,
                            size: MobiusStyle.glyphMark,
                            foreground: palette.accent
                        )
                        .accessibilityHidden(true)
                    }
                    SessionActivityIndicator(
                        state: session.activity.state,
                        isUnread: isUnread
                    )
                    .frame(
                        width: MobiusStyle.iconButtonSize,
                        height: MobiusStyle.iconButtonSize
                    )
                }
                .frame(minHeight: MobiusStyle.iconButtonSize)
                .contentShape(Rectangle())
            }
            .buttonStyle(.mobiusPlain)
            .disabled(!model.canOpenSession && session.sessionId != model.selectedSessionID)
            .accessibilityValue(accessibilityValue(isUnread: isUnread))
            .accessibilityAddTraits(isSelected ? .isSelected : [])
        }
        .padding(.leading, MobiusSpace.s)
        .frame(minHeight: MobiusStyle.iconButtonSize)

        if showsControls {
            row.contextMenu { controls }
        } else {
            row
        }
    }

    @ViewBuilder
    private var controls: some View {
        Button(
            session.pinned ? "Unpin chat" : "Pin chat",
            glyph: session.pinned ? .pushPinSlash : .pushPin
        ) {
            model.setSessionPinned(session, pinned: !session.pinned)
        }
        .disabled(!model.canRenameSession)
        Button("Rename chat", glyph: .pencilSimple) {
            model.beginRenamingSession(session)
        }
        .disabled(!model.canRenameSession)
        let swarms = model.availableSwarms(for: session)
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
            .disabled(!model.canMutateSwarm)
        }
        Button("Delete chat", glyph: .trash, role: .destructive) {
            model.beginDeletingSession(session)
        }
        .disabled(!model.canRenameSession)
    }

    private var secondaryText: String? {
        if let detail { return detail }
        guard showsWorkspace, let path = session.sessionContext.workspaceLabel else { return nil }
        let name = URL(fileURLWithPath: path).lastPathComponent
        return name.isEmpty ? path : name
    }

    private func accessibilityValue(isUnread: Bool) -> Text {
        let state: String? = switch session.activity.state {
        case .running: "In progress"
        case .awaitingApproval: "Awaiting approval"
        case .idle: isUnread ? "Finished, unread" : nil
        }
        return Text(verbatim: [secondaryText, session.pinned ? "Pinned" : nil, state]
            .compactMap { $0 }
            .joined(separator: ", "))
    }
}

private struct SwarmMemberConnector: View {
    @Environment(\.mobiusPalette) private var palette
    let isLast: Bool

    var body: some View {
        Path { path in
            path.move(to: CGPoint(x: 5, y: 0))
            path.addLine(to: CGPoint(x: 5, y: isLast ? 22 : 44))
            path.move(to: CGPoint(x: 5, y: 22))
            path.addLine(to: CGPoint(x: 17, y: 22))
        }
        .stroke(palette.line, style: StrokeStyle(lineWidth: 1, lineCap: .round))
        .frame(width: 18, height: 44)
        .accessibilityHidden(true)
    }
}

struct SessionActivityIndicator: View {
    @Environment(\.mobiusPalette) private var palette
    let state: SessionActivityState
    let isUnread: Bool

    var body: some View {
        Group {
            switch state {
            case .running:
                MobiusSpinner(size: MobiusStyle.glyphMark, foreground: palette.accent)
            case .awaitingApproval:
                Circle()
                    .trim(from: 0.08, to: 0.76)
                    .stroke(
                        palette.warning,
                        style: StrokeStyle(lineWidth: 1.7, lineCap: .round)
                    )
                    .rotationEffect(.degrees(-90))
                    .frame(width: 11, height: 11)
            case .idle:
                if isUnread {
                    Circle()
                        .fill(palette.accent)
                        .frame(width: 7, height: 7)
                        .frame(width: 11, height: 11)
                } else {
                    Color.clear.frame(width: 11, height: 11)
                }
            }
        }
        .accessibilityHidden(true)
    }
}

private struct WorkspaceSessions: Identifiable {
    let id: String
    let name: String
    let path: String
    let sessions: [SessionRecord]
    let swarms: [WorkspaceSwarm]

    var latestUpdatedAt: Int64 {
        max(
            sessions.first?.updatedAt ?? 0,
            swarms.first?.record.updatedAtMs ?? 0
        )
    }
}

private struct WorkspaceSwarm: Identifiable {
    var id: String { record.id }

    let record: SwarmRecord
    let workspaceID: String
    let members: [SwarmCatalogMember]

    var activeCount: Int {
        members.count { $0.session.activity.state != .idle }
    }
}

private struct SwarmCatalogMember: Identifiable {
    var id: String { session.sessionId }

    let session: SessionRecord
    let handle: String
}
