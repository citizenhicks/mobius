import SwiftUI

struct RoutineWorkspace: Identifiable {
    var id: String { path }
    let path: String
    let name: String
}

enum RoutineEditorTarget: Identifiable {
    case create(String)
    case edit(Routine)

    var id: String {
        switch self {
        case .create(let botID): "create:\(botID)"
        case .edit(let routine): "edit:\(routine.id)"
        }
    }

    var botID: String {
        switch self {
        case .create(let botID): botID
        case .edit(let routine): routine.botId
        }
    }

    var routine: Routine? {
        if case .edit(let routine) = self { return routine }
        return nil
    }
}

struct RoutineRow: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var confirmsDeletion = false
    let routine: Routine
    let edit: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.xs) {
            HStack(spacing: MobiusSpace.s) {
                MobiusIcon(statusGlyph, foreground: statusColor)
                Text(routineScheduleSummary(routine.schedule))
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
                    .lineLimit(1)
                Spacer(minLength: 0)
                Text(nextRunText)
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
                    .lineLimit(1)
            }
            CollapsibleText(text: routine.instructions, collapsedLineLimit: 3)
                .font(MobiusStyle.bodyFont.weight(.semibold))
            Text(verbatim: routine.workspace)
                .font(MobiusStyle.captionFont)
                .foregroundStyle(palette.muted)
                .lineLimit(1)
                .truncationMode(.middle)
        }
        .accessibilityElement(children: .combine)
        .mobiusSwipeActions {
            MobiusSwipeAction(title: "Delete", glyph: .trash, tone: "error") {
                confirmsDeletion = true
            }
            MobiusSwipeAction(title: "Edit", glyph: .pencilSimple, action: edit)
            MobiusSwipeAction(
                title: routine.enabled ? "Pause" : "Resume",
                glyph: routine.enabled ? .stopFill : .playFill,
                tone: routine.enabled ? "warning" : "success",
                action: toggleEnabled
            )
            MobiusSwipeAction(title: "Run", glyph: .playFill, tone: "success") {
                model.runRoutine(routine)
            }
        }
        .alert("Delete this routine?", isPresented: $confirmsDeletion) {
            Button("Delete", role: .destructive) { model.deleteRoutine(routine) }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This removes the routine and its run history. This cannot be undone.")
        }
    }

    private var statusGlyph: MobiusGlyph {
        if routine.finished { return .checkCircle }
        return routine.enabled ? .playFill : .stopFill
    }

    private var statusColor: Color {
        if routine.finished { return palette.muted }
        return routine.enabled ? palette.signal : palette.warning
    }

    private var nextRunText: LocalizedStringResource {
        guard let nextRunAt = routine.nextRunAt else { return "Next —" }
        let date = Date(timeIntervalSince1970: TimeInterval(nextRunAt))
        return "Next \(date, format: .relative(presentation: .numeric, unitsStyle: .abbreviated))"
    }

    private func toggleEnabled() {
        model.updateRoutine(
            routine,
            botID: routine.botId,
            workspace: routine.workspace,
            instructions: routine.instructions,
            schedule: routine.schedule,
            endsAt: routine.endsAt,
            enabled: !routine.enabled
        )
    }
}

struct RoutineRunRow: View {
    @Environment(\.mobiusPalette) private var palette
    @State private var confirmsDeletion = false
    let run: RoutineRun
    let name: String
    let awaitsApproval: Bool
    let open: () -> Void
    let delete: () -> Void

    var body: some View {
        Button(action: open) {
            HStack(alignment: .top, spacing: MobiusSpace.m) {
                Circle()
                    .fill(statusColor)
                    .frame(width: 9, height: 9)
                    .padding(.top, MobiusSpace.xs)
                VStack(alignment: .leading, spacing: MobiusSpace.xs) {
                    MobiusTitleText(verbatim: name)
                        .lineLimit(1)
                    Text(Date(timeIntervalSince1970: TimeInterval(run.startedAt)), style: .relative)
                        .font(MobiusStyle.metadataFont)
                        .foregroundStyle(palette.muted)
                    if let message = run.message {
                        Text(verbatim: message)
                            .font(MobiusStyle.bodyFont)
                            .foregroundStyle(palette.muted)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                if awaitsApproval {
                    MobiusIcon(
                        .bellDot,
                        size: MobiusStyle.glyphMark,
                        foreground: palette.warning
                    )
                    .accessibilityHidden(true)
                }
                Text(routineRunStatusLabel(run.status))
                    .font(MobiusStyle.metadataFont.weight(.bold))
                    .foregroundStyle(statusColor)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.mobiusPlain)
        .disabled(run.sessionId == nil)
        .accessibilityValue(awaitsApproval ? "Awaiting approval" : "")
        .accessibilityHint(
            run.sessionId == nil
                ? "No transcript"
                : awaitsApproval ? "Opens approval" : "Opens run transcript"
        )
        .mobiusSwipeActions {
            MobiusSwipeAction(
                title: "Delete",
                glyph: .trash,
                tone: "error",
                isEnabled: run.status != .running
            ) {
                confirmsDeletion = true
            }
        }
        .alert("Delete this routine run?", isPresented: $confirmsDeletion) {
            Button("Delete", role: .destructive, action: delete)
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This removes the run history and its conversation transcript.")
        }
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

private enum RoutineScheduleMode: String, CaseIterable, Identifiable {
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

private enum RoutineIntervalUnit: String, CaseIterable, Identifiable {
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

private enum RoutineEndMode: String, CaseIterable, Identifiable {
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

private enum RoutineDurationUnit: String, CaseIterable, Identifiable {
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

private func routineDate(
    for schedule: SimpleRoutineSchedule,
    timeZone: TimeZone
) -> Date {
    var calendar = Calendar.current
    calendar.timeZone = timeZone
    return calendar.date(
        bySettingHour: schedule.hour,
        minute: schedule.minute,
        second: 0,
        of: .now
    ) ?? .now
}

struct RoutineEditorSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.locale) private var locale

    let botID: String
    let routine: Routine?
    let workspaces: [RoutineWorkspace]

    @State private var workspace: String
    @State private var instructions: String
    @State private var mode: RoutineScheduleMode
    @State private var onceDate: Date
    @State private var intervalValue: Int
    @State private var intervalUnit: RoutineIntervalUnit
    @State private var cronExpression: String
    @State private var dailyTime: Date
    @State private var weeklyTime: Date
    @State private var weekday: Int
    @State private var timeZoneIdentifier: String
    @State private var endMode: RoutineEndMode
    @State private var durationValue: Int
    @State private var durationUnit: RoutineDurationUnit
    @State private var endDate: Date
    @State private var enabled: Bool

    init(botID: String, routine: Routine?, workspaces: [RoutineWorkspace]) {
        self.botID = botID
        self.routine = routine
        self.workspaces = workspaces

        let schedule = routine?.schedule
        let parsedCron = schedule?.expression.flatMap(simpleRoutineSchedule)
        let initialMode: RoutineScheduleMode = switch schedule?.kind {
        case .once: .once
        case .interval: .interval
        case .cron: parsedCron.map { $0.weekday == nil ? .daily : .weekly } ?? .advanced
        case nil: .once
        }
        let initialDate = Date(
            timeIntervalSince1970: TimeInterval(
                schedule?.at ?? Int64(Date.now.timeIntervalSince1970 + 3_600)
            )
        )
        let scheduleTimeZone = TimeZone(identifier: schedule?.timeZone ?? "") ?? .current
        let initialCronDate = parsedCron.map {
            routineDate(for: $0, timeZone: scheduleTimeZone)
        } ?? initialDate
        let seconds = schedule?.everySeconds ?? 600
        let initialUnit: RoutineIntervalUnit = if seconds.isMultiple(of: 3_600) {
            .hours
        } else if seconds.isMultiple(of: 60) {
            .minutes
        } else {
            .seconds
        }

        _workspace = State(initialValue: routine?.workspace ?? workspaces.first?.path ?? "")
        _instructions = State(initialValue: routine?.instructions ?? "")
        _mode = State(initialValue: initialMode)
        _onceDate = State(initialValue: initialDate)
        _intervalValue = State(initialValue: max(1, Int(seconds / initialUnit.seconds)))
        _intervalUnit = State(initialValue: initialUnit)
        _cronExpression = State(initialValue: schedule?.expression ?? "")
        _dailyTime = State(initialValue: initialCronDate)
        _weeklyTime = State(initialValue: initialCronDate)
        _weekday = State(initialValue: (parsedCron?.weekday ?? 1) + 1)
        _timeZoneIdentifier = State(initialValue: scheduleTimeZone.identifier)
        _endMode = State(initialValue: routine?.endsAt == nil ? .never : .date)
        _durationValue = State(initialValue: 1)
        _durationUnit = State(initialValue: .hours)
        _endDate = State(initialValue: Date(
            timeIntervalSince1970: TimeInterval(
                routine?.endsAt ?? Int64(Date.now.timeIntervalSince1970 + 86_400)
            )
        ))
        _enabled = State(initialValue: routine?.enabled ?? true)
    }

    var body: some View {
        NavigationStack {
            PageScaffold(
                title: .localized(routine == nil ? "New routine" : "Edit routine"),
                detail: summary,
                showsBackdrop: false
            ) {
                if asksForApproval {
                    StatusBanner(
                        tone: .warning,
                        title: "Routine may pause",
                        detail: "This Bot uses Ask. Approval-required actions will wait for you before the routine can continue."
                    )
                    .settingsStandaloneRow()
                }

                Section("Workspace") {
                    Picker("Workspace", selection: $workspace) {
                        ForEach(workspaces) { item in
                            Text(verbatim: item.name).tag(item.path)
                        }
                    }
                }

                Section("Task") {
                    TextField(
                        "Describe what this Bot should do",
                        text: $instructions,
                        axis: .vertical
                    )
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
                        ForEach(RoutineScheduleMode.allCases) { mode in
                            Text(mode.title).tag(mode)
                        }
                    }
                    scheduleControls
                    if mode == .daily || mode == .weekly || mode == .advanced {
                        Picker("Time zone", selection: $timeZoneIdentifier) {
                            ForEach(TimeZone.knownTimeZoneIdentifiers, id: \.self) { identifier in
                                Text(verbatim: identifier).tag(identifier)
                            }
                        }
                    }
                }

                if mode != .once {
                    Section("End") {
                        Picker("Ends", selection: $endMode) {
                            ForEach(RoutineEndMode.allCases) { mode in
                                Text(mode.title).tag(mode)
                            }
                        }
                        if endMode == .duration {
                            Stepper(value: $durationValue, in: 1...365) {
                                Text(durationSummary)
                            }
                            Picker("Unit", selection: $durationUnit) {
                                ForEach(RoutineDurationUnit.allCases) { unit in
                                    Text(unit.title).tag(unit)
                                }
                            }
                        } else if endMode == .date {
                            DatePicker(
                                "Date",
                                selection: $endDate,
                                in: Date.now...,
                                displayedComponents: [.date, .hourAndMinute]
                            )
                        }
                    }
                }

                if routine != nil {
                    Section { Toggle("Enabled", isOn: $enabled) }
                }
            }
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: dismiss.callAsFunction)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button(routine == nil ? "Create" : "Save", action: save)
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
            DatePicker(
                "Run at",
                selection: $onceDate,
                in: Date.now...,
                displayedComponents: [.date, .hourAndMinute]
            )
        case .interval:
            Stepper(
                value: $intervalValue,
                in: (intervalUnit == .seconds ? 60 : 1)...365
            ) {
                Text(intervalSummary)
            }
            Picker("Unit", selection: $intervalUnit) {
                ForEach(RoutineIntervalUnit.allCases) { unit in
                    Text(unit.title).tag(unit)
                }
            }
            .onChange(of: intervalUnit) { _, unit in
                if unit == .seconds { intervalValue = max(intervalValue, 60) }
            }
        case .daily:
            DatePicker("Time", selection: $dailyTime, displayedComponents: [.hourAndMinute])
                .environment(\.timeZone, selectedTimeZone)
        case .weekly:
            Picker("Day", selection: $weekday) {
                ForEach(1...7, id: \.self) { day in
                    Text(verbatim: weekdayName(day)).tag(day)
                }
            }
            DatePicker("Time", selection: $weeklyTime, displayedComponents: [.hourAndMinute])
                .environment(\.timeZone, selectedTimeZone)
        case .advanced:
            TextField("0 9 * * 1-5", text: $cronExpression)
                .font(MobiusStyle.bodyFont.monospaced())
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
        }
    }

    private var selectedTimeZone: TimeZone {
        TimeZone(identifier: timeZoneIdentifier) ?? .current
    }

    private var schedule: RoutineSchedule? {
        switch mode {
        case .once:
            return .once(at: Int64(onceDate.timeIntervalSince1970))
        case .interval:
            return .interval(seconds: Int64(intervalValue) * intervalUnit.seconds)
        case .daily:
            return .cron(
                expression(for: dailyTime, weekday: nil),
                timeZone: selectedTimeZone.identifier
            )
        case .weekly:
            return .cron(
                expression(for: weeklyTime, weekday: weekday - 1),
                timeZone: selectedTimeZone.identifier
            )
        case .advanced:
            let expression = cronExpression.trimmingCharacters(in: .whitespacesAndNewlines)
            return expression.isEmpty
                ? nil
                : .cron(expression, timeZone: selectedTimeZone.identifier)
        }
    }

    private var endsAt: Int64? {
        guard mode != .once else { return nil }
        return switch endMode {
        case .never: nil
        case .duration:
            Int64(
                Date.now.timeIntervalSince1970
                    + Double(durationValue) * durationUnit.seconds
            )
        case .date:
            Int64(endDate.timeIntervalSince1970)
        }
    }

    private var canSave: Bool {
        !workspace.isEmpty
            && !instructions.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && schedule != nil
            && (mode != .interval || (schedule?.everySeconds ?? 0) >= 60)
            && (endsAt == nil || endsAt! > Int64(Date.now.timeIntervalSince1970))
    }

    private var asksForApproval: Bool {
        model.bots.first { $0.id == botID }?
            .config.config.middleware.settings["sandbox"]?["approval_policy"] == .string("ask")
    }

    private var summary: MobiusText {
        guard let schedule else { return .localized("Choose a valid schedule.") }
        let scheduleResource = routineScheduleSummary(schedule)
        guard let endsAt else { return .localized(scheduleResource) }
        let scheduleText = MobiusText.localized(scheduleResource).resolved(locale: locale)
        let date = Date(timeIntervalSince1970: TimeInterval(endsAt))
        return .localized(
            "\(scheduleText) · ends \(date, format: .dateTime.month(.abbreviated).day().hour().minute())"
        )
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

    private func expression(for date: Date, weekday: Int?) -> String {
        var calendar = Calendar.current
        calendar.timeZone = selectedTimeZone
        let components = calendar.dateComponents([.hour, .minute], from: date)
        let minute = components.minute ?? 0
        let hour = components.hour ?? 0
        return weekday.map { "\(minute) \(hour) * * \($0)" }
            ?? "\(minute) \(hour) * * *"
    }

    private func save() {
        guard let schedule else { return }
        if let routine {
            model.updateRoutine(
                routine,
                botID: botID,
                workspace: workspace,
                instructions: instructions,
                schedule: schedule,
                endsAt: endsAt,
                enabled: enabled
            )
        } else {
            model.createRoutine(
                botID: botID,
                workspace: workspace,
                instructions: instructions,
                schedule: schedule,
                endsAt: endsAt
            )
        }
        dismiss()
    }
}

struct RoutineRunTranscriptSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette

    @ViewBuilder
    var body: some View {
        if let error = model.routineRunPreviewError {
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
        } else if model.routineRunPreview != nil {
            ReadOnlyTranscriptSheet(
                entries: model.routineRunPreviewEntries,
                fileSessionID: model.presentedRoutineRun?.sessionId,
                hasEarlier: model.routineRunPreviewNextBeforeSequence != nil,
                isLoading: model.isLoadingRoutineRunPreview,
                isRunning: model.presentedRoutineRun?.status == .running,
                loadEarlier: model.loadEarlierRoutineRunPreview,
                header: { header }
            )
        } else {
            ProgressView()
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .mobiusSheet()
        }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
            if let routine = model.routineRunPreview?.routine {
                Text(verbatim: routine.instructions)
                    .font(MobiusStyle.controlFont.weight(.semibold))
                    .lineLimit(1)
            }
            if let run = model.routineRunPreview?.run ?? model.presentedRoutineRun {
                Text("Run · \(Text(routineRunStatusLabel(run.status)))")
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
            }
        }
        .frame(maxWidth: .infinity, minHeight: MobiusStyle.iconButtonSize, alignment: .leading)
        .padding(.horizontal, MobiusSpace.l)
        .padding(.vertical, MobiusSpace.s)
    }
}

private func routineRunStatusLabel(_ status: RoutineRunStatus) -> LocalizedStringResource {
    switch status {
    case .succeeded: "Succeeded"
    case .failed: "Failed"
    case .running: "Running"
    case .skipped: "Skipped"
    }
}

private func routineScheduleSummary(_ schedule: RoutineSchedule) -> LocalizedStringResource {
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
        if seconds.isMultiple(of: 60) { return "Every \(seconds / 60) minutes" }
        if seconds == 1 { return "Every 1 second" }
        return "Every \(seconds) seconds"
    case .cron:
        guard let parsed = simpleRoutineSchedule(schedule.expression ?? "") else {
            return "Custom schedule"
        }
        let timeZone = TimeZone(identifier: schedule.timeZone ?? "") ?? .current
        let date = routineDate(for: parsed, timeZone: timeZone)
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
