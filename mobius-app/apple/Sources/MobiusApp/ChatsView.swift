import SwiftUI

private enum ChatOrganization: CaseIterable, Identifiable {
    case byProject
    case chronological
    case tasks

    var id: Self { self }

    var title: String {
        switch self {
        case .byProject: "By project"
        case .chronological: "Chronological list"
        case .tasks: "Tasks"
        }
    }

    var heading: String {
        switch self {
        case .byProject: "Projects"
        case .chronological: "Recent chats"
        case .tasks: "Recent tasks"
        }
    }

    var glyph: MobiusGlyph {
        switch self {
        case .byProject: .folder
        case .chronological: .clock
        case .tasks: .calendarDots
        }
    }
}

struct ChatsView: View {
    private static let sessionPageSize = 10

    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
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
                } else if displayedSessions.isEmpty {
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
                                    Text(account.machineName)
                                        .tag(Optional(account.id))
                                }
                            }
                            .labelsHidden()
                        } label: {
                            HStack(spacing: MobiusSpace.xs) {
                                Circle()
                                    .fill(model.connectionState.tone.color(in: palette))
                                    .frame(width: 6, height: 6)
                                Text(account.machineName)
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
                            "\(account.machineName), \(model.connectionState.label)"
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
            prompt: organization == .tasks ? "Search tasks" : "Search chats"
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
                organization == .tasks ? "Filter tasks needing attention" : "Filter chats needing attention"
            )
            .accessibilityValue(showsAttentionOnly ? "On" : "Off")
            .accessibilityAddTraits(showsAttentionOnly ? .isSelected : [])
            .help(
                showsAttentionOnly
                    ? (organization == .tasks ? "Show all tasks" : "Show all chats")
                    : (organization == .tasks ? "Show active and unread tasks" : "Show active and unread chats")
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
        case .chronological, .tasks:
            ForEach(chronologicalSessions) { session in
                sessionRow(session, showsWorkspace: true)
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
        .accessibilityValue(organization.title)
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

    private func loadingWorkspace(_ name: String, chats: [String]) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 0) {
                HStack(spacing: MobiusSpace.s) {
                    MobiusIcon(.folder, foreground: palette.muted)
                        .unredacted()
                    Text(name)
                        .font(MobiusStyle.controlFont)
                    MobiusIcon(
                        .caretDown,
                        size: 12,
                        foreground: palette.muted
                    )
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

            ForEach(chats, id: \.self) { title in
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
        var sessions = model.sessions.filter { session in
            organization == .tasks ? session.isCronTask : !session.isCronTask
        }
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

    private var emptySessionsMessage: String {
        if !searchText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return organization == .tasks ? "No tasks match your search" : "No chats match your search"
        }
        if !model.connectionState.isReady && model.sessions.isEmpty {
            return model.connectionState.label
        }
        if showsAttentionOnly {
            return organization == .tasks ? "No tasks need attention" : "No chats need attention"
        }
        return organization == .tasks ? "No tasks yet" : "No chats yet"
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
        Dictionary(grouping: displayedSessions) {
            $0.sessionContext.workspaceId ?? $0.sessionContext.workspaceLabel ?? "workspace"
        }
        .map { id, sessions in
            let path = sessions.first?.sessionContext.workspaceLabel ?? "Workspace"
            return WorkspaceSessions(
                id: id,
                name: workspaceName(path),
                path: path,
                sessions: sessions.sorted(by: pinnedThenRecent)
            )
        }
        .sorted {
            if $0.id == model.workspace?.id { return true }
            if $1.id == model.workspace?.id { return false }
            let firstUpdated = $0.sessions.first?.updatedAt ?? 0
            let secondUpdated = $1.sessions.first?.updatedAt ?? 0
            if firstUpdated != secondUpdated { return firstUpdated > secondUpdated }
            return $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending
        }
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
        return VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 0) {
                Button {
                    expansionBinding(for: group.id).wrappedValue.toggle()
                } label: {
                    HStack(spacing: MobiusSpace.s) {
                        MobiusIcon(.folder, foreground: palette.muted)
                        Text(group.name)
                            .font(MobiusStyle.controlFont)
                            .lineLimit(1)
                            .truncationMode(.middle)
                        MobiusIcon(
                            collapsedWorkspaces.contains(group.id) ? .caretRight : .caretDown,
                            size: 12,
                            foreground: palette.muted
                        )
                    }
                    .frame(
                        maxWidth: .infinity,
                        minHeight: MobiusStyle.iconButtonSize,
                        alignment: .leading
                    )
                    .contentShape(Rectangle())
                }
                .buttonStyle(.mobiusPlain)
                .accessibilityValue(
                    collapsedWorkspaces.contains(group.id) ? "Collapsed" : "Expanded"
                )
                .help(group.path)

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

            if !collapsedWorkspaces.contains(group.id) {
                ForEach(group.sessions.prefix(visibleCount)) { session in
                    sessionRow(session)
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

    private func sessionRow(
        _ session: SessionRecord,
        showsWorkspace: Bool = false
    ) -> some View {
        let isSelected = session.sessionId == model.selectedSessionID
        let isUnread = model.unreadSessionIDs.contains(session.sessionId)
        let title = model.displayedTitle(for: session)
        let workspace = session.sessionContext.workspaceLabel.map(workspaceName) ?? ""
        let activityValue: String
        switch session.activity.state {
        case .running:
            activityValue = "In progress"
        case .awaitingApproval:
            activityValue = "Awaiting approval"
        case .idle:
            activityValue = isUnread ? "Finished, unread" : ""
        }
        return HStack(spacing: MobiusSpace.xs) {
            Button {
                model.openChat(session.sessionId)
            } label: {
                HStack(spacing: MobiusSpace.s) {
                    VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                        MobiusTitleText(
                            title: title,
                            cursorColor: isSelected ? palette.accent : .primary
                        )
                        .fontWeight(isSelected ? .semibold : nil)
                        .lineLimit(1)
                        .foregroundStyle(isSelected ? palette.accent : .primary)
                        if showsWorkspace && !workspace.isEmpty {
                            Text(workspace)
                                .font(MobiusStyle.captionFont)
                                .foregroundStyle(palette.muted)
                                .lineLimit(1)
                                .truncationMode(.middle)
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    SessionActivityIndicator(
                        state: session.activity.state,
                        isUnread: isUnread
                    )
                    if session.pinned {
                        MobiusIcon(
                            .pushPin,
                            size: MobiusStyle.glyphMark,
                            foreground: palette.accent
                        )
                        .accessibilityHidden(true)
                    }
                }
                .frame(minHeight: MobiusStyle.iconButtonSize)
                .contentShape(Rectangle())
            }
            .buttonStyle(.mobiusPlain)
            .disabled(!model.canOpenSession && session.sessionId != model.selectedSessionID)
            .accessibilityValue(
                [showsWorkspace ? workspace : "", session.pinned ? "Pinned" : "", activityValue]
                    .filter { !$0.isEmpty }
                    .joined(separator: ", ")
            )
            .accessibilityAddTraits(isSelected ? .isSelected : [])
        }
        .padding(.horizontal, MobiusSpace.s)
        .frame(minHeight: MobiusStyle.iconButtonSize)
        .background(
            isSelected ? palette.accentSoft.opacity(0.55) : .clear,
            in: MobiusStyle.controlShape
        )
        .overlay {
            MobiusStyle.controlShape.stroke(
                isSelected ? palette.accent.opacity(0.5) : .clear,
                lineWidth: MobiusStyle.borderWidth
            )
            .allowsHitTesting(false)
        }
    }
}

private struct SessionActivityIndicator: View {
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
}
