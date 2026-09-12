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
        if chat.selectedSessionID == nil, case .chat(.new)? = navigationPath.last {
            chat.pendingNewChatWorkspace = path
            workspaceError = nil
            showsWorkspaceBrowser = false
            createPendingVoiceChat()
            return
        }
        chat.changeComposerDraftOwner(to: nil)
        chat.discardComposerAttachments()
        resetRootSessionState()
        chat.selectedSessionID = nil
        chat.sessionToRestoreID = nil
        chat.sessionOpenCursor = nil
        chat.pendingNewChatWorkspace = path
        chat.pendingNewChatBotIDs = []
        workspaceError = nil
        chat.resetSessionState()
        destination = .chats
        navigationPath = [.chat(.new)]
        showsWorkspaceBrowser = false
        cacheChatCatalog(lastSessionID: nil)
        if bots.count == 1 { selectBotForNewChat(bots[0]) }
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
}
