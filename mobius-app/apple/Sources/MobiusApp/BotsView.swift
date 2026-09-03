import SwiftUI

struct BotsView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var showsNewBot = false
    @State private var showsNewSwarm = false
    @State private var botToDelete: BotRecord?
    @State private var botToRename: BotRecord?
    @State private var botRenameDraft = ""
    @State private var swarmToRename: SwarmRecord?
    @State private var swarmRenameDraft = ""
    @State private var swarmToDelete: SwarmRecord?

    var body: some View {
        @Bindable var model = model
        PageScaffold(
            title: "Bots",
            detail: "Durable agents, their routines, and the swarms they form.",
            sharesHeaderBackground: true,
            headerAccessory: { headerActions }
        ) {
            Section("Bots") {
                if model.bots.isEmpty {
                    Text("No Bots yet.")
                        .foregroundStyle(palette.muted)
                } else {
                    ForEach(orderedBots) { bot in
                        botRow(bot)
                        .mobiusSwipeActions {
                            if bot.handle != "mobius" {
                                MobiusSwipeAction(title: "Delete", glyph: .trash, tone: "error") {
                                    botToDelete = bot
                                }
                            }
                            MobiusSwipeAction(title: "Rename", glyph: .pencilSimple) {
                                botRenameDraft = bot.name
                                botToRename = bot
                            }
                        }
                    }
                }
            }

            Section("Swarms") {
                if model.swarms.isEmpty {
                    Text("No swarms yet.")
                        .foregroundStyle(palette.muted)
                } else {
                    ForEach(orderedSwarms) { swarm in
                        swarmRow(swarm)
                        .mobiusSwipeActions {
                            MobiusSwipeAction(title: "Delete", glyph: .trash, tone: "error") {
                                swarmToDelete = swarm
                            }
                            MobiusSwipeAction(title: "Rename", glyph: .pencilSimple) {
                                swarmRenameDraft = swarm.title
                                swarmToRename = swarm
                            }
                        }
                    }
                }
            }
        }
        .task(id: model.connectionState.isReady) {
            guard model.connectionState.isReady else { return }
            model.refreshBots()
            model.refreshRoutines()
        }
        .refreshable {
            model.refreshBots()
            model.refreshRoutines()
        }
        .sheet(isPresented: $showsNewBot) {
            NewBotSheet()
        }
        .sheet(isPresented: $showsNewSwarm) {
            NewSwarmSheet()
        }
        .sheet(item: $model.presentedRoutineRun, onDismiss: model.closeRoutineRunPreview) { _ in
            RoutineRunTranscriptSheet()
        }
        .alert("Delete this Bot and all its data?", isPresented: botDeletionPresented) {
            Button("Delete Bot and All Data", role: .destructive) {
                if let botToDelete { model.deleteBot(botToDelete) }
                botToDelete = nil
            }
            Button("Cancel", role: .cancel) { botToDelete = nil }
        } message: {
            if let botToDelete {
                Text(
                    "This permanently deletes every conversation, routine, run history, and swarm membership owned by @\(botToDelete.handle). If this Bot leads a swarm, its Swarm Chat and collective scratchpad are also deleted. Active work must finish first."
                )
            }
        }
        .alert("Rename Bot", isPresented: botRenamePresented) {
            TextField("Bot name", text: $botRenameDraft)
            Button("Cancel", role: .cancel) { botToRename = nil }
            Button("Rename") {
                if let botToRename {
                    model.beginEditingBot(botToRename)
                    model.botNameDraft = botRenameDraft.trimmingCharacters(
                        in: .whitespacesAndNewlines
                    )
                    model.saveBotDraft()
                }
                botToRename = nil
            }
            .disabled(botRenameDraft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
        .alert("Rename swarm", isPresented: swarmRenamePresented) {
            TextField("Swarm name", text: $swarmRenameDraft)
            Button("Cancel", role: .cancel) { swarmToRename = nil }
            Button("Rename") {
                if let swarmToRename {
                    model.renameSwarm(swarmToRename, title: swarmRenameDraft)
                }
                swarmToRename = nil
            }
            .disabled(swarmRenameDraft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
        .alert("Disband this swarm?", isPresented: swarmDeletionPresented) {
            Button("Disband", role: .destructive) {
                if let swarmToDelete { model.disbandSwarm(swarmToDelete) }
                swarmToDelete = nil
            }
            Button("Cancel", role: .cancel) { swarmToDelete = nil }
        } message: {
            Text("This permanently deletes the shared Swarm Chat and collective scratchpad.")
        }
    }

    private var headerActions: some View {
        HeaderActionGroup {
            Button {
                showsNewBot = true
            } label: {
                MobiusIcon(.aiScan, gutter: false)
            }
            .disabled(!model.canMutateBots)
            .groupedHeaderAction(prominent: true)
            .accessibilityLabel("New Bot")
            .help("New Bot")
            Button {
                showsNewSwarm = true
            } label: {
                MobiusIcon(.swarm, gutter: false)
            }
            .disabled(model.availableBotsForSwarm().count < 2 || !model.canMutateSwarm)
            .groupedHeaderAction()
            .accessibilityLabel("New Swarm")
            .help("New Swarm")
        }
    }

    private func botRow(_ bot: BotRecord) -> some View {
        let swarm = model.swarm(containingBot: bot.id)
        return SettingsNavigationRow(
            hint: "Shows Bot details",
            open: { model.navigationPath = [.bot(bot.id)] },
            marks: {
                if model.hasBackgroundApproval(forBotID: bot.id) {
                    MobiusIcon(
                        .bellDot,
                        size: MobiusStyle.glyphMark,
                        foreground: palette.warning
                    )
                    .accessibilityLabel("\(bot.name) has work awaiting approval")
                }
            }
        ) {
            HStack(spacing: MobiusSpace.s) {
                MobiusIcon(
                    .aiScan,
                    size: MobiusStyle.glyphLead,
                    foreground: bot.tint.color
                )
                .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                    Text(verbatim: bot.name)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    BotOwnershipLine(
                        identity: "@\(bot.handle)",
                        swarmName: swarm?.title
                    )
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .accessibilityElement(children: .combine)
        }
    }

    private func swarmRow(_ swarm: SwarmRecord) -> some View {
        SettingsNavigationRow(
            hint: "Shows swarm roster and Swarm Chat",
            open: { model.openSwarm(swarm.id) },
            marks: EmptyView.init
        ) {
            SettingsRowLabel(
                title: .verbatim(swarm.title),
                detail: .verbatim("\(swarm.members.count) Bots · led by \(leaderHandle(for: swarm))")
            ) {
                MobiusIcon(
                    .swarm,
                    size: MobiusStyle.glyphLead,
                    foreground: palette.accent
                )
                .accessibilityHidden(true)
            }
        }
    }

    private func leaderHandle(for swarm: SwarmRecord) -> String {
        swarm.members.first { $0.botId == swarm.leaderBotId }.map { "@\($0.handle)" }
            ?? model.bots.first { $0.id == swarm.leaderBotId }.map { "@\($0.handle)" }
            ?? "—"
    }

    private var botDeletionPresented: Binding<Bool> {
        Binding(
            get: { botToDelete != nil },
            set: { if !$0 { botToDelete = nil } }
        )
    }

    private var swarmRenamePresented: Binding<Bool> {
        Binding(
            get: { swarmToRename != nil },
            set: { if !$0 { swarmToRename = nil } }
        )
    }

    private var botRenamePresented: Binding<Bool> {
        Binding(
            get: { botToRename != nil },
            set: { if !$0 { botToRename = nil } }
        )
    }

    private var swarmDeletionPresented: Binding<Bool> {
        Binding(
            get: { swarmToDelete != nil },
            set: { if !$0 { swarmToDelete = nil } }
        )
    }

    private var orderedBots: [BotRecord] {
        model.bots.sorted {
            $0.name.localizedStandardCompare($1.name) == .orderedAscending
        }
    }

    private var orderedSwarms: [SwarmRecord] {
        model.swarms.sorted {
            if $0.updatedAtMs != $1.updatedAtMs { return $0.updatedAtMs > $1.updatedAtMs }
            return $0.title.localizedStandardCompare($1.title) == .orderedAscending
        }
    }
}

struct BotOwnershipLine: View {
    @Environment(\.mobiusPalette) private var palette
    let identity: String
    var swarmName: String?

    var body: some View {
        HStack(spacing: MobiusSpace.xs) {
            Text(verbatim: identity)
            if let swarmName {
                Text(verbatim: "•")
                    .accessibilityHidden(true)
                MobiusIcon(.swarm, size: MobiusStyle.glyphMark, gutter: false)
                    .accessibilityHidden(true)
                Text(verbatim: swarmName)
                    .lineLimit(1)
            }
        }
        .font(MobiusStyle.captionFont)
        .foregroundStyle(palette.muted)
        .lineLimit(1)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(Text(verbatim: accessibilityDescription))
    }

    private var accessibilityDescription: String {
        swarmName.map { "\(identity), swarm \($0)" } ?? identity
    }
}

private struct NewBotSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var description = ""

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Name", text: $name)
                        .textInputAutocapitalization(.words)
                    TextField(
                        "Operational description",
                        text: $description,
                        axis: .vertical
                    )
                    .lineLimit(3...6)
                } header: {
                    Text("Identity")
                } footer: {
                    Text("möbius assigns the handle, color, and current Bot defaults.")
                }
            }
            .scrollContentBackground(.hidden)
            .navigationTitle("New Bot")
            .toolbarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: dismiss.callAsFunction)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Create", action: create)
                        .disabled(!canCreate)
                }
            }
        }
        .mobiusSheet()
    }

    private var canCreate: Bool {
        !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && !description.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && model.canMutateBots
    }

    private func create() {
        model.createBot(name: name, description: description)
        dismiss()
    }
}

private struct NewSwarmSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.mobiusPalette) private var palette
    @State private var title = ""
    @State private var leaderBotID = ""
    @State private var memberBotIDs: Set<String> = []

    var body: some View {
        NavigationStack {
            List {
                Section("Name") {
                    TextField("Swarm name", text: $title)
                }
                Section("Leader") {
                    Picker("Leader", selection: $leaderBotID) {
                        Text("Choose a leader").tag("")
                        ForEach(availableBots) { bot in
                            Text(verbatim: "\(bot.name) · @\(bot.handle)").tag(bot.id)
                        }
                    }
                }
                Section {
                    ForEach(availableBots.filter { $0.id != leaderBotID }) { bot in
                        Button {
                            toggle(bot.id)
                        } label: {
                            HStack(spacing: MobiusSpace.s) {
                                MobiusIcon(
                                    .aiScan,
                                    foreground: bot.tint.color
                                )
                                VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                                    MobiusTitleText(verbatim: bot.name)
                                    Text(verbatim: "@\(bot.handle)")
                                        .font(MobiusStyle.captionFont)
                                        .foregroundStyle(palette.muted)
                                }
                                Spacer(minLength: 0)
                                if memberBotIDs.contains(bot.id) {
                                    MobiusIcon(.check, foreground: palette.accent)
                                        .accessibilityHidden(true)
                                }
                            }
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.mobiusPlain)
                        .accessibilityValue(
                            memberBotIDs.contains(bot.id) ? Text("Selected") : Text("Not selected")
                        )
                    }
                } header: {
                    Text("Coworkers")
                } footer: {
                    Text("Each Bot can belong to one swarm.")
                }
            }
            .scrollContentBackground(.hidden)
            .navigationTitle("New Swarm")
            .toolbarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: dismiss.callAsFunction)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Create", action: create)
                        .disabled(!canCreate)
                }
            }
        }
        .mobiusSheet()
        .onChange(of: leaderBotID) { _, newValue in
            memberBotIDs.remove(newValue)
        }
    }

    private var availableBots: [BotRecord] {
        model.availableBotsForSwarm()
    }

    private var canCreate: Bool {
        !title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && !leaderBotID.isEmpty
            && !memberBotIDs.isEmpty
            && model.canMutateSwarm
    }

    private func toggle(_ id: String) {
        if !memberBotIDs.insert(id).inserted { memberBotIDs.remove(id) }
    }

    private func create() {
        model.createSwarm(
            title: title,
            leaderBotID: leaderBotID,
            memberBotIDs: memberBotIDs
        )
        dismiss()
    }
}

struct BotDetailView: View {
    private static let runPageSize = 5

    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var showsSettings = false
    @State private var editedRoutine: RoutineEditorTarget?
    @State private var visibleRunCount = runPageSize
    let botID: String

    var body: some View {
        Group {
            if let bot {
                PageScaffold(
                    title: .verbatim(bot.name),
                    detail: .verbatim(""),
                    sharesHeaderBackground: true,
                    headerAccessory: {
                        HeaderActionGroup {
                            Button {
                                editedRoutine = .create(botID)
                            } label: {
                                MobiusIcon(.plus, gutter: false)
                            }
                            .disabled(workspaces.isEmpty || !model.connectionState.isReady)
                            .groupedHeaderAction(prominent: true)
                            .accessibilityLabel("New routine")
                            .help("New routine")
                            Button {
                                model.beginEditingBot(bot)
                                showsSettings = true
                            } label: {
                                MobiusIcon(.slidersHorizontal, gutter: false)
                            }
                            .groupedHeaderAction()
                            .accessibilityLabel("Edit Bot")
                            .help("Edit Bot")
                        }
                    }
                ) {
                    Section("Description") {
                        Text(verbatim: bot.description)
                            .font(MobiusStyle.bodyFont)
                            .fixedSize(horizontal: false, vertical: true)
                    }

                    if let swarm = model.swarm(containingBot: bot.id) {
                        Section("Swarm") {
                            Button {
                                model.navigationPath.append(.swarm(swarm.id))
                            } label: {
                                MobiusLabel(verbatim: swarm.title, glyph: .swarm)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                            }
                            .buttonStyle(.mobiusPlain)
                        }
                    }

                    Section("Routines") {
                        if let error = model.routineError {
                            StatusBanner(
                                tone: .error,
                                title: .localized("Routine rejected"),
                                detail: .verbatim(error)
                            )
                        }
                        if botRoutines.isEmpty {
                            Text("No routines yet.")
                                .foregroundStyle(palette.muted)
                        } else {
                            ForEach(botRoutines) { routine in
                                RoutineRow(
                                    routine: routine,
                                    edit: { editedRoutine = .edit(routine) }
                                )
                            }
                        }
                    }

                    Section("Run history") {
                        if botRuns.isEmpty {
                            Text("No routine runs yet.")
                                .foregroundStyle(palette.muted)
                        } else {
                            ForEach(botRuns.prefix(visibleRunCount)) { run in
                                let approval = model.backgroundApproval(
                                    forSessionID: run.sessionId
                                )
                                RoutineRunRow(
                                    run: run,
                                    name: routineName(run.routineId),
                                    awaitsApproval: approval != nil,
                                    open: {
                                        if let approval {
                                            model.resumeBotSession(
                                                botID: approval.botId,
                                                sessionID: approval.sessionId
                                            )
                                        } else {
                                            model.presentRoutineRun(run)
                                        }
                                    },
                                    delete: { model.deleteRoutineRun(run) }
                                )
                            }
                            if visibleRunCount < botRuns.count {
                                CatalogMoreButton(accessibilityLabel: "Show more routine runs") {
                                    visibleRunCount += Self.runPageSize
                                }
                            }
                        }
                    }

                    Section {
                        SettingsNavigationRow(
                            hint: "Shows conversations handled by this Bot",
                            open: { model.openBotChats(bot.id) },
                            marks: EmptyView.init
                        ) {
                            SettingsRowLabel(title: "Conversations") {
                                MobiusIcon(
                                    .note01,
                                    size: MobiusStyle.glyphLead,
                                    foreground: bot.tint.color
                                )
                                .accessibilityHidden(true)
                            }
                        }
                        SettingsNavigationRow(
                            hint: "Shows private conversations created by routines and Swarm work",
                            open: { model.openBotSessions(bot.id) },
                            marks: {
                                if model.hasBackgroundApproval(forBotID: bot.id) {
                                    MobiusIcon(
                                        .bellDot,
                                        size: MobiusStyle.glyphMark,
                                        foreground: palette.warning
                                    )
                                    .accessibilityLabel("Background work awaiting approval")
                                }
                            }
                        ) {
                            SettingsRowLabel(title: "Background work") {
                                MobiusIcon(
                                    .eyeOff,
                                    size: MobiusStyle.glyphLead,
                                    foreground: bot.tint.color
                                )
                                .accessibilityHidden(true)
                            }
                        }
                    }
                }
                .navigationSubtitle("@\(bot.handle)")
            } else {
                MobiusUnavailable(
                    title: "Bot unavailable",
                    glyph: .aiScan,
                    detail: "This Bot is no longer available."
                )
            }
        }
        .task {
            model.refreshRoutines()
        }
        .sheet(isPresented: $showsSettings) {
            NavigationStack {
                AgentSettingsView(scope: .bot(botID))
                    .toolbar {
                        ToolbarItem(placement: .cancellationAction) {
                            Button("Done") { showsSettings = false }
                        }
                    }
            }
            .mobiusSheet(detents: [.large])
        }
        .sheet(item: $editedRoutine) { target in
            RoutineEditorSheet(
                botID: target.botID,
                routine: target.routine,
                workspaces: workspaces
            )
        }
    }

    private var bot: BotRecord? { model.bots.first { $0.id == botID } }

    private var botRoutines: [Routine] {
        model.routines.filter { $0.botId == botID }.sorted {
            ($0.nextRunAt ?? Int64.max) < ($1.nextRunAt ?? Int64.max)
        }
    }

    private var botRuns: [RoutineRun] {
        model.routineRuns.filter { $0.botId == botID }.sorted { $0.startedAt > $1.startedAt }
    }

    private var workspaces: [RoutineWorkspace] {
        var seen = Set<String>()
        return model.sessions.compactMap { session in
            guard let path = session.sessionContext.workspaceLabel,
                  seen.insert(path).inserted
            else { return nil }
            let component = URL(fileURLWithPath: path).lastPathComponent
            return RoutineWorkspace(path: path, name: component.isEmpty ? path : component)
        }
        .sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
    }

    private func routineName(_ id: String) -> String {
        model.routines.first { $0.id == id }?.instructions ?? "Routine"
    }
}

struct BotSessionsView: View {
    private static let pageSize = 10

    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var visibleCount = pageSize
    let botID: String

    var body: some View {
        Group {
            if let bot {
                PageScaffold(
                    title: "Background work",
                    detail: "Private Bot conversations stay out of Chats."
                ) {
                    Section {
                        if model.isLoadingBotSessions && sessions.isEmpty {
                            HStack(spacing: MobiusSpace.s) {
                                MobiusSpinner(
                                    size: MobiusStyle.glyphInline,
                                    foreground: palette.muted
                                )
                                Text("Loading Bot work…")
                                    .foregroundStyle(palette.muted)
                            }
                            .frame(minHeight: MobiusStyle.rowTouch)
                        } else if sessions.isEmpty {
                            Text("No background conversations yet.")
                                .foregroundStyle(palette.muted)
                        } else {
                            ForEach(sessions.prefix(visibleCount)) { session in
                                SessionCatalogRow(
                                    session: session,
                                    showsWorkspace: false,
                                    showsControls: false,
                                    detail: sessionDetail(session),
                                    open: { model.openBotSession($0.sessionId) }
                                )
                            }
                            if visibleCount < sessions.count {
                                CatalogMoreButton(
                                    accessibilityLabel: "Show more background conversations"
                                ) {
                                    visibleCount += Self.pageSize
                                }
                            }
                        }
                    }
                }
                .navigationSubtitle("@\(bot.handle)")
            } else {
                MobiusUnavailable(
                    title: "Bot unavailable",
                    glyph: .aiScan,
                    detail: "This Bot is no longer available."
                )
            }
        }
        .task(id: "\(botID):\(model.connectionState.isReady)") {
            model.refreshBotSessions(botID)
        }
        .refreshable {
            model.refreshBotSessions(botID)
        }
    }

    private var bot: BotRecord? { model.bots.first { $0.id == botID } }

    private var sessions: [SessionRecord] {
        guard model.botSessionsBotID == botID else { return [] }
        return model.botSessions.sorted {
            if $0.updatedAt != $1.updatedAt { return $0.updatedAt > $1.updatedAt }
            return $0.sessionId < $1.sessionId
        }
    }

    private func sessionDetail(_ session: SessionRecord) -> String? {
        session.sessionContext.originLabel
    }
}
