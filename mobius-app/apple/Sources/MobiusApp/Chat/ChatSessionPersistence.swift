import Foundation

extension ChatSessionModel {
    func enqueueTranscriptIO(
        _ operation: @escaping @MainActor @Sendable () async -> Void
    ) {
        let previous = transcriptIOTask
        transcriptIOTask = Task {
            await previous?.value
            await operation()
        }
    }

    func scheduleComposerDraftSave() {
        guard !suppressesComposerDraftSave,
              !isLoadingComposerDraft,
              !isLoadingComposerEditRecovery,
              let owner = composerDraftOwner
        else { return }
        composerDraftSaveTask?.cancel()
        if var pending = pendingWidgetEdit,
           pending.owner == owner,
           pending.recovery.phase == .editing {
            guard composer.utf8.count <= maximumComposerBytes else { return }
            pending.recovery.editedInput = composer
            pendingWidgetEdit = pending
            let recovery = pending.recovery
            composerDraftSaveTask = Task { [weak self] in
                do {
                    try await Task.sleep(for: .milliseconds(400))
                } catch {
                    return
                }
                guard let self,
                      self.pendingWidgetEdit?.owner == owner,
                      self.pendingWidgetEdit?.recovery.phase == .editing,
                      self.pendingWidgetEdit?.recovery.editedInput == recovery.editedInput
                else { return }
                self.composerDraftSaveTask = nil
                self.enqueueComposerEditRecoverySave(recovery, owner: owner)
            }
            return
        }
        guard stashedComposerDraft == nil else { return }
        let draft = ComposerDraft(text: composer, reply: composerReply)
        composerDraftSaveTask = Task { [weak self] in
            do {
                try await Task.sleep(for: .milliseconds(400))
            } catch {
                return
            }
            guard let self, owner == composerDraftOwner else { return }
            composerDraftSaveTask = nil
            enqueueComposerDraftSave(draft, owner: owner)
        }
    }

    func flushComposerDraft() {
        composerDraftSaveTask?.cancel()
        composerDraftSaveTask = nil
        guard stashedComposerDraft == nil, let owner = composerDraftOwner else { return }
        enqueueComposerDraftSave(
            ComposerDraft(text: composer, reply: composerReply),
            owner: owner
        )
    }

    func enqueueComposerDraftSave(_ draft: ComposerDraft, owner: ComposerDraftOwner) {
        let previous = composerDraftIOTask
        let store = store
        composerDraftIOTask = Task {
            await previous?.value
            await store.saveComposerDraft(
                draft,
                accountID: owner.accountID,
                sessionID: owner.sessionID
            )
        }
    }

    func enqueueComposerEditRecoverySave(
        _ recovery: ComposerEditRecovery,
        owner: ComposerDraftOwner,
        completion: ((Result<Void, Error>) -> Void)? = nil
    ) {
        let previous = composerDraftIOTask
        let store = store
        composerDraftIOTask = Task {
            await previous?.value
            do {
                try await store.saveComposerEditRecovery(
                    recovery,
                    accountID: owner.accountID,
                    sessionID: owner.sessionID
                )
                completion?(.success(()))
            } catch {
                completion?(.failure(error))
            }
        }
    }

    func enqueueComposerEditRecoveryRemoval(owner: ComposerDraftOwner) {
        let previous = composerDraftIOTask
        let store = store
        composerDraftIOTask = Task {
            await previous?.value
            var lastError: Error?
            for _ in 0..<2 {
                do {
                    try await store.removeComposerEditRecovery(
                        accountID: owner.accountID,
                        sessionID: owner.sessionID
                    )
                    return
                } catch {
                    lastError = error
                }
            }
            if let lastError {
                showToast(verbatim: localizedErrorDescription(lastError), tone: .error)
            }
        }
    }

    func prepareComposerEditRecovery(for owner: ComposerDraftOwner) {
        guard composerDraftOwner == owner else { return }
        if pendingWidgetEdit?.owner == owner {
            if replayRequestID == nil { reconcileComposerEditRecovery() }
            return
        }
        let generation = UUID()
        composerEditRecoveryGeneration = generation
        isLoadingComposerEditRecovery = true
        let previous = composerDraftIOTask
        let store = store
        composerDraftIOTask = Task { [weak self] in
            await previous?.value
            let recovery = await store.loadComposerEditRecovery(
                accountID: owner.accountID,
                sessionID: owner.sessionID
            )
            guard let self,
                  self.composerEditRecoveryGeneration == generation,
                  self.composerDraftOwner == owner
            else { return }
            self.isLoadingComposerEditRecovery = false
            self.pendingWidgetEdit = recovery.map {
                PendingWidgetEdit(owner: owner, recovery: $0)
            }
            if self.replayRequestID == nil { self.reconcileComposerEditRecovery() }
        }
    }

    func observeReplayCompletion(_ buffered: BufferedAgentEvent) {
        guard replayRequestID != nil else { return }
        let event = buffered.record.event
        let type = event.msg["type"]?.stringValue
        let message = type == "message" ? try? MessageEventPayload(json: event.msg) : nil
        if let submissionID = event.submissionId,
           message?.author == .user
               || (type == "frontend"
                   && event.msg["frontendType"]?.stringValue == "widget"),
           replayCompletionSubmissionIDs.count < maximumObservedReplaySubmissions
               || replayCompletionSubmissionIDs.contains(submissionID) {
            replayCompletionSubmissionIDs.insert(submissionID)
        }

        var messages: [ReplayUserMessage] = []
        if let message, message.author == .user {
            let sequence = message.messageTarget?.checkpointSequence
                ?? buffered.record.sequence
            messages.append(ReplayUserMessage(sequence: sequence, text: message.text))
        }
        guard !messages.isEmpty else { return }
        replayUserMessages.append(contentsOf: messages.suffix(maximumObservedReplaySubmissions))
        if replayUserMessages.count > maximumObservedReplaySubmissions {
            replayUserMessages.removeFirst(
                replayUserMessages.count - maximumObservedReplaySubmissions
            )
        }
    }

    func reconcileComposerEditRecovery() {
        guard replayRequestID == nil,
              !isLoadingComposerEditRecovery
        else { return }
        defer {
            replayCompletionSubmissionIDs.removeAll(keepingCapacity: true)
            replayUserMessages.removeAll(keepingCapacity: true)
            completedComposerEditReplay = false
        }
        guard let pending = pendingWidgetEdit,
              pending.owner == composerDraftOwner
        else { return }
        let matchingWidgetInput = mountedWidgets.first(where: {
            $0.capability == pending.recovery.capability
                && $0.widget.id == pending.recovery.widgetID
        })?.widget.action?.capabilityInput
        let renderedEditedInput: Bool = if let baseline = pending.recovery.submissionBaselineSequence {
            transcript.contains {
                $0.kind == .user
                    && $0.text == pending.recovery.editedInput
                    && ($0.messageTarget?.checkpointSequence ?? 0) > baseline
            } || replayUserMessages.contains {
                $0.sequence > baseline && $0.text == pending.recovery.editedInput
            }
        } else {
            false
        }
        switch pending.recovery.phase {
        case .removingQueuedInput where matchingWidgetInput == pending.recovery.originalInput:
            completeComposerEditRecovery(pending)
        case .submitting where matchingWidgetInput == pending.recovery.editedInput
            || replayCompletionSubmissionIDs.contains(pending.recovery.requestID)
            || renderedEditedInput:
            completeComposerEditRecovery(pending)
        case .removingQueuedInput, .editing:
            restoreComposerEditMode(pending)
        case .submitting where completedComposerEditReplay:
            restoreComposerEditMode(pending)
        case .submitting:
            break
        case .completed:
            completeComposerEditRecovery(pending)
        }
    }

    func restoreComposerEditMode(requestID: String) {
        guard let pending = pendingWidgetEdit,
              pending.recovery.requestID == requestID,
              pending.recovery.phase == .submitting
        else { return }
        restoreComposerEditMode(pending)
    }

    func restoreComposerEditMode(_ current: PendingWidgetEdit) {
        var pending = current
        pending.recovery.phase = .editing
        pendingWidgetEdit = pending
        stashedComposerDraft = pending.recovery.displacedDraft
        suppressesComposerDraftSave = true
        composer = pending.recovery.editedInput
        suppressesComposerDraftSave = false
        composerFocusRequest &+= 1
        enqueueComposerEditRecoverySave(pending.recovery, owner: pending.owner)
    }

    func rejectComposerEdit(requestID: String) {
        guard let pending = pendingWidgetEdit,
              pending.recovery.requestID == requestID
        else { return }
        switch pending.recovery.phase {
        case .removingQueuedInput:
            completeComposerEditRecovery(pending)
        case .submitting:
            restoreComposerEditMode(pending)
        case .editing, .completed:
            break
        }
    }

    func completeSubmittedComposerEdit(requestID: String) {
        guard let pending = pendingWidgetEdit,
              pending.recovery.requestID == requestID,
              pending.recovery.phase == .submitting
        else { return }
        completeComposerEditRecovery(pending)
    }

    private func completeComposerEditRecovery(_ current: PendingWidgetEdit) {
        guard let pending = pendingWidgetEdit,
              pending.owner == current.owner,
              pending.recovery.requestID == current.recovery.requestID
        else { return }
        var completed = pending
        completed.recovery.phase = .completed
        pendingWidgetEdit = completed
        enqueueComposerEditRecoverySave(completed.recovery, owner: completed.owner) { [weak self] result in
            guard let self,
                  self.pendingWidgetEdit?.owner == completed.owner,
                  self.pendingWidgetEdit?.recovery.requestID == completed.recovery.requestID,
                  self.pendingWidgetEdit?.recovery.phase == .completed
            else { return }
            switch result {
            case .success:
                self.pendingWidgetEdit = nil
                self.stashedComposerDraft = nil
                self.cacheSelectedTranscript()
            case .failure(let error):
                self.showToast(verbatim: self.localizedErrorDescription(error), tone: .error)
            }
        }
    }

    func changeComposerDraftOwner(to owner: ComposerDraftOwner?) {
        guard owner != composerDraftOwner else { return }
        composerDraftSaveTask?.cancel()
        composerDraftSaveTask = nil
        let previousOwner = composerDraftOwner
        if var pending = pendingWidgetEdit,
           pending.owner == previousOwner,
           pending.recovery.phase == .editing,
           composer.utf8.count <= maximumComposerBytes {
            pending.recovery.editedInput = composer
            pendingWidgetEdit = pending
            enqueueComposerEditRecoverySave(pending.recovery, owner: pending.owner)
        }
        let previousDraft = ComposerDraft(
            text: pendingWidgetEdit?.recovery.displacedDraft ?? composer,
            reply: pendingWidgetEdit == nil ? composerReply : nil
        )
        pendingWidgetEdit = nil
        stashedComposerDraft = nil
        composerEditRecoveryGeneration = UUID()
        isLoadingComposerEditRecovery = false
        let previousIO = composerDraftIOTask
        let generation = UUID()
        composerDraftGeneration = generation
        composerDraftOwner = owner
        isLoadingComposerDraft = owner != nil
        suppressesComposerDraftSave = true
        composer = previousOwner == nil ? previousDraft.text : ""
        composerReply = previousOwner == nil ? previousDraft.reply : nil
        suppressesComposerDraftSave = false
        let store = store
        composerDraftIOTask = Task { [weak self] in
            await previousIO?.value
            if let previousOwner {
                await store.saveComposerDraft(
                    previousDraft,
                    accountID: previousOwner.accountID,
                    sessionID: previousOwner.sessionID
                )
            }
            guard let owner else { return }
            let restored = await store.loadComposerDraft(
                accountID: owner.accountID,
                sessionID: owner.sessionID
            )
            guard let self,
                  composerDraftGeneration == generation,
                  composerDraftOwner == owner
            else { return }
            let current = ComposerDraft(text: composer, reply: composerReply)
            let merged = restored.appending(current)
            suppressesComposerDraftSave = true
            composer = merged.text
            composerReply = merged.reply
            suppressesComposerDraftSave = false
            isLoadingComposerDraft = false
            scheduleComposerDraftSave()
        }
    }

    func discardComposerDraft() {
        composerDraftSaveTask?.cancel()
        composerDraftSaveTask = nil
        invalidateComposerEditRecovery()
        composerDraftGeneration = UUID()
        composerDraftOwner = nil
        isLoadingComposerDraft = false
        suppressesComposerDraftSave = true
        composer = ""
        suppressesComposerDraftSave = false
        composerReply = nil
    }

    func invalidateComposerEditRecovery(for owner: ComposerDraftOwner? = nil) {
        if let owner {
            guard pendingWidgetEdit?.owner == owner || composerDraftOwner == owner else { return }
        }
        composerDraftSaveTask?.cancel()
        composerDraftSaveTask = nil
        pendingWidgetEdit = nil
        stashedComposerDraft = nil
        composerEditRecoveryGeneration = UUID()
        isLoadingComposerEditRecovery = false
    }

    func restoreDraft(id: String) {
        guard let draft = pendingDrafts.removeValue(forKey: id) else { return }
        restoreDraft(draft)
    }

    func restoreDraft(_ draft: PendingComposerDraft) {
        let restored = ComposerDraft(text: draft.text, reply: draft.reply).appending(
            ComposerDraft(text: composer, reply: composerReply)
        )
        suppressesComposerDraftSave = true
        composer = restored.text
        composerReply = restored.reply
        suppressesComposerDraftSave = false
        scheduleComposerDraftSave()
        let currentIDs = Set(composerAttachments.compactMap { item -> String? in
            guard case .uploaded(let attachment) = item.state else { return nil }
            return attachment.id
        })
        let available = max(0, attachmentReferenceLimit - composerAttachments.count)
        composerAttachments.insert(contentsOf: draft.attachments
            .filter { !currentIDs.contains($0.id) }
            .prefix(available)
            .map { attachment in
                ComposerAttachment(
                    id: UUID(),
                    name: attachment.name,
                    size: attachment.size,
                    mediaType: attachment.mediaType,
                    state: .uploaded(attachment)
                )
            }, at: 0)
    }

    func restorePendingDrafts() {
        let drafts = pendingDrafts.keys.sorted().compactMap { pendingDrafts[$0] }
        pendingDrafts.removeAll()
        guard !drafts.isEmpty else { return }
        for draft in drafts.reversed() { restoreDraft(draft) }
    }
}
