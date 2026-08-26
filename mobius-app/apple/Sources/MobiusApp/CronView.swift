import SwiftUI

struct CronView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var presentedTaskSheet: CronTaskSheet?

    var body: some View {
        @Bindable var model = model
        PageScaffold(
            title: "Scheduled",
            detail: "Run tasks on the gateway workspace, even when this app is closed.",
            headerAccessory: {
                Button {
                    presentedTaskSheet = .create
                } label: {
                    MobiusIcon(.plus, gutter: false)
                }
                .mobiusProminentIconButton()
                .disabled(!model.connectionState.isReady || projectOptions.isEmpty)
                .accessibilityLabel("Add scheduled task")
                .help("Add scheduled task")
            }
        ) {
            if let error = model.cronError {
                StatusBanner(tone: .error, title: "Scheduled task rejected", detail: error)
                    .settingsStandaloneRow()
            }

            Section("Tasks") {
                if model.cronTasks.isEmpty {
                    Text("No scheduled tasks yet.").foregroundStyle(palette.muted)
                } else {
                    ForEach(model.cronTasks) { task in
                        CronTaskRow(
                            task: task,
                            projectName: projectName(for: task),
                            edit: { presentedTaskSheet = .edit(task) }
                        )
                    }
                }
            }

            Section("Run history") {
                if model.cronRuns.isEmpty {
                    Text("No scheduled runs yet.").foregroundStyle(palette.muted)
                } else {
                    ForEach(model.cronRuns) { run in
                        if let task = model.cronTasks.first(where: { $0.id == run.taskId }) {
                            CronRunRow(
                                run: run,
                                taskName: task.task,
                                open: { model.presentCronRun(run) }
                            )
                        }
                    }
                }
            }
        }
        .task { if model.connectionState.isReady { model.refreshCron() } }
        .refreshable { model.refreshCron() }
        .sheet(item: $presentedTaskSheet) { sheet in
            CronTaskEditorSheet(
                task: sheet.task,
                projects: projectOptions
            )
        }
        .sheet(item: $model.presentedCronRun, onDismiss: model.closeCronRunPreview) { _ in
            ScheduledRunTranscriptSheet()
        }
    }

    private var projectOptions: [CronProject] {
        Dictionary(grouping: model.sessions) {
            $0.sessionContext.workspaceId
                ?? $0.sessionContext.workspaceLabel
                ?? $0.sessionId
        }
        .compactMap { id, sessions in
            guard let source = sessions.max(by: { $0.updatedAt < $1.updatedAt }) else { return nil }
            let path = source.sessionContext.workspaceLabel ?? "Workspace"
            return CronProject(
                id: id,
                sourceSessionID: source.sessionId,
                name: URL(fileURLWithPath: path).lastPathComponent.isEmpty
                    ? path
                    : URL(fileURLWithPath: path).lastPathComponent
            )
        }
        .sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
    }

    private func projectName(for task: CronTask) -> String {
        projectOptions.first { $0.sourceSessionID == task.sourceSessionId }?.name
            ?? model.sessions.first { $0.sessionId == task.sourceSessionId }
                .flatMap { $0.sessionContext.workspaceLabel }
                .map { URL(fileURLWithPath: $0).lastPathComponent }
            ?? "Workspace"
    }
}

private struct CronProject: Identifiable, Hashable {
    let id: String
    let sourceSessionID: String
    let name: String
}

private enum CronTaskSheet: Identifiable {
    case create
    case edit(CronTask)

    var id: String {
        switch self {
        case .create: "create"
        case .edit(let task): "edit:\(task.id)"
        }
    }

    var task: CronTask? {
        if case .edit(let task) = self { return task }
        return nil
    }
}

private struct CronTaskRow: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let task: CronTask
    let projectName: String
    let edit: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.m) {
            VStack(alignment: .leading, spacing: MobiusSpace.xs) {
                Text(task.task)
                    .font(MobiusStyle.bodyFont.weight(.semibold))
                    .fixedSize(horizontal: false, vertical: true)
                Text(projectName)
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
            }
            LabeledContent("Schedule", value: cronScheduleSummary(task.schedule))
            LabeledContent("Status", value: statusText)
            if let nextRunAt = task.nextRunAt {
                LabeledContent("Next", value: Date(timeIntervalSince1970: TimeInterval(nextRunAt)).formatted(.relative(presentation: .named)))
            }
            if let lastRun = model.cronRuns
                .filter({ $0.taskId == task.id })
                .max(by: { $0.startedAt < $1.startedAt }) {
                LabeledContent("Last", value: Date(timeIntervalSince1970: TimeInterval(lastRun.startedAt)).formatted(.relative(presentation: .named)))
            }
            MobiusActionRow(collapsesToIcons: true) {
                Button("Run", glyph: .playFill) { model.runCron(task) }
                    .mobiusProminentButton()
                Button(task.enabled ? "Pause" : "Resume", glyph: task.enabled ? .stopFill : .playFill) {
                    model.updateCron(
                        task,
                        sourceSessionID: task.sourceSessionId,
                        instructions: task.task,
                        schedule: task.schedule,
                        endsAt: task.endsAt,
                        enabled: !task.enabled
                    )
                }
                Button("Edit", glyph: .pencilSimple) { edit() }
                Button("Delete", glyph: .trash, role: .destructive) { model.deleteCron(task) }
            }
        }
    }

    private var statusText: String {
        if task.finished { return "Finished" }
        return task.enabled ? "Active" : "Paused"
    }
}

private struct CronRunRow: View {
    @Environment(\.mobiusPalette) private var palette
    let run: CronRun
    let taskName: String
    let open: () -> Void

    var body: some View {
        Button(action: open) {
            HStack(alignment: .top, spacing: MobiusSpace.m) {
                Circle().fill(statusColor).frame(width: 9, height: 9).padding(.top, MobiusSpace.xs)
                VStack(alignment: .leading, spacing: MobiusSpace.xs) {
                    HStack {
                        Text(taskName)
                            .font(MobiusStyle.bodyFont.weight(.semibold))
                            .lineLimit(1)
                        Spacer(minLength: MobiusSpace.s)
                        Text(run.status.rawValue.capitalized)
                            .font(MobiusStyle.metadataFont.weight(.bold))
                            .foregroundStyle(statusColor)
                    }
                    Text(Date(timeIntervalSince1970: TimeInterval(run.startedAt)), style: .relative)
                        .font(MobiusStyle.metadataFont)
                        .foregroundStyle(palette.muted)
                    if let message = run.message {
                        Text(message)
                            .font(MobiusStyle.bodyFont)
                            .foregroundStyle(palette.muted)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                MobiusIcon(.caretRight, size: MobiusStyle.glyphMark, foreground: palette.muted)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(run.sessionId == nil)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            run.sessionId == nil
                ? "\(run.status.rawValue) run for \(taskName) has no transcript"
                : "Open \(run.status.rawValue) run for \(taskName)"
        )
    }

    private var statusColor: Color {
        switch run.status {
        case .succeeded: palette.signal
        case .failed: palette.danger
        case .running: palette.accent
        case .skipped: palette.muted
        }
    }
}

private enum CronScheduleMode: String, CaseIterable, Identifiable {
    case once, interval, daily, weekly, advanced
    var id: Self { self }
    var title: String {
        switch self {
        case .once: "Once"
        case .interval: "Every"
        case .daily: "Daily"
        case .weekly: "Weekly"
        case .advanced: "Advanced cron"
        }
    }
}

private enum CronIntervalUnit: String, CaseIterable, Identifiable {
    case seconds, minutes, hours
    var id: Self { self }
    var title: String { rawValue.capitalized }
    var seconds: Int64 {
        switch self {
        case .seconds: 1
        case .minutes: 60
        case .hours: 3_600
        }
    }
}

private enum CronEndMode: String, CaseIterable, Identifiable {
    case never, duration, date
    var id: Self { self }
    var title: String {
        switch self {
        case .never: "Never"
        case .duration: "After duration"
        case .date: "At date"
        }
    }
}

private enum CronDurationUnit: String, CaseIterable, Identifiable {
    case minutes, hours, days
    var id: Self { self }
    var title: String { rawValue.capitalized }
    var seconds: TimeInterval {
        switch self {
        case .minutes: 60
        case .hours: 3_600
        case .days: 86_400
        }
    }
}

private func cronDate(for schedule: SimpleCronSchedule, timeZone: TimeZone = .current) -> Date {
    var calendar = Calendar.current
    calendar.timeZone = timeZone
    return calendar.date(
        bySettingHour: schedule.hour,
        minute: schedule.minute,
        second: 0,
        of: .now
    ) ?? .now
}

private struct CronTaskEditorSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.mobiusPalette) private var palette
    let task: CronTask?
    let projects: [CronProject]
    @State private var sourceSessionID: String
    @State private var instructions: String
    @State private var mode: CronScheduleMode
    @State private var onceDate: Date
    @State private var intervalValue: Int
    @State private var intervalUnit: CronIntervalUnit
    @State private var cronExpression: String
    @State private var dailyTime: Date
    @State private var weeklyTime: Date
    @State private var weekday: Int
    @State private var endMode: CronEndMode
    @State private var durationValue: Int
    @State private var durationUnit: CronDurationUnit
    @State private var endDate: Date
    @State private var enabled: Bool

    init(task: CronTask?, projects: [CronProject]) {
        self.task = task
        self.projects = projects
        let schedule = task?.schedule
        let parsedCron = schedule?.expression.flatMap(simpleCronSchedule)
        let initialMode: CronScheduleMode = switch schedule?.kind {
        case .once: .once
        case .interval: .interval
        case .cron: parsedCron.map { $0.weekday == nil ? .daily : .weekly } ?? .advanced
        case nil: .once
        }
        let initialDate = Date(timeIntervalSince1970: TimeInterval(schedule?.at ?? Int64(Date.now.timeIntervalSince1970 + 3_600)))
        let scheduleTimeZone = TimeZone(identifier: schedule?.timeZone ?? "") ?? .current
        let initialCronDate = parsedCron.map { cronDate(for: $0, timeZone: scheduleTimeZone) } ?? initialDate
        let seconds = schedule?.everySeconds ?? 600
        let initialUnit: CronIntervalUnit = if seconds.isMultiple(of: 3_600) {
            .hours
        } else if seconds.isMultiple(of: 60) {
            .minutes
        } else {
            .seconds
        }
        let initialValue = max(1, Int(seconds / initialUnit.seconds))
        _sourceSessionID = State(initialValue: task?.sourceSessionId ?? projects.first?.sourceSessionID ?? "")
        _instructions = State(initialValue: task?.task ?? "")
        _mode = State(initialValue: initialMode)
        _onceDate = State(initialValue: initialDate)
        _intervalValue = State(initialValue: initialValue)
        _intervalUnit = State(initialValue: initialUnit)
        _cronExpression = State(initialValue: schedule?.expression ?? "")
        _dailyTime = State(initialValue: initialCronDate)
        _weeklyTime = State(initialValue: initialCronDate)
        _weekday = State(initialValue: (parsedCron?.weekday ?? 1) + 1)
        _endMode = State(initialValue: task?.endsAt == nil ? .never : .date)
        _durationValue = State(initialValue: 1)
        _durationUnit = State(initialValue: .hours)
        _endDate = State(initialValue: Date(timeIntervalSince1970: TimeInterval(task?.endsAt ?? Int64(Date.now.timeIntervalSince1970 + 86_400))))
        _enabled = State(initialValue: task?.enabled ?? true)
    }

    var body: some View {
        NavigationStack {
            Form {
                Section("Task") {
                    Picker("Project", selection: $sourceSessionID) {
                        ForEach(projects) { project in
                            Text(project.name).tag(project.sourceSessionID)
                        }
                    }
                    TextEditor(text: $instructions)
                        .frame(minHeight: 120)
                        .textInputAutocapitalization(.sentences)
                }

                Section("Schedule") {
                    Picker("Repeat", selection: $mode) {
                        ForEach(CronScheduleMode.allCases) { mode in
                            Text(mode.title).tag(mode)
                        }
                    }
                    scheduleControls
                }

                if mode != .once {
                    Section("End") {
                        Picker("Ends", selection: $endMode) {
                            ForEach(CronEndMode.allCases) { mode in
                                Text(mode.title).tag(mode)
                            }
                        }
                        if endMode == .duration {
                            Stepper("After \(durationValue) \(durationUnit.rawValue)", value: $durationValue, in: 1...365)
                            Picker("Unit", selection: $durationUnit) {
                                ForEach(CronDurationUnit.allCases) { unit in
                                    Text(unit.title).tag(unit)
                                }
                            }
                        } else if endMode == .date {
                            DatePicker("Date", selection: $endDate, in: Date.now..., displayedComponents: [.date, .hourAndMinute])
                        }
                    }
                }

                if task != nil {
                    Section {
                        Toggle("Enabled", isOn: $enabled)
                    }
                }

                Section("Summary") {
                    Text(summary)
                        .foregroundStyle(palette.muted)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .scrollContentBackground(.hidden)
            .navigationTitle(task == nil ? "New scheduled task" : "Edit scheduled task")
            .toolbarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: dismiss.callAsFunction)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button(task == nil ? "Create" : "Save", action: save)
                        .disabled(!canSave)
                }
            }
        }
        .presentationDetents([.medium, .large])
    }

    @ViewBuilder
    private var scheduleControls: some View {
        switch mode {
        case .once:
            DatePicker("Run at", selection: $onceDate, in: Date.now..., displayedComponents: [.date, .hourAndMinute])
        case .interval:
            Stepper(
                "Every \(intervalValue) \(intervalUnit.rawValue)",
                value: $intervalValue,
                in: (intervalUnit == .seconds ? 60 : 1)...365
            )
            Picker("Unit", selection: $intervalUnit) {
                ForEach(CronIntervalUnit.allCases) { unit in
                    Text(unit.title).tag(unit)
                }
            }
            .onChange(of: intervalUnit) { _, unit in
                if unit == .seconds { intervalValue = max(intervalValue, 60) }
            }
        case .daily:
            DatePicker("Time", selection: $dailyTime, displayedComponents: [.hourAndMinute])
        case .weekly:
            Picker("Day", selection: $weekday) {
                ForEach(1...7, id: \.self) { day in
                    Text(Calendar.current.weekdaySymbols[day - 1]).tag(day)
                }
            }
            DatePicker("Time", selection: $weeklyTime, displayedComponents: [.hourAndMinute])
        case .advanced:
            TextField("0 9 * * 1-5", text: $cronExpression)
                .font(MobiusStyle.bodyFont.monospaced())
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
        }
    }

    private var schedule: CronSchedule? {
        switch mode {
        case .once:
            return .once(at: Int64(onceDate.timeIntervalSince1970))
        case .interval:
            return .interval(seconds: Int64(intervalValue) * intervalUnit.seconds)
        case .daily:
            return .cron(
                cronExpression(for: dailyTime, weekday: nil),
                timeZone: TimeZone.current.identifier
            )
        case .weekly:
            return .cron(
                cronExpression(for: weeklyTime, weekday: weekday - 1),
                timeZone: TimeZone.current.identifier
            )
        case .advanced:
            let expression = cronExpression.trimmingCharacters(in: .whitespacesAndNewlines)
            return expression.isEmpty
                ? nil
                : .cron(expression, timeZone: TimeZone.current.identifier)
        }
    }

    private var endsAt: Int64? {
        guard mode != .once else { return nil }
        return switch endMode {
        case .never: nil
        case .duration: Int64(Date.now.timeIntervalSince1970 + Double(durationValue) * durationUnit.seconds)
        case .date: Int64(endDate.timeIntervalSince1970)
        }
    }

    private var canSave: Bool {
        !sourceSessionID.isEmpty
            && !instructions.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && schedule != nil
            && (mode != .interval || (schedule?.everySeconds ?? 0) >= 60)
            && (endsAt == nil || endsAt! > Int64(Date.now.timeIntervalSince1970))
    }

    private var summary: String {
        guard let schedule else { return "Choose a valid schedule." }
        let end = endsAt.map { " · ends \(Date(timeIntervalSince1970: TimeInterval($0)).formatted(date: .abbreviated, time: .shortened))" } ?? ""
        return cronScheduleSummary(schedule) + end
    }

    private func cronExpression(for date: Date, weekday: Int?) -> String {
        let components = Calendar.current.dateComponents([.hour, .minute], from: date)
        let minute = components.minute ?? 0
        let hour = components.hour ?? 0
        return weekday.map { "\(minute) \(hour) * * \($0)" } ?? "\(minute) \(hour) * * *"
    }

    private func save() {
        guard let schedule else { return }
        if let task {
            model.updateCron(
                task,
                sourceSessionID: sourceSessionID,
                instructions: instructions,
                schedule: schedule,
                endsAt: endsAt,
                enabled: enabled
            )
        } else {
            model.createCron(
                sourceSessionID: sourceSessionID,
                task: instructions,
                schedule: schedule,
                endsAt: endsAt
            )
        }
        dismiss()
    }
}

struct ScheduledRunTranscriptSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette

    @ViewBuilder
    var body: some View {
        if let error = model.cronRunPreviewError {
            VStack(spacing: 0) {
                header
                StatusBanner(tone: .error, title: "Run transcript unavailable", detail: error)
                    .padding(MobiusSpace.l)
                Spacer(minLength: 0)
            }
            .background(palette.canvas)
            .presentationDetents([.medium, .large])
        } else if model.cronRunPreview != nil {
            ReadOnlyTranscriptSheet(
                entries: model.cronRunPreviewEntries,
                hasEarlier: model.cronRunPreviewNextBeforeSequence != nil,
                isLoading: model.isLoadingCronRunPreview,
                loadEarlier: model.loadEarlierCronRunPreview,
                header: { header }
            )
        } else {
            ProgressView()
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(palette.canvas)
        }
    }

    private var header: some View {
        HStack(spacing: MobiusSpace.s) {
            VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                if let task = model.cronRunPreview?.task {
                    Text(task.task)
                        .font(MobiusStyle.controlFont.weight(.semibold))
                        .lineLimit(1)
                }
                if let run = model.cronRunPreview?.run ?? model.presentedCronRun {
                    Text("Run · \(run.status.rawValue.capitalized)")
                        .font(MobiusStyle.metadataFont)
                        .foregroundStyle(palette.muted)
                }
            }
            Spacer(minLength: 0)
            SettingsInfoButton(
                title: "Run transcript",
                detail: model.cronRunPreview.map {
                    "\(cronScheduleSummary($0.task.schedule)) · read-only run transcript"
                } ?? "Read-only run transcript",
                glyph: .info
            )
        }
        .frame(maxWidth: .infinity, minHeight: MobiusStyle.iconButtonSize, alignment: .leading)
        .padding(.leading, MobiusSpace.l)
        .padding(.trailing, MobiusStyle.iconRowPadding)
        .padding(.vertical, MobiusSpace.s)
        .accessibilityElement(children: .contain)
    }
}

private func cronScheduleSummary(_ schedule: CronSchedule) -> String {
    switch schedule.kind {
    case .once:
        guard let at = schedule.at else { return "Once" }
        return "Once · \(Date(timeIntervalSince1970: TimeInterval(at)).formatted(date: .abbreviated, time: .shortened))"
    case .interval:
        let seconds = schedule.everySeconds ?? 0
        if seconds.isMultiple(of: 3_600) { return "Every \(seconds / 3_600) hour\(seconds == 3_600 ? "" : "s")" }
        if seconds.isMultiple(of: 60) {
            return "Every \(max(1, seconds / 60)) minute\(seconds == 60 ? "" : "s")"
        }
        return "Every \(seconds) seconds"
    case .cron:
        guard let parsed = simpleCronSchedule(schedule.expression ?? "") else {
            return "Custom schedule"
        }
        let timeZone = TimeZone(identifier: schedule.timeZone ?? "") ?? .current
        let formatter = DateFormatter()
        formatter.timeZone = timeZone
        formatter.timeStyle = .short
        let time = formatter.string(from: cronDate(for: parsed, timeZone: timeZone))
        guard let weekday = parsed.weekday else { return "Daily at \(time)" }
        return "Every \(Calendar.current.weekdaySymbols[weekday]) at \(time)"
    }
}
