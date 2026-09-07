import Foundation

extension ChatSessionModel {
    /// Starts the on-device rewrite with the submitted first message. The task is stored,
    /// but deliberately not awaited, so the gateway turn and Foundation Models run together.
    func startChatTitle(
        prompt submittedPrompt: String,
        submissionID: String,
        sessionID: String
    ) {
        let prompt = submittedPrompt.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let previewTitle = ChatTitleWriter.preview(for: prompt),
            titleEligibleSessionIDs.contains(sessionID)
                || (pendingChatTitles[sessionID] == nil
                    && sessions.contains(where: {
                        $0.sessionId == sessionID
                            && $0.explicitTitle == nil
                            && ($0.firstUserMessage ?? "")
                                .trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                    })),
            let accountID = gateway.selectedAccountID
        else { return }
        guard sessions.first(where: { $0.sessionId == sessionID })?.explicitTitle == nil
        else {
            titleEligibleSessionIDs.remove(sessionID)
            return
        }
        let attempt = ChatTitleAttempt(
            accountID: accountID,
            sessionID: sessionID,
            submissionID: submissionID,
            prompt: prompt
        )
        pendingChatTitles[sessionID] = PendingChatTitle(
            attempt: attempt,
            previewTitle: previewTitle,
            generatedTitle: nil,
            renameRequestID: nil,
            submissionConfirmed: false
        )
        titleEligibleSessionIDs.remove(sessionID)
        let titleWriter = titleWriter
        let locale = locale
        chatTitleTasks[sessionID] = Task { [weak self] in
            let outcome = await titleWriter.title(for: prompt, locale: locale) {
                [weak self] message in
                self?.showToast(verbatim: message, tone: .warning)
            }
            guard let self else { return }
            self.finishChatTitle(outcome, attempt: attempt)
        }
    }

    func reconcileChatTitleAfterReplay() {
        guard let sessionID = selectedSessionID,
            let pending = pendingChatTitles[sessionID],
            !pending.submissionConfirmed
        else { return }
        let promptWasReplayed =
            replayCompletionSubmissionIDs.contains(
                pending.attempt.submissionID
            )
            || replayUserMessages.contains {
                $0.text.trimmingCharacters(in: .whitespacesAndNewlines) == pending.attempt.prompt
            }
        if promptWasReplayed {
            confirmChatTitle(sessionID: sessionID)
        } else {
            cancelChatTitle(sessionID, rearm: true)
        }
    }

    private func finishChatTitle(_ outcome: ChatTitleWriter.Outcome, attempt: ChatTitleAttempt) {
        guard pendingChatTitles[attempt.sessionID]?.attempt == attempt else { return }
        chatTitleTasks.removeValue(forKey: attempt.sessionID)
        guard !Task.isCancelled, gateway.selectedAccountID == attempt.accountID
        else {
            pendingChatTitles.removeValue(forKey: attempt.sessionID)
            return
        }
        switch outcome {
        case .title(let title):
            pendingChatTitles[attempt.sessionID]?.generatedTitle = title
        case .failed(let message):
            showToast(verbatim: message, tone: .warning)
        case .cancelled:
            break
        }
        reconcileChatTitles()
    }

    func confirmChatTitle(submissionID: String) {
        guard
            let sessionID = pendingChatTitles.first(where: {
                $0.value.attempt.submissionID == submissionID
            })?.key
        else { return }
        confirmChatTitle(sessionID: sessionID)
    }

    func confirmChatTitle(sessionID: String) {
        guard pendingChatTitles[sessionID] != nil else { return }
        pendingChatTitles[sessionID]?.submissionConfirmed = true
        persistGeneratedChatTitles()
    }

    func reconcileChatTitles() {
        for sessionID in Array(pendingChatTitles.keys) {
            guard let pending = pendingChatTitles[sessionID] else { continue }
            guard pending.attempt.accountID == gateway.selectedAccountID else {
                cancelChatTitle(sessionID)
                continue
            }
            guard let session = sessions.first(where: { $0.sessionId == sessionID }) else {
                continue
            }
            if let durableTitle = session.explicitTitle {
                if durableTitle == pending.generatedTitle {
                    completeChatTitle(sessionID)
                } else {
                    // An explicit user or another client always wins.
                    cancelChatTitle(sessionID)
                }
                continue
            }

            let catalogPrompt = (session.firstUserMessage ?? "")
                .trimmingCharacters(in: .whitespacesAndNewlines)
            if !catalogPrompt.isEmpty {
                guard pending.attempt.prompt.hasPrefix(catalogPrompt) else {
                    cancelChatTitle(sessionID)
                    continue
                }
                if !pending.submissionConfirmed {
                    pendingChatTitles[sessionID]?.submissionConfirmed = true
                }
                if pending.generatedTitle == nil, chatTitleTasks[sessionID] == nil {
                    // The catalog now owns the same deterministic preview, so the temporary
                    // override is no longer needed.
                    completeChatTitle(sessionID)
                }
            } else if let requestID = pending.renameRequestID,
                requestID != sessionMutationRequestID
            {
                // The mutation slot cleared without the generated title reaching the catalog.
                cancelChatTitle(sessionID)
            }
        }
        persistGeneratedChatTitles()
    }

    func persistGeneratedChatTitles() {
        guard gateway.connectionState.isReady,
            sessionMutationRequestID == nil,
            let accountID = gateway.selectedAccountID
        else { return }

        for sessionID in pendingChatTitles.keys.sorted() {
            guard var pending = pendingChatTitles[sessionID],
                pending.attempt.accountID == accountID,
                let title = pending.generatedTitle,
                pending.submissionConfirmed,
                pending.renameRequestID == nil
            else { continue }
            if sessions.first(where: { $0.sessionId == sessionID })?.explicitTitle != nil {
                cancelChatTitle(sessionID)
                continue
            }
            guard
                let requestID = requestSessionRename(
                    sessionID: sessionID,
                    title: title,
                    generatedTitleSessionID: sessionID
                )
            else { return }
            pending.renameRequestID = requestID
            pendingChatTitles[sessionID] = pending
            return
        }
    }

    func cancelChatTitle(_ sessionID: String, rearm: Bool = false) {
        chatTitleTasks.removeValue(forKey: sessionID)?.cancel()
        pendingChatTitles.removeValue(forKey: sessionID)
        if rearm { titleEligibleSessionIDs.insert(sessionID) }
    }

    func cancelChatTitle(submissionID: String, rearm: Bool) {
        guard
            let sessionID = pendingChatTitles.first(where: {
                $0.value.attempt.submissionID == submissionID
            })?.key
        else { return }
        cancelChatTitle(sessionID, rearm: rearm)
    }

    private func completeChatTitle(_ sessionID: String) {
        chatTitleTasks.removeValue(forKey: sessionID)?.cancel()
        pendingChatTitles.removeValue(forKey: sessionID)
        titleEligibleSessionIDs.remove(sessionID)
    }

    func prepareChatTitle(for sessionID: String) {
        cancelChatTitle(sessionID)
        titleEligibleSessionIDs.insert(sessionID)
    }

}
