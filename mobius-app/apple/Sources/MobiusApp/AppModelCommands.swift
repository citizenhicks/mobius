import Foundation

extension AppModel {
    func start() async {
        guard let account = selectedAccount else {
            #if DEBUG
            if !pairingCode.isEmpty, !pairingEndpoint.isEmpty { pair(); return }
            #endif
            showsPairing = true
            return
        }
        let catalog = await store.loadChatCatalog(accountID: account.id)
        let cachedTranscript: CachedTranscript? =
            if let sessionID = catalog?.lastSessionID {
                await store.loadTranscript(accountID: account.id, sessionID: sessionID)
            } else {
                nil
            }
        guard selectedAccountID == account.id else { return }
        if let catalog {
            applyBots(catalog.bots)
            applySwarms(catalog.swarms)
            applySessionCatalog(catalog.sessions)
            if let sessionID = catalog.lastSessionID {
                destination = .chats
                navigationPath = [.chat(.session(sessionID))]
                selectedSessionID = sessionID
                sessionToRestoreID = sessionID
                latestSequence = cachedTranscript?.sequence
                nextHistoryBeforeSequence = cachedTranscript?.nextBeforeSequence
                transcript = cachedTranscript?.transcript ?? []
                currentUsage = cachedTranscript?.currentUsage ?? TokenUsage()
                lastUsage = cachedTranscript?.lastUsage ?? TokenUsage()
                updateContextTokens()
                changeComposerDraftOwner(
                    to: ComposerDraftOwner(
                        accountID: account.id,
                        sessionID: sessionID
                    )
                )
                cacheChatCatalog(lastSessionID: sessionID)
            }
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
            pairingError = localizedErrorDescription(error)
        }
    }

    func pair() {
        cancelReconnect()
        automaticReconnectBlocked = false
        pairingError = nil
        do {
            let code = pairingCode.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !code.isEmpty else {
                let message = localizedString("Enter the one-time code shown by the gateway.")
                pairingError = message
                showToast(verbatim: message, tone: .error)
                return
            }
            let setup = try GatewayPairingSetup(endpoint: pairingEndpoint, code: code)
            let endpoint = setup.endpoint
            let endpointName = endpoint.displayName(locale: language.locale)
            let account = accounts.first(where: { $0.endpoint == endpoint })
                ?? GatewayAccount(
                    endpoint: endpoint,
                    displayName: endpointName,
                    machineName: endpointName
                )
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
                    code: setup.code,
                    clientLabel: "möbius Apple",
                    clientKind: .currentApplePlatform
                ))
            }
        } catch {
            pairingError = localizedErrorDescription(error)
            showToast(verbatim: localizedErrorDescription(error), tone: .error)
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
            showToast(verbatim: localizedErrorDescription(error), tone: .error)
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
        pairingError = localizedString("Enter a new one-time code to repair this pairing.")
        showsPairing = true
    }

    func chooseWorkspace(_ selectedPath: String) {
        let path = selectedPath.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !path.isEmpty else {
            workspaceError = localizedString("Choose a folder on the gateway host.")
            return
        }
        guard canCreateSession else { return }
        changeComposerDraftOwner(to: nil)
        discardComposerAttachments()
        discardFilePresentation()
        selectedSessionID = nil
        sessionToRestoreID = nil
        sessionOpenCursor = nil
        pendingNewChatWorkspace = path
        pendingNewChatBotID = nil
        workspaceError = nil
        resetSessionState()
        destination = .chats
        navigationPath = [.chat(.new)]
        showsWorkspaceBrowser = false
        cacheChatCatalog(lastSessionID: nil)
        if bots.count == 1 { selectBotForNewChat(bots[0]) }
    }

    func selectBotForNewChat(_ bot: BotRecord) {
        guard canCreateSession,
              bots.contains(where: { $0.id == bot.id }),
              pendingNewChatWorkspace != nil,
              case .chat(.new)? = navigationPath.last
        else { return }
        pendingNewChatBotID = bot.id
        workspaceError = nil
    }

    @discardableResult
    func createPendingSession() -> String? {
        guard canCreateSession,
              let path = pendingNewChatWorkspace,
              let botID = pendingNewChatBotID,
              bots.contains(where: { $0.id == botID }),
              case .chat(.new)? = navigationPath.last
        else { return nil }
        let id = requestID("create")
        sessionRequestID = id
        workspaceError = nil
        isChangingWorkspace = true
        connectionState = .loading
        transmit(.createSession(requestID: id, workspace: path, botID: botID)) {
            [weak self] message in
            guard let self, self.sessionRequestID == id else { return }
            self.restoreDraft(id: id)
            self.sessionRequestID = nil
            self.isChangingWorkspace = false
            self.connectionState = .ready
            self.workspaceError = message
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
            showToast(verbatim: localizedErrorDescription(error), tone: .error)
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
        guard let path = workspace?.path,
              let selectedSession,
              let bot = bots.first(where: { $0.id == selectedSession.sessionContext.botId })
        else { return }
        chooseWorkspace(path)
        selectBotForNewChat(bot)
    }

    func openChat(_ sessionID: String) {
        guard canOpenSession || sessionID == selectedSessionID else { return }
        chatPresentationRevision &+= 1
        destination = .chats
        openSession(sessionID)
        navigationPath = [.chat(.session(sessionID))]
        cacheChatCatalog(lastSessionID: sessionID)
    }

    func openBotChats(_ botID: String) {
        guard bots.contains(where: { $0.id == botID }) else { return }
        chatBotFilterIDs = [botID]
        destination = .chats
        navigationPath = []
    }

    func openBotSessions(_ botID: String) {
        guard bots.contains(where: { $0.id == botID }) else { return }
        if botSessionsBotID != botID {
            botSessionsRequestID = nil
            botSessions = []
            isLoadingBotSessions = false
        }
        botSessionsBotID = botID
        destination = .bots
        if navigationPath.last != .botSessions(botID) {
            navigationPath.append(.botSessions(botID))
        }
        refreshBotSessions(botID)
    }

    func refreshBotSessions(_ botID: String) {
        guard connectionState.isReady,
              botSessionsRequestID == nil,
              bots.contains(where: { $0.id == botID })
        else { return }
        if botSessionsBotID != botID { botSessions = [] }
        botSessionsBotID = botID
        let id = requestID("bot-sessions")
        botSessionsRequestID = id
        isLoadingBotSessions = true
        transmit(.listBotSessions(requestID: id, botID: botID)) { [weak self] _ in
            guard self?.botSessionsRequestID == id else { return }
            self?.botSessionsRequestID = nil
            self?.isLoadingBotSessions = false
        }
    }

    func openBotSession(_ sessionID: String) {
        guard canOpenSession || sessionID == selectedSessionID,
              botSessions.contains(where: { $0.sessionId == sessionID })
        else { return }
        chatPresentationRevision &+= 1
        openSession(sessionID)
        navigationPath.append(.chat(.session(sessionID)))
    }

    func resumeBotSession(botID: String, sessionID: String) {
        guard bots.contains(where: { $0.id == botID }) else { return }
        if let visible = sessions.first(where: { $0.sessionId == sessionID }) {
            guard visible.sessionContext.botId == botID else {
                showToast("The source conversation belongs to another Bot.", tone: .error)
                return
            }
            pendingBotSessionResume = nil
            openChat(sessionID)
            return
        }
        guard canOpenSession else { return }
        pendingBotSessionResume = (botID, sessionID)
        if botSessionsBotID != botID {
            botSessionsRequestID = nil
            botSessions = []
            isLoadingBotSessions = false
        }
        botSessionsBotID = botID
        refreshBotSessions(botID)
    }

    func openSwarm(_ swarmID: String) {
        guard swarms.contains(where: { $0.id == swarmID }) else { return }
        destination = .bots
        navigationPath = [.swarm(swarmID)]
    }

    func swarm(containingBot botID: String) -> SwarmRecord? {
        swarms.first { swarm in
            swarm.leaderBotId == botID
                || swarm.members.contains { $0.botId == botID }
        }
    }

    func availableBotsForSwarm(excluding botID: String? = nil) -> [BotRecord] {
        bots.filter { bot in
            bot.id != botID && swarm(containingBot: bot.id) == nil
        }.sorted {
            $0.name.localizedStandardCompare($1.name) == .orderedAscending
        }
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

    func createSwarm(title rawTitle: String, leaderBotID: String, memberBotIDs: Set<String>) {
        let title = rawTitle.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canMutateSwarm,
              !title.isEmpty,
              bots.contains(where: { $0.id == leaderBotID }),
              swarm(containingBot: leaderBotID) == nil
        else { return }
        let allowed = Set(availableBotsForSwarm(excluding: leaderBotID).map(\.id))
        guard !memberBotIDs.isEmpty, memberBotIDs.isSubset(of: allowed) else {
            showToast("Those Bots can no longer form a swarm.", tone: .warning)
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
        guard self.swarm(containingBot: bot.id) == nil else {
            showToast("That Bot already belongs to a swarm.", tone: .warning)
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
        workspace rawWorkspace: String,
        text rawText: String
    ) -> String? {
        let workspace = rawWorkspace.trimmingCharacters(in: .whitespacesAndNewlines)
        let text = rawText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canPostSwarmMessage,
              !workspace.isEmpty,
              !text.isEmpty,
              swarms.contains(where: { $0.id == swarmID })
        else { return nil }
        let id = requestID("swarm-message")
        swarmMessageRequestID = id
        transmit(.postSwarmMessage(
            requestID: id,
            swarmID: swarmID,
            workspace: workspace,
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
        transmit(request(id)) { [weak self] _ in
            if self?.swarmMutationRequestID == id { self?.swarmMutationRequestID = nil }
        }
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
            attachmentReferenceLimit - composerAttachments.count - attachmentImportReservations
        )
        let selectedURLs = Array(urls.prefix(available))
        if urls.count > selectedURLs.count {
            showToast(
                "You can attach up to \(attachmentReferenceLimit) files to a message.",
                tone: .warning
            )
        }
        guard !selectedURLs.isEmpty else { return }

        var reservedCount = selectedURLs.count
        attachmentImportReservations += reservedCount
        defer { attachmentImportReservations -= reservedCount }
        for url in selectedURLs {
            guard generation == attachmentImportGeneration else { return }
            do {
                let imported = try await Self.loadImportedAttachment(
                    url,
                    maximumBytes: attachmentFileByteLimit
                )
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
                if currentBytes > attachmentDraftByteLimit - Int64(imported.data.count) {
                    showToast(
                        AttachmentImportError.totalTooLarge(attachmentDraftByteLimit)
                            .localizedDescriptionResource,
                        tone: .error
                    )
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
                if let error = error as? AttachmentImportError {
                    showToast(error.localizedDescriptionResource, tone: .error)
                } else {
                    showToast(verbatim: localizedErrorDescription(error), tone: .error)
                }
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
            self?.showToast(verbatim: message, tone: .error)
        }
    }

    func createWorkspaceFile() {
        guard canOpenSession, let sessionID = selectedSessionID else { return }
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

    @discardableResult
    func sendMessage(delivery requestedDelivery: ActiveMessageDelivery? = nil) -> Bool {
        guard connectionState.isReady,
              sessionRequestID == nil
        else { return false }
        let text = composer.trimmingCharacters(in: .whitespacesAndNewlines)
        let attachments = uploadedComposerAttachments
        guard attachments.count <= attachmentReferenceLimit else { return false }
        guard !text.isEmpty || !attachments.isEmpty else { return false }
        guard attachments.isEmpty || canSubmitAttachments else {
            showToast(attachmentSubmissionUnavailableMessage, tone: .warning)
            return false
        }
        guard canSendComposer else { return false }
        guard !composerHasUnfinishedAttachments else {
            showToast("Wait for attachments to finish uploading.", tone: .warning)
            return false
        }
        guard text.utf8.count <= maximumComposerBytes else {
            showToast("Messages are limited to 1 MiB.", tone: .error)
            return false
        }
        if activeTurnID != nil, !attachments.isEmpty {
            showToast("Attachments can be sent with a new turn.", tone: .warning)
            return false
        }
        if selectedSessionID == nil {
            guard let requestID = createPendingSession() else { return false }
            dismissComposerFocus()
            pendingDrafts[requestID] = PendingComposerDraft(
                text: text,
                attachments: attachments
            )
            composerDraftSaveTask?.cancel()
            composerDraftSaveTask = nil
            suppressesComposerDraftSave = true
            composer = ""
            suppressesComposerDraftSave = false
            composerAttachments = []
            return true
        }
        guard let sessionID = selectedSessionID else { return false }
        let id = requestID("input")
        let targetTurnID = activeTurnID
        let delivery = targetTurnID == nil ? nil : requestedDelivery
        let operation = AgentOperation.message(MessageSubmission(
            author: .user,
            text: text,
            attachments: attachments,
            requestedDelivery: delivery,
            targetTurnId: targetTurnID
        ))
        // Past every guard, so a rejected send leaves the keyboard up with the text still
        // there to fix. The send button and the return key both land here, which is why this
        // belongs on the model rather than in the composer's own submit path.
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
        transmit(.submit(
            sessionID: sessionID,
            submission: Submission(id: id, op: operation)
        )) { [weak self] _ in
            guard let self else { return }
            self.restoreDraft(id: id)
            self.cancelChatTitle(submissionID: id, rearm: true)
        }
        if let stashedText, !stashedText.isEmpty {
            composer = stashedText
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
                self.showToast(verbatim: self.localizedErrorDescription(error), tone: .error)
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

    private func submitComposerEdit(
        sessionID: String,
        requestID: String,
        text: String,
        operation: AgentOperation
    ) {
        guard var pending = pendingWidgetEdit,
              let accountID = selectedAccountID,
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
        enqueueComposerEditRecoverySave(pending.recovery, owner: pending.owner) { [weak self] result in
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

    func submitScratchpadOperation(_ operation: AgentOperation, scope: ScratchpadScope) {
        guard connectionState.isReady else { return }
        transmit(.submitScratchpad(
            requestID: requestID("scratchpad"),
            scope: scope,
            operation: operation
        ))
    }

    func refreshScratchpad(scope: ScratchpadScope) {
        submitScratchpadOperation(.capabilityCommand(
            capability: "scratchpad",
            command: "scratchpad",
            arguments: "refresh",
            input: nil,
            target: nil
        ), scope: scope)
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
