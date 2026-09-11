import SwiftUI

struct BotsView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var newBotFormID: UUID?
    @State private var newSwarmFormID: UUID?
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
            manualSection: "bots",
            sharesHeaderBackground: true,
            headerAccessory: { headerActions }
        ) {
            if let newBotFormID {
                Section {
                    NewBotForm { self.newBotFormID = nil }
                        .id(newBotFormID)
                        .transition(.opacity.combined(with: .move(edge: .top)))
                }
            }
            Section("Bots") {
                if model.bots.isEmpty {
                    Text("No Bots yet.")
                        .foregroundStyle(palette.muted)
                } else {
                    ForEach(orderedBots) { bot in
                        botRow(bot)
                            .mobiusSwipeActions {
                                if bot.handle != "mobius" {
                                    MobiusSwipeAction(title: "Delete", glyph: .trash, tone: "error")
                                    {
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

            if let newSwarmFormID {
                Section {
                    NewSwarmForm { self.newSwarmFormID = nil }
                        .id(newSwarmFormID)
                        .transition(.opacity.combined(with: .move(edge: .top)))
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
        .task(id: model.gateway.connectionState.isReady) {
            guard model.gateway.connectionState.isReady else { return }
            model.refreshBots()
            model.refreshRoutines()
        }
        .refreshable {
            model.refreshBots()
            model.refreshRoutines()
        }
        .animation(reduceMotion ? nil : .smooth(duration: 0.3), value: newBotFormID)
        .animation(reduceMotion ? nil : .smooth(duration: 0.3), value: model.bots.map(\.id))
        .animation(reduceMotion ? nil : .smooth(duration: 0.3), value: newSwarmFormID)
        .animation(reduceMotion ? nil : .smooth(duration: 0.3), value: model.swarms.map(\.id))
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
                newBotFormID = UUID()
            } label: {
                MobiusIcon(.aiScan, gutter: false)
            }
            .disabled(!model.canMutateBots || newBotFormID != nil || newSwarmFormID != nil)
            .groupedHeaderAction(prominent: true)
            .accessibilityLabel("New Bot")
            .help("New Bot")
            Button {
                if model.beginCreatingSwarm() { newSwarmFormID = UUID() }
            } label: {
                MobiusIcon(.swarm, gutter: false)
            }
            .disabled(
                !model.canMutateSwarm || newSwarmFormID != nil || newBotFormID != nil
                    || model.bots.contains(where: \.collaborationEnabled)
                        && model.availableBotsForSwarm().count < 2
            )
            .groupedHeaderAction()
            .accessibilityLabel("New Swarm")
            .help("Create a Swarm with Bots that have collaboration enabled in Bot capabilities.")
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
        let detail: LocalizedStringResource =
            swarm.members.count == 1
            ? "1 Bot · led by \(leaderHandle(for: swarm))"
            : "\(swarm.members.count) Bots · led by \(leaderHandle(for: swarm))"

        return SettingsNavigationRow(
            hint: "Shows swarm roster and Swarm Chat",
            open: { model.openSwarm(swarm.id) },
            marks: {
                if model.hasSwarmAttention(forSwarmID: swarm.id) {
                    MobiusIcon(
                        .bellDot,
                        size: MobiusStyle.glyphMark,
                        foreground: palette.warning
                    )
                    .accessibilityLabel("\(swarm.title) has work needing attention")
                }
            }
        ) {
            SettingsRowLabel(
                title: .verbatim(swarm.title),
                detail: .localized(detail)
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
        .accessibilityLabel(accessibilityDescription)
    }

    private var accessibilityDescription: Text {
        guard let swarmName else { return Text(verbatim: identity) }
        return Text("\(identity), swarm \(swarmName)")
    }
}

struct NewBotForm: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var name = ""
    @State private var description = ""
    @State private var submitted = false
    @State private var submittedRequestID: String?
    let onClose: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.l) {
            HStack(spacing: MobiusSpace.m) {
                MobiusIcon(.aiScan, size: 36, foreground: palette.accent)
                    .padding(MobiusSpace.m)
                    .background(palette.accentSoft, in: .rect(cornerRadius: 16))
                VStack(alignment: .leading, spacing: MobiusSpace.xs) {
                    Text(name.nonEmpty ?? String(localized: "Your Bot"))
                        .font(.headline)
                    Text(description.nonEmpty ?? String(localized: "A purpose of its own"))
                        .font(MobiusStyle.captionFont)
                        .foregroundStyle(palette.muted)
                        .lineLimit(2)
                }
                Spacer(minLength: 0)
            }
            TextField("Name", text: $name)
                .textInputAutocapitalization(.words)
                .padding(MobiusSpace.m)
                .background(palette.recessed, in: MobiusStyle.controlShape)
                .disabled(isSaving)
            TextField("Operational description", text: $description, axis: .vertical)
                .lineLimit(3...6)
                .padding(MobiusSpace.m)
                .background(palette.recessed, in: MobiusStyle.controlShape)
                .disabled(isSaving)
            Text("möbius assigns the handle, color, and current Bot defaults.")
                .font(MobiusStyle.captionFont)
                .foregroundStyle(palette.muted)
            if submitted {
                switch model.botApplyState {
                case .busy(let message), .conflict(let message), .invalid(let message),
                    .failed(let message):
                    Text(verbatim: message)
                        .font(MobiusStyle.captionFont)
                        .foregroundStyle(palette.danger)
                default: EmptyView()
                }
            }
            HStack(spacing: MobiusSpace.m) {
                Button("Cancel", action: onClose)
                    .buttonStyle(.mobiusGlass)
                    .disabled(isSaving)
                Button(action: create) {
                    if isSaving {
                        ProgressView().frame(maxWidth: .infinity)
                    } else {
                        Text("Create").frame(maxWidth: .infinity)
                    }
                }
                .mobiusProminentButton()
                .accessibilityLabel("Create")
                .disabled(!canCreate)
            }
            .buttonBorderShape(.capsule)
            .controlSize(.large)
            .buttonSizing(.flexible)
        }
        .padding(.vertical, MobiusSpace.s)
        .onChange(of: model.botMutationRequestID) { old, new in
            guard let submittedRequestID, old == submittedRequestID, new == nil else { return }
            self.submittedRequestID = nil
            if model.botApplyState == .applied { onClose() }
        }
    }

    private var isSaving: Bool {
        submittedRequestID != nil && submittedRequestID == model.botMutationRequestID
    }

    private var canCreate: Bool {
        name.nonEmpty != nil && description.nonEmpty != nil && model.canMutateBots
    }

    private func create() {
        guard canCreate else { return }
        submitted = true
        model.createBot(name: name, description: description)
        submittedRequestID = model.botMutationRequestID
    }
}

struct NewSwarmForm: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var title = ""
    @State private var leaderBotID = ""
    @State private var memberBotIDs: Set<String> = []
    @State private var submitted = false
    @State private var submittedRequestID: String?
    let onClose: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.l) {
            HStack(spacing: MobiusSpace.m) {
                MobiusIcon(.swarm, size: 36, foreground: palette.accent)
                    .padding(MobiusSpace.m)
                    .background(palette.accentSoft, in: .rect(cornerRadius: 16))
                VStack(alignment: .leading, spacing: MobiusSpace.xs) {
                    Text(title.nonEmpty ?? String(localized: "New Swarm")).font(.headline)
                    Text("Leader and coworkers")
                        .font(MobiusStyle.captionFont)
                        .foregroundStyle(palette.muted)
                }
            }
            TextField("Swarm name", text: $title)
                .textInputAutocapitalization(.words)
                .padding(MobiusSpace.m)
                .background(palette.recessed, in: MobiusStyle.controlShape)
                .disabled(isSaving)
            Picker("Leader", selection: $leaderBotID) {
                Text("Choose a leader").tag("")
                ForEach(availableBots) { bot in
                    Text(verbatim: "\(bot.name) · @\(bot.handle)").tag(bot.id)
                }
            }
            .pickerStyle(.menu)
            .disabled(isSaving)
            VStack(alignment: .leading, spacing: MobiusSpace.s) {
                Text("Coworkers").font(MobiusStyle.captionFont).foregroundStyle(palette.muted)
                ForEach(availableBots.filter { $0.id != leaderBotID }) { bot in
                    Button {
                        if !memberBotIDs.insert(bot.id).inserted { memberBotIDs.remove(bot.id) }
                    } label: {
                        HStack(spacing: MobiusSpace.m) {
                            SettingsRowLabel(
                                title: .verbatim(bot.name), detail: .verbatim("@\(bot.handle)")
                            ) {
                                MobiusIcon(.aiScan, foreground: bot.tint.color)
                            }
                            Spacer(minLength: 0)
                            if memberBotIDs.contains(bot.id) {
                                MobiusIcon(.checkCircle, foreground: palette.accent)
                            } else {
                                Circle().strokeBorder(palette.line, lineWidth: 1)
                                    .frame(
                                        width: MobiusStyle.glyphInline,
                                        height: MobiusStyle.glyphInline)
                            }
                        }
                        .frame(minHeight: MobiusStyle.rowTouch)
                        .contentShape(.rect)
                    }
                    .buttonStyle(.mobiusPlain)
                    .accessibilityValue(
                        memberBotIDs.contains(bot.id) ? Text("Selected") : Text("Not selected"))
                }
            }
            .disabled(isSaving)
            .animation(reduceMotion ? nil : .smooth(duration: 0.25), value: leaderBotID)
            Text(
                "Enable Swarm collaboration in Bot capabilities to make a Bot available here. Each Bot can belong to one swarm."
            )
            .font(MobiusStyle.captionFont)
            .foregroundStyle(palette.muted)
            if submitted {
                switch model.swarmApplyState {
                case .busy(let message), .conflict(let message), .invalid(let message),
                    .failed(let message):
                    Text(verbatim: message).font(MobiusStyle.captionFont).foregroundStyle(
                        palette.danger)
                default: EmptyView()
                }
            }
            HStack(spacing: MobiusSpace.m) {
                Button("Cancel", action: onClose)
                    .buttonStyle(.mobiusGlass)
                    .disabled(isSaving)
                Button(action: create) {
                    if isSaving {
                        ProgressView().frame(maxWidth: .infinity)
                    } else {
                        Text("Create").frame(maxWidth: .infinity)
                    }
                }
                .mobiusProminentButton()
                .accessibilityLabel("Create")
                .disabled(!canCreate)
            }
            .buttonBorderShape(.capsule)
            .controlSize(.large)
            .buttonSizing(.flexible)
        }
        .padding(.vertical, MobiusSpace.s)
        .onChange(of: leaderBotID) { _, newValue in memberBotIDs.remove(newValue) }
        .onChange(of: model.swarmMutationRequestID) { old, new in
            guard let submittedRequestID, old == submittedRequestID, new == nil else { return }
            self.submittedRequestID = nil
            if model.swarmApplyState == .applied { onClose() }
        }
    }

    private var availableBots: [BotRecord] { model.availableBotsForSwarm() }
    private var isSaving: Bool {
        submittedRequestID != nil && submittedRequestID == model.swarmMutationRequestID
    }
    private var canCreate: Bool {
        let allowed = Set(availableBots.map(\.id))
        return title.nonEmpty != nil && allowed.contains(leaderBotID)
            && !memberBotIDs.isEmpty && !memberBotIDs.contains(leaderBotID)
            && memberBotIDs.isSubset(of: allowed) && model.canMutateSwarm
    }
    private func create() {
        guard canCreate else { return }
        submitted = true
        model.createSwarm(title: title, leaderBotID: leaderBotID, memberBotIDs: memberBotIDs)
        submittedRequestID = model.swarmMutationRequestID
    }
}

struct BotDetailView: View {
    private static let runPageSize = 5

    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var showsSettings = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var newRoutineFormID: UUID?
    @State private var editedRoutine: Routine?
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
                                newRoutineFormID = UUID()
                            } label: {
                                MobiusIcon(.plus, gutter: false)
                            }
                            .disabled(
                                newRoutineFormID != nil || workspaces.isEmpty
                                    || !model.gateway.connectionState.isReady
                            )
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

                    routineSections

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
        .sheet(item: $editedRoutine) { routine in
            NavigationStack {
                PageScaffold(title: "Edit routine", detail: "", showsBackdrop: false) {
                    Section {
                        RoutineForm(botID: routine.botId, routine: routine, workspaces: workspaces)
                        {
                            editedRoutine = nil
                        }
                    }
                }
            }
            .mobiusSheet(detents: [.large])
        }
        .animation(reduceMotion ? nil : .smooth(duration: 0.3), value: newRoutineFormID)
    }

    @ViewBuilder
    private var routineSections: some View {
        if let newRoutineFormID {
            Section {
                RoutineForm(botID: botID, workspaces: workspaces) {
                    self.newRoutineFormID = nil
                }
                .id(newRoutineFormID)
                .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        Section("Routines") {
            if newRoutineFormID == nil, let error = model.routineError {
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
                        edit: { editedRoutine = routine }
                    )
                }
            }
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
        return model.chat.sessions.compactMap { session in
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
                        if model.chat.isLoadingBotSessions && sessions.isEmpty {
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
        .task(id: "\(botID):\(model.gateway.connectionState.isReady)") {
            model.refreshBotSessions(botID)
        }
        .refreshable {
            model.refreshBotSessions(botID)
        }
    }

    private var bot: BotRecord? { model.bots.first { $0.id == botID } }

    private var sessions: [SessionRecord] {
        guard model.chat.botSessionsBotID == botID else { return [] }
        return model.chat.botSessions.sorted {
            if $0.updatedAt != $1.updatedAt { return $0.updatedAt > $1.updatedAt }
            return $0.sessionId < $1.sessionId
        }
    }

    private func sessionDetail(_ session: SessionRecord) -> String? {
        session.sessionContext.originLabel
    }
}
