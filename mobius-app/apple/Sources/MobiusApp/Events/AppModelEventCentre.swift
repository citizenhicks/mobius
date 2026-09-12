import Foundation

extension AppModel {
    var eventCentreItems: [EventCentreItem] {
        (approvalEvents + extensionEvents + routineEvents).sorted {
            if $0.requiresAction != $1.requiresAction { return $0.requiresAction }
            if $0.occurredAt != $1.occurredAt { return $0.occurredAt > $1.occurredAt }
            return $0.id < $1.id
        }
    }

    func startEventCentreRefresh() {
        guard eventCentreRefreshTask == nil else { return }
        // ponytail: preflight failures have no session event; poll every 30s until the gateway pushes routine history.
        eventCentreRefreshTask = Task { [weak self] in
            while !Task.isCancelled {
                if self?.appIsInBackground == false, self?.isAppLocked == false {
                    self?.refreshRoutines()
                }
                try? await Task.sleep(for: .seconds(30))
            }
        }
    }

    var hasUnreadEvents: Bool { eventCentreItems.contains(where: isEventUnread) }

    func isEventUnread(_ event: EventCentreItem) -> Bool {
        eventReadRevisions[event.id] != event.revision
    }

    func markEventsRead(_ events: [EventCentreItem]) {
        guard let accountID = gateway.selectedAccountID else { return }
        for event in events { eventReadRevisions[event.id] = event.revision }
        settingsDefaults.set(
            eventReadRevisions, forKey: "event-centre-read-\(accountID.uuidString)")
    }

    func openEvent(_ event: EventCentreItem) {
        markEventsRead([event])
        openNotificationTarget(event.target)
    }

    private var approvalEvents: [EventCentreItem] {
        var events = backgroundApprovals.map { approval in
            approvalEvent(
                requestID: approval.requestId, sessionID: approval.sessionId,
                title: bots.first { $0.id == approval.botId }?.name ?? localizedString("Bot")
            )
        }
        for session in chat.sessions where session.activity.state == .awaitingApproval {
            guard let requestID = session.activity.approvalRequestId else { continue }
            events.append(
                approvalEvent(
                    requestID: requestID, sessionID: session.sessionId,
                    title: displayedTitle(for: session)
                ))
        }
        if let approval = chat.pendingApproval, let sessionID = chat.selectedSessionID {
            events.append(
                approvalEvent(
                    requestID: approval.id, sessionID: sessionID,
                    title: localizedString("Current chat")))
        }
        var seen = Set<String>()
        return events.filter { seen.insert($0.id).inserted }
    }

    private func approvalEvent(requestID: String, sessionID: String, title: String)
        -> EventCentreItem
    {
        EventCentreItem(
            id: "approval:\(requestID)", revision: requestID, title: title,
            detail: localizedString("Approval required"), glyph: .shieldCheck, tone: .warning,
            requiresAction: true, target: .session(sessionID)
        )
    }

    private var extensionEvents: [EventCentreItem] {
        extensions.filter(\.needsHookTrust).map { record in
            EventCentreItem(
                id: "extension:\(record.id)", revision: record.digest, title: record.name,
                detail: localizedString("Review untrusted hooks"), glyph: .squaresFour,
                tone: .warning, requiresAction: true, target: .extensionPackage(record.id)
            )
        }
    }

    private var routineEvents: [EventCentreItem] {
        routineRuns.filter { $0.status != .running }.map { run in
            let (status, glyph, tone): (LocalizedStringResource, MobiusGlyph, ToastTone) =
                switch run.status {
                case .succeeded: ("Routine succeeded", .checkCircle, .success)
                case .failed: ("Routine failed", .warning, .error)
                case .skipped: ("Routine skipped", .warning, .warning)
                case .running: ("Routine running", .playFill, .info)
                }
            return EventCentreItem(
                id: "routine:\(run.id)", revision: run.status.rawValue,
                title: bots.first { $0.id == run.botId }?.name ?? localizedString("Bot"),
                detail: [localizedString(status), run.message].compactMap { $0 }.joined(
                    separator: " · "),
                glyph: glyph, tone: tone,
                occurredAt: TimeInterval(run.finishedAt ?? run.startedAt),
                target: .routineRun(run.id)
            )
        }
    }

}
