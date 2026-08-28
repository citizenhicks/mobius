import Foundation

extension AppModel {
    /// Rebuilds record-owned presentation in sequence order because a replace/append pair
    /// can straddle history pages. The cached base already includes records at its cursor.
    func mergeHistory(_ records: [RecordedEvent]) {
        for record in records { transcriptRecords[record.sequence] = record }

        var earlier: [TranscriptEntry] = []
        var rebuilt = copiedTranscript(transcriptRecordBase)
        var earlierTurnState = TranscriptHistoryTurnState()
        var rebuiltTurnState = TranscriptHistoryTurnState(turnID: rebuilt.last?.turnID)
        let records = transcriptRecords.values.sorted { $0.sequence < $1.sequence }
        for record in records {
            if let baseSequence = transcriptRecordBaseSequence,
               record.sequence <= baseSequence {
                reduceHistory(
                    record,
                    into: &earlier,
                    turnState: &earlierTurnState
                )
            } else {
                reduceHistory(
                    record,
                    into: &rebuilt,
                    turnState: &rebuiltTurnState
                )
            }
        }
        let baseIDs = Set(transcriptRecordBase.map(\.id))
        let baseTargets = Set(transcriptRecordBase.compactMap(\.messageTarget))
        earlier.removeAll {
            baseIDs.contains($0.id)
                || $0.messageTarget.map(baseTargets.contains) == true
        }
        rebuilt.insert(contentsOf: earlier, at: 0)
        transcript = rebuilt
    }

    func copiedTranscript(_ entries: [TranscriptEntry]) -> [TranscriptEntry] {
        entries.map { entry in
            TranscriptEntry(
                id: entry.id,
                presentationID: entry.presentationID,
                text: entry.text,
                kind: entry.kind,
                capability: entry.capability,
                role: entry.role,
                update: entry.update,
                title: entry.title,
                symbol: entry.symbol,
                group: entry.group,
                format: entry.format,
                tone: entry.tone,
                pending: entry.pending,
                modelStepID: entry.modelStepID,
                turnID: entry.turnID,
                startsTurn: entry.startsTurn,
                turnTerminal: entry.turnTerminal,
                turnElapsedMs: entry.turnElapsedMs,
                sourceSequence: entry.sourceSequence,
                recordedAtMs: entry.recordedAtMs,
                messageTarget: entry.messageTarget,
                files: entry.files
            )
        }
    }

    func reduceHistory(
        _ record: RecordedEvent,
        into entries: inout [TranscriptEntry],
        turnState: inout TranscriptHistoryTurnState,
        recordID: String? = nil
    ) {
        let event = record.event.msg
        let type = event["type"]?.stringValue ?? "unknown"
        let turnID = historyTurnID(
            for: type,
            event: event,
            entries: &entries,
            turnState: &turnState
        )
        let entryStart = entries.count
        defer {
            if turnID == nil,
               entries.count > entryStart,
               turnState.unassignedEntryStart == nil {
                turnState.unassignedEntryStart = entryStart
            }
        }
        for (index, block) in record.blocks.enumerated() {
            apply(
                block,
                sequence: record.sequence,
                blockIndex: index,
                recordedAtMs: record.recordedAtMs,
                turnID: turnID,
                recordID: recordID,
                to: &entries
            )
        }

        switch type {
        case "user_message":
            let startsTurn = turnID != nil && turnState.awaitingInitialUserTurnID == turnID
            if startsTurn { turnState.awaitingInitialUserTurnID = nil }
            let attachments = event["attachments"]?.arrayValue?.compactMap {
                try? SessionFileReference(json: $0)
            } ?? []
            appendText(
                event["message"]?.stringValue,
                kind: .user,
                id: "event:\(recordID ?? String(record.sequence)):user",
                turnID: turnID,
                startsTurn: startsTurn,
                sourceSequence: record.sequence,
                recordedAtMs: record.recordedAtMs,
                messageTarget: messageTarget(from: event),
                files: attachments,
                to: &entries
            )
        case "peer_message":
            appendPeerMessage(event, record: record, to: &entries)
        case "agent_message_content_delta", "agent_reasoning_content_delta":
            reduceHistoryDelta(
                type: type,
                event: event,
                record: record,
                turnID: turnID,
                entries: &entries
            )
        case "model_step_completed":
            applyModelStepCompletion(event, turnID: turnID, record: record, to: &entries)
        case "agent_message":
            let kind: TranscriptEntry.Kind = event["phase"]?.stringValue == "commentary"
                ? .commentary
                : .assistant
            if let modelStepID = event["modelStepId"]?.stringValue,
               let index = entries.lastIndex(where: {
                   $0.modelStepID == modelStepID && $0.kind == kind && !$0.pending
               }) {
                if entries[index].turnID == nil { entries[index].turnID = turnID }
                entries[index].messageTarget = messageTarget(from: event)
            } else {
                completeStream(
                    text: event["message"]?.stringValue ?? "",
                    kind: kind,
                    modelStepID: event["modelStepId"]?.stringValue,
                    turnID: turnID,
                    messageTarget: messageTarget(from: event),
                    sourceSequence: record.sequence,
                    recordedAtMs: record.recordedAtMs,
                    in: &entries
                )
            }
        case "task_complete":
            finishHistoryTurn(record, turnID: turnID, aborted: false, entries: &entries)
            turnState = TranscriptHistoryTurnState()
        case "turn_aborted":
            finishHistoryTurn(record, turnID: turnID, aborted: true, entries: &entries)
            turnState = TranscriptHistoryTurnState()
        default:
            break
        }
    }

    private func historyTurnID(
        for type: String,
        event: JSONValue,
        entries: inout [TranscriptEntry],
        turnState: inout TranscriptHistoryTurnState
    ) -> String? {
        let explicitTurnID = event["turnId"]?.stringValue
        if type == "task_started" {
            turnState = TranscriptHistoryTurnState(
                turnID: explicitTurnID,
                awaitingInitialUserTurnID: explicitTurnID
            )
        } else if let explicitTurnID {
            if turnState.turnID == nil,
               let start = turnState.unassignedEntryStart,
               start < entries.count {
                for index in start..<entries.count where entries[index].turnID == nil {
                    entries[index].turnID = explicitTurnID
                }
                if let firstUser = entries[start...].firstIndex(where: {
                    $0.kind == .user && !$0.startsTurn
                }) {
                    entries[firstUser].startsTurn = true
                }
            }
            turnState.turnID = explicitTurnID
            turnState.unassignedEntryStart = nil
        }
        return explicitTurnID ?? turnState.turnID
    }

    private func reduceHistoryDelta(
        type: String,
        event: JSONValue,
        record: RecordedEvent,
        turnID: String?,
        entries: inout [TranscriptEntry]
    ) {
        guard let modelStepID = event["modelStepId"]?.stringValue else { return }
        let reasoning = type == "agent_reasoning_content_delta"
        let commentary = event["phase"]?.stringValue == "commentary"
        let phase = reasoning ? "reasoning" : (commentary ? "commentary" : "final_answer")
        let id = streamID(modelStepID: modelStepID, phase: phase)
        let kind: TranscriptEntry.Kind = reasoning
            ? .reasoning
            : (commentary ? .commentary : .assistant)
        let delta = event["delta"]?.stringValue ?? ""
        guard !delta.isEmpty else { return }
        if let index = entries.lastIndex(where: { $0.id == id }) {
            entries[index].text.append(delta)
            if entries[index].turnID == nil { entries[index].turnID = turnID }
            entries[index].sourceSequence = record.sequence
            entries[index].recordedAtMs = record.recordedAtMs
        } else {
            entries.append(TranscriptEntry(
                id: id,
                presentationID: TranscriptEntry.narrativePresentationID(
                    modelStepID: modelStepID,
                    phase: phase,
                    ordinal: 0
                ),
                text: delta,
                kind: kind,
                format: "plain_text",
                tone: "neutral",
                pending: true,
                modelStepID: modelStepID,
                turnID: turnID,
                sourceSequence: record.sequence,
                recordedAtMs: record.recordedAtMs
            ))
        }
    }

    private func finishHistoryTurn(
        _ record: RecordedEvent,
        turnID: String?,
        aborted: Bool,
        entries: inout [TranscriptEntry]
    ) {
        for entry in entries where entry.pending { entry.pending = false }
        guard let turnID else { return }
        markTranscriptTurnFinished(
            turnID,
            terminalSourceSequence: aborted ? record.sequence : nil,
            finishedAtMs: record.recordedAtMs,
            in: &entries
        )
    }

    func updateContextTokens() {
        contextTokens = max(
            0,
            max(lastUsage.totalTokens, lastUsage.inputTokens + lastUsage.outputTokens)
        )
    }

    func setPairingCode(_ code: String, expiresAt: Date) {
        pairingCodeExpiryTask?.cancel()
        guard expiresAt > .now else {
            pairingCodeInfo = nil
            pairingCodeExpiryTask = nil
            return
        }
        pairingCodeInfo = PairingCodeInfo(code: code, expiresAt: expiresAt)
        pairingCodeExpiryTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(max(0, expiresAt.timeIntervalSinceNow)))
            guard !Task.isCancelled,
                  let self,
                  self.pairingCodeInfo?.expiresAt == expiresAt
            else { return }
            self.pairingCodeInfo = nil
            self.pairingCodeExpiryTask = nil
        }
    }

    func decodeApproval(_ value: JSONValue) -> PendingApproval? {
        guard let id = value["id"]?.stringValue else { return nil }
        let calls = value["calls"]?.arrayValue?.compactMap { call -> ApprovalCall? in
            guard let callID = call["callId"]?.stringValue,
                  let name = call["name"]?.stringValue
            else { return nil }
            return ApprovalCall(
                id: callID,
                name: name,
                arguments: call["arguments"]?.prettyPrinted ?? "{}"
            )
        } ?? []
        return PendingApproval(
            id: id,
            reason: value["reason"]?.stringValue ?? "möbius needs permission to continue.",
            calls: calls
        )
    }

}
