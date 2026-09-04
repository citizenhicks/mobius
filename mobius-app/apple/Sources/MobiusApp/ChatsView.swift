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
    @State private var collapsedWorkspaces: Set<String> = []
    @State private var visibleSessionCounts: [String: Int] = [:]
    @State private var showsAttentionOnly = false
    @State private var organization = ChatOrganization.byProject
    @State private var searchText = ""
    @State private var selectedSessionIDs: Set<String>?

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
            if selectedSessionIDs != nil {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { selectedSessionIDs = nil }
                }
                ToolbarItem(placement: .primaryAction) {
                    Button("Delete selected chats", glyph: .trash, role: .destructive) {
                        deleteSelectedSessions()
                    }
                    .labelStyle(.iconOnly)
                    .disabled(selectedSessions.isEmpty || !model.canRenameSession)
                    .accessibilityValue(Text("\(selectedSessions.count) selected"))
                    .help("Delete selected chats")
                }
            } else {
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
        }
        .searchable(
            text: $searchText,
            prompt: "Search chats"
        )
        .searchToolbarBehavior(.automatic)
        .searchPresentationToolbarBehavior(.avoidHidingContent)
        .onChange(of: model.sessions.map(\.sessionId)) { _, sessionIDs in
            guard let selection = selectedSessionIDs, !selection.isEmpty else { return }
            let remaining = selection.intersection(sessionIDs)
            selectedSessionIDs = remaining.isEmpty ? nil : remaining
        }
        .task(id: model.cloudSession?.credentialID) {
            await model.refreshCloudAccount()
        }
    }

    private var usesIPadLayout: Bool {
        MobiusLayout.usesIPadLayout(platform: GatewayClientKind.currentApplePlatform)
    }

    private var catalogHeading: some View {
        HStack(spacing: MobiusSpace.s) {
            Text(organization.heading)
                .font(.title2.weight(.semibold))
                .layoutPriority(1)
            if !filteredBots.isEmpty {
                ScrollView(.horizontal) {
                    HStack(spacing: MobiusSpace.s) {
                        ForEach(filteredBots) { bot in
                            HStack(spacing: MobiusSpace.xs) {
                                Text(verbatim: "•")
                                    .accessibilityHidden(true)
                                MobiusLabel(
                                    verbatim: "@\(bot.handle)",
                                    glyph: .aiScan,
                                    iconColor: bot.tint.color,
                                    iconSize: MobiusStyle.glyphInline
                                )
                            }
                            .font(MobiusStyle.captionFont)
                            .foregroundStyle(palette.muted)
                        }
                    }
                    .fixedSize(horizontal: true, vertical: false)
                }
                .scrollIndicators(.hidden)
                .scrollBounceBehavior(.basedOnSize)
            }
            if showsLoadingCatalog {
                MobiusSpinner(size: MobiusStyle.glyphMark)
            }
            Spacer(minLength: 0)
            if !model.chatBotFilterIDs.isEmpty {
                Button {
                    model.chatBotFilterIDs.removeAll()
                } label: {
                    MobiusIcon(.filterMailRemove)
                }
                .buttonStyle(MobiusIconButtonStyle(bare: true))
                .accessibilityLabel("Clear Bot filters")
                .accessibilityValue(
                    model.chatBotFilterIDs.count == 1
                        ? Text("1 selected")
                        : Text("\(model.chatBotFilterIDs.count) selected")
                )
                .help("Clear Bot filters")
                .disabled(showsLoadingCatalog)
            }
            Button {
                showsAttentionOnly.toggle()
            } label: {
                MobiusIcon(attentionFilterGlyph)
            }
            .buttonStyle(MobiusIconButtonStyle(prominent: showsAttentionOnly, bare: true))
            .accessibilityLabel(
                "Filter chats needing attention"
            )
            .accessibilityValue(attentionFilterAccessibilityValue)
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
            WorkspaceSessionCatalog(
                sessions: displayedSessions,
                collapsedWorkspaces: $collapsedWorkspaces,
                visibleSessionCounts: $visibleSessionCounts,
                pageSize: Self.sessionPageSize,
                selectedSessionIDs: selectionBinding
            )
        case .chronological:
            ForEach(chronologicalSessions) { session in
                SessionCatalogRow(
                    session: session,
                    showsWorkspace: true,
                    selectedSessionIDs: selectionBinding
                )
            }
        }
    }

    private var organizationMenu: some View {
        HeaderOptionsMenu(label: "Organize and filter chats") {
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
            Section("Filter") {
                Button {
                    model.chatBotFilterIDs.removeAll()
                } label: {
                    MobiusLabel(
                        title: "All",
                        glyph: model.chatBotFilterIDs.isEmpty ? .check : .aiScan
                    )
                }
                ForEach(orderedBots) { bot in
                    Button {
                        toggleBotFilter(bot.id)
                    } label: {
                        botFilterLabel(bot)
                    }
                }
            }
            Section("Manage") {
                Button("Select chats to delete", glyph: .trash) {
                    selectedSessionIDs = []
                }
                .disabled(displayedSessions.isEmpty || !model.canRenameSession)
            }
            if model.hasCloudAccount, let limit = model.cloudAccount?.luna {
                Divider()
                Text(
                    "\(limit.remainingFraction.formatted(.percent.precision(.fractionLength(0)))) usage remaining"
                )
            }
        }
        .accessibilityValue(
            model.chatBotFilterIDs.isEmpty
                ? Text("\(organization.title), all Bots")
                : model.chatBotFilterIDs.count == 1
                    ? Text("\(organization.title), 1 Bot selected")
                    : Text("\(organization.title), \(model.chatBotFilterIDs.count) Bots selected")
        )
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
        var sessions = model.chatCatalogSessions
        if showsAttentionOnly {
            let attentionSessionIDs = model.attentionSessionIDs
            sessions = sessions.filter { attentionSessionIDs.contains($0.sessionId) }
        }
        let query = searchText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !query.isEmpty else { return sessions }
        return sessions.filter { session in
            model.displayedTitle(for: session).localizedStandardContains(query)
                || session.sessionId.localizedStandardContains(query)
                || (model.bot(for: session)?.handle.localizedStandardContains(query) ?? false)
                || (session.sessionContext.workspaceLabel ?? "")
                    .localizedStandardContains(query)
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
        if !model.chatBotFilterIDs.isEmpty {
            return "No chats for the selected Bots"
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

    private var orderedBots: [BotRecord] {
        model.bots.sorted {
            $0.name.localizedStandardCompare($1.name) == .orderedAscending
        }
    }

    private var filteredBots: [BotRecord] {
        orderedBots.filter { model.chatBotFilterIDs.contains($0.id) }
    }

    private var attentionFilterGlyph: MobiusGlyph {
        if showsAttentionOnly { return .bellOff }
        return model.attentionSessionIDs.isEmpty ? .bell : .bellDot
    }

    private var attentionFilterAccessibilityValue: Text {
        if showsAttentionOnly { return Text("On") }
        let count = model.attentionSessionIDs.count
        return count == 0
            ? Text("Off, no chats need attention")
            : Text("Off, \(count) chats need attention")
    }

    @ViewBuilder
    private func botFilterLabel(_ bot: BotRecord) -> some View {
        let selected = model.chatBotFilterIDs.contains(bot.id)
        let glyph = selected ? MobiusGlyph.check : .aiScan
        let color = selected ? palette.accent : bot.tint.color
        let title = "\(bot.name) (@\(bot.handle))"
        if let image = glyph.menuImage(color) {
            Label { Text(verbatim: title) } icon: { image }
        } else {
            MobiusLabel(verbatim: title, glyph: glyph, iconColor: color)
        }
    }

    private func toggleBotFilter(_ botID: String) {
        if !model.chatBotFilterIDs.insert(botID).inserted {
            model.chatBotFilterIDs.remove(botID)
        }
    }

    private var selectionBinding: Binding<Set<String>>? {
        guard selectedSessionIDs != nil else { return nil }
        return Binding(
            get: { selectedSessionIDs ?? [] },
            set: { selectedSessionIDs = $0 }
        )
    }

    private var selectedSessions: [SessionRecord] {
        guard let selectedSessionIDs else { return [] }
        return model.sessions.filter { selectedSessionIDs.contains($0.sessionId) }
    }

    private func deleteSelectedSessions() {
        let sessions = selectedSessions
        guard !sessions.isEmpty else { return }
        model.beginDeletingSessions(sessions)
    }

}

struct WorkspaceSessionCatalog: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    let sessions: [SessionRecord]
    @Binding var collapsedWorkspaces: Set<String>
    @Binding var visibleSessionCounts: [String: Int]
    var pageSize = 10
    var selectedSessionIDs: Binding<Set<String>>? = nil

    var body: some View {
        ForEach(WorkspaceSessions.grouped(sessions, prioritizing: model.workspace?.id)) { group in
            workspaceGroup(group)
        }
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
            visibleSessionCounts[group.id, default: pageSize],
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

                if selectedSessionIDs == nil {
                    Button {
                        model.chooseWorkspace(group.path)
                    } label: {
                        MobiusLabel(
                            title: "New chat in \(group.name)",
                            glyph: .notePencil,
                            iconColor: palette.muted
                        )
                        .labelStyle(.iconOnly)
                        .frame(
                            width: MobiusStyle.iconButtonSize,
                            height: MobiusStyle.iconButtonSize
                        )
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.mobiusPlain)
                    .disabled(!model.canCreateSession)
                    .help("New chat in \(group.path)")
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            if isExpanded {
                ForEach(group.sessions.prefix(visibleCount)) { session in
                    SessionCatalogRow(
                        session: session,
                        selectedSessionIDs: selectedSessionIDs
                    )
                }
                if visibleCount < group.sessions.count {
                    CatalogMoreButton(
                        accessibilityLabel: "Show more chats in \(group.name)"
                    ) {
                        visibleSessionCounts[group.id] = visibleCount + pageSize
                    }
                }
            }
        }
    }
}

struct CatalogMoreButton: View {
    @Environment(\.mobiusPalette) private var palette
    let accessibilityLabel: LocalizedStringResource
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            MobiusLabel(
                title: "Show more",
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
        .accessibilityLabel(Text(accessibilityLabel))
    }
}

struct SessionCatalogRow: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let session: SessionRecord
    var showsWorkspace = false
    var showsControls = true
    var detail: String?
    var open: ((SessionRecord) -> Void)? = nil
    var selectedSessionIDs: Binding<Set<String>>? = nil

    @ViewBuilder
    var body: some View {
        let isSelecting = selectedSessionIDs != nil
        let isSelected = selectedSessionIDs?.wrappedValue.contains(session.sessionId)
            ?? (session.sessionId == model.selectedSessionID)
        let isUnread = model.unreadSessionIDs.contains(session.sessionId)
        let row = HStack(spacing: MobiusSpace.xs) {
            Button {
                activate()
            } label: {
                HStack(spacing: MobiusSpace.s) {
                    VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                        MobiusTitleText(verbatim: model.displayedTitle(for: session))
                            .lineLimit(1)
                        if model.bot(for: session) != nil { ownershipLine }
                        if let supportingText, !supportingText.isEmpty {
                            Text(verbatim: supportingText)
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
                    ZStack {
                        if isSelecting {
                            MobiusIcon(
                                isSelected ? .checkCircle : .circle,
                                foreground: isSelected ? palette.accent : palette.muted
                            )
                            .accessibilityHidden(true)
                        } else {
                            SessionActivityIndicator(
                                state: session.activity.state,
                                isUnread: isUnread
                            )
                        }
                    }
                    .frame(
                        width: MobiusStyle.iconButtonSize,
                        height: MobiusStyle.iconButtonSize
                    )
                }
                .frame(minHeight: MobiusStyle.iconButtonSize)
                .contentShape(Rectangle())
            }
            .buttonStyle(.mobiusPlain)
            .disabled(
                !isSelecting
                    && !model.canOpenSession
                    && session.sessionId != model.selectedSessionID
            )
            .accessibilityValue(
                accessibilityValue(isUnread: isUnread, selection: isSelecting ? isSelected : nil)
            )
            .accessibilityAddTraits(isSelected ? .isSelected : [])
        }
        .padding(.leading, MobiusSpace.s)
        .frame(minHeight: MobiusStyle.iconButtonSize)

        if showsControls && !isSelecting {
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
        Button("Delete chat", glyph: .trash, role: .destructive) {
            model.beginDeletingSession(session)
        }
        .disabled(!model.canRenameSession)
    }

    private func activate() {
        guard let selectedSessionIDs else {
            if let open { open(session) } else { model.openChat(session.sessionId) }
            return
        }
        var selection = selectedSessionIDs.wrappedValue
        if !selection.insert(session.sessionId).inserted {
            selection.remove(session.sessionId)
        }
        selectedSessionIDs.wrappedValue = selection
    }

    private var ownershipLine: some View {
        let bot = model.bot(for: session)
        let swarm = bot.flatMap { model.swarm(containingBot: $0.id) }
        return BotOwnershipLine(
            identity: bot.map { "@\($0.handle)" } ?? "",
            swarmName: swarm?.title
        )
    }

    private var supportingText: String? {
        if let detail { return detail }
        guard showsWorkspace, let path = session.sessionContext.workspaceLabel else { return nil }
        let name = URL(fileURLWithPath: path).lastPathComponent
        return name.isEmpty ? path : name
    }

    private var ownershipDescription: String {
        guard let bot = model.bot(for: session) else { return "" }
        let handle = "@\(bot.handle)"
        guard let swarm = model.swarm(containingBot: bot.id) else {
            return handle
        }
        return "\(handle), swarm \(swarm.title)"
    }

    private func accessibilityValue(isUnread: Bool, selection: Bool?) -> Text {
        let state: String? = switch session.activity.state {
        case .running: "In progress"
        case .awaitingApproval: "Awaiting approval"
        case .idle: isUnread ? "Finished, unread" : nil
        }
        let selectionState = selection.map {
            model.localizedString($0 ? "Selected" : "Not selected")
        }
        return Text(verbatim: [selectionState, ownershipDescription, supportingText, session.pinned ? "Pinned" : nil, state]
            .compactMap { $0 }
            .joined(separator: ", "))
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
