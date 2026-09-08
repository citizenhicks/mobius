import Foundation
import Observation

extension AppModel {
    @discardableResult
    func createRoutine(
        botID: String,
        workspace: String,
        instructions: String,
        schedule: RoutineSchedule,
        endsAt: Int64?
    ) -> String? {
        let instructions = instructions.trimmingCharacters(in: .whitespacesAndNewlines)
        guard gateway.connectionState.isReady, !botID.isEmpty, !workspace.isEmpty,
            !instructions.isEmpty
        else { return nil }
        let id = requestID("routine-create")
        routineRequestIDs.insert(id)
        routineError = nil
        gateway.transmit(
            .createRoutine(
                requestID: id,
                botID: botID,
                workspace: workspace,
                instructions: instructions,
                schedule: schedule,
                endsAt: endsAt
            )
        ) { [weak self] message in
            self?.routineRequestIDs.remove(id)
            self?.routineError = message
        }
        return id
    }

    @discardableResult
    func updateRoutine(
        _ routine: Routine,
        botID: String,
        workspace: String,
        instructions: String,
        schedule: RoutineSchedule,
        endsAt: Int64?,
        enabled: Bool
    ) -> String? {
        let instructions = instructions.trimmingCharacters(in: .whitespacesAndNewlines)
        guard gateway.connectionState.isReady, !botID.isEmpty, !workspace.isEmpty,
            !instructions.isEmpty
        else { return nil }
        let id = requestID("routine-update")
        routineRequestIDs.insert(id)
        routineError = nil
        gateway.transmit(
            .updateRoutine(
                requestID: id,
                id: routine.id,
                botID: botID,
                workspace: workspace,
                instructions: instructions,
                schedule: schedule,
                endsAt: endsAt,
                enabled: enabled
            )
        ) { [weak self] message in
            self?.routineRequestIDs.remove(id)
            self?.routineError = message
        }
        return id
    }

    func deleteRoutine(_ routine: Routine) {
        guard gateway.connectionState.isReady else { return }
        let id = requestID("routine-delete")
        routineRequestIDs.insert(id)
        gateway.transmit(.deleteRoutine(requestID: id, id: routine.id)) { [weak self] message in
            self?.routineRequestIDs.remove(id)
            self?.routineError = message
        }
    }

    func deleteRoutineRun(_ run: RoutineRun) {
        guard gateway.connectionState.isReady, run.status != .running else { return }
        let id = requestID("routine-run-delete")
        routineRequestIDs.insert(id)
        gateway.transmit(.deleteRoutineRun(requestID: id, id: run.id)) { [weak self] message in
            self?.routineRequestIDs.remove(id)
            self?.routineError = message
        }
    }

    func runRoutine(_ routine: Routine) {
        guard gateway.connectionState.isReady else { return }
        let id = requestID("routine-run")
        routineRequestIDs.insert(id)
        gateway.transmit(.runRoutine(requestID: id, id: routine.id)) { [weak self] message in
            self?.routineRequestIDs.remove(id)
            self?.routineError = message
        }
    }

    func refreshRoutines() {
        guard gateway.connectionState.isReady else { return }
        gateway.transmit(.listRoutines(requestID: requestID("routine-list"), botID: nil))
        gateway.transmit(.listRoutineHistory(requestID: requestID("routine-history"), id: nil))
    }

    func presentRoutineRun(_ run: RoutineRun) {
        chat.cancelSessionFileThumbnailDownloads()
        presentedRoutineRun = run
        routineRunPreview = nil
        routineRunPreviewEntries = []
        routineRunPreviewNextBeforeSequence = nil
        routineRunPreviewError = nil
        routineRunPreviewPollingTask?.cancel()
        routineRunPreviewPollingTask = nil
        loadRoutineRunPreview(runID: run.id)
        routineRunPreviewPollingTask = Task { [weak self] in
            // ponytail: poll only while a sheet is open; add push subscriptions if live viewers scale.
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(2))
                guard !Task.isCancelled, let self,
                    self.presentedRoutineRun?.id == run.id
                else { return }
                guard (self.routineRunPreview?.run.status ?? run.status) == .running else { return }
                self.loadRoutineRunPreview(runID: run.id)
            }
        }
    }

    func closeRoutineRunPreview() {
        chat.cancelSessionFileThumbnailDownloads()
        routineRunPreviewPollingTask?.cancel()
        routineRunPreviewPollingTask = nil
        routineRunPreviewRequestID = nil
        routineRunPreviewRequestBeforeSequence = nil
        presentedRoutineRun = nil
        routineRunPreview = nil
        routineRunPreviewEntries = []
        routineRunPreviewNextBeforeSequence = nil
        routineRunPreviewError = nil
        isLoadingRoutineRunPreview = false
    }

    func loadEarlierRoutineRunPreview() {
        guard let runID = presentedRoutineRun?.id,
            let beforeSequence = routineRunPreviewNextBeforeSequence
        else { return }
        loadRoutineRunPreview(runID: runID, beforeSequence: beforeSequence)
    }

    func loadEarlierRoutineRunPreviewAndWait() async {
        guard !Task.isCancelled, routineRunPreviewRequestID == nil else { return }
        loadEarlierRoutineRunPreview()
        guard routineRunPreviewRequestID != nil else { return }
        for await loading in Observations({ self.isLoadingRoutineRunPreview }) {
            if !loading { return }
        }
    }

    private func loadRoutineRunPreview(runID: String, beforeSequence: UInt64? = nil) {
        guard gateway.connectionState.isReady, routineRunPreviewRequestID == nil else { return }
        let id = requestID("routine-preview")
        routineRunPreviewRequestID = id
        routineRunPreviewRequestBeforeSequence = beforeSequence
        isLoadingRoutineRunPreview = routineRunPreview == nil || beforeSequence != nil
        gateway.transmit(
            .getRoutineRunPreview(
                requestID: id,
                id: runID,
                beforeSequence: beforeSequence
            )
        ) { [weak self] message in
            guard let self, self.routineRunPreviewRequestID == id else { return }
            self.routineRunPreviewRequestID = nil
            self.routineRunPreviewRequestBeforeSequence = nil
            self.isLoadingRoutineRunPreview = false
            self.routineRunPreviewError = message
        }
    }
}
