import Foundation
import Observation

extension ChatSessionModel {
    func requestEarlierHistory() {
        guard canLoadEarlierHistory else { return }
        let window = transcriptWindow
        if window.hasEarlierEntries {
            transcriptWindowAnchor = .visibleTurns(window.turnCount + transcriptTurnsPerPage)
            _ = transcriptWindow
            historyLoadCompletionRevision &+= 1
            historyLoadSuccessRevision &+= 1
            return
        }
        guard let sessionID = selectedSessionID,
            let beforeSequence = nextHistoryBeforeSequence
        else { return }
        let id = requestID("history")
        historyRequestID = id
        isLoadingEarlierHistory = true
        transcriptWindowAnchor = .visibleTurns(window.turnCount)
        transcriptWindowCache = window
        gateway.transmit(
            .getSessionHistory(
                requestID: id,
                sessionID: sessionID,
                beforeSequence: beforeSequence
            )
        ) { [weak self] _ in
            guard self?.historyRequestID == id else { return }
            self?.finishHistoryLoad()
        }
    }

    func loadEarlierHistory() async {
        guard !Task.isCancelled, historyRequestID == nil else { return }
        let initialRevision = historyLoadCompletionRevision
        requestEarlierHistory()
        guard historyLoadCompletionRevision == initialRevision,
            historyRequestID != nil
        else { return }
        for await revision in Observations({ self.historyLoadCompletionRevision }) {
            if revision != initialRevision { return }
        }
    }
}
