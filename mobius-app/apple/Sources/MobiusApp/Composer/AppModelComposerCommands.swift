import Foundation
import Observation

extension AppModel {
    func importAttachments(_ urls: [URL]) async {
        guard canImportAttachments else { return }
        let available = max(0, attachmentReferenceLimit - chat.composerAttachments.count)
        let selectedURLs = Array(urls.prefix(available))
        if urls.count > selectedURLs.count {
            showToast(
                "You can attach up to \(attachmentReferenceLimit) files to a message.",
                tone: .warning
            )
        }
        guard !selectedURLs.isEmpty else { return }

        let imports = selectedURLs.compactMap { url in
            reserveComposerAttachment(named: url.lastPathComponent).map { ($0, url) }
        }
        for (id, url) in imports {
            await completeComposerAttachmentImport(url, reservedID: id)
        }
    }

    @discardableResult
    func reserveComposerAttachment(named name: String) -> UUID? {
        guard canImportAttachments else { return nil }
        guard chat.composerAttachments.count < attachmentReferenceLimit else {
            showToast(
                "You can attach up to \(attachmentReferenceLimit) files to a message.",
                tone: .warning
            )
            return nil
        }
        let id = UUID()
        chat.composerAttachments.append(
            ComposerAttachment(
                id: id,
                name: name,
                size: 0,
                mediaType: "application/octet-stream",
                state: .preparing
            ))
        return id
    }

    func completeComposerAttachmentImport(_ url: URL, reservedID: UUID) async {
        guard
            chat.composerAttachments.contains(where: {
                $0.id == reservedID && $0.state == .preparing
            })
        else { return }
        do {
            let imported = try await ChatSessionModel.loadImportedAttachment(
                url,
                maximumBytes: attachmentFileByteLimit
            )
            guard canImportAttachments,
                let index = chat.composerAttachments.firstIndex(where: {
                    $0.id == reservedID && $0.state == .preparing
                })
            else {
                cancelComposerAttachmentImport(reservedID)
                return
            }
            let currentBytes = chat.composerAttachments.reduce(Int64(0)) { total, attachment in
                let (sum, overflow) = total.addingReportingOverflow(attachment.size)
                return overflow || attachment.size < 0 ? .max : sum
            }
            if currentBytes > attachmentDraftByteLimit - Int64(imported.data.count) {
                cancelComposerAttachmentImport(reservedID)
                showToast(
                    AttachmentImportError.totalTooLarge(attachmentDraftByteLimit)
                        .localizedDescriptionResource,
                    tone: .error
                )
                return
            }
            chat.sessionFileData[reservedID] = imported.data
            if let thumbnail = imported.thumbnail {
                chat.cacheFileThumbnail(thumbnail, for: .composer(reservedID))
            }
            chat.composerAttachments[index].name = imported.name
            chat.composerAttachments[index].size = Int64(imported.data.count)
            chat.composerAttachments[index].mediaType = imported.mediaType
            chat.composerAttachments[index].state = .queued
            if chat.selectedSessionID == nil, chat.sessionRequestID == nil {
                createPendingSession()
            }
            chat.startNextSessionFileUpload()
        } catch {
            guard cancelComposerAttachmentImport(reservedID) else { return }
            if let error = error as? AttachmentImportError {
                showToast(error.localizedDescriptionResource, tone: .error)
            } else {
                showToast(verbatim: localizedErrorDescription(error), tone: .error)
            }
        }
    }

    @discardableResult
    func cancelComposerAttachmentImport(_ id: UUID) -> Bool {
        guard
            let index = chat.composerAttachments.firstIndex(where: {
                $0.id == id && $0.state == .preparing
            })
        else { return false }
        chat.composerAttachments.remove(at: index)
        return true
    }

    func removeComposerAttachment(_ id: UUID) {
        guard let attachment = chat.composerAttachments.first(where: { $0.id == id }) else {
            return
        }
        chat.discardComposerAttachment(attachment)
        chat.startNextSessionFileUpload()
    }

    func retryComposerAttachment(_ id: UUID) {
        guard chat.sessionFileData[id] != nil,
            let index = chat.composerAttachments.firstIndex(where: { $0.id == id }),
            case .failed = chat.composerAttachments[index].state
        else { return }
        chat.composerAttachments[index].state = .queued
        chat.startNextSessionFileUpload()
    }

    @discardableResult
    func sendMessage(delivery requestedDelivery: ActiveMessageDelivery? = nil) -> Bool {
        guard gateway.connectionState.isReady,
            chat.sessionRequestID == nil
        else { return false }
        let text = chat.composer.trimmingCharacters(in: .whitespacesAndNewlines)
        let attachments = uploadedComposerAttachments
        let reply = chat.composerReply
        guard chat.composerAttachments.count <= attachmentReferenceLimit else { return false }
        guard !text.isEmpty || !chat.composerAttachments.isEmpty else { return false }
        guard chat.composerAttachments.isEmpty || canSubmitAttachments else {
            showToast(attachmentSubmissionUnavailableMessage, tone: .warning)
            return false
        }
        guard canSendComposer else { return false }
        guard text.utf8.count <= maximumComposerBytes else {
            showToast("Messages are limited to 1 MiB.", tone: .error)
            return false
        }
        if chat.pendingWidgetEdit == nil, text.hasPrefix("/") {
            return sendComposerCommand(text)
        }
        if chat.activeTurnID != nil, !attachments.isEmpty {
            showToast("Attachments can be sent with a new turn.", tone: .warning)
            return false
        }
        if chat.selectedSessionID == nil {
            guard let requestID = createPendingSession() else { return false }
            chat.dismissComposerFocus()
            chat.pendingDrafts[requestID] = PendingComposerDraft(
                text: text,
                attachments: attachments,
                reply: reply
            )
            chat.composerDraftSaveTask?.cancel()
            chat.composerDraftSaveTask = nil
            chat.suppressesComposerDraftSave = true
            chat.composer = ""
            chat.suppressesComposerDraftSave = false
            chat.composerReply = nil
            return true
        }
        guard !composerHasUnfinishedAttachments else {
            showToast("Wait for attachments to finish uploading.", tone: .warning)
            return false
        }
        return chat.submitExistingSessionMessage(
            text: text,
            attachments: attachments,
            reply: reply,
            requestedDelivery: requestedDelivery
        )
    }

    private func sendComposerCommand(_ text: String) -> Bool {
        let parts = text.dropFirst().split(maxSplits: 1, whereSeparator: \.isWhitespace)
        guard let name = parts.first, !name.isEmpty else { return false }
        guard
            let contribution = chat.contributions.first(where: {
                $0.commands.contains { $0.name == name }
            }), let command = contribution.commands.first(where: { $0.name == name })
        else {
            showToast("Unknown command /\(name).", tone: .warning)
            return false
        }
        guard !command.requiresIdle || chat.activeTurnID == nil else {
            showToast("/\(name) is available when the agent is idle.", tone: .warning)
            return false
        }
        guard chat.composerAttachments.isEmpty, chat.composerReply == nil else {
            showToast(
                "Send attachments and replies as a message before using a command.", tone: .warning)
            return false
        }
        guard let sessionID = chat.selectedSessionID else { return false }
        let id = requestID("command")
        chat.pendingDrafts[id] = PendingComposerDraft(text: chat.composer, attachments: [])
        chat.composer = ""
        chat.dismissComposerFocus()
        gateway.transmit(
            .submit(
                sessionID: sessionID,
                submission: Submission(
                    id: id,
                    op: .capabilityCommand(
                        capability: contribution.capability,
                        command: command.name,
                        arguments: parts.count == 2
                            ? parts[1].trimmingCharacters(in: .whitespacesAndNewlines) : "",
                        input: nil,
                        target: nil
                    )
                ))
        ) { [weak self] _ in
            self?.chat.restoreDraft(id: id)
        }
        return true
    }

    var activeMessageDelivery: ActiveMessageDelivery {
        for feature in middlewareFeatures {
            for setting in feature.settings {
                guard case .select(let options, _) = setting.kind,
                    Set(options.compactMap { ActiveMessageDelivery(rawValue: $0.value) })
                        == Set(ActiveMessageDelivery.allCases),
                    let value = agentDraft?.middleware.settings[feature.id]?[setting.id],
                    case .string(let rawValue) = value,
                    let delivery = ActiveMessageDelivery(rawValue: rawValue)
                else { continue }
                return delivery
            }
        }
        return .steer
    }

    func editWidgetInputInComposer(_ mounted: MountedWidget) {
        guard gateway.connectionState.isReady,
            !chat.isLoadingComposerDraft,
            !chat.isLoadingComposerEditRecovery,
            let sessionID = chat.selectedSessionID,
            let accountID = gateway.selectedAccountID,
            let operation = mounted.widget.action,
            let input = operation.capabilityInput
        else { return }
        guard chat.composerAttachments.isEmpty else {
            showToast(
                "Finish the attachment draft before editing a queued message.", tone: .warning)
            return
        }
        guard chat.composerReply == nil else {
            showToast("Finish the reply draft before editing a queued message.", tone: .warning)
            return
        }
        guard chat.pendingWidgetEdit == nil, chat.stashedComposerDraft == nil else { return }
        chat.flushComposerDraft()
        let requestID = requestID("edit")
        let owner = ComposerDraftOwner(accountID: accountID, sessionID: sessionID)
        let recovery = ComposerEditRecovery(
            capability: mounted.capability,
            widgetID: mounted.widget.id,
            originalInput: input,
            displacedDraft: chat.composer,
            editedInput: input,
            requestID: requestID,
            submissionBaselineSequence: nil,
            phase: .removingQueuedInput
        )
        chat.pendingWidgetEdit = PendingWidgetEdit(owner: owner, recovery: recovery)
        chat.enqueueComposerEditRecoverySave(recovery, owner: owner) { [weak self] result in
            guard let self,
                self.chat.pendingWidgetEdit?.owner == owner,
                self.chat.pendingWidgetEdit?.recovery.requestID == requestID
            else { return }
            if case .failure(let error) = result {
                self.chat.pendingWidgetEdit = nil
                self.showToast(verbatim: self.localizedErrorDescription(error), tone: .error)
                return
            }
            guard self.gateway.connectionState.isReady, self.chat.selectedSessionID == sessionID
            else { return }
            guard self.gateway.selectedAccountID == accountID else { return }
            self.gateway.transmit(
                .submit(
                    sessionID: sessionID,
                    submission: Submission(id: requestID, op: operation)
                ))
        }
    }
}
