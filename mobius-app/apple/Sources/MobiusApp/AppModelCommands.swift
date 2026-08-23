import Foundation

extension AppModel {
    func start() {
        guard let account = selectedAccount else {
            #if DEBUG
            if !pairingCode.isEmpty, !pairingEndpoint.isEmpty { pair(); return }
            #endif
            showsPairing = true
            return
        }
        connect(to: account)
    }

    func applyPairingSetup(_ rawValue: String) {
        prefillPairing { try GatewayPairingSetup(rawValue) }
    }

    func applyPairingSetup(_ setup: GatewayPairingSetup) {
        prefillPairing { setup }
    }

    func applyPairingURL(_ url: URL) {
        prefillPairing { try GatewayPairingSetup(url: url) }
    }

    func handleOpenURL(_ url: URL) {
        applyPairingURL(url)
    }

    private func prefillPairing(_ parse: () throws -> GatewayPairingSetup) {
        cancelReconnect()
        showsPairing = true
        do {
            let setup = try parse()
            pairingEndpoint = setup.endpoint.rawValue
            pairingCode = setup.code
            pairingError = nil
        } catch {
            pairingError = error.localizedDescription
        }
    }

    func pair() {
        cancelReconnect()
        automaticReconnectBlocked = false
        pairingError = nil
        do {
            let endpoint = try GatewayEndpoint(pairingEndpoint)
            let code = pairingCode.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !code.isEmpty else {
                let message = "Enter the one-time code shown by the gateway."
                pairingError = message
                showToast(message, tone: .error)
                return
            }
            let account = accounts.first(where: { $0.endpoint == endpoint })
                ?? GatewayAccount(endpoint: endpoint)
            let sameGateway = account.id == selectedAccountID
            let sessionID = sameGateway ? presentedChatSessionID : nil
            let generation = resetGatewayState(
                preservingDrafts: sameGateway,
                preservingSession: sessionID != nil
            )
            sessionToRestoreID = sessionID
            pendingPairingAccount = account
            beginConnection(to: endpoint, generation: generation) { [weak self] in
                guard let self, self.connectionGeneration == generation else { return }
                try await self.requestSender(.pair(
                    code: code,
                    clientLabel: "möbius Apple",
                    clientKind: .currentApplePlatform
                ))
            }
        } catch {
            pairingError = error.localizedDescription
            showToast(error.localizedDescription, tone: .error)
        }
    }

    func selectAccount(_ id: UUID?) {
        guard let id, let account = accounts.first(where: { $0.id == id }) else { return }
        connect(to: account)
    }

    func renameGateway(_ account: GatewayAccount, to name: String) {
        do {
            let renamed = try store.rename(account, to: name)
            guard let index = accounts.firstIndex(where: { $0.id == renamed.id }) else { return }
            accounts[index] = renamed
            showToast("Gateway renamed.", tone: .success)
        } catch {
            showToast(error.localizedDescription, tone: .error)
        }
    }

    func reconnect() {
        guard let account = selectedAccount else { return }
        connect(to: account)
    }

    func setSceneActive(_ active: Bool) {
        guard active else {
            cancelReconnect()
            reconnectsOnActivation = true
            return
        }
        guard reconnectsOnActivation, pendingPairingAccount == nil else { return }
        reconnectsOnActivation = false
        reconnect()
    }

    func repairSelectedGateway() {
        guard let account = selectedAccount else {
            showsPairing = true
            return
        }
        pairingEndpoint = account.endpoint.rawValue
        pairingCode = ""
        pairingError = "Enter a new one-time code to repair this pairing."
        showsPairing = true
    }

    func chooseWorkspace(_ selectedPath: String) {
        let path = selectedPath.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !path.isEmpty else {
            workspaceError = "Choose a folder on the gateway host."
            return
        }
        guard canCreateSession else { return }
        sessionToRestoreID = nil
        sessionOpenCursor = nil
        let id = requestID("create")
        sessionRequestID = id
        workspaceError = nil
        isChangingWorkspace = true
        connectionState = .loading
        transmit(.createSession(requestID: id, workspace: path)) { [weak self] message in
            self?.sessionRequestID = nil
            self?.isChangingWorkspace = false
            self?.connectionState = .ready
            self?.workspaceError = message
        }
    }

    func openWorkspaceBrowser() {
        guard canCreateSession else { return }
        showsWorkspaceBrowser = true
        loadDirectory(workspace?.path ?? (selectedGatewayIsMobiusCloud ? "." : "/"))
    }

    func loadDirectory(_ path: String) {
        let id = requestID("directories")
        directoryRequestID = id
        directoryError = nil
        isLoadingDirectories = true
        transmit(.listDirectories(requestID: id, path: path, includeFiles: false)) { [weak self] message in
            guard self?.directoryRequestID == id else { return }
            self?.directoryRequestID = nil
            self?.isLoadingDirectories = false
            self?.directoryError = message
        }
    }

    func createWorkspaceDirectory(named rawName: String) {
        let name = rawName.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty else {
            directoryError = "Enter a folder name."
            return
        }
        guard name != ".", name != "..", !name.contains("/"), !name.contains("\\") else {
            directoryError = "Enter a single folder name."
            return
        }
        guard let parent = directoryListing?.path, canCreateSession else { return }
        let id = requestID("create-directory")
        directoryRequestID = id
        directoryError = nil
        isLoadingDirectories = true
        transmit(.createWorkspaceDirectory(requestID: id, parent: parent, name: name)) {
            [weak self] message in
            guard self?.directoryRequestID == id else { return }
            self?.directoryRequestID = nil
            self?.isLoadingDirectories = false
            self?.directoryError = message
        }
    }

    /// Removing a gateway only tears down the connection when it is the active one.
    func forgetGateway(_ account: GatewayAccount) {
        Task { [weak self] in
            _ = await self?.removeGateway(account)
        }
    }

    func removeGateway(_ account: GatewayAccount) async -> Bool {
        let isActive = account.id == selectedAccountID
        let pendingDraftIO = isActive ? composerDraftIOTask : nil
        if isActive {
            cancelReconnect()
            discardComposerDraft()
        }
        do {
            await pendingDraftIO?.value
            try await store.remove(account)
            accounts.removeAll { $0.id == account.id }
            if isActive {
                selectedAccountID = nil
                if let next = accounts.first {
                    connect(to: next)
                } else {
                    resetGatewayState(preservingDrafts: false)
                    await client.disconnect()
                    showsPairing = true
                }
            }
            showToast("Gateway removed.", tone: .info)
            return true
        } catch {
            showToast(error.localizedDescription, tone: .error)
            return false
        }
    }

    func openNewSession() {
        guard canCreateSession else { return }
        destination = .chats
        navigationPath = []
        openWorkspaceBrowser()
    }

    func openNewSessionInCurrentWorkspace() {
        guard let path = workspace?.path else { return }
        chooseWorkspace(path)
    }

    func openChat(_ sessionID: String) {
        guard canOpenSession || sessionID == selectedSessionID else { return }
        destination = .chats
        openSession(sessionID)
        navigationPath = [.chat(.session(sessionID))]
    }

    func openSession(_ sessionID: String) {
        guard canOpenSession, sessionID != selectedSessionID else { return }
        let generation = UUID()
        transcriptLoadGeneration = generation
        let accountID = selectedAccountID
        let previous = transcriptIOTask
        transcriptIOTask = Task { [weak self, store] in
            await previous?.value
            let cached: CachedTranscript? = if let accountID {
                await store.loadTranscript(accountID: accountID, sessionID: sessionID)
            } else {
                nil
            }
            guard let self,
                  generation == transcriptLoadGeneration,
                  accountID == selectedAccountID,
                  canOpenSession,
                  sessionID != selectedSessionID
            else { return }
            requestSessionOpen(
                sessionID,
                lastSequence: cached?.sequence,
                cachedTranscript: cached,
                presentedTranscript: cached?.transcript
            )
        }
    }

    func loadEarlierHistory() {
        guard canLoadEarlierHistory else { return }
        let window = transcriptWindow
        if window.hasEarlierEntries {
            transcriptWindowAnchor = .visibleTurns(
                window.turnCount + transcriptTurnsPerPage
            )
            _ = transcriptWindow
            historyLoadCompletionRevision &+= 1
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
        transmit(.getSessionHistory(
            requestID: id,
            sessionID: sessionID,
            beforeSequence: beforeSequence
        )) { [weak self] _ in
            guard self?.historyRequestID == id else { return }
            self?.finishHistoryLoad()
        }
    }

    func finishHistoryLoad() {
        let wasLoading = historyRequestID != nil || isLoadingEarlierHistory
        historyRequestID = nil
        isLoadingEarlierHistory = false
        if wasLoading { historyLoadCompletionRevision &+= 1 }
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
        presentedTranscript: [TranscriptEntry]? = nil
    ) {
        replayCompletionSubmissionIDs.removeAll(keepingCapacity: true)
        replayUserMessages.removeAll(keepingCapacity: true)
        completedComposerEditReplay = false
        if sessionID != selectedSessionID {
            discardComposerAttachments()
            discardFilePresentation()
            cancelSessionFileThumbnailDownloads()
        }
        sessionToRestoreID = nil
        sessionOpeningID = sessionID
        sessionOpenCursor = lastSequence
        pendingCachedTranscript = cachedTranscript
        pendingPresentedTranscript = presentedTranscript
        let id = requestID("open")
        sessionRequestID = id
        connectionState = .loading
        transmit(.openSession(
            requestID: id,
            sessionID: sessionID,
            lastSequence: lastSequence
        )) { [weak self] _ in
            guard self?.sessionRequestID == id else { return }
            self?.sessionRequestID = nil
            self?.sessionOpeningID = nil
            self?.sessionOpenCursor = nil
            self?.pendingCachedTranscript = nil
            self?.pendingPresentedTranscript = nil
            self?.connectionState = .ready
        }
    }

    // Renaming, pinning and deleting address a session by id, so they work on any chat in the
    // catalogue rather than only the open one.
    @discardableResult
    func renameSession(_ session: SessionRecord, title: String) -> String? {
        let title = title.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !title.isEmpty else { return nil }
        guard sessionMutationRequestID == nil else {
            showToast("Another chat update is finishing.", tone: .info)
            return nil
        }
        cancelChatTitle(session.sessionId)
        return requestSessionRename(sessionID: session.sessionId, title: title)
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
        transmit(.renameSession(
            requestID: id,
            sessionID: sessionID,
            title: title
        )) { [weak self] _ in
            guard let self else { return }
            if self.sessionMutationRequestID == id { self.sessionMutationRequestID = nil }
            if let generatedTitleSessionID,
               self.pendingChatTitles[generatedTitleSessionID]?.renameRequestID == id {
                self.cancelChatTitle(generatedTitleSessionID)
            }
        }
        return id
    }

    func setSessionPinned(_ session: SessionRecord, pinned: Bool) {
        guard sessionMutationRequestID == nil else { return }
        let id = requestID("session-pin")
        sessionMutationRequestID = id
        transmit(.setSessionPinned(
            requestID: id,
            sessionID: session.sessionId,
            pinned: pinned
        )) { [weak self] _ in
            if self?.sessionMutationRequestID == id { self?.sessionMutationRequestID = nil }
        }
    }

    func deleteSession(_ session: SessionRecord) {
        guard sessionMutationRequestID == nil else { return }
        let deletesSelectedSession = session.sessionId == selectedSessionID
        let deletesPresentedSession = presentedChatSessionID == session.sessionId
        if let accountID = selectedAccountID {
            enqueueTranscriptIO { [store] in
                await store.removeTranscript(accountID: accountID, sessionID: session.sessionId)
            }
        }
        let id = requestID("session-delete")
        sessionMutationRequestID = id
        pendingDeletedSessionID = session.sessionId
        pendingDeletedPresentedSessionID = deletesPresentedSession ? session.sessionId : nil
        transmit(.deleteSession(
            requestID: id,
            sessionID: session.sessionId
        )) { [weak self] _ in
            guard let self, self.sessionMutationRequestID == id else { return }
            let sessionID = self.pendingDeletedPresentedSessionID
            self.sessionMutationRequestID = nil
            self.pendingDeletedSessionID = nil
            self.pendingDeletedPresentedSessionID = nil
            self.restoreDeletedPresentedSession(sessionID)
        }
        if deletesSelectedSession { clearSelectedSession() }
    }

    func restoreDeletedPresentedSession(_ sessionID: String?) {
        guard let sessionID,
              destination == .chats,
              navigationPath.isEmpty
        else { return }
        navigationPath = [.chat(.session(sessionID))]
        restoreSession(sessionID)
    }

    func refreshWorkspaceChanges() {
        refreshGitDiff()
        if showsInspector, filesInspectorTab == .modified {
            switch modifiedFilesScope {
            case .lastTurn: break
            case .unstaged: break
            case .staged: refreshStagedGitDiff()
            case .committed: refreshCommittedGitDiff()
            }
        }
        refreshWorkspaceFiles()
    }

    func refreshGitDiff() {
        guard connectionState.isReady, let sessionID = selectedSessionID else { return }
        let id = requestID("git-diff")
        gitDiffRequestID = id
        isLoadingGitDiff = true
        transmit(.getGitDiff(requestID: id, sessionID: sessionID, scope: .unstaged)) { [weak self] _ in
            guard self?.gitDiffRequestID == id else { return }
            self?.gitDiffRequestID = nil
            self?.isLoadingGitDiff = false
        }
    }

    func refreshStagedGitDiff() {
        guard connectionState.isReady, let sessionID = selectedSessionID else { return }
        let id = requestID("staged-git-diff")
        stagedGitDiffRequestID = id
        isLoadingStagedGitDiff = true
        transmit(.getGitDiff(requestID: id, sessionID: sessionID, scope: .staged)) { [weak self] _ in
            guard self?.stagedGitDiffRequestID == id else { return }
            self?.stagedGitDiffRequestID = nil
            self?.isLoadingStagedGitDiff = false
        }
    }

    func refreshCommittedGitDiff() {
        guard connectionState.isReady, let sessionID = selectedSessionID else { return }
        let id = requestID("committed-git-diff")
        committedGitDiffRequestID = id
        isLoadingCommittedGitDiff = true
        transmit(.getGitDiff(requestID: id, sessionID: sessionID, scope: .committed)) { [weak self] _ in
            guard self?.committedGitDiffRequestID == id else { return }
            self?.committedGitDiffRequestID = nil
            self?.isLoadingCommittedGitDiff = false
        }
    }

    func selectFilesInspectorTab(_ tab: FilesInspectorTab) {
        guard filesInspectorTab != tab else { return }
        filesInspectorTab = tab
        refreshFiles(for: tab)
    }

    func selectModifiedFilesScope(_ scope: ModifiedFilesScope) {
        guard modifiedFilesScope != scope else { return }
        modifiedFilesScope = scope
        refreshModifiedFiles(scope)
    }

    func refreshWorkspaceFiles() {
        guard connectionState.isReady,
              let sessionID = selectedSessionID
        else { return }
        let id = requestID("workspace-files")
        workspaceFilesRequestID = id
        workspaceFilesTruncated = false
        isLoadingWorkspaceFiles = true
        transmit(.listWorkspaceFiles(
            requestID: id,
            sessionID: sessionID,
            scope: .all
        )) { [weak self] _ in
            guard self?.workspaceFilesRequestID == id else { return }
            self?.workspaceFilesRequestID = nil
            self?.isLoadingWorkspaceFiles = false
        }
    }

    func switchGitBranch(to branch: String) {
        guard canModifySelectedSession,
              let sessionID = selectedSessionID,
              let gitStatus,
              branch != gitStatus.currentBranch,
              gitStatus.branches.contains(branch)
        else { return }
        let id = requestID("git-branch")
        gitBranchRequestID = id
        transmit(.switchGitBranch(requestID: id, sessionID: sessionID, branch: branch)) { [weak self] _ in
            if self?.gitBranchRequestID == id { self?.gitBranchRequestID = nil }
        }
    }

    func importAttachments(_ urls: [URL]) async {
        guard canImportAttachments, let sessionID = selectedSessionID else { return }
        let generation = attachmentImportGeneration
        let available = max(
            0,
            maximumSessionFileReferences - composerAttachments.count - attachmentImportReservations
        )
        let selectedURLs = Array(urls.prefix(available))
        if urls.count > selectedURLs.count {
            showToast("You can attach up to 16 files to a message.", tone: .warning)
        }
        guard !selectedURLs.isEmpty else { return }

        var reservedCount = selectedURLs.count
        attachmentImportReservations += reservedCount
        defer { attachmentImportReservations -= reservedCount }
        for url in selectedURLs {
            guard generation == attachmentImportGeneration else { return }
            do {
                let imported = try await Self.loadImportedAttachment(url)
                attachmentImportReservations -= 1
                reservedCount -= 1
                guard generation == attachmentImportGeneration,
                      sessionID == selectedSessionID,
                      canImportAttachments
                else { return }
                let currentBytes = composerAttachments.reduce(Int64(0)) { total, attachment in
                    let (sum, overflow) = total.addingReportingOverflow(attachment.size)
                    return overflow || attachment.size < 0 ? .max : sum
                }
                if currentBytes > maximumComposerAttachmentBytes - Int64(imported.data.count) {
                    showToast(AttachmentImportError.totalTooLarge.localizedDescription, tone: .error)
                    continue
                }
                let id = UUID()
                sessionFileData[id] = imported.data
                if let thumbnail = imported.thumbnail {
                    cacheFileThumbnail(thumbnail, for: .composer(id))
                }
                composerAttachments.append(ComposerAttachment(
                    id: id,
                    name: imported.name,
                    size: Int64(imported.data.count),
                    mediaType: imported.mediaType,
                    state: .queued
                ))
            } catch {
                attachmentImportReservations -= 1
                reservedCount -= 1
                guard generation == attachmentImportGeneration else { return }
                showToast(error.localizedDescription, tone: .error)
            }
        }
        startNextSessionFileUpload()
    }

    func removeComposerAttachment(_ id: UUID) {
        guard activeSessionFileUpload?.localID != id else { return }
        sessionFileData[id] = nil
        removeFileThumbnail(for: .composer(id))
        composerAttachments.removeAll { $0.id == id }
    }

    func retryComposerAttachment(_ id: UUID) {
        guard sessionFileData[id] != nil,
              let index = composerAttachments.firstIndex(where: { $0.id == id }),
              case .failed = composerAttachments[index].state
        else { return }
        composerAttachments[index].state = .queued
        startNextSessionFileUpload()
    }

    func refreshSessionFiles() {
        guard connectionState.isReady, let sessionID = selectedSessionID else { return }
        let id = requestID("session-files")
        sessionFilesRequestID = id
        isLoadingSessionFiles = true
        transmit(.listSessionFiles(requestID: id, sessionID: sessionID)) { [weak self] _ in
            guard self?.sessionFilesRequestID == id else { return }
            self?.sessionFilesRequestID = nil
            self?.isLoadingSessionFiles = false
        }
    }

    func previewSessionFile(_ file: SessionFileReference) {
        downloadSessionFile(file, purpose: .preview)
    }

    func saveOrShareSessionFile(_ file: SessionFileReference) {
        downloadSessionFile(file, purpose: .share)
    }

    private func downloadSessionFile(
        _ file: SessionFileReference,
        purpose: SessionFileDownloadPurpose
    ) {
        guard let sessionID = selectedSessionID else { return }
        guard file.size <= Int64(maximumPresentedFileBytes) else {
            showToast("File downloads are limited to 50 MiB.", tone: .warning)
            return
        }
        discardFilePresentation()
        returnsToFilesAfterFilePresentation = showsInspector
        let id = requestID("session-file-read")
        let generation = UUID()
        filePresentationGeneration = generation
        sessionFileDownload = SessionFileDownload(
            generation: generation,
            file: file,
            sessionID: sessionID,
            purpose: purpose,
            data: Data(),
            requestID: id
        )
        isLoadingFilePresentation = true
        transmit(.readSessionFile(
            requestID: id,
            sessionID: sessionID,
            fileID: file.id,
            offset: 0,
            maxBytes: 256 * 1024
        )) { [weak self] message in
            guard self?.sessionFileDownload?.requestID == id else { return }
            self?.sessionFileDownload = nil
            self?.isLoadingFilePresentation = false
            self?.showToast(message, tone: .error)
        }
    }

    func previewWorkspaceFile(_ file: WorkspaceFileRecord) {
        guard let sessionID = selectedSessionID else { return }
        guard file.size <= UInt64(maximumPresentedFileBytes) else {
            showToast("Quick Look previews are limited to 50 MiB.", tone: .warning)
            return
        }
        discardFilePresentation()
        returnsToFilesAfterFilePresentation = showsInspector
        let id = requestID("workspace-file-read")
        let generation = UUID()
        filePresentationGeneration = generation
        workspaceFilePreviewDownload = WorkspaceFilePreviewDownload(
            generation: generation,
            file: file,
            sessionID: sessionID,
            data: Data(),
            requestID: id
        )
        isLoadingFilePresentation = true
        transmit(.readWorkspaceFile(
            requestID: id,
            sessionID: sessionID,
            path: file.path,
            offset: 0,
            maxBytes: 256 * 1024
        )) { [weak self] message in
            guard self?.workspaceFilePreviewDownload?.requestID == id else { return }
            self?.workspaceFilePreviewDownload = nil
            self?.isLoadingFilePresentation = false
            self?.showToast(message, tone: .error)
        }
    }

    func createWorkspaceFile() {
        guard canModifySelectedSession, let sessionID = selectedSessionID else { return }
        discardFilePresentation()
        returnsToFilesAfterFilePresentation = showsInspector
        revealFilePresentation()
        let id = UUID()
        filePresentationGeneration = id
        textFilePreview = TextFilePreview(
            id: id,
            name: "New File",
            contents: "",
            workspaceSessionID: sessionID,
            workspacePath: ""
        )
    }

    func saveWorkspaceFile(sessionID: String, path: String, content: String) {
        guard canModifySelectedSession,
              selectedSessionID == sessionID,
              workspaceFileWriteRequestID == nil,
              path.utf8.count <= 4_096,
              !path.isEmpty,
              content.utf8.count <= maximumWorkspaceTextFileBytes
        else { return }
        let id = requestID("workspace-file-write")
        workspaceFileWriteRequestID = id
        isSavingWorkspaceFile = true
        transmit(.writeWorkspaceFile(
            requestID: id,
            sessionID: sessionID,
            path: path,
            content: content
        )) { [weak self] message in
            guard self?.workspaceFileWriteRequestID == id else { return }
            self?.workspaceFileWriteRequestID = nil
            self?.isSavingWorkspaceFile = false
            self?.showToast(message, tone: .error)
        }
    }

    func updateWorkspaceFileDraft(id: UUID, path: String) {
        guard var draft = textFilePreview,
              draft.id == id,
              draft.workspaceSessionID != nil,
              draft.workspacePath != nil
        else { return }
        draft.workspacePath = path
        textFilePreview = draft
    }

    func updateWorkspaceFileDraft(id: UUID, contents: String) {
        guard var draft = textFilePreview,
              draft.id == id,
              draft.workspaceSessionID != nil,
              draft.workspacePath != nil
        else { return }
        draft.contents = contents
        textFilePreview = draft
    }

    func discardFilePresentation(preservingWorkspaceTextDraft: Bool = false) {
        filePresentationGeneration = UUID()
        sessionFileDownload = nil
        workspaceFilePreviewDownload = nil
        isLoadingFilePresentation = false
        if let previewTemporaryDirectory {
            Task.detached(priority: .utility) {
                try? FileManager.default.removeItem(at: previewTemporaryDirectory)
            }
        }
        previewTemporaryDirectory = nil
        previewURL = nil
        if !preservingWorkspaceTextDraft || textFilePreview?.workspaceSessionID == nil {
            textFilePreview = nil
        }
        sessionFileShareItem = nil
        if textFilePreview == nil { returnsToFilesAfterFilePresentation = false }
    }

    func closeFilePresentation() {
        let returnsToFiles = returnsToFilesAfterFilePresentation
        discardFilePresentation()
        if returnsToFiles { showsInspector = true }
    }

    func revealFilePresentation() {
        if returnsToFilesAfterFilePresentation { showsInspector = false }
    }

    func sendMessage() {
        guard connectionState.isReady,
              sessionRequestID == nil,
              let sessionID = selectedSessionID
        else { return }
        let text = composer.trimmingCharacters(in: .whitespacesAndNewlines)
        let attachments = uploadedComposerAttachments
        guard attachments.count <= maximumSessionFileReferences else { return }
        guard !text.isEmpty || !attachments.isEmpty else { return }
        guard attachments.isEmpty || canSubmitAttachments else {
            showToast(attachmentSubmissionUnavailableMessage, tone: .warning)
            return
        }
        guard canSendComposer else { return }
        guard !composerHasUnfinishedAttachments else {
            showToast("Wait for attachments to finish uploading.", tone: .warning)
            return
        }
        guard text.utf8.count <= maximumComposerBytes else {
            showToast("Messages are limited to 1 MiB.", tone: .error)
            return
        }
        if activeTurnID != nil, !attachments.isEmpty {
            showToast("Attachments can be sent with a new turn.", tone: .warning)
            return
        }
        let id = requestID("input")
        // Past every guard, so a rejected send leaves the keyboard up with the text still
        // there to fix. The send button and the return key both land here, which is why this
        // belongs on the model rather than in the composer's own submit path.
        dismissComposerFocus()
        if pendingWidgetEdit?.recovery.phase == .editing {
            submitComposerEdit(sessionID: sessionID, requestID: id, text: text)
            return
        }
        let stashedText = stashedComposerDraft
        let op: AgentOperation
        if let activeTurnID, let activeOperation {
            op = .activeInput(operation: activeOperation, turnID: activeTurnID, text: text)
        } else {
            op = .userInput(text: text, attachments: attachments)
            startChatTitle(prompt: text, submissionID: id, sessionID: sessionID)
        }
        pendingDrafts[id] = PendingComposerDraft(text: text, attachments: attachments)
        composerDraftSaveTask?.cancel()
        composerDraftSaveTask = nil
        if let owner = composerDraftOwner {
            enqueueComposerDraftSave(stashedText ?? text, owner: owner)
        }
        stashedComposerDraft = nil
        suppressesComposerDraftSave = true
        composer = ""
        suppressesComposerDraftSave = false
        composerAttachments = []
        transmit(.submit(sessionID: sessionID, submission: Submission(id: id, op: op))) { [weak self] _ in
            guard let self else { return }
            self.restoreDraft(id: id)
            self.cancelChatTitle(submissionID: id, rearm: true)
        }
        if let stashedText, !stashedText.isEmpty {
            composer = stashedText
        }
    }

    func editWidgetInputInComposer(_ mounted: MountedWidget) {
        guard connectionState.isReady,
              !isLoadingComposerDraft,
              !isLoadingComposerEditRecovery,
              let sessionID = selectedSessionID,
              let accountID = selectedAccountID,
              let operation = mounted.widget.action,
              let input = operation.capabilityInput
        else { return }
        guard composerAttachments.isEmpty else {
            showToast("Finish the attachment draft before editing a queued message.", tone: .warning)
            return
        }
        guard pendingWidgetEdit == nil, stashedComposerDraft == nil else { return }
        flushComposerDraft()
        let requestID = requestID("edit")
        let owner = ComposerDraftOwner(accountID: accountID, sessionID: sessionID)
        let recovery = ComposerEditRecovery(
            capability: mounted.capability,
            widgetID: mounted.widget.id,
            originalInput: input,
            displacedDraft: composer,
            editedInput: input,
            requestID: requestID,
            submissionBaselineSequence: nil,
            phase: .removingQueuedInput
        )
        pendingWidgetEdit = PendingWidgetEdit(owner: owner, recovery: recovery)
        enqueueComposerEditRecoverySave(recovery, owner: owner) { [weak self] result in
            guard let self,
                  self.pendingWidgetEdit?.owner == owner,
                  self.pendingWidgetEdit?.recovery.requestID == requestID
            else { return }
            if case .failure(let error) = result {
                self.pendingWidgetEdit = nil
                self.showToast(error.localizedDescription, tone: .error)
                return
            }
            guard self.connectionState.isReady, self.selectedSessionID == sessionID else { return }
            guard self.selectedAccountID == accountID else { return }
            self.transmit(.submit(
                sessionID: sessionID,
                submission: Submission(id: requestID, op: operation)
            ))
        }
    }

    private func submitComposerEdit(sessionID: String, requestID: String, text: String) {
        guard var pending = pendingWidgetEdit,
              let accountID = selectedAccountID,
              pending.owner == ComposerDraftOwner(accountID: accountID, sessionID: sessionID),
              pending.recovery.phase == .editing
        else { return }
        let operation: AgentOperation
        if let activeTurnID, let activeOperation {
            operation = .activeInput(operation: activeOperation, turnID: activeTurnID, text: text)
        } else {
            operation = .userInput(text: text, attachments: [])
        }
        pending.recovery.editedInput = text
        pending.recovery.requestID = requestID
        pending.recovery.submissionBaselineSequence = latestSequence
        pending.recovery.phase = .submitting
        pendingWidgetEdit = pending
        composerDraftSaveTask?.cancel()
        composerDraftSaveTask = nil
        enqueueComposerEditRecoverySave(pending.recovery, owner: pending.owner) { [weak self] result in
            guard let self,
                  self.pendingWidgetEdit?.owner == pending.owner,
                  self.pendingWidgetEdit?.recovery.requestID == requestID,
                  self.pendingWidgetEdit?.recovery.phase == .submitting
            else { return }
            if case .failure(let error) = result {
                self.restoreComposerEditMode(requestID: requestID)
                self.showToast(error.localizedDescription, tone: .error)
                return
            }
            guard self.connectionState.isReady, self.selectedSessionID == sessionID else {
                self.restoreComposerEditMode(requestID: requestID)
                return
            }
            guard self.selectedAccountID == pending.owner.accountID else {
                self.restoreComposerEditMode(requestID: requestID)
                return
            }
            self.stashedComposerDraft = nil
            self.suppressesComposerDraftSave = true
            self.composer = pending.recovery.displacedDraft
            self.suppressesComposerDraftSave = false
            self.transmit(
                .submit(
                    sessionID: sessionID,
                    submission: Submission(id: requestID, op: operation)
                )
            ) { [weak self] _ in
                self?.restoreComposerEditMode(requestID: requestID)
            }
        }
    }

    func refreshProfile() {
        guard connectionState.isReady else { return }
        transmit(.getProfile(requestID: requestID("profile")))
    }

    func submitWidget(_ mounted: MountedWidget) {
        guard let sessionID = selectedSessionID, let action = mounted.widget.action else { return }
        let id = requestID("widget")
        transmit(.submit(sessionID: sessionID, submission: Submission(id: id, op: action)))
    }

    func submitMessageAction(_ mounted: MountedWidget, target: MessageTarget) {
        guard let sessionID = selectedSessionID, let action = mounted.widget.action else { return }
        let submittedAction = switch action {
        case .capabilityCommand(let capability, let command, let arguments, let input, _):
            AgentOperation.capabilityCommand(
                capability: capability,
                command: command,
                arguments: arguments,
                input: input,
                target: target
            )
        default:
            action
        }
        transmit(.submit(
            sessionID: sessionID,
            submission: Submission(id: requestID("widget"), op: submittedAction)
        ))
    }

    func submitFrontendOperation(_ operation: AgentOperation) {
        guard let sessionID = selectedSessionID else { return }
        if case .capabilityCommand(let capability, _, _, _, _) = operation,
           middlewareFeatures.contains(where: { $0.id == capability }),
           !isCapabilityEnabled(capability) {
            return
        }
        transmit(.submit(
            sessionID: sessionID,
            submission: Submission(id: requestID("widget-action"), op: operation)
        ))
    }

    func loadPreviewPage(_ operation: AgentOperation) {
        guard let sessionID = selectedSessionID, !isLoadingPreviewPage else { return }
        if case .capabilityCommand(let capability, _, _, _, _) = operation,
           middlewareFeatures.contains(where: { $0.id == capability }),
           !isCapabilityEnabled(capability) {
            return
        }
        let id = requestID("preview-page")
        previewPageRequestID = id
        isLoadingPreviewPage = true
        transmit(.submit(
            sessionID: sessionID,
            submission: Submission(id: id, op: operation)
        )) { [weak self] _ in
            guard self?.previewPageRequestID == id else { return }
            self?.previewPageRequestID = nil
            self?.isLoadingPreviewPage = false
        }
    }

    func submitPickerOption(_ option: FrontendPickerOption) {
        guard let sessionID = selectedSessionID else { return }
        let id = requestID("picker")
        pendingPicker = nil
        if case .capabilityCommand = option.op { previewSelections[id] = option }
        transmit(.submit(
            sessionID: sessionID,
            submission: Submission(id: id, op: option.op)
        )) { [weak self] _ in
            self?.previewSelections.removeValue(forKey: id)
        }
    }

}
