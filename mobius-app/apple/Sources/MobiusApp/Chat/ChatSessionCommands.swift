import Foundation

extension ChatSessionModel {
    @discardableResult
    func submitExistingSessionMessage(
        text: String,
        attachments: [SessionFileReference],
        reply: MessageReply?,
        requestedDelivery: ActiveMessageDelivery?
    ) -> Bool {
        guard let sessionID = selectedSessionID else { return false }
        let id = requestID("input")
        let targetTurnID = composerTargetTurnID
        let delivery = targetTurnID == nil ? nil : requestedDelivery
        let operation = AgentOperation.message(
            MessageSubmission(
                author: .user,
                text: text,
                attachments: attachments,
                reply: reply,
                requestedDelivery: delivery,
                targetTurnId: targetTurnID
            ))
        dismissComposerFocus()
        if pendingWidgetEdit?.recovery.phase == .editing {
            submitComposerEdit(
                sessionID: sessionID,
                requestID: id,
                text: text,
                operation: operation
            )
            return true
        }
        let stashedText = stashedComposerDraft
        if targetTurnID == nil {
            startChatTitle(prompt: text, submissionID: id, sessionID: sessionID)
        }
        pendingDrafts[id] = PendingComposerDraft(
            text: text,
            attachments: attachments,
            reply: reply
        )
        composerDraftSaveTask?.cancel()
        composerDraftSaveTask = nil
        if let owner = composerDraftOwner {
            enqueueComposerDraftSave(
                ComposerDraft(
                    text: stashedText ?? text,
                    reply: stashedText == nil ? reply : nil
                ),
                owner: owner
            )
        }
        stashedComposerDraft = nil
        suppressesComposerDraftSave = true
        composer = ""
        suppressesComposerDraftSave = false
        composerReply = nil
        composerAttachments = []
        gateway.transmit(
            .submit(
                sessionID: sessionID,
                submission: Submission(id: id, op: operation)
            )
        ) { [weak self] _ in
            guard let self else { return }
            self.restoreDraft(id: id)
            self.cancelChatTitle(submissionID: id, rearm: true)
        }
        if let stashedText, !stashedText.isEmpty { composer = stashedText }
        return true
    }

    private func submitComposerEdit(
        sessionID: String,
        requestID: String,
        text: String,
        operation: AgentOperation
    ) {
        guard var pending = pendingWidgetEdit,
            let accountID = gateway.selectedAccountID,
            pending.owner == ComposerDraftOwner(accountID: accountID, sessionID: sessionID),
            pending.recovery.phase == .editing
        else { return }
        pending.recovery.editedInput = text
        pending.recovery.requestID = requestID
        pending.recovery.submissionBaselineSequence = latestSequence
        pending.recovery.phase = .submitting
        pendingWidgetEdit = pending
        composerDraftSaveTask?.cancel()
        composerDraftSaveTask = nil
        enqueueComposerEditRecoverySave(pending.recovery, owner: pending.owner) {
            [weak self] result in
            guard let self,
                self.pendingWidgetEdit?.owner == pending.owner,
                self.pendingWidgetEdit?.recovery.requestID == requestID,
                self.pendingWidgetEdit?.recovery.phase == .submitting
            else { return }
            if case .failure(let error) = result {
                self.restoreComposerEditMode(requestID: requestID)
                self.showToast(verbatim: self.localizedErrorDescription(error), tone: .error)
                return
            }
            guard self.gateway.connectionState.isReady, self.selectedSessionID == sessionID else {
                self.restoreComposerEditMode(requestID: requestID)
                return
            }
            guard self.gateway.selectedAccountID == pending.owner.accountID else {
                self.restoreComposerEditMode(requestID: requestID)
                return
            }
            self.stashedComposerDraft = nil
            self.suppressesComposerDraftSave = true
            self.composer = pending.recovery.displacedDraft
            self.suppressesComposerDraftSave = false
            self.gateway.transmit(
                .submit(
                    sessionID: sessionID,
                    submission: Submission(id: requestID, op: operation)
                )
            ) { [weak self] _ in
                self?.restoreComposerEditMode(requestID: requestID)
            }
        }
    }

    var canBrowseSessions: Bool {
        (gateway.connectionState.isReady || gateway.selectedAccountID != nil)
            && pendingDrafts.isEmpty
            && sessionRequestID == nil
            && sessionMutationRequestID == nil
            && sessionFileUploadRequests.isEmpty
            && abandonedSessionFileUploadRequests.isEmpty
            && sessionFileDeleteRequests.isEmpty
            && composerAttachments.isEmpty
            && pendingWidgetEdit == nil
            && !isLoadingComposerEditRecovery
    }

    @discardableResult
    func requestSessionRename(
        sessionID: String,
        title: String,
        generatedTitleSessionID: String? = nil
    ) -> String? {
        guard sessionMutationRequestID == nil else { return nil }
        let id = requestID("session-rename")
        sessionMutationRequestID = id
        gateway.transmit(.renameSession(requestID: id, sessionID: sessionID, title: title)) {
            [weak self] _ in
            guard let self else { return }
            if self.sessionMutationRequestID == id { self.sessionMutationRequestID = nil }
            if let generatedTitleSessionID,
                self.pendingChatTitles[generatedTitleSessionID]?.renameRequestID == id
            {
                self.cancelChatTitle(generatedTitleSessionID)
            }
        }
        return id
    }

    func setSessionPinned(_ session: SessionRecord, pinned: Bool) {
        guard sessionMutationRequestID == nil else { return }
        let id = requestID("session-pin")
        sessionMutationRequestID = id
        gateway.transmit(
            .setSessionPinned(
                requestID: id,
                sessionID: session.sessionId,
                pinned: pinned
            )
        ) { [weak self] _ in
            if self?.sessionMutationRequestID == id { self?.sessionMutationRequestID = nil }
        }
    }

    func openSession(_ sessionID: String) {
        guard canBrowseSessions || sessionID == selectedSessionID else { return }
        guard
            gateway.connectionState.isReady || sessionID == selectedSessionID
                || sessions.contains(where: { $0.sessionId == sessionID })
                || botSessions.contains(where: { $0.sessionId == sessionID })
        else { return }
        markSessionRead(sessionID)
        guard sessionID != selectedSessionID else { return }
        flushStreamDeltas()
        cacheSelectedTranscript()
        let generation = UUID()
        transcriptLoadGeneration = generation
        let accountID = gateway.selectedAccountID
        let wasReady = gateway.connectionState.isReady
        presentCachedSession(sessionID, transcript: nil)
        sessionToRestoreID = sessionID
        sessionOpeningID = sessionID
        let openRequestID = wasReady ? requestID("open") : nil
        sessionRequestID = openRequestID
        if wasReady { gateway.connectionState = .loading }
        let previous = transcriptIOTask
        transcriptIOTask = Task { [weak self, store] in
            await previous?.value
            let cached: CachedTranscript? =
                if let accountID {
                    await store.loadTranscript(accountID: accountID, sessionID: sessionID)
                } else {
                    nil
                }
            guard let self,
                generation == self.transcriptLoadGeneration,
                accountID == self.gateway.selectedAccountID,
                self.sessionOpeningID == sessionID,
                self.sessionRequestID == openRequestID,
                self.replayRequestID == nil
            else { return }
            if wasReady {
                self.requestSessionOpen(
                    sessionID,
                    lastSequence: cached?.sequence,
                    cachedTranscript: cached,
                    presentedTranscript: cached?.transcript,
                    requestID: openRequestID
                )
            } else {
                self.presentCachedSession(sessionID, transcript: cached)
                self.sessionToRestoreID = sessionID
                self.sessionRequestID = nil
                self.sessionOpeningID = nil
            }
        }
    }

    func presentCachedSession(_ sessionID: String, transcript cached: CachedTranscript?) {
        if selectedSessionID != sessionID {
            onDiscardFilePresentation?()
            changeComposerDraftOwner(
                to: gateway.selectedAccountID.map {
                    ComposerDraftOwner(accountID: $0, sessionID: sessionID)
                })
            resetSessionState()
        }
        selectedSessionID = sessionID
        latestSequence = cached?.sequence
        nextHistoryBeforeSequence = cached?.nextBeforeSequence
        transcript = cached?.transcript ?? []
        currentUsage = cached?.currentUsage ?? TokenUsage()
        lastUsage = cached?.lastUsage ?? TokenUsage()
        updateContextTokens()
    }

    func restoreSession(_ sessionID: String) {
        flushStreamDeltas()
        guard sessionID == selectedSessionID,
            let sequence = latestSequence
        else {
            requestSessionOpen(sessionID, lastSequence: nil)
            return
        }
        let base = CachedTranscript(
            sequence: sequence,
            nextBeforeSequence: nextHistoryBeforeSequence,
            transcript: transcript,
            currentUsage: currentUsage,
            lastUsage: lastUsage
        )
        let presentation = CachedTranscript(
            sequence: sequence,
            nextBeforeSequence: nextHistoryBeforeSequence,
            transcript: displayedTranscript,
            currentUsage: currentUsage,
            lastUsage: lastUsage
        ).transcript
        requestSessionOpen(
            sessionID,
            lastSequence: sequence,
            cachedTranscript: base,
            presentedTranscript: presentation
        )
    }

    func requestSessionOpen(
        _ sessionID: String,
        lastSequence: UInt64?,
        cachedTranscript: CachedTranscript? = nil,
        presentedTranscript: [TranscriptEntry]? = nil,
        requestID: String? = nil
    ) {
        transcriptLoadGeneration = UUID()
        replayCompletionSubmissionIDs.removeAll(keepingCapacity: true)
        replayUserMessages.removeAll(keepingCapacity: true)
        completedComposerEditReplay = false
        if sessionID != selectedSessionID {
            transcriptWindowAnchor = .tail
            discardComposerAttachments()
            onDiscardFilePresentation?()
            cancelSessionFileThumbnailDownloads()
        }
        sessionToRestoreID = nil
        sessionOpeningID = sessionID
        sessionOpenCursor = lastSequence
        pendingCachedTranscript = cachedTranscript
        pendingPresentedTranscript = presentedTranscript
        let id = requestID ?? self.requestID("open")
        sessionRequestID = id
        gateway.connectionState = .loading
        gateway.transmit(
            .openSession(
                requestID: id,
                sessionID: sessionID,
                lastSequence: lastSequence
            )
        ) { [weak self] _ in
            guard self?.sessionRequestID == id else { return }
            self?.sessionRequestID = nil
            self?.sessionOpeningID = nil
            self?.sessionOpenCursor = nil
            self?.pendingCachedTranscript = nil
            self?.pendingPresentedTranscript = nil
            self?.gateway.connectionState = .ready
        }
    }
}
