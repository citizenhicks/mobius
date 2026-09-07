import Foundation
import Observation

extension AppModel {
    func start() async {
        guard !Task.isCancelled else { return }
        gateway.setAppInBackground(appIsInBackground)
        guard let account = gateway.selectedAccount else {
            #if DEBUG
            if !gateway.pairingCode.isEmpty, !gateway.pairingEndpoint.isEmpty {
                pair()
                return
            }
            #endif
            showsPairing = true
            return
        }
        guard startedAccountID != account.id else { return }
        if let startupTask {
            await startupTask.value
            guard !Task.isCancelled, gateway.selectedAccountID == account.id else { return }
            if startedAccountID == account.id { return }
        }
        guard !Task.isCancelled, gateway.selectedAccountID == account.id else { return }
        let generation = gateway.connectionGeneration
        let taskID = UUID()
        let task = Task<Void, Never> { [weak self] in
            guard let self else { return }
            await self.performStart(account: account, generation: generation)
        }
        startupTask = task
        startupTaskID = taskID
        await task.value
        if startupTaskID == taskID {
            startupTask = nil
            startupTaskID = nil
        }
    }

    private func performStart(account: GatewayAccount, generation: UUID) async {
        let catalog = await store.loadChatCatalog(accountID: account.id)
        let cachedTranscript: CachedTranscript? =
            if let sessionID = catalog?.lastSessionID {
                await store.loadTranscript(accountID: account.id, sessionID: sessionID)
            } else {
                nil
            }
        guard !Task.isCancelled,
              gateway.selectedAccountID == account.id,
              gateway.connectionGeneration == generation
        else { return }
        if let catalog {
            applyBots(catalog.bots)
            applySwarms(catalog.swarms)
            applySessionCatalog(catalog.sessions)
            if let sessionID = catalog.lastSessionID {
                destination = .chats
                navigationPath = [.chat(.session(sessionID))]
                chat.presentCachedSession(sessionID, transcript: cachedTranscript)
                chat.sessionToRestoreID = sessionID
                cacheChatCatalog(lastSessionID: sessionID)
            }
        }
        startedAccountID = account.id
        guard !appIsInBackground else { return }
        connect(to: account)
    }

    func applyPairingSetup(_ rawValue: String) {
        prefillPairing { try GatewayPairingSetup(rawValue) }
    }

    func applyPairingSetup(_ setup: GatewayPairingSetup) {
        prefillPairing { setup }
    }

    private func prefillPairing(_ parse: () throws -> GatewayPairingSetup) {
        gateway.cancelReconnect()
        showsPairing = true
        do {
            let setup = try parse()
            gateway.applyPairingSetup(setup)
        } catch {
            gateway.pairingError = localizedErrorDescription(error)
        }
    }

    func pair() {
        gateway.pair()
        if let message = gateway.pairingError {
            showToast(verbatim: message, tone: .error)
        }
    }

    func selectAccount(_ id: UUID?) {
        guard let id, let account = gateway.accounts.first(where: { $0.id == id }) else { return }
        connect(to: account)
    }

    func renameGateway(_ account: GatewayAccount, to name: String) {
        do {
            try gateway.rename(account, to: name)
            showToast("Gateway renamed.", tone: .success)
        } catch {
            showToast(verbatim: localizedErrorDescription(error), tone: .error)
        }
    }

    func reconnect() {
        guard let account = gateway.selectedAccount else { return }
        connect(to: account)
    }

    func connect(to account: GatewayAccount) {
        guard !isClearingLocalData else { return }
        let isCloud = account.id == cloud.cloudGateway?.id
        if isCloud, cloud.cloudIssue == .subscriptionExpired {
            cloud.handleCloudSubscriptionExpired()
            showToast(
                verbatim: localizedString(
                    MobiusCloudError.subscriptionRequired.localizedDescriptionResource
                ),
                tone: .warning
            )
            return
        }
        gateway.connect(to: account, reconnectsUntilReady: isCloud)
    }

    func setSceneActive(_ active: Bool) {
        gateway.setSceneActive(active, reconnectWhenActive: !selectedGatewayIsMobiusCloud)
    }

    func repairSelectedGateway() {
        guard gateway.selectedAccount != nil else {
            showsPairing = true
            return
        }
        gateway.repairSelectedGateway()
        showsPairing = true
    }

    func chooseWorkspace(_ selectedPath: String) {
        let path = selectedPath.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !path.isEmpty else {
            workspaceError = localizedString("Choose a folder on the gateway host.")
            return
        }
        guard canCreateSession else { return }
        if newVoiceChatIntent == .selectingWorkspace { newVoiceChatIntent = .selectingBot }
        chat.changeComposerDraftOwner(to: nil)
        chat.discardComposerAttachments()
        resetRootSessionState()
        chat.selectedSessionID = nil
        chat.sessionToRestoreID = nil
        chat.sessionOpenCursor = nil
        chat.pendingNewChatWorkspace = path
        chat.pendingNewChatBotID = nil
        workspaceError = nil
        chat.resetSessionState()
        destination = .chats
        navigationPath = [.chat(.new)]
        showsWorkspaceBrowser = false
        cacheChatCatalog(lastSessionID: nil)
        if bots.count == 1 { selectBotForNewChat(bots[0]) }
    }

    func selectBotForNewChat(_ bot: BotRecord) {
        guard canCreateSession,
              bots.contains(where: { $0.id == bot.id }),
              chat.pendingNewChatWorkspace != nil,
              case .chat(.new)? = navigationPath.last
        else { return }
        chat.pendingNewChatBotID = bot.id
        workspaceError = nil
        createPendingVoiceChat()
    }

    @discardableResult
    func createPendingSession() -> String? {
        guard canCreateSession,
              let path = chat.pendingNewChatWorkspace,
              let botID = chat.pendingNewChatBotID,
              bots.contains(where: { $0.id == botID }),
              case .chat(.new)? = navigationPath.last
        else { return nil }
        let id = requestID("create")
        chat.sessionRequestID = id
        workspaceError = nil
        isChangingWorkspace = true
        gateway.connectionState = .loading
        gateway.transmit(.createSession(requestID: id, workspace: path, botID: botID)) {
            [weak self] message in
            guard let self, self.chat.sessionRequestID == id else { return }
            self.chat.restoreDraft(id: id)
            self.chat.sessionRequestID = nil
            self.isChangingWorkspace = false
            self.gateway.connectionState = .ready
            self.workspaceError = message
            self.cancelVoiceChatIntent()
        }
        return id
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
        gateway.transmit(.listDirectories(requestID: id, path: path, includeFiles: false)) { [weak self] message in
            guard self?.directoryRequestID == id else { return }
            self?.directoryRequestID = nil
            self?.isLoadingDirectories = false
            self?.directoryError = message
        }
    }

    func createWorkspaceDirectory(named rawName: String) {
        let name = rawName.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty else {
            directoryError = localizedString("Enter a folder name.")
            return
        }
        guard name != ".", name != "..", !name.contains("/"), !name.contains("\\") else {
            directoryError = localizedString("Enter a single folder name.")
            return
        }
        guard let parent = directoryListing?.path, canCreateSession else { return }
        let id = requestID("create-directory")
        directoryRequestID = id
        directoryError = nil
        isLoadingDirectories = true
        gateway.transmit(.createWorkspaceDirectory(requestID: id, parent: parent, name: name)) {
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
        let wasSelected = gateway.prepareAccountRemoval(account)
        if wasSelected {
            chat.quiesce()
            resetGatewayDependentState(preservingDrafts: false)
        }
        await chat.drainIO()
        var removalError: Error?
        do {
            try await store.remove(account)
        } catch {
            removalError = error
        }
        if wasSelected, gateway.selectedAccountID == nil {
            if let next = gateway.accounts.first {
                connect(to: next)
            } else {
                await gateway.shutdown().value
                if gateway.selectedAccountID == nil { showsPairing = true }
            }
        }
        if let removalError {
            showToast(verbatim: localizedErrorDescription(removalError), tone: .error)
            return false
        } else {
            showToast("Gateway removed.", tone: .info)
            return true
        }
    }

    func openNewSession() {
        guard canCreateSession else { return }
        cancelVoiceChatIntent()
        chat.stopRealtimeVoice()
        destination = .chats
        navigationPath = []
        openWorkspaceBrowser()
    }

    func openNewSessionInCurrentWorkspace() {
        guard let path = workspace?.path,
              let selectedSession,
              let bot = bots.first(where: { $0.id == selectedSession.sessionContext.botId })
        else { return }
        chooseWorkspace(path)
        selectBotForNewChat(bot)
    }

    func openChat(_ sessionID: String) {
        guard canBrowseSessions || sessionID == chat.selectedSessionID else { return }
        cancelVoiceChatIntent()
        chat.chatPresentationRevision &+= 1
        destination = .chats
        chat.openSession(sessionID)
        navigationPath = [.chat(.session(sessionID))]
        cacheChatCatalog(lastSessionID: sessionID)
    }

    func openBotChats(_ botID: String) {
        guard bots.contains(where: { $0.id == botID }) else { return }
        chat.chatBotFilterIDs = [botID]
        destination = .chats
        navigationPath = []
    }

    func openBotSessions(_ botID: String) {
        guard bots.contains(where: { $0.id == botID }) else { return }
        if chat.botSessionsBotID != botID {
            chat.botSessionsRequestID = nil
            chat.botSessions = []
            chat.isLoadingBotSessions = false
        }
        chat.botSessionsBotID = botID
        destination = .bots
        if navigationPath.last != .botSessions(botID) {
            navigationPath.append(.botSessions(botID))
        }
        refreshBotSessions(botID)
    }

    func refreshBotSessions(_ botID: String) {
        guard gateway.connectionState.isReady,
              chat.botSessionsRequestID == nil,
              bots.contains(where: { $0.id == botID })
        else { return }
        if chat.botSessionsBotID != botID { chat.botSessions = [] }
        chat.botSessionsBotID = botID
        let id = requestID("bot-sessions")
        chat.botSessionsRequestID = id
        chat.isLoadingBotSessions = true
        gateway.transmit(.listBotSessions(requestID: id, botID: botID)) { [weak self] _ in
            guard self?.chat.botSessionsRequestID == id else { return }
            self?.chat.botSessionsRequestID = nil
            self?.chat.isLoadingBotSessions = false
        }
    }

    func openBotSession(_ sessionID: String) {
        guard canBrowseSessions || sessionID == chat.selectedSessionID,
              chat.botSessions.contains(where: { $0.sessionId == sessionID })
        else { return }
        cancelVoiceChatIntent()
        chat.chatPresentationRevision &+= 1
        chat.openSession(sessionID)
        navigationPath.append(.chat(.session(sessionID)))
    }

    func resumeBotSession(botID: String, sessionID: String) {
        guard bots.contains(where: { $0.id == botID }) else { return }
        if let visible = chat.sessions.first(where: { $0.sessionId == sessionID }) {
            guard visible.sessionContext.botId == botID else {
                showToast("The source conversation belongs to another Bot.", tone: .error)
                return
            }
            chat.pendingBotSessionResume = nil
            openChat(sessionID)
            return
        }
        guard canOpenSession else { return }
        chat.pendingBotSessionResume = (botID, sessionID)
        if chat.botSessionsBotID != botID {
            chat.botSessionsRequestID = nil
            chat.botSessions = []
            chat.isLoadingBotSessions = false
        }
        chat.botSessionsBotID = botID
        refreshBotSessions(botID)
    }

    func openSwarm(_ swarmID: String) {
        guard swarms.contains(where: { $0.id == swarmID }) else { return }
        destination = .bots
        navigationPath = [.swarm(swarmID)]
    }

    func openSwarmChat(_ swarmID: String) {
        guard swarms.contains(where: { $0.id == swarmID }) else { return }
        destination = .bots
        navigationPath = [.swarm(swarmID), .swarmChat(swarmID)]
    }

    func swarm(containingBot botID: String) -> SwarmRecord? {
        swarms.first { swarm in
            swarm.leaderBotId == botID
                || swarm.members.contains { $0.botId == botID }
        }
    }

    func availableBotsForSwarm(excluding botID: String? = nil) -> [BotRecord] {
        bots.filter { bot in
            bot.collaborationEnabled && bot.id != botID && swarm(containingBot: bot.id) == nil
        }.sorted {
            $0.name.localizedStandardCompare($1.name) == .orderedAscending
        }
    }

    func beginCreatingSwarm() -> Bool {
        guard canMutateSwarm else { return false }
        guard bots.contains(where: \.collaborationEnabled) else {
            showToast("Swarm is off")
            return false
        }
        return availableBotsForSwarm().count >= 2
    }

    // Renaming, pinning and deleting address a session by id, so they work on any chat in the
    // catalogue rather than only the open one.
    @discardableResult
    func renameSession(_ session: SessionRecord, title: String) -> String? {
        let title = title.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !title.isEmpty else { return nil }
        guard chat.sessionMutationRequestID == nil else {
            showToast("Another chat update is finishing.", tone: .info)
            return nil
        }
        chat.cancelChatTitle(session.sessionId)
        return chat.requestSessionRename(sessionID: session.sessionId, title: title)
    }

    func attachFolder(_ selectedPath: String) {
        let path = selectedPath.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canModifySelectedSession,
              let sessionID = chat.selectedSessionID,
              chat.sessionMutationRequestID == nil,
              !path.isEmpty
        else { return }
        let id = requestID("session-attach-folder")
        chat.sessionMutationRequestID = id
        gateway.transmit(.attachSessionFolder(
            requestID: id,
            sessionID: sessionID,
            folder: path
        )) { [weak self] _ in
            if self?.chat.sessionMutationRequestID == id { self?.chat.sessionMutationRequestID = nil }
        }
    }

    func deleteSession(_ session: SessionRecord) {
        deleteSessions([session])
    }

    func deleteSessions(_ sessions: [SessionRecord]) {
        guard chat.sessionMutationRequestID == nil else { return }
        var seen = Set<String>()
        let sessionIDs = sessions.map(\.sessionId).filter { seen.insert($0).inserted }
        guard !sessionIDs.isEmpty else { return }
        let deletesSelectedSession = chat.selectedSessionID.map(sessionIDs.contains) == true
        let deletedPresentedSessionID = presentedChatSessionID.flatMap { presented in
            sessionIDs.contains(presented) ? presented : nil
        }
        if let accountID = gateway.selectedAccountID {
            chat.enqueueTranscriptIO { [store] in
                for sessionID in sessionIDs {
                    await store.removeTranscript(accountID: accountID, sessionID: sessionID)
                }
            }
        }
        let id = requestID("session-delete")
        chat.sessionMutationRequestID = id
        chat.pendingDeletedSessionIDs = sessionIDs
        chat.pendingDeletedPresentedSessionID = deletedPresentedSessionID
        gateway.transmit(.deleteSessions(
            requestID: id,
            sessionIDs: sessionIDs
        )) { [weak self] _ in
            guard let self, self.chat.sessionMutationRequestID == id else { return }
            let sessionID = self.chat.pendingDeletedPresentedSessionID
            self.chat.sessionMutationRequestID = nil
            self.chat.pendingDeletedSessionIDs = []
            self.chat.pendingDeletedPresentedSessionID = nil
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
        chat.restoreSession(sessionID)
    }

    func createSwarm(title rawTitle: String, leaderBotID: String, memberBotIDs: Set<String>) {
        let title = rawTitle.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canMutateSwarm, !title.isEmpty else { return }
        let allowed = Set(availableBotsForSwarm().map(\.id))
        guard allowed.contains(leaderBotID),
              !memberBotIDs.isEmpty,
              !memberBotIDs.contains(leaderBotID),
              memberBotIDs.isSubset(of: allowed)
        else {
            showToast("Choose available Bots with Swarm collaboration enabled.", tone: .warning)
            return
        }
        let selectedCoworkers = bots.compactMap { bot in
            memberBotIDs.contains(bot.id) ? bot.id : nil
        }
        sendSwarmMutation("swarm-create") { requestID in
            .createSwarm(
                requestID: requestID,
                title: title,
                leaderBotID: leaderBotID,
                memberBotIDs: selectedCoworkers
            )
        }
    }

    func addSwarmMember(_ bot: BotRecord, to swarm: SwarmRecord) {
        guard availableBotsForSwarm().contains(where: { $0.id == bot.id }) else {
            showToast("Choose an available Bot with Swarm collaboration enabled.", tone: .warning)
            return
        }
        sendSwarmMutation("swarm-add") { requestID in
            .addSwarmMember(
                requestID: requestID,
                swarmID: swarm.id,
                botID: bot.id
            )
        }
    }

    func leaveSwarm(_ swarm: SwarmRecord, botID: String) {
        guard swarm.leaderBotId != botID,
              self.swarm(containingBot: botID)?.id == swarm.id
        else { return }
        sendSwarmMutation("swarm-leave") { requestID in
            .leaveSwarm(requestID: requestID, swarmID: swarm.id, botID: botID)
        }
    }

    func renameSwarm(_ swarm: SwarmRecord, title rawTitle: String) {
        let title = rawTitle.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !title.isEmpty, title != swarm.title else { return }
        sendSwarmMutation("swarm-rename") { requestID in
            .renameSwarm(requestID: requestID, swarmID: swarm.id, title: title)
        }
    }

    func disbandSwarm(_ swarm: SwarmRecord) {
        sendSwarmMutation("swarm-disband") { requestID in
            .disbandSwarm(requestID: requestID, swarmID: swarm.id)
        }
    }

    @discardableResult
    func postSwarmMessage(
        to swarmID: String,
        text rawText: String
    ) -> String? {
        let text = rawText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canPostSwarmMessage,
              !text.isEmpty,
              swarms.contains(where: { $0.id == swarmID })
        else { return nil }
        let id = requestID("swarm-message")
        swarmMessageRequestID = id
        gateway.transmit(.postSwarmMessage(
            requestID: id,
            swarmID: swarmID,
            text: text
        )) { [weak self] _ in
            if self?.swarmMessageRequestID == id { self?.swarmMessageRequestID = nil }
        }
        return id
    }

    private func sendSwarmMutation(
        _ requestPrefix: String,
        request: (String) -> GatewayRequest
    ) {
        guard canMutateSwarm else { return }
        let id = requestID(requestPrefix)
        swarmMutationRequestID = id
        gateway.transmit(request(id)) { [weak self] _ in
            if self?.swarmMutationRequestID == id { self?.swarmMutationRequestID = nil }
        }
    }

    func refreshWorkspaceChanges() {
        refreshGitDiff()
        if showsInspector, filesInspectorTab == .modified {
            if let scope = modifiedFilesScope.gitScope, scope != .unstaged {
                refreshGitDiff(scope)
            }
        }
        refreshWorkspaceFiles()
    }

    func refreshGitDiff(_ scope: GitDiffScope = .unstaged) {
        guard gateway.connectionState.isReady, let sessionID = chat.selectedSessionID else { return }
        let id = requestID("git-diff")
        gitDiffs[scope, default: GitDiffState()].requestID = id
        gateway.transmit(.getGitDiff(requestID: id, sessionID: sessionID, scope: scope)) { [weak self] _ in
            guard self?.gitDiffs[scope]?.requestID == id else { return }
            self?.gitDiffs[scope]?.requestID = nil
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
        guard gateway.connectionState.isReady,
              let sessionID = chat.selectedSessionID
        else { return }
        let id = requestID("workspace-files")
        workspaceFilesRequestID = id
        workspaceFilesTruncated = false
        isLoadingWorkspaceFiles = true
        gateway.transmit(.listWorkspaceFiles(
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
              let sessionID = chat.selectedSessionID,
              let gitStatus,
              branch != gitStatus.currentBranch,
              gitStatus.branches.contains(branch)
        else { return }
        let id = requestID("git-branch")
        gitBranchRequestID = id
        gateway.transmit(.switchGitBranch(requestID: id, sessionID: sessionID, branch: branch)) { [weak self] _ in
            if self?.gitBranchRequestID == id { self?.gitBranchRequestID = nil }
        }
    }

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
        chat.composerAttachments.append(ComposerAttachment(
            id: id,
            name: name,
            size: 0,
            mediaType: "application/octet-stream",
            state: .preparing
        ))
        return id
    }

    func completeComposerAttachmentImport(_ url: URL, reservedID: UUID) async {
        guard chat.composerAttachments.contains(where: {
            $0.id == reservedID && $0.state == .preparing
        }) else { return }
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
        guard let index = chat.composerAttachments.firstIndex(where: {
            $0.id == id && $0.state == .preparing
        }) else { return false }
        chat.composerAttachments.remove(at: index)
        return true
    }

    func removeComposerAttachment(_ id: UUID) {
        guard let attachment = chat.composerAttachments.first(where: { $0.id == id }) else { return }
        chat.discardComposerAttachment(attachment)
        chat.startNextSessionFileUpload()
        if chat.pendingNewChatBotID != nil, chat.pendingDrafts.count == 1 {
            submitPendingNewChatDraft(requestID: chat.pendingDrafts.keys.first)
        }
    }

    func retryComposerAttachment(_ id: UUID) {
        guard chat.sessionFileData[id] != nil,
              let index = chat.composerAttachments.firstIndex(where: { $0.id == id }),
              case .failed = chat.composerAttachments[index].state
        else { return }
        chat.composerAttachments[index].state = .queued
        chat.startNextSessionFileUpload()
    }

    func previewSessionFile(_ file: SessionFileReference, sessionID: String?) {
        downloadSessionFile(file, sessionID: sessionID, purpose: .preview)
    }

    func saveOrShareSessionFile(_ file: SessionFileReference, sessionID: String?) {
        downloadSessionFile(file, sessionID: sessionID, purpose: .share)
    }

    private func downloadSessionFile(
        _ file: SessionFileReference,
        sessionID: String?,
        purpose: SessionFileDownloadPurpose
    ) {
        guard let sessionID else { return }
        guard file.size <= Int64(maximumPresentedFileBytes) else {
            showToast("File downloads are limited to 50 MiB.", tone: .warning)
            return
        }
        discardFilePresentation()
        returnsToFilesAfterFilePresentation = showsInspector
        let id = requestID("session-file-read")
        let generation = UUID()
        filePresentationGeneration = generation
        chat.sessionFileDownload = SessionFileDownload(
            generation: generation,
            file: file,
            sessionID: sessionID,
            purpose: purpose,
            data: Data(),
            requestID: id
        )
        isLoadingFilePresentation = true
        gateway.transmit(.readSessionFile(
            requestID: id,
            sessionID: sessionID,
            fileID: file.id,
            offset: 0,
            maxBytes: 256 * 1024
        )) { [weak self] message in
            guard self?.chat.sessionFileDownload?.requestID == id else { return }
            self?.chat.sessionFileDownload = nil
            self?.isLoadingFilePresentation = false
            self?.showToast(verbatim: message, tone: .error)
        }
    }

    func workspaceFile(for link: URL) -> WorkspaceFileRecord? {
        let scheme = link.scheme?.lowercased()
        if let scheme, !["file", "sandbox", "workspace"].contains(scheme) { return nil }
        var path = link.path
        if let root = workspace?.path {
            let prefix = root.hasSuffix("/") ? root : "\(root)/"
            if path.hasPrefix(prefix) { path = String(path.dropFirst(prefix.count)) }
        }
        if scheme == "sandbox", path.hasPrefix("/mnt/data/") {
            path = String(path.dropFirst("/mnt/data/".count))
        }
        if scheme == "workspace" { path = String(path.drop(while: { $0 == "/" })) }
        while path.hasPrefix("./") { path.removeFirst(2) }
        guard !path.isEmpty, !path.hasPrefix("/") else { return nil }
        return workspaceFiles.first { $0.path == path }
    }

    func previewWorkspaceFile(_ file: WorkspaceFileRecord) {
        guard let sessionID = chat.selectedSessionID else { return }
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
        gateway.transmit(.readWorkspaceFile(
            requestID: id,
            sessionID: sessionID,
            path: file.path,
            offset: 0,
            maxBytes: 256 * 1024
        )) { [weak self] message in
            guard self?.workspaceFilePreviewDownload?.requestID == id else { return }
            self?.workspaceFilePreviewDownload = nil
            self?.isLoadingFilePresentation = false
            self?.showToast(verbatim: message, tone: .error)
        }
    }

    func createWorkspaceFile() {
        guard canOpenSession, let sessionID = chat.selectedSessionID else { return }
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
              chat.selectedSessionID == sessionID,
              workspaceFileWriteRequestID == nil,
              path.utf8.count <= 4_096,
              !path.isEmpty,
              content.utf8.count <= maximumWorkspaceTextFileBytes
        else { return }
        let id = requestID("workspace-file-write")
        workspaceFileWriteRequestID = id
        isSavingWorkspaceFile = true
        gateway.transmit(.writeWorkspaceFile(
            requestID: id,
            sessionID: sessionID,
            path: path,
            content: content
        )) { [weak self] message in
            guard self?.workspaceFileWriteRequestID == id else { return }
            self?.workspaceFileWriteRequestID = nil
            self?.isSavingWorkspaceFile = false
            self?.showToast(verbatim: message, tone: .error)
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
        chat.sessionFileDownload = nil
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
        guard let contribution = chat.contributions.first(where: {
            $0.commands.contains { $0.name == name }
        }), let command = contribution.commands.first(where: { $0.name == name }) else {
            showToast("Unknown command /\(name).", tone: .warning)
            return false
        }
        guard !command.requiresIdle || chat.activeTurnID == nil else {
            showToast("/\(name) is available when the agent is idle.", tone: .warning)
            return false
        }
        guard chat.composerAttachments.isEmpty, chat.composerReply == nil else {
            showToast("Send attachments and replies as a message before using a command.", tone: .warning)
            return false
        }
        guard let sessionID = chat.selectedSessionID else { return false }
        let id = requestID("command")
        chat.pendingDrafts[id] = PendingComposerDraft(text: chat.composer, attachments: [])
        chat.composer = ""
        chat.dismissComposerFocus()
        gateway.transmit(.submit(sessionID: sessionID, submission: Submission(
            id: id,
            op: .capabilityCommand(
                capability: contribution.capability,
                command: command.name,
                arguments: parts.count == 2
                    ? parts[1].trimmingCharacters(in: .whitespacesAndNewlines) : "",
                input: nil,
                target: nil
            )
        ))) { [weak self] _ in
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
            showToast("Finish the attachment draft before editing a queued message.", tone: .warning)
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
            guard self.gateway.connectionState.isReady, self.chat.selectedSessionID == sessionID else { return }
            guard self.gateway.selectedAccountID == accountID else { return }
            self.gateway.transmit(.submit(
                sessionID: sessionID,
                submission: Submission(id: requestID, op: operation)
            ))
        }
    }

    func refreshProfile() {
        guard gateway.connectionState.isReady else { return }
        gateway.transmit(.getProfile(requestID: requestID("profile")))
    }

    func submitWidget(_ mounted: MountedWidget) {
        guard canSubmitFrontendAction(capability: mounted.capability),
              let sessionID = chat.selectedSessionID,
              let action = mounted.widget.action
        else { return }
        let id = requestID("widget")
        chat.previewWidgetRequestID = id
        gateway.transmit(.submit(sessionID: sessionID, submission: Submission(id: id, op: action))) { [weak self] _ in
            if self?.chat.previewWidgetRequestID == id { self?.chat.previewWidgetRequestID = nil }
        }
    }

    func submitMessageAction(_ mounted: MountedWidget, target: MessageTarget) {
        guard canSubmitFrontendAction(capability: mounted.capability),
              let sessionID = chat.selectedSessionID,
              let action = mounted.widget.action
        else { return }
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
        gateway.transmit(.submit(
            sessionID: sessionID,
            submission: Submission(id: requestID("widget"), op: submittedAction)
        ))
    }

    func submitFrontendOperation(_ operation: AgentOperation) {
        let capability: String? = if case .capabilityCommand(let capability, _, _, _, _) = operation {
            capability
        } else {
            nil
        }
        guard canSubmitFrontendAction(capability: capability),
              let sessionID = chat.selectedSessionID
        else { return }
        gateway.transmit(.submit(
            sessionID: sessionID,
            submission: Submission(id: requestID("widget-action"), op: operation)
        ))
    }

    func submitContributionOperation(_ operation: AgentOperation, scope: ContributionScope) {
        guard gateway.connectionState.isReady else { return }
        if case .swarm(let id) = scope,
           !swarms.contains(where: { $0.id == id }) {
            return
        }
        gateway.transmit(.submitContribution(
            requestID: requestID("contribution"),
            scope: scope,
            operation: operation
        ))
    }

    func refreshContributions(scope: ContributionScope) {
        guard gateway.connectionState.isReady else { return }
        if case .swarm(let id) = scope, !swarms.contains(where: { $0.id == id }) { return }
        gateway.transmit(.getContributions(requestID: requestID("contributions"), scope: scope))
    }

    func loadPreviewPage(_ operation: AgentOperation) {
        let capability: String? = if case .capabilityCommand(let capability, _, _, _, _) = operation {
            capability
        } else {
            nil
        }
        guard canSubmitFrontendAction(capability: capability),
              let sessionID = chat.selectedSessionID,
              !chat.isLoadingPreviewPage
        else { return }
        let id = requestID("preview-page")
        chat.previewPageRequestID = id
        chat.isLoadingPreviewPage = true
        gateway.transmit(.submit(
            sessionID: sessionID,
            submission: Submission(id: id, op: operation)
        )) { [weak self] _ in
            guard self?.chat.previewPageRequestID == id else { return }
            self?.chat.previewPageRequestID = nil
            self?.chat.isLoadingPreviewPage = false
        }
    }

    func loadPreviewPageAndWait(_ operation: AgentOperation) async {
        guard !Task.isCancelled, chat.previewPageRequestID == nil else { return }
        loadPreviewPage(operation)
        guard chat.previewPageRequestID != nil else { return }
        for await loading in Observations({ self.chat.isLoadingPreviewPage }) {
            if !loading { return }
        }
    }

    func submitPickerOption(_ option: FrontendPickerOption) {
        let capability: String? = if case .capabilityCommand(let capability, _, _, _, _) = option.op {
            capability
        } else {
            nil
        }
        guard canSubmitFrontendAction(capability: capability),
              let sessionID = chat.selectedSessionID
        else { return }
        let id = requestID("picker")
        chat.pendingPicker = nil
        if case .capabilityCommand = option.op { chat.previewSelections[id] = option }
        gateway.transmit(.submit(
            sessionID: sessionID,
            submission: Submission(id: id, op: option.op)
        )) { [weak self] _ in
            self?.chat.previewSelections.removeValue(forKey: id)
        }
    }

    private func canSubmitFrontendAction(capability: String?) -> Bool {
        guard gateway.connectionState.isReady,
              chat.sessionRequestID == nil,
              chat.selectedSessionID != nil
        else { return false }
        guard let capability,
              middlewareFeatures.contains(where: { $0.id == capability })
        else { return true }
        return isCapabilityEnabled(capability)
    }

}
