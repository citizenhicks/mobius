import Foundation

extension AppModel {
    func beginReplying(to entry: TranscriptEntry) {
        guard canBeginReply,
              !entry.pending,
              let target = entry.messageTarget
        else { return }
        let text = entry.text.isEmpty
            ? entry.files.map(\.name).joined(separator: ", ")
            : entry.text
        guard !text.isEmpty else { return }
        composerReply = MessageReply(target: target, text: text)
        composerFocusRequest &+= 1
    }

    func openMessageReply(_ reply: MessageReply) {
        messageNavigationRequest = MessageNavigationRequest(target: reply.target)
    }

    func reduce(record: RecordedEvent) {
        pinTranscriptWindowIfNeeded()
        let event = record.event
        let type = event.msg["type"]?.stringValue ?? "unknown"
        let message = type == "message" ? try? MessageEventPayload(json: event.msg) : nil
        let turnID = event.msg["turnId"]?.stringValue ?? activeTurnID
        prepareTranscriptEvent(event, type: type, message: message)
        let wasRendered = applyPresentation(from: record, turnID: turnID)

        if handleNarrativeEvent(
            type,
            event: event,
            message: message,
            record: record,
            turnID: turnID
        ) {
            return
        }
        if handleTurnEvent(type, event: event, record: record, turnID: turnID, wasRendered: wasRendered) {
            return
        }
        _ = handleTranscriptStateEvent(type, event: event)
    }

    private func prepareTranscriptEvent(
        _ event: AgentEventRecord,
        type: String,
        message: MessageEventPayload?
    ) {
        let confirmsSteeringDelivery = replayRequestID == nil
            && message?.author == .user
            && message?.delivery == .steer
            && event.submissionId != nil
            && message?.messageTarget != nil
        if confirmsSteeringDelivery { steeringDeliveryRevision &+= 1 }
        // Anything that is not a delta may read or finalize the streams the buffer feeds,
        // so buffered text must land first to keep transcript order exact.
        if type != "assistant_content_delta" {
            flushStreamDeltas()
        }
        if message?.delivery == .turn,
           message?.author == .user,
           let submissionID = event.submissionId {
            confirmChatTitle(submissionID: submissionID)
        }
        if let submissionID = event.submissionId {
            if type == "warning" || type == "error" || type == "submission_rejected" {
                if let draft = pendingDrafts.removeValue(forKey: submissionID) { restoreDraft(draft) }
                previewSelections.removeValue(forKey: submissionID)
                if previewPageRequestID == submissionID {
                    previewPageRequestID = nil
                    isLoadingPreviewPage = false
                }
                rejectComposerEdit(requestID: submissionID)
            } else {
                pendingDrafts.removeValue(forKey: submissionID)
                if message?.author == .user
                    || (type == "frontend"
                        && event.msg["frontendType"]?.stringValue == "widget") {
                    completeSubmittedComposerEdit(requestID: submissionID)
                }
                flushComposerDraft()
            }
        }
    }

    private func applyPresentation(from record: RecordedEvent, turnID: String?) -> Bool {
        let event = record.event
        let modelStepID = event.msg["modelStepId"]?.stringValue
        for (index, rendered) in record.blocks.enumerated() {
            apply(
                rendered,
                sequence: record.sequence,
                blockIndex: index,
                recordedAtMs: record.recordedAtMs,
                turnID: turnID,
                modelStepID: modelStepID
            )
        }
        if let preview = record.preview {
            let completesPageLoad = event.submissionId == previewPageRequestID
            if completesPageLoad {
                previewPageRequestID = nil
            }
            apply(
                preview,
                selection: event.submissionId.flatMap { previewSelections.removeValue(forKey: $0) }
            )
            if completesPageLoad { isLoadingPreviewPage = false }
        }
        return !record.blocks.isEmpty
    }

    private func handleNarrativeEvent(
        _ type: String,
        event: AgentEventRecord,
        message: MessageEventPayload?,
        record: RecordedEvent,
        turnID: String?
    ) -> Bool {
        switch type {
        case "message":
            guard let message else { return true }
            appendMessage(
                message,
                record: record,
                turnID: turnID,
                startsTurn: message.delivery.startsTurn && consumeInitialTurnMarker(turnID)
            )
            return true
        case "assistant_content_delta":
            let phase = event.msg["phase"]?.stringValue
            guard let modelStepID = event.msg["modelStepId"]?.stringValue else { return true }
            let kind: TranscriptEntry.Kind = switch phase {
            case "reasoning": .reasoning
            case "commentary": .commentary
            default: .assistant
            }
            appendStream(
                id: streamID(modelStepID: modelStepID, phase: phase ?? "final_answer"),
                delta: event.msg["delta"]?.stringValue ?? "",
                kind: kind,
                modelStepID: modelStepID,
                turnID: turnID,
                record: record
            )
            return true
        case "model_step_completed":
            applyModelStepCompletion(event.msg, turnID: turnID, record: record)
            return true
        case "assistant_message":
            applyAssistantMessage(event.msg, turnID: turnID, record: record)
            return true
        default:
            return false
        }
    }

    private func consumeInitialTurnMarker(_ turnID: String?) -> Bool {
        guard turnID != nil, awaitingInitialMessageTurnID == turnID else { return false }
        awaitingInitialMessageTurnID = nil
        return true
    }

    private func handleTurnEvent(
        _ type: String,
        event: AgentEventRecord,
        record: RecordedEvent,
        turnID: String?,
        wasRendered: Bool
    ) -> Bool {
        switch type {
        case "model_step_started":
            if replayRequestID == nil { runStats.active?.modelCalls += 1 }
        case "turn_started":
            activeTurnID = event.msg["turnId"]?.stringValue
            awaitingInitialMessageTurnID = activeTurnID
            if replayRequestID == nil,
               let turnID = activeTurnID,
               runStats.active?.turnId != turnID {
                runStats.active = RunSummary(
                    sessionId: selectedSessionID ?? "",
                    submissionId: event.submissionId ?? "",
                    turnId: turnID,
                    startedAtMs: Int64(Date.now.timeIntervalSince1970 * 1_000),
                    finishedAtMs: nil,
                    elapsedMs: 0,
                    outcome: nil,
                    modelCalls: 0,
                    toolCalls: 0,
                    failedToolCalls: 0,
                    usage: TokenUsage()
                )
            }
            if let window = event.msg["modelContextWindow"]?.intValue {
                modelContextWindow = Int64(window)
            }
        case "turn_complete":
            finishTranscriptTurn(record, turnID: turnID, aborted: false, wasRendered: wasRendered)
        case "turn_aborted":
            finishTranscriptTurn(record, turnID: turnID, aborted: true, wasRendered: wasRendered)
        case "web_search_begin", "web_search_end", "warning", "error", "submission_rejected":
            break
        case "tool_call_begin":
            if replayRequestID == nil { runStats.active?.toolCalls += 1 }
        case "tool_call_end":
            if replayRequestID == nil, event.msg["isError"]?.boolValue == true {
                runStats.active?.failedToolCalls += 1
            }
        default:
            return false
        }
        return true
    }

    private func finishTranscriptTurn(
        _ record: RecordedEvent,
        turnID: String?,
        aborted: Bool,
        wasRendered: Bool
    ) {
        finishPendingTranscriptEntries()
        if let turnID {
            markTranscriptTurnFinished(
                turnID,
                terminalSourceSequence: aborted ? record.sequence : nil,
                finishedAtMs: record.recordedAtMs,
                in: &transcript
            )
        }
        awaitingInitialMessageTurnID = nil
        activeTurnID = nil
        if replayRequestID == nil { runStats.active = nil }
        refreshWorkspaceChanges()
        if filesInspectorTab == .chatFiles { refreshSessionFiles() }
        pendingApproval = nil
        approvalRequestID = nil
        if aborted, !wasRendered { finishPendingTranscriptEntries() }
    }

    private func handleTranscriptStateEvent(
        _ type: String,
        event: AgentEventRecord
    ) -> Bool {
        switch type {
        case "model_changed":
            selectedModelRoute = event.msg["route"]?.stringValue ?? selectedModelRoute
            if let window = event.msg["modelContextWindow"]?.intValue {
                modelContextWindow = Int64(window)
            }
        case "session_resume_requested":
            if let sessionID = event.msg["sessionId"]?.stringValue { openChat(sessionID) }
        case "exec_approval_request":
            approvalRequestID = nil
            pendingApproval = decodeApproval(event.msg)
        case "token_count":
            if let usage = event.msg["info"]?["totalTokenUsage"],
               let decoded = TokenUsage(json: usage) {
                currentUsage = decoded
            }
            if let usage = event.msg["info"]?["lastTokenUsage"],
               let latest = TokenUsage(json: usage) {
                lastUsage = latest
                updateContextTokens()
            }
            if let window = event.msg["info"]?["modelContextWindow"]?.intValue {
                modelContextWindow = Int64(window)
            }
        case "frontend":
            applyFrontendEvent(event.msg, submissionID: event.submissionId)
        default:
            return false
        }
        return true
    }

    private func applyFrontendEvent(_ event: JSONValue, submissionID: String?) {
        switch event["frontendType"]?.stringValue {
        case "render":
            break
        case "widget":
            guard let capability = event["capability"]?.stringValue,
                  let item = event["item"],
                  let widget = try? FrontendWidget(json: item)
            else { return }
            upsertWidget(MountedWidget(capability: capability, widget: widget))
            acknowledgeWidgetEdit(
                submissionID: submissionID,
                capability: capability,
                widgetID: widget.id
            )
        case "remove_widget":
            guard let capability = event["capability"]?.stringValue,
                  let id = event["id"]?.stringValue
            else { return }
            mountedWidgets.removeAll { $0.capability == capability && $0.widget.id == id }
            acknowledgeWidgetEdit(
                submissionID: submissionID,
                capability: capability,
                widgetID: id
            )
        case "picker":
            guard let title = event["title"]?.stringValue else { return }
            let options = event["options"]?.arrayValue?.compactMap {
                try? FrontendPickerOption(json: $0)
            } ?? []
            guard !options.isEmpty else { return }
            pendingPicker = FrontendPickerPrompt(title: title, options: options)
        default:
            break
        }
    }

    private func acknowledgeWidgetEdit(
        submissionID: String?,
        capability: String,
        widgetID: String
    ) {
        guard var pending = pendingWidgetEdit,
              pending.recovery.phase == .removingQueuedInput,
              pending.recovery.requestID == submissionID,
              pending.recovery.capability == capability,
              pending.recovery.widgetID == widgetID
        else { return }
        pending.recovery.phase = .editing
        pendingWidgetEdit = pending
        flushComposerDraft()
        stashedComposerDraft = pending.recovery.displacedDraft
        suppressesComposerDraftSave = true
        composer = pending.recovery.editedInput
        suppressesComposerDraftSave = false
        composerFocusRequest &+= 1
        enqueueComposerEditRecoverySave(pending.recovery, owner: pending.owner)
    }

    func upsertWidget(_ mounted: MountedWidget) {
        if let index = mountedWidgets.firstIndex(where: { $0.id == mounted.id }) {
            mountedWidgets[index] = mounted
        } else {
            mountedWidgets.append(mounted)
        }
    }

    func apply(
        _ rendered: RenderedBlock,
        sequence: UInt64,
        blockIndex: Int,
        recordedAtMs: Int64,
        turnID: String?,
        modelStepID: String?
    ) {
        mutateTranscriptPreservingPrefix { entries in
            apply(
                rendered,
                sequence: sequence,
                blockIndex: blockIndex,
                recordedAtMs: recordedAtMs,
                turnID: turnID,
                modelStepID: modelStepID,
                to: &entries
            )
        }
        let block = rendered.block
        if block.format == "unified_diff", !block.pending {
            refreshWorkspaceChanges()
        }
    }

    func apply(
        _ rendered: RenderedBlock,
        sequence: UInt64,
        blockIndex: Int,
        recordedAtMs: Int64,
        turnID: String?,
        modelStepID: String? = nil,
        recordID: String? = nil,
        to entries: inout [TranscriptEntry]
    ) {
        let block = rendered.block
        let sourceID = block.id
            ?? recordID.map { "record:\($0):\(blockIndex)" }
            ?? "record:\(sequence):\(blockIndex)"
        let id = scopedBlockID(capability: rendered.capability, sourceID: sourceID)
        let appending = block.update == .append
        let kind: TranscriptEntry.Kind = block.tone == "error" ? .error : .event
        if let index = entries.firstIndex(where: { $0.id == id }) {
            let previousUpdate = entries[index].update
            // Grouping keys off kind and role, and both can change on an entry that is
            // already on screen — an event turning into an error keeps its id. The row
            // projection cannot see that from the array alone, so it is told here.
            if entries[index].kind != kind || entries[index].role != block.role {
                invalidateTranscriptProjection()
            }
            entries[index].text = appending ? entries[index].text + block.text : block.text
            entries[index].kind = kind
            entries[index].capability = rendered.capability
            entries[index].role = block.role
            entries[index].update = appending && previousUpdate == .append ? .append : .replace
            entries[index].title = block.title
            entries[index].symbol = block.symbol
            if block.group != nil { entries[index].group = block.group }
            if let turnID, entries[index].turnID != turnID {
                entries[index].turnID = turnID
                invalidateTranscriptProjection()
            }
            if let modelStepID { entries[index].modelStepID = modelStepID }
            entries[index].pending = block.pending
            entries[index].sourceSequence = sequence
            entries[index].recordedAtMs = recordedAtMs
            entries[index].format = block.format
            entries[index].tone = block.tone
            let currentFiles = entries[index].files
            entries[index].files = mergedFiles(
                currentFiles,
                with: block.files,
                appending: appending
            )
        } else {
            entries.append(TranscriptEntry(
                id: id,
                text: appending && recordID == nil
                    ? String(block.text.drop(while: { $0 == "\n" }))
                    : block.text,
                kind: kind,
                capability: rendered.capability,
                role: block.role,
                update: block.update,
                title: block.title,
                symbol: block.symbol,
                group: block.group,
                format: block.format,
                tone: block.tone,
                pending: block.pending,
                modelStepID: modelStepID,
                turnID: turnID,
                sourceSequence: sequence,
                recordedAtMs: recordedAtMs,
                files: block.files
            ))
        }
    }

    private func scopedBlockID(capability: String, sourceID: String) -> String {
        "block:\(capability.utf8.count):\(capability)\(sourceID)"
    }

    private func mergedFiles(
        _ current: [SessionFileReference],
        with incoming: [SessionFileReference],
        appending: Bool
    ) -> [SessionFileReference] {
        guard appending else { return incoming }
        var result = current
        for file in incoming {
            if let index = result.firstIndex(where: { $0.id == file.id }) {
                result[index] = file
            } else {
                result.append(file)
            }
        }
        return result
    }

    func apply(_ preview: RenderedPreview, selection: FrontendPickerOption?) {
        var pageEntries: [TranscriptEntry] = []
        var turnState = TranscriptHistoryTurnState()
        for (index, rendered) in preview.events.enumerated() {
            reduceHistory(
                RecordedEvent(
                    sequence: UInt64(index + 1),
                    recordedAtMs: rendered.recordedAtMs,
                    event: AgentEventRecord(submissionId: nil, msg: rendered.event),
                    streamMetrics: [],
                    blocks: rendered.blocks,
                    preview: nil
                ),
                into: &pageEntries,
                turnState: &turnState,
                recordID: "\(preview.pageId):\(index)"
            )
        }
        let existing = previews.first { $0.id == preview.id }
        let visibleEntries = switch preview.update {
        case .replace:
            pageEntries
        case .prepend:
            mergePreviewPages(older: pageEntries, newer: existing?.entries ?? [])
        }
        let record = TranscriptPreview(
            id: preview.id,
            title: preview.title,
            context: preview.subtitle.isEmpty ? existing?.context ?? "" : preview.subtitle,
            status: selection?.description ?? existing?.status,
            model: selection?.detail ?? existing?.model,
            entries: visibleEntries,
            next: preview.next
        )
        if let index = previews.firstIndex(where: { $0.id == preview.id }) {
            previews[index] = record
        } else {
            previews.append(record)
        }
        if selection != nil || presentedPreview?.id == preview.id { presentedPreview = record }
    }

    func applyRoutineRunPreview(_ preview: RoutineRunPreview) {
        guard routineRunPreviewRequestID == preview.requestID else { return }
        var pageEntries: [TranscriptEntry] = []
        var turnState = TranscriptHistoryTurnState()
        for record in preview.records {
            reduceHistory(record, into: &pageEntries, turnState: &turnState)
        }
        routineRunPreviewEntries = if routineRunPreviewRequestBeforeSequence == nil {
            pageEntries
        } else {
            mergePreviewPages(older: pageEntries, newer: routineRunPreviewEntries)
        }
        routineRunPreview = preview
        presentedRoutineRun = preview.run
        routineRunPreviewNextBeforeSequence = preview.nextBeforeSequence
        routineRunPreviewRequestID = nil
        routineRunPreviewRequestBeforeSequence = nil
        isLoadingRoutineRunPreview = false
        routineRunPreviewError = nil
    }

    private func mergePreviewPages(
        older: [TranscriptEntry],
        newer: [TranscriptEntry]
    ) -> [TranscriptEntry] {
        var merged = copiedTranscript(older)
        var indices: [String: Int] = [:]
        for index in merged.indices { indices[merged[index].id] = index }
        for source in newer {
            let entry = copiedTranscript([source])[0]
            if let index = indices[entry.id] {
                let previous = merged[index]
                if entry.update == .append {
                    entry.text = previous.text + entry.text
                    entry.files = mergedFiles(previous.files, with: entry.files, appending: true)
                    if entry.group == nil { entry.group = previous.group }
                    entry.update = previous.update == .append ? .append : .replace
                } else if entry.modelStepID != nil, entry.pending, previous.pending {
                    entry.text = previous.text + entry.text
                }
                merged[index] = entry
            } else {
                indices[entry.id] = merged.count
                merged.append(entry)
            }
        }
        return merged
    }

    func appendText(
        _ text: String?,
        kind: TranscriptEntry.Kind,
        tone: String = "neutral",
        id: String? = nil,
        presentationID: String? = nil,
        modelStepID: String? = nil,
        turnID: String? = nil,
        startsTurn: Bool = false,
        sourceSequence: UInt64? = nil,
        recordedAtMs: Int64? = nil,
        messageTarget: MessageTarget? = nil,
        files: [SessionFileReference] = []
    ) {
        mutateTranscriptPreservingPrefix { entries in
            appendText(
                text,
                kind: kind,
                tone: tone,
                id: id,
                presentationID: presentationID,
                modelStepID: modelStepID,
                turnID: turnID,
                startsTurn: startsTurn,
                sourceSequence: sourceSequence,
                recordedAtMs: recordedAtMs,
                messageTarget: messageTarget,
                files: files,
                to: &entries
            )
        }
    }

    func appendMessage(
        _ message: MessageEventPayload,
        record: RecordedEvent,
        turnID: String?,
        startsTurn: Bool
    ) {
        mutateTranscriptPreservingPrefix { entries in
            appendMessage(
                message,
                record: record,
                turnID: turnID,
                startsTurn: startsTurn,
                to: &entries
            )
        }
    }

    func appendMessage(
        _ message: MessageEventPayload,
        record: RecordedEvent,
        turnID: String?,
        startsTurn: Bool,
        recordID: String? = nil,
        to entries: inout [TranscriptEntry]
    ) {
        guard !message.text.isEmpty || !message.attachments.isEmpty else { return }
        let id: String
        if let peer = message.author.peerFields {
            id = "message:peer:\(peer.sessionID.utf8.count):\(peer.sessionID):\(peer.messageID)"
        } else if let submissionID = record.event.submissionId {
            id = "message:submission:\(submissionID.utf8.count):\(submissionID)"
        } else if let recordID {
            id = "message:record:\(recordID.utf8.count):\(recordID)"
        } else {
            id = "message:record:\(record.sequence)"
        }
        let kind: TranscriptEntry.Kind = message.author == .user ? .user : .peer
        entries.append(TranscriptEntry(
            id: id,
            text: message.text,
            kind: kind,
            format: "plain_text",
            pending: false,
            turnID: turnID,
            startsTurn: startsTurn,
            sourceSequence: record.sequence,
            recordedAtMs: record.recordedAtMs,
            messageTarget: message.messageTarget,
            reply: message.reply,
            files: message.attachments,
            messageMetadata: TranscriptMessageMetadata(
                author: message.author,
                delivery: message.delivery
            )
        ))
    }

    func appendText(
        _ text: String?,
        kind: TranscriptEntry.Kind,
        tone: String = "neutral",
        id: String? = nil,
        presentationID: String? = nil,
        modelStepID: String? = nil,
        turnID: String? = nil,
        startsTurn: Bool = false,
        sourceSequence: UInt64? = nil,
        recordedAtMs: Int64? = nil,
        messageTarget: MessageTarget? = nil,
        files: [SessionFileReference] = [],
        to entries: inout [TranscriptEntry]
    ) {
        let text = text ?? ""
        guard !text.isEmpty || !files.isEmpty else { return }
        entries.append(TranscriptEntry(
            id: id ?? UUID().uuidString,
            presentationID: presentationID,
            text: text,
            kind: kind,
            format: "plain_text",
            tone: tone,
            pending: false,
            modelStepID: modelStepID,
            turnID: turnID,
            startsTurn: startsTurn,
            sourceSequence: sourceSequence,
            recordedAtMs: recordedAtMs,
            messageTarget: messageTarget,
            files: files
        ))
    }

    // Deltas arrive several times per frame, and every application re-lays-out the whole
    // growing message. Batching to ~20 flushes a second keeps the text pipeline off the
    // critical path; ordering against non-delta events is preserved by the flush in `reduce`.
    func streamID(modelStepID: String, phase: String) -> String {
        "model-stream:\(modelStepID.utf8.count):\(modelStepID)\(phase)"
    }

    private func snapshotID(
        modelStepID: String,
        phase: String,
        outputIndex: Int,
        partIndex: Int
    ) -> String {
        "model-output:\(modelStepID.utf8.count):\(modelStepID):\(phase):\(outputIndex):\(partIndex)"
    }

    private func appendStream(
        id: String,
        delta: String,
        kind: TranscriptEntry.Kind,
        modelStepID: String,
        turnID: String?,
        record: RecordedEvent
    ) {
        guard !delta.isEmpty else { return }
        if let last = bufferedDeltas.indices.last, bufferedDeltas[last].id == id {
            bufferedDeltas[last].delta += delta
            if bufferedDeltas[last].turnID == nil { bufferedDeltas[last].turnID = turnID }
            bufferedDeltas[last].sourceSequence = record.sequence
            bufferedDeltas[last].recordedAtMs = record.recordedAtMs
        } else {
            bufferedDeltas.append((
                id: id,
                delta: delta,
                kind: kind,
                modelStepID: modelStepID,
                turnID: turnID,
                sourceSequence: record.sequence,
                recordedAtMs: record.recordedAtMs
            ))
        }
        guard deltaFlushTask == nil else { return }
        deltaFlushTask = Task { [weak self] in
            do {
                try await Task.sleep(for: .milliseconds(50))
            } catch {
                return
            }
            self?.flushStreamDeltas()
        }
    }

    func flushStreamDeltas() {
        pinTranscriptWindowIfNeeded()
        deltaFlushTask?.cancel()
        deltaFlushTask = nil
        for buffered in bufferedDeltas {
            if let index = transcript.lastIndex(where: { $0.id == buffered.id }) {
                transcript[index].text.append(buffered.delta)
                if transcript[index].turnID == nil, let turnID = buffered.turnID {
                    transcript[index].turnID = turnID
                    invalidateTranscriptProjection()
                }
                transcript[index].sourceSequence = buffered.sourceSequence
                transcript[index].recordedAtMs = buffered.recordedAtMs
            } else {
                mutateTranscriptPreservingPrefix { entries in
                    entries.append(TranscriptEntry(
                        id: buffered.id,
                        presentationID: buffered.kind.narrativePhase.map {
                            TranscriptEntry.narrativePresentationID(
                                modelStepID: buffered.modelStepID,
                                phase: $0,
                                ordinal: 0
                            )
                        },
                        text: buffered.delta,
                        kind: buffered.kind,
                        format: "plain_text",
                        tone: "neutral",
                        pending: true,
                        modelStepID: buffered.modelStepID,
                        turnID: buffered.turnID,
                        sourceSequence: buffered.sourceSequence,
                        recordedAtMs: buffered.recordedAtMs
                    ))
                }
            }
        }
        bufferedDeltas.removeAll()
    }

    func applyModelStepCompletion(
        _ event: JSONValue,
        turnID: String?,
        record: RecordedEvent
    ) {
        applyModelStepCompletion(event, turnID: turnID, record: record, to: &transcript)
    }

    func applyModelStepCompletion(
        _ event: JSONValue,
        turnID: String?,
        record: RecordedEvent,
        to entries: inout [TranscriptEntry]
    ) {
        guard let modelStepID = event["modelStepId"]?.stringValue,
              let outcome = event["outcome"],
              let status = outcome["status"]?.stringValue
        else { return }
        guard status != "completed" else { return }
        // Block source ids are namespaced by model step, so a step that ends without
        // completing can never finish its pending blocks. The backend closes live
        // ones with its own end events; this sweep only keeps replay after a crash
        // from stranding them.
        for entry in entries
        where entry.pending
            && entry.capability.map({ capability in
                entry.id.hasPrefix(
                    scopedBlockID(capability: capability, sourceID: "\(modelStepID)/")
                )
            }) == true
        {
            entry.pending = false
            if entry.turnID == nil { entry.turnID = turnID }
            entry.tone = "warning"
            entry.sourceSequence = record.sequence
            entry.recordedAtMs = record.recordedAtMs
        }
        for entry in entries where entry.modelStepID == modelStepID && entry.pending {
            entry.pending = false
            if entry.turnID == nil { entry.turnID = turnID }
            if status == "retrying" { entry.tone = "warning" }
            entry.sourceSequence = record.sequence
            entry.recordedAtMs = record.recordedAtMs
        }
    }

    func applyAssistantMessage(
        _ event: JSONValue,
        turnID: String?,
        record: RecordedEvent
    ) {
        applyAssistantMessage(event, turnID: turnID, record: record, to: &transcript)
    }

    func applyAssistantMessage(
        _ event: JSONValue,
        turnID: String?,
        record: RecordedEvent,
        to entries: inout [TranscriptEntry]
    ) {
        guard let modelStepID = event["modelStepId"]?.stringValue,
              let content = event["content"]?.arrayValue
        else { return }
        let previousSnapshotIndex = entries.firstIndex(where: {
            $0.modelStepID == modelStepID && !$0.pending
                && [.reasoning, .commentary, .assistant].contains($0.kind)
        })
        entries.removeAll {
            $0.modelStepID == modelStepID
                && [.reasoning, .commentary, .assistant].contains($0.kind)
        }
        let targetItem = content.lastIndex(where: {
            $0["phase"]?.stringValue != "reasoning" && $0["text"]?.stringValue?.isEmpty == false
        })
        let messageTarget = messageTarget(from: event)
        var nextPresentationOrdinal: [String: Int] = [:]
        let snapshotEntries = content.enumerated().compactMap { index, item -> TranscriptEntry? in
            guard let outputIndex = item["outputIndex"]?.intValue,
                  let partIndex = item["partIndex"]?.intValue,
                  let phase = item["phase"]?.stringValue,
                  let text = item["text"]?.stringValue,
                  !text.isEmpty
            else { return nil }
            let kind: TranscriptEntry.Kind
            switch phase {
            case "reasoning": kind = .reasoning
            case "commentary": kind = .commentary
            case "final_answer": kind = .assistant
            default: return nil
            }
            let ordinal = nextPresentationOrdinal[phase, default: 0]
            nextPresentationOrdinal[phase] = ordinal + 1
            return TranscriptEntry(
                id: snapshotID(
                    modelStepID: modelStepID,
                    phase: phase,
                    outputIndex: outputIndex,
                    partIndex: partIndex
                ),
                presentationID: TranscriptEntry.narrativePresentationID(
                    modelStepID: modelStepID,
                    phase: phase,
                    ordinal: ordinal
                ),
                text: text,
                kind: kind,
                format: "plain_text",
                tone: "neutral",
                pending: false,
                modelStepID: modelStepID,
                turnID: turnID,
                sourceSequence: record.sequence,
                recordedAtMs: record.recordedAtMs,
                messageTarget: index == targetItem ? messageTarget : nil,
                annotations: item["annotations"]?.arrayValue ?? []
            )
        }
        guard !snapshotEntries.isEmpty else { return }
        let insertionIndex = min(previousSnapshotIndex ?? entries.endIndex, entries.endIndex)
        entries.insert(contentsOf: snapshotEntries, at: insertionIndex)
        let annotations = snapshotEntries.flatMap(\.annotations)
        if !annotations.isEmpty,
           let searchIndex = entries.lastIndex(where: {
               $0.isWebSearch && $0.modelStepID == modelStepID
           }) {
            entries[searchIndex].annotations = annotations
        }
    }

    func completeStream(
        text: String,
        kind: TranscriptEntry.Kind,
        modelStepID: String?,
        turnID: String?,
        messageTarget: MessageTarget?,
        sourceSequence: UInt64?,
        recordedAtMs: Int64?
    ) {
        mutateTranscriptPreservingPrefix { entries in
            completeStream(
                text: text,
                kind: kind,
                modelStepID: modelStepID,
                turnID: turnID,
                messageTarget: messageTarget,
                sourceSequence: sourceSequence,
                recordedAtMs: recordedAtMs,
                in: &entries
            )
        }
    }

    func completeStream(
        text: String,
        kind: TranscriptEntry.Kind,
        modelStepID: String?,
        turnID: String?,
        messageTarget: MessageTarget?,
        sourceSequence: UInt64?,
        recordedAtMs: Int64?,
        in entries: inout [TranscriptEntry]
    ) {
        if let index = entries.lastIndex(where: {
            $0.pending && $0.kind == kind
                && (modelStepID == nil || $0.modelStepID == modelStepID)
        }) {
            entries[index].text = text
            if entries[index].pending { invalidateTranscriptProjection() }
            entries[index].pending = false
            if entries[index].turnID == nil { entries[index].turnID = turnID }
            entries[index].messageTarget = messageTarget
            if let sourceSequence { entries[index].sourceSequence = sourceSequence }
            if let recordedAtMs { entries[index].recordedAtMs = recordedAtMs }
        } else {
            let presentationID = modelStepID.flatMap { modelStepID in
                kind.narrativePhase.map {
                    TranscriptEntry.narrativePresentationID(
                        modelStepID: modelStepID,
                        phase: $0,
                        ordinal: 0
                    )
                }
            }
            appendText(
                text,
                kind: kind,
                presentationID: presentationID,
                modelStepID: modelStepID,
                turnID: turnID,
                sourceSequence: sourceSequence,
                recordedAtMs: recordedAtMs,
                messageTarget: messageTarget,
                to: &entries
            )
        }
    }

    func messageTarget(from event: JSONValue) -> MessageTarget? {
        event["messageTarget"].flatMap { MessageTarget(json: $0) }
    }

    private func finishPendingTranscriptEntries() {
        let changed = transcript.contains(where: \.pending)
        for entry in transcript where entry.pending {
            entry.pending = false
        }
        if changed { invalidateTranscriptProjection() }
    }

    func markTranscriptTurnFinished(
        _ turnID: String,
        terminalSourceSequence: UInt64? = nil,
        finishedAtMs: Int64?,
        in entries: inout [TranscriptEntry]
    ) {
        let terminalEntries: [TranscriptEntry]
        if let terminalSourceSequence {
            guard let terminal = entries.last(where: {
                $0.turnID == turnID && $0.sourceSequence == terminalSourceSequence
            }) else { return }
            terminalEntries = [terminal]
        } else {
            guard let final = entries.last(where: {
                $0.turnID == turnID && $0.kind == .assistant
            }) else { return }
            let finalModelStepID = final.modelStepID
            let finalSourceSequence = final.sourceSequence
            terminalEntries = entries.filter { entry in
                entry.turnID == turnID
                    && entry.kind == .assistant
                    && (finalModelStepID.map { entry.modelStepID == $0 }
                        ?? (entry === final
                            || finalSourceSequence.map { entry.sourceSequence == $0 } == true))
            }
        }
        let startedAtMs = entries
            .filter { $0.turnID == turnID }
            .compactMap(\.recordedAtMs)
            .min()
        let terminalAtMs = finishedAtMs
            ?? terminalEntries.compactMap(\.recordedAtMs).max()
        let elapsedMs = startedAtMs.flatMap { startedAtMs in
            terminalAtMs.map { UInt64(max(0, $0 - startedAtMs)) }
        }
        var changed = false
        for entry in terminalEntries {
            if !entry.turnTerminal {
                entry.turnTerminal = true
                changed = true
            }
            if let elapsedMs, entry.turnElapsedMs != elapsedMs {
                entry.turnElapsedMs = elapsedMs
                changed = true
            }
        }
        if changed { invalidateTranscriptProjection() }
    }

}
