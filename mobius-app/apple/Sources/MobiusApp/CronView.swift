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
                StatusBanner(
                    tone: .error,
                    title: .localized("Scheduled task rejected"),
                    detail: .verbatim(error)
                )
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
                let runs = namedRuns
                if runs.isEmpty {
                    Text("No scheduled runs yet.").foregroundStyle(palette.muted)
                } else {
                    ForEach(runs, id: \.run.id) { entry in
                        CronRunRow(
                            run: entry.run,
                            taskName: entry.name,
                            open: { model.presentCronRun(entry.run) }
                        )
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

    /// A run whose task is gone — deleted, or still loading — has no name to show, so it
    /// drops out. Resolving before the branch keeps the placeholder honest when that empties
    /// the section; testing `cronRuns` directly left a bare header with no rows under it.
    private var namedRuns: [(run: CronRun, name: String)] {
        model.cronRuns.compactMap { run in
            model.cronTasks.first { $0.id == run.taskId }.map { (run, $0.task) }
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
            let path = source.sessionContext.workspaceLabel
            let name = path.map { path in
                let component = URL(fileURLWithPath: path).lastPathComponent
                return component.isEmpty ? path : component
            }
            return CronProject(
                id: id,
                sourceSessionID: source.sessionId,
                name: name.map(MobiusText.verbatim) ?? .localized("Workspace"),
                sortName: name ?? ""
            )
        }
        .sorted { $0.sortName.localizedCaseInsensitiveCompare($1.sortName) == .orderedAscending }
    }

    private func projectName(for task: CronTask) -> MobiusText {
        if let name = projectOptions.first(where: {
            $0.sourceSessionID == task.sourceSessionId
        })?.name {
            return name
        }
        if let path = model.sessions.first(where: { $0.sessionId == task.sourceSessionId })?
            .sessionContext.workspaceLabel {
            return .verbatim(URL(fileURLWithPath: path).lastPathComponent)
        }
        return .localized("Workspace")
    }
}

private struct CronProject: Identifiable {
    let id: String
    let sourceSessionID: String
    let name: MobiusText
    let sortName: String
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
    @State private var confirmsDeletion = false
    let task: CronTask
    let projectName: MobiusText
    let edit: () -> Void

    var body: some View {
        let status = taskStatus
        let schedule = cronScheduleSummary(task.schedule)
        let nextRun = nextRunText

        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            HStack(alignment: .firstTextBaseline, spacing: MobiusSpace.s) {
                MobiusIcon(status.glyph, size: MobiusStyle.glyphInline, foreground: status.color)
                Spacer(minLength: MobiusSpace.s)
                Text(schedule)
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }

            CollapsibleText(text: task.task, collapsedLineLimit: 3)
                .font(MobiusStyle.bodyFont.weight(.semibold))

            HStack(alignment: .firstTextBaseline, spacing: MobiusSpace.s) {
                projectName.text
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: MobiusSpace.s)
                nextRunLabel
                    .lineLimit(1)
            }
            .font(MobiusStyle.metadataFont)
            .foregroundStyle(palette.muted)
        }
        .contentShape(Rectangle())
        .accessibilityElement(children: .contain)
        .accessibilityLabel(Text("\(Text(status.label)) scheduled task: \(task.task)"))
        .accessibilityValue(
            Text("\(Text(schedule)); workspace \(projectName.text); \(Text(nextRun))")
        )
        .mobiusSwipeActions {
            MobiusSwipeAction(title: "Delete", glyph: .trash, tone: "error") {
                confirmsDeletion = true
            }
            MobiusSwipeAction(title: "Edit", glyph: .pencilSimple, action: edit)
            MobiusSwipeAction(
                title: task.enabled ? "Pause" : "Resume",
                glyph: task.enabled ? .stopFill : .playFill,
                tone: task.enabled ? "warning" : "success",
                action: toggleEnabled
            )
            MobiusSwipeAction(title: "Run", glyph: .playFill, tone: "success") {
                model.runCron(task)
            }
        }
        .alert("Delete this scheduled task?", isPresented: $confirmsDeletion) {
            Button("Delete", role: .destructive) { model.deleteCron(task) }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This removes the task and its run history. This cannot be undone.")
        }
    }

    private var taskStatus: (
        label: LocalizedStringResource,
        glyph: MobiusGlyph,
        color: Color
    ) {
        if task.finished { return ("Finished", .checkCircle, palette.muted) }
        if task.enabled { return ("Active", .playFill, palette.signal) }
        return ("Paused", .stopFill, palette.warning)
    }

    private var nextRunText: LocalizedStringResource {
        guard let nextRunAt = task.nextRunAt else { return "Next —" }
        let date = Date(timeIntervalSince1970: TimeInterval(nextRunAt))
        return "Next \(date, format: .relative(presentation: .numeric, unitsStyle: .abbreviated))"
    }

    @ViewBuilder
    private var nextRunLabel: some View {
        Text(nextRunText)
    }

    private func toggleEnabled() {
        model.updateCron(
            task,
            sourceSessionID: task.sourceSessionId,
            instructions: task.task,
            schedule: task.schedule,
            endsAt: task.endsAt,
            enabled: !task.enabled
        )
    }
}

private struct CronRunRow: View {
    @Environment(\.mobiusPalette) private var palette
    let run: CronRun
    let taskName: String
    let open: () -> Void

    var body: some View {
        let status = cronRunStatusLabel(run.status)
        Button(action: open) {
            HStack(alignment: .top, spacing: MobiusSpace.m) {
                Circle().fill(statusColor).frame(width: 9, height: 9).padding(.top, MobiusSpace.xs)
                VStack(alignment: .leading, spacing: MobiusSpace.xs) {
                    HStack {
                        Text(verbatim: taskName)
                            .font(MobiusStyle.bodyFont.weight(.semibold))
                            .lineLimit(1)
                        Spacer(minLength: MobiusSpace.s)
                        Text(status)
                            .font(MobiusStyle.metadataFont.weight(.bold))
                            .foregroundStyle(statusColor)
                    }
                    Text(Date(timeIntervalSince1970: TimeInterval(run.startedAt)), style: .relative)
                        .font(MobiusStyle.metadataFont)
                        .foregroundStyle(palette.muted)
                    if let message = run.message {
                        Text(verbatim: message)
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
                ? Text("\(Text(status)) run for \(taskName) has no transcript")
                : Text("Open \(Text(status)) run for \(taskName)")
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

private func cronRunStatusLabel(_ status: CronRunStatus) -> LocalizedStringResource {
    switch status {
    case .succeeded: "Succeeded"
    case .failed: "Failed"
    case .running: "Running"
    case .skipped: "Skipped"
    }
}

private enum CronScheduleMode: String, CaseIterable, Identifiable {
    case once, interval, daily, weekly, advanced
    var id: Self { self }
    var title: LocalizedStringResource {
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
    var title: LocalizedStringResource {
        switch self {
        case .seconds: "Seconds"
        case .minutes: "Minutes"
        case .hours: "Hours"
        }
    }
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
    var title: LocalizedStringResource {
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
    var title: LocalizedStringResource {
        switch self {
        case .minutes: "Minutes"
        case .hours: "Hours"
        case .days: "Days"
        }
    }
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
    @Environment(\.locale) private var locale
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
            PageScaffold(
                title: .localized(editorTitle),
                detail: summary,
                showsBackdrop: false
            ) {
                Section("Workspace") {
                    Picker("Workspace", selection: $sourceSessionID) {
                        ForEach(projects) { project in
                            project.name.text.tag(project.sourceSessionID)
                        }
                    }
                }

                Section("Task") {
                    TextField("Describe what möbius should do", text: $instructions, axis: .vertical)
                        .font(MobiusStyle.bodyFont)
                        .lineLimit(4...8)
                        .textFieldStyle(.plain)
                        .textInputAutocapitalization(.sentences)
                        .labelsHidden()
                        .accessibilityLabel("Task")
                        .promptCard()
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
                            Stepper(value: $durationValue, in: 1...365) {
                                Text(durationSummary)
                            }
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
            }
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: dismiss.callAsFunction)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button(action: save) { Text(editorActionTitle) }
                        .disabled(!canSave)
                }
            }
        }
        .mobiusSheet()
    }

    @ViewBuilder
    private var scheduleControls: some View {
        switch mode {
        case .once:
            DatePicker("Run at", selection: $onceDate, in: Date.now..., displayedComponents: [.date, .hourAndMinute])
        case .interval:
            Stepper(
                value: $intervalValue,
                in: (intervalUnit == .seconds ? 60 : 1)...365
            ) { Text(intervalSummary) }
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
                    Text(verbatim: weekdayName(day)).tag(day)
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

    private var editorTitle: LocalizedStringResource {
        task == nil ? "New scheduled task" : "Edit scheduled task"
    }

    private var editorActionTitle: LocalizedStringResource {
        task == nil ? "Create" : "Save"
    }

    private var summary: MobiusText {
        guard let schedule else { return .localized("Choose a valid schedule.") }
        let scheduleResource = cronScheduleSummary(schedule)
        guard let endsAt else { return .localized(scheduleResource) }
        let scheduleText = MobiusText.localized(scheduleResource).resolved(locale: locale)
        let date = Date(timeIntervalSince1970: TimeInterval(endsAt))
        return .localized("\(scheduleText) · ends \(date, format: .dateTime.month(.abbreviated).day().hour().minute())")
    }

    private var intervalSummary: LocalizedStringResource {
        switch (intervalUnit, intervalValue) {
        case (.seconds, 1): "Every 1 second"
        case (.seconds, _): "Every \(intervalValue) seconds"
        case (.minutes, 1): "Every 1 minute"
        case (.minutes, _): "Every \(intervalValue) minutes"
        case (.hours, 1): "Every 1 hour"
        case (.hours, _): "Every \(intervalValue) hours"
        }
    }

    private var durationSummary: LocalizedStringResource {
        switch (durationUnit, durationValue) {
        case (.minutes, 1): "After 1 minute"
        case (.minutes, _): "After \(durationValue) minutes"
        case (.hours, 1): "After 1 hour"
        case (.hours, _): "After \(durationValue) hours"
        case (.days, 1): "After 1 day"
        case (.days, _): "After \(durationValue) days"
        }
    }

    private func weekdayName(_ day: Int) -> String {
        var calendar = Calendar.current
        calendar.locale = locale
        return calendar.weekdaySymbols[day - 1]
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
    @Environment(\.locale) private var locale

    @ViewBuilder
    var body: some View {
        if let error = model.cronRunPreviewError {
            VStack(spacing: 0) {
                header
                StatusBanner(
                    tone: .error,
                    title: .localized("Run transcript unavailable"),
                    detail: .verbatim(error)
                )
                    .padding(MobiusSpace.l)
                Spacer(minLength: 0)
            }
            .mobiusSheet()
        } else if model.cronRunPreview != nil {
            ReadOnlyTranscriptSheet(
                entries: model.cronRunPreviewEntries,
                fileSessionID: model.presentedCronRun?.sessionId,
                hasEarlier: model.cronRunPreviewNextBeforeSequence != nil,
                isLoading: model.isLoadingCronRunPreview,
                isRunning: model.presentedCronRun?.status == .running,
                loadEarlier: model.loadEarlierCronRunPreview,
                header: { header }
            )
        } else {
            ProgressView()
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .mobiusSheet()
        }
    }

    private var header: some View {
        HStack(spacing: MobiusSpace.s) {
            VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                if let task = model.cronRunPreview?.task {
                    Text(verbatim: task.task)
                        .font(MobiusStyle.controlFont.weight(.semibold))
                        .lineLimit(1)
                }
                if let run = model.cronRunPreview?.run ?? model.presentedCronRun {
                    Text("Run · \(Text(cronRunStatusLabel(run.status)))")
                        .font(MobiusStyle.metadataFont)
                        .foregroundStyle(palette.muted)
                }
            }
            Spacer(minLength: 0)
            SettingsInfoButton(
                title: .localized("Run transcript"),
                detail: runTranscriptDetail,
                glyph: .info
            )
        }
        .frame(maxWidth: .infinity, minHeight: MobiusStyle.iconButtonSize, alignment: .leading)
        .padding(.leading, MobiusSpace.l)
        .padding(.trailing, MobiusStyle.iconRowPadding)
        .padding(.vertical, MobiusSpace.s)
        .accessibilityElement(children: .contain)
    }

    private var runTranscriptDetail: MobiusText {
        guard let preview = model.cronRunPreview else {
            return .localized("Read-only run transcript")
        }
        let schedule = MobiusText.localized(cronScheduleSummary(preview.task.schedule))
            .resolved(locale: locale)
        return .localized("\(schedule) · read-only run transcript")
    }
}

private func cronScheduleSummary(_ schedule: CronSchedule) -> LocalizedStringResource {
    switch schedule.kind {
    case .once:
        guard let at = schedule.at else { return "Once" }
        let date = Date(timeIntervalSince1970: TimeInterval(at))
        return "Once · \(date, format: .dateTime.month(.abbreviated).day().hour().minute())"
    case .interval:
        let seconds = schedule.everySeconds ?? 0
        if seconds == 3_600 { return "Every 1 hour" }
        if seconds.isMultiple(of: 3_600) { return "Every \(seconds / 3_600) hours" }
        if seconds == 60 { return "Every 1 minute" }
        if seconds.isMultiple(of: 60) {
            return "Every \(max(1, seconds / 60)) minutes"
        }
        if seconds == 1 { return "Every 1 second" }
        return "Every \(seconds) seconds"
    case .cron:
        guard let parsed = simpleCronSchedule(schedule.expression ?? "") else {
            return "Custom schedule"
        }
        let timeZone = TimeZone(identifier: schedule.timeZone ?? "") ?? .current
        let date = cronDate(for: parsed, timeZone: timeZone)
        var timeStyle = Date.FormatStyle(date: .omitted, time: .shortened)
        timeStyle.timeZone = timeZone
        guard parsed.weekday != nil else {
            return "Daily at \(date, format: timeStyle)"
        }
        var weekdayStyle = Date.FormatStyle().weekday(.wide)
        weekdayStyle.timeZone = timeZone
        return "Every \(date, format: weekdayStyle) at \(date, format: timeStyle)"
    }
}
