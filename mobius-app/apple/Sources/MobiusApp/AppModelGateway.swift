import Foundation

extension AppModel {
    func connect(to account: GatewayAccount, retrying: Bool = false) {
        if account.id == mobiusCloudGateway?.id,
           cloudIssue == .subscriptionExpired {
            handleCloudSubscriptionExpired()
            showToast(
                verbatim: localizedString(
                    MobiusCloudError.subscriptionRequired.localizedDescriptionResource
                ),
                tone: .warning
            )
            return
        }
        cancelReconnect()
        if !retrying {
            reconnectAttempt = 0
            automaticReconnectBlocked = false
        }
        let sameGateway = account.id == selectedAccountID
        let sessionID = sameGateway ? presentedChatSessionID : nil
        let generation = resetGatewayState(
            preservingDrafts: sameGateway,
            preservingSession: sessionID != nil
        )
        sessionToRestoreID = sessionID
        selectedAccountID = account.id
        store.select(account)
        restoreSessionReadState()
        connectionState = .connecting
        Task { [weak self] in
            guard let self, self.connectionGeneration == generation else { return }
            await self.client.disconnect()
            guard self.connectionGeneration == generation else { return }
            do {
                let token = try self.store.token(for: account)
                self.beginConnection(to: account.endpoint, generation: generation) { [weak self] in
                    guard let self, self.connectionGeneration == generation else { return }
                    try await self.requestSender(.authenticate(
                        token: token,
                        clientKind: .currentApplePlatform
                    ))
                }
            } catch {
                self.automaticReconnectBlocked = true
                self.connectionState = .failed(error.localizedDescription)
                self.showToast(verbatim: self.localizedErrorDescription(error), tone: .error)
                if let storeError = error as? GatewayStore.StoreError,
                   case .missingToken = storeError {
                    self.repairSelectedGateway()
                }
            }
        }
    }

    func beginConnection(
        to endpoint: GatewayEndpoint,
        generation: UUID,
        authenticate: @escaping @MainActor @Sendable () async throws -> Void
    ) {
        connectionState = .connecting

        Task { [weak self] in
            guard let self else { return }
            do {
                let stream = try await self.connectionOpener(endpoint)
                guard generation == self.connectionGeneration else { return }
                self.connectionState = .authenticating
                self.eventTask = Task { [weak self] in
                    do {
                        var handledFrames = 0
                        for try await frame in stream {
                            guard let self, generation == self.connectionGeneration else { return }
                            self.handle(frame)
                            handledFrames += 1
                            if handledFrames.isMultiple(of: 32) { await Task.yield() }
                        }
                        self?.connectionEnded(generation: generation, message: "The gateway closed the connection.")
                    } catch {
                        self?.connectionEnded(generation: generation, error: error)
                    }
                }
                if self.selectedGatewayIsMobiusCloud { self.scheduleReconnect() }
                try await authenticate()
            } catch {
                self.connectionEnded(generation: generation, error: error)
            }
        }
    }

    func transmit(
        _ request: GatewayRequest,
        onFailure: (@MainActor (String) -> Void)? = nil
    ) {
        let generation = connectionGeneration
        Task { [weak self] in
            guard let self, generation == self.connectionGeneration else { return }
            do {
                try await self.requestSender(request)
            } catch {
                guard generation == self.connectionGeneration else { return }
                let message = GatewayWireError.disconnected.localizedDescription
                onFailure?(message)
                self.connectionEnded(generation: generation, message: message)
            }
        }
    }

    func handle(_ envelope: GatewayEnvelope) {
        switch envelope {
        case .realtimeVoiceStarted, .realtimeVoiceEnded, .realtimeVoiceFailed:
            handleRealtimeVoiceEnvelope(envelope)
        case .paired, .authenticated, .ready:
            handleConnectionEnvelope(envelope)
        case .sessionOpened, .sessionReplayComplete, .sessionHistory, .sessionChanged:
            handleSessionEnvelope(envelope)
        case .gatewayConfigured, .contributionsChanged, .accepted, .rejected,
             .agentEvent, .sessions, .backgroundApprovals, .swarmAttentions, .botSessions,
             .bots, .swarms, .clients:
            handleGatewayUpdateEnvelope(envelope)
        case .providerCredentialSaved, .pairingCode, .providerLoginStarted,
             .providerLoginFinished, .gitCredentialStatus, .sshIdentities,
             .sshIdentityGenerated, .profile:
            handleCredentialEnvelope(envelope)
        case .gitDiff, .workspaceFiles, .workspaceFileChunk, .sessionFileUploadReady,
             .sessionFileUploadChunkAccepted, .sessionFileUploadCompleted,
             .sessionFiles, .sessionFileChunk, .directories:
            handleFileEnvelope(envelope)
        case .routines, .routineHistory, .routineRunPreview, .error:
            handleRoutineOrFailureEnvelope(envelope)
        }
    }

    private func handleConnectionEnvelope(_ envelope: GatewayEnvelope) {
        switch envelope {
        case .paired(_, let token):
            guard let account = pendingPairingAccount else { return }
            do {
                try store.save(account, token: token)
                accounts = store.loadAccounts()
                selectedAccountID = account.id
                restoreSessionReadState()
                pendingPairingAccount = nil
                pairingCode = ""
                showsPairing = false
                showToast("Gateway paired.", tone: .success)
                completeCloudPairing(.success(()))
            } catch {
                pairingError = localizedErrorDescription(error)
                showToast(verbatim: localizedErrorDescription(error), tone: .error)
                completeCloudPairing(.failure(error))
            }
        case .authenticated:
            connectionState = .loading
        case .ready(let payload):
            applyGatewayReady(payload)
        default:
            break
        }
    }

    private func handleSessionEnvelope(_ envelope: GatewayEnvelope) {
        switch envelope {
        case .sessionOpened(let requestID, let payload):
            guard requestID == sessionRequestID else { break }
            applySessionReady(payload, opened: true, replayRequestID: requestID)
        case .sessionReplayComplete(let requestID, let sessionID):
            guard requestID == replayRequestID, sessionID == selectedSessionID else { break }
            finishSessionReplay()
        case .sessionHistory(
            let requestID,
            let sessionID,
            let records,
            let nextBeforeSequence
        ):
            guard requestID == historyRequestID, sessionID == selectedSessionID else { break }
            flushStreamDeltas()
            mergeHistory(records)
            self.nextHistoryBeforeSequence = nextBeforeSequence
            if !records.isEmpty,
               case .visibleTurns(let count) = transcriptWindowAnchor {
                transcriptWindowAnchor = .visibleTurns(count + transcriptTurnsPerPage)
                _ = transcriptWindow
            }
            finishHistoryLoad(succeeded: true)
        case .sessionChanged(let payload):
            guard payload.session.sessionId == selectedSessionID else { break }
            applySessionReady(payload, opened: false)
        default:
            break
        }
    }

    private func handleGatewayUpdateEnvelope(_ envelope: GatewayEnvelope) {
        switch envelope {
        case .gatewayConfigured(let requestID, let payload):
            applyGatewayConfigurationResponse(requestID: requestID, payload: payload)
        case .contributionsChanged(_, let scope, let contributions):
            applyScopedContributions(contributions, scope: scope)
        case .accepted(let requestID):
            handleAccepted(requestID)
        case .rejected(let rejection):
            handleRejected(rejection)
        case .agentEvent(let sessionID, let record):
            guard sessionID == selectedSessionID else { break }
            let buffered = BufferedAgentEvent(record: record)
            applyAgentEvent(buffered)
            if replayRequestID == nil, shouldCacheTranscript(after: record.event) {
                cacheSelectedTranscript()
            }
        case .sessions(let requestID, let sessions):
            applySessionResponse(requestID: requestID, sessions: sessions)
        case .backgroundApprovals(let approvals):
            applyBackgroundApprovals(approvals, notifyingNew: true)
        case .swarmAttentions(let attentions):
            applySwarmAttentions(attentions, notifyingNew: true)
        case .botSessions(let requestID, let botID, let sessions):
            applyBotSessionsResponse(requestID: requestID, botID: botID, sessions: sessions)
        case .bots(let requestID, let bots):
            applyBotsResponse(requestID: requestID, bots: bots)
        case .swarms(let requestID, let swarms):
            applySwarmsResponse(requestID: requestID, swarms: swarms)
        case .clients:
            break
        default:
            break
        }
    }

    private func applyScopedContributions(
        _ contributions: [FrontendContribution],
        scope: ContributionScope
    ) {
        switch scope {
        case .global:
            gatewayContributions = contributions
        case .swarm(let id):
            guard swarms.contains(where: { $0.id == id }) else { return }
            swarmContributions[id] = contributions
        }
    }

    private func applySessionResponse(requestID: String?, sessions: [SessionRecord]) {
        if requestID == sessionMutationRequestID {
            sessionMutationRequestID = nil
            pendingDeletedPresentedSessionID = nil
        }
        applySessionCatalog(sessions)
    }

    private func applyBotSessionsResponse(
        requestID: String?,
        botID: String,
        sessions: [SessionRecord]
    ) {
        guard requestID == botSessionsRequestID, botID == botSessionsBotID else { return }
        botSessionsRequestID = nil
        isLoadingBotSessions = false
        let valid = applyBotSessions(sessions, botID: botID)
        guard let resume = pendingBotSessionResume, resume.botID == botID else { return }
        pendingBotSessionResume = nil
        guard valid else { return }
        guard sessions.contains(where: { $0.sessionId == resume.sessionID }) else {
            showToast("That Bot work is no longer available.", tone: .warning)
            return
        }
        openBotSession(resume.sessionID)
    }

    private func applyBotsResponse(requestID: String?, bots: [BotRecord]) {
        let completedMutation = requestID != nil && requestID == botMutationRequestID
        if completedMutation { botMutationRequestID = nil }
        applyBots(bots)
        guard completedMutation else { return }
        if let editingBotID,
           let bot = bots.first(where: { $0.id == editingBotID }) {
            editingBotRevision = bot.config.revision
            botNameDraft = bot.name
            botDescriptionDraft = bot.description
            botTintDraft = bot.tint
            botDraft = bot.config.config
        }
        botApplyState = .applied
        showToast(
            verbatim: botMutationSuccessMessage ?? localizedString("Bot saved."),
            tone: .success
        )
        botMutationSuccessMessage = nil
    }

    private func applySwarmsResponse(requestID: String?, swarms: [SwarmRecord]) {
        if requestID == swarmMutationRequestID { swarmMutationRequestID = nil }
        let posted = requestID != nil && requestID == swarmMessageRequestID
        if posted { swarmMessageRequestID = nil }
        if applySwarms(swarms), posted {
            completedSwarmMessageRequestID = requestID
        }
    }

    private func handleCredentialEnvelope(_ envelope: GatewayEnvelope) {
        switch envelope {
        case .providerCredentialSaved(let requestID, let instance, let provider):
            guard let pending = pendingProviderCredential,
                  requestID == pending.requestID,
                  instance == pending.instance,
                  provider == pending.provider
            else { break }
            // An API key belongs to the one setup that received it.
            if let index = providerInstances.firstIndex(where: { $0.instance == instance }) {
                providerInstances[index].configured = true
                providerInstances[index].credentialHint = pending.credentialHint
            }
            pendingProviderCredential = nil
            providerAPIKey = ""
            providerActionState = .credentialSaved(instance)
            showToast("\(providerLabel(for: instance)) credential saved.", tone: .success)
        case .pairingCode(let requestID, let code, let expiresAt):
            guard requestID == pairingCodeRequestID else { break }
            pairingCodeRequestID = nil
            setPairingCode(
                code,
                expiresAt: Date(timeIntervalSince1970: TimeInterval(expiresAt))
            )
        case .providerLoginStarted(let requestID, _, let provider, let url, let code):
            guard requestID == providerLoginRequestID else { break }
            providerActionState = .deviceCode(
                provider: provider,
                url: url,
                code: code
            )
        case .providerLoginFinished(let requestID, _, let provider):
            if requestID == providerLoginRequestID {
                providerLoginRequestID = nil
                providerActionState = .loginFinished(provider)
                showToast("Signed in to \(provider).", tone: .success)
            }
            // A browser login is shared by every setup of that provider.
            for index in providerInstances.indices
            where providerInstances[index].provider == provider {
                providerInstances[index].configured = true
            }
        case .gitCredentialStatus(let requestID, let available, let username):
            guard requestID == gitCredentialRequestID else { break }
            let approved = isApprovingGitCredential
            gitCredentialRequestID = nil
            isApprovingGitCredential = false
            isCheckingGitCredential = false
            gitCredentialAvailable = available
            gitCredentialUsername = available ? username : nil
            gitCredentialError = nil
            if approved, available {
                showToast("Git credential saved by the gateway host.", tone: .success)
            }
        case .sshIdentities(let requestID, let identities):
            guard requestID == sshIdentityRequestID else { break }
            sshIdentityRequestID = nil
            isLoadingSshIdentities = false
            sshIdentityError = nil
            sshIdentities = identities
        case .sshIdentityGenerated(let requestID, let identity, let publicKey):
            guard requestID == sshIdentityRequestID else { break }
            sshIdentityRequestID = nil
            isGeneratingSshIdentity = false
            sshIdentityError = nil
            sshIdentities = [identity]
            generatedSshIdentity = GeneratedSshIdentity(
                identity: identity,
                publicKey: publicKey
            )
            showToast("SSH identity created on the gateway host.", tone: .success)
        case .profile(_, let profile):
            self.profile = profile
        default:
            break
        }
    }

    private func handleFileEnvelope(_ envelope: GatewayEnvelope) {
        switch envelope {
        case .gitDiff(let requestID, let sessionID, let scope, let diff):
            guard sessionID == selectedSessionID, requestID == gitDiffs[scope]?.requestID else { break }
            gitDiffs[scope]?.requestID = nil
            gitDiffs[scope]?.text = diff
        case .workspaceFiles(let requestID, let sessionID, let files, let truncated):
            guard requestID == workspaceFilesRequestID,
                  sessionID == selectedSessionID
            else { break }
            workspaceFilesRequestID = nil
            isLoadingWorkspaceFiles = false
            workspaceFiles = files
            workspaceFilesTruncated = truncated
        case .workspaceFileChunk(
            let requestID,
            let sessionID,
            let path,
            let offset,
            let data,
            let nextOffset
        ):
            handleWorkspaceFileChunk(
                requestID: requestID,
                sessionID: sessionID,
                path: path,
                offset: offset,
                data: data,
                nextOffset: nextOffset
            )
        case .sessionFileUploadReady(let requestID, let sessionID, let uploadID, let maxChunkBytes):
            handleSessionFileUploadReady(
                requestID: requestID,
                sessionID: sessionID,
                uploadID: uploadID,
                maxChunkBytes: maxChunkBytes
            )
        case .sessionFileUploadChunkAccepted(let requestID, let sessionID, let uploadID, let nextOffset):
            handleSessionFileUploadChunkAccepted(
                requestID: requestID,
                sessionID: sessionID,
                uploadID: uploadID,
                nextOffset: nextOffset
            )
        case .sessionFileUploadCompleted(let requestID, let sessionID, let file):
            handleSessionFileUploadCompleted(
                requestID: requestID,
                sessionID: sessionID,
                file: file
            )
            if pendingNewChatBotID != nil, pendingDrafts.count == 1 {
                submitPendingNewChatDraft(requestID: pendingDrafts.keys.first)
            }
        case .sessionFiles(let requestID, let sessionID, let files):
            guard requestID == sessionFilesRequestID, sessionID == selectedSessionID else { break }
            sessionFilesRequestID = nil
            isLoadingSessionFiles = false
            sessionFiles = files
        case .sessionFileChunk(
            let requestID,
            let sessionID,
            let fileID,
            let offset,
            let data,
            let nextOffset
        ):
            handleSessionFileChunk(
                requestID: requestID,
                sessionID: sessionID,
                fileID: fileID,
                offset: offset,
                data: data,
                nextOffset: nextOffset
            )
        case .directories(let requestID, let listing):
            guard requestID == directoryRequestID else { break }
            directoryRequestID = nil
            directoryListing = listing
            directoryError = nil
            isLoadingDirectories = false
        default:
            break
        }
    }

    private func handleRoutineOrFailureEnvelope(_ envelope: GatewayEnvelope) {
        switch envelope {
        case .routines(let requestID, let records):
            routineRequestIDs.remove(requestID)
            let botIDs = Set(bots.map(\.id))
            routines = records.filter { botIDs.contains($0.botId) }
        case .routineHistory(let requestID, let runs):
            routineRequestIDs.remove(requestID)
            let botIDs = Set(bots.map(\.id))
            routineRuns = runs.filter { botIDs.contains($0.botId) }
        case .routineRunPreview(let preview):
            applyRoutineRunPreview(preview)
        case .error(let failure):
            let wasPairing = pendingPairingAccount != nil
            if wasPairing { pairingError = failure.message }
            if cloudPairingContinuation != nil {
                completeCloudPairing(.failure(MobiusCloudError.provisioningFailed))
            }
            if failure.code == "unauthorized", !wasPairing {
                automaticReconnectBlocked = true
                cancelReconnect()
                repairSelectedGateway()
            }
            showToast(verbatim: failure.message, tone: .error)
            if failure.fatal {
                cancelVoiceChatIntent()
                stopRealtimeVoice()
                automaticReconnectBlocked = true
                cancelReconnect()
                connectionGeneration = UUID()
                eventTask?.cancel()
                eventTask = nil
                restorePendingDrafts()
                cancelExtensionAndCredentialRequests()
                sshIdentityError = failure.message
                connectionState = .failed(failure.message)
            }
        default:
            break
        }
    }

    private func applyAgentEvent(_ buffered: BufferedAgentEvent) {
        guard latestSequence.map({ buffered.record.sequence > $0 }) ?? true else { return }
        let isLiveEvent = replayRequestID == nil
        observeReplayCompletion(buffered)
        latestSequence = buffered.record.sequence
        if isLiveEvent,
           buffered.record.event.msg["type"]?.stringValue == "context_compacted" {
            sessionCompactionCount += 1
        }
        transcriptRecords[buffered.record.sequence] = buffered.record
        reduce(
            record: buffered.record
        )
    }

    private func finishSessionReplay() {
        let completedRequestID = replayRequestID
        flushStreamDeltas()
        if let replaySnapshotSequence { latestSequence = replaySnapshotSequence }
        replayRequestID = nil
        replaySnapshotSequence = nil
        replayPresentedTranscript = nil
        connectionState = .ready
        completedComposerEditReplay = true
        reconcileChatTitleAfterReplay()
        reconcileComposerEditRecovery()
        requestSessionData()
        cacheSelectedTranscript()
        submitPendingNewChatDraft(requestID: completedRequestID)
        completePendingVoiceChat(requestID: completedRequestID)
    }

    func submitPendingNewChatDraft(requestID: String?) {
        guard let requestID,
              pendingDrafts[requestID] != nil,
              let sessionID = selectedSessionID
        else { return }
        guard !composerHasUnfinishedAttachments else {
            startNextSessionFileUpload()
            return
        }
        let draftIO = composerDraftIOTask
        Task { [weak self] in
            await draftIO?.value
            guard let self,
                  self.connectionState.isReady,
                  self.selectedSessionID == sessionID,
                  let draft = self.pendingDrafts.removeValue(forKey: requestID)
            else { return }
            self.pendingNewChatBotID = nil
            let nextDraft = self.composer
            self.suppressesComposerDraftSave = true
            self.composer = draft.text
            self.suppressesComposerDraftSave = false
            self.stashedComposerDraft = nextDraft
            guard self.sendMessage() else {
                self.stashedComposerDraft = nil
                self.suppressesComposerDraftSave = true
                self.composer = nextDraft
                self.suppressesComposerDraftSave = false
                self.restoreDraft(draft)
                return
            }
        }
    }

    /// A disconnected submission is ambiguous until replay proves whether it reached the
    /// checkpoint. Only then can the restored draft safely become title-eligible again.
    private func reconcileChatTitleAfterReplay() {
        guard let sessionID = selectedSessionID,
              let pending = pendingChatTitles[sessionID],
              !pending.submissionConfirmed
        else { return }
        let promptWasReplayed = replayCompletionSubmissionIDs.contains(
            pending.attempt.submissionID
        ) || replayUserMessages.contains {
            $0.text.trimmingCharacters(in: .whitespacesAndNewlines) == pending.attempt.prompt
        }
        if promptWasReplayed {
            confirmChatTitle(sessionID: sessionID)
        } else {
            cancelChatTitle(sessionID, rearm: true)
        }
    }

    private func shouldCacheTranscript(after event: AgentEventRecord) -> Bool {
        switch event.msg["type"]?.stringValue {
        case "turn_complete", "turn_aborted": true
        default: false
        }
    }

    func cacheSelectedTranscript() {
        guard !isClearingLocalData,
              let accountID = selectedAccountID,
              let sessionID = selectedSessionID,
              let latestSequence,
              activeTurnID == nil,
              pendingApproval == nil,
              pendingWidgetEdit == nil
        else { return }
        let snapshot = CachedTranscript(
            sequence: latestSequence,
            nextBeforeSequence: nextHistoryBeforeSequence,
            transcript: transcript,
            currentUsage: currentUsage,
            lastUsage: lastUsage
        )
        enqueueTranscriptIO { [store] in
            await store.saveTranscript(
                snapshot,
                accountID: accountID,
                sessionID: sessionID
            )
        }
    }

    private func applyGatewayReady(_ payload: ReadyPayload) {
        cancelReconnect()
        reconnectAttempt = 0
        automaticReconnectBlocked = false
        applyGatewayCatalog(payload)
        if sessionRequestID == nil { connectionState = .ready }
        applySessionCatalog(payload.sessions)
        refreshProfile()
        guard sessionRequestID == nil else { return }
        if openPendingRemoteNotification() {
            sessionToRestoreID = nil
            return
        }
        if let sessionToRestoreID {
            guard presentedChatSessionID == sessionToRestoreID else {
                clearSelectedSession()
                return
            }
            if let session = sessions.first(where: { $0.sessionId == sessionToRestoreID }) {
                restoreSession(session.sessionId)
            } else {
                showToast("The previously selected chat is no longer available.", tone: .error)
                clearSelectedSession()
            }
        }
    }

    func applyGatewayConfigurationResponse(
        requestID: String,
        payload: ReadyPayload
    ) {
        let removedProvider = pendingProviderRemoval.flatMap {
            $0.requestID == requestID ? $0 : nil
        }
        let removedProviderLabel = removedProvider.flatMap { removal in
            providerInstances.first { $0.instance == removal.instance }?.label
        }
        let editedBotDefaultsDraft = requestID == botDefaultsRequestID
            ? botDefaultsDraft
            : nil
        applyGatewayReady(payload)
        if let removedProvider {
            pendingProviderRemoval = nil
            if providerDraft?.instance == removedProvider.instance { providerDraft = nil }
            if navigationPath.last == .settings(.provider(removedProvider.instance)) {
                navigationPath.removeLast()
            }
            providerActionState = .idle
            let provider = removedProviderLabel ?? localizedString("Provider")
            showToast("\(provider) removed.", tone: .success)
        } else if requestID == providerRegistrationRequestID {
            providerRegistrationRequestID = nil
            providerActionState = .idle
            showToast("Provider saved.", tone: .success)
        } else if requestID == botDefaultsRequestID {
            botDefaultsRequestID = nil
            if let editedBotDefaultsDraft,
               let submittedBotDefaultsDraft,
               editedBotDefaultsDraft != submittedBotDefaultsDraft {
                botDefaultsDraft = editedBotDefaultsDraft
            }
            submittedBotDefaultsDraft = nil
            botDefaultsApplyState = .applied
            showToast("Bot defaults saved for new chats.", tone: .success)
        } else {
            completeExtensionAction(requestID: requestID)
        }
    }

    func applyGatewayCatalog(_ payload: ReadyPayload) {
        let machineName = selectedGatewayIsMobiusCloud
            ? mobiusCloudGatewayDisplayName
            : payload.machineName
        gatewayMachineName = machineName
        rememberGatewayMachineName(machineName)
        let previousBotDefaults = botDefaultsSnapshot
        let pendingBotDefaultsDraft: AgentComposition? = if botDefaultsRequestID != nil {
            botDefaultsDraft
        } else {
            nil
        }
        providerStatuses = payload.providers
        providerInstances = payload.providerInstances
        sessionFileLimits = payload.sessionFileLimits
        modelChoices = payload.models
        modelProviders = payload.modelProviders
        middlewareFeatures = payload.middlewareFeatures
        extensions = payload.extensions
        gatewayContributions = payload.contributions
        applyBots(payload.bots)
        applySwarms(payload.swarms)
        applyBackgroundApprovals(payload.backgroundApprovals, notifyingNew: false)
        applySwarmAttentions(payload.swarmAttentions, notifyingNew: false)
        botDefaultsSnapshot = payload.botDefaults
        botDefaultsDraft = payload.botDefaults.map { incomingSnapshot in
            pendingBotDefaultsDraft ?? refreshedAgentDraft(
                currentDraft: botDefaultsDraft,
                currentSnapshot: previousBotDefaults,
                incomingSnapshot: incomingSnapshot
            )
        }
        if providerDraft == nil, let instance = providerInstances.first {
            editProviderInstance(instance)
        }
        if !selectedRouteSupportsRealtimeVoice { stopRealtimeVoice() }
    }

    private func rememberGatewayMachineName(_ machineName: String) {
        guard let account = selectedAccount,
              account.machineName != machineName,
              let index = accounts.firstIndex(where: { $0.id == account.id })
        else { return }
        accounts[index].machineName = machineName
        try? store.recordMachineName(machineName, for: account)
    }

    private func applySessionReady(
        _ payload: SessionReadyPayload,
        opened: Bool,
        replayRequestID: String? = nil
    ) {
        guard let bot = bots.first(where: { $0.id == payload.session.context.botId }) else {
            cancelVoiceChatIntent()
            restorePendingDrafts()
            sessionRequestID = nil
            sessionOpeningID = nil
            sessionOpenCursor = nil
            pendingCachedTranscript = nil
            pendingPresentedTranscript = nil
            isChangingWorkspace = false
            pendingNewChatBotID = nil
            connectionState = .ready
            showToast("The gateway returned a chat with an unknown Bot.", tone: .error)
            return
        }
        let createdByThisClient = opened && isChangingWorkspace
        let createdWithPendingDraft = createdByThisClient
            && replayRequestID.map { pendingDrafts[$0] != nil } == true
        let cursor = sessionOpenCursor
        let cached = opened && sessionOpeningID == payload.session.sessionId
            ? pendingCachedTranscript
            : nil
        let presented = opened && sessionOpeningID == payload.session.sessionId
            ? pendingPresentedTranscript
            : nil
        if selectedSessionID != payload.session.sessionId {
            if !createdWithPendingDraft { restorePendingDrafts() }
            changeComposerDraftOwner(to: selectedAccountID.map {
                ComposerDraftOwner(accountID: $0, sessionID: payload.session.sessionId)
            })
            resetSessionState(preservingComposerAttachments: createdWithPendingDraft)
        }
        if opened {
            latestSequence = cursor
            self.replayRequestID = replayRequestID
            replaySnapshotSequence = payload.latestSequence
            sessionOpenCursor = nil
            sessionOpeningID = nil
            pendingCachedTranscript = nil
            replayPresentedTranscript = presented ?? []
            pendingPresentedTranscript = nil
            transcriptRecordBase = cached?.transcript ?? []
            transcriptRecordBaseSequence = cursor
            transcriptRecords.removeAll(keepingCapacity: true)
            transcript = cached?.transcript ?? []
            if let cached {
                nextHistoryBeforeSequence = cached.nextBeforeSequence
            } else {
                nextHistoryBeforeSequence = payload.nextBeforeSequence
            }
            if let cached {
                currentUsage = cached.currentUsage
                lastUsage = cached.lastUsage
                updateContextTokens()
            }
        }
        sessionRequestID = nil
        workspace = payload.workspace
        gitStatus = payload.git
        workspaceError = nil
        isChangingWorkspace = false
        showsWorkspaceBrowser = false
        pendingNewChatWorkspace = nil
        if !createdWithPendingDraft, pendingDrafts.isEmpty { pendingNewChatBotID = nil }
        selectedSessionID = payload.session.sessionId
        if createdByThisClient {
            destination = .chats
            navigationPath = [.chat(.session(payload.session.sessionId))]
            prepareChatTitle(for: payload.session.sessionId)
        }
        if isChatVisible {
            markSessionRead(payload.session.sessionId)
        }
        selectedModelRoute = payload.session.model.route
        modelContextWindow = payload.session.model.modelContextWindow
        contextLimitTokens = payload.contextLimitTokens ?? modelContextWindow
        contributions = payload.contributions
        mountedWidgets = payload.contributions.flatMap { contribution in
            contribution.widgets.map {
                MountedWidget(capability: contribution.capability, widget: $0)
            }
        }
        for widget in payload.widgets {
            upsertWidget(MountedWidget(capability: widget.capability, widget: widget.item))
        }
        runStats = payload.runStats
        sessionCompactionCount = payload.compactionCount
        activeTurnID = payload.runStats.active?.turnId
        agentDraft = refreshedAgentDraft(
            currentDraft: agentDraft,
            currentSnapshot: agentSnapshot,
            incomingSnapshot: bot.config
        )
        agentSnapshot = bot.config
        if !opened { connectionState = .ready }
        if let accountID = selectedAccountID {
            prepareComposerEditRecovery(
                for: ComposerDraftOwner(
                    accountID: accountID,
                    sessionID: payload.session.sessionId
                )
            )
        }
        persistGeneratedChatTitles()
    }

    func applySessionCatalog(_ records: [SessionRecord]) {
        guard records.allSatisfy({ session in
            bots.contains { $0.id == session.sessionContext.botId }
        }) else {
            showToast("The gateway returned a chat with an unknown Bot.", tone: .error)
            return
        }
        applySessions(records)
    }

    func applySessions(_ records: [SessionRecord]) {
        guard Set(records.map(\.sessionId)).count == records.count else {
            showToast("The gateway returned duplicate chat identifiers.", tone: .error)
            return
        }
        if sessions != records {
            let previous = Dictionary(
                sessions.map { ($0.sessionId, $0) },
                uniquingKeysWith: { _, latest in latest }
            )
            sessions = records
            for session in sessions {
                applyActivityTransition(
                    from: previous[session.sessionId],
                    to: session
                )
            }
        }
        if let selected = sessions.first(where: { $0.sessionId == selectedSessionID }) {
            applyExecutionStats(selected.executionStats)
            if selected.activity.state == .idle { runStats.active = nil }
        }
        if let accountID = selectedAccountID { reconcileSessionReadState(accountID: accountID) }
        let visible = Set(sessions.map(\.sessionId))
        unreadSessionIDs.formIntersection(visible)
        reconcileChatTitles()
        cacheChatCatalog()
        if connectionState.isReady, openPendingRemoteNotification() { return }
        guard selectedSessionID != nil,
              selectedSession == nil,
              sessionRequestID == nil
        else { return }
        clearSelectedSession()
    }

    private func reconcileSessionReadState(accountID: UUID) {
        guard var cursors = sessionReadCursors else {
            let cursors = Dictionary(uniqueKeysWithValues: sessions.map { session in
                (session.sessionId, sessionReadCursor(for: session))
            })
            sessionReadCursors = cursors
            store.saveSessionReadCursors(cursors, accountID: accountID)
            return
        }
        var changed = false
        for session in sessions {
            if reconcileReadCursor(for: session, cursors: &cursors) { changed = true }
        }
        guard changed else { return }
        sessionReadCursors = cursors
        store.saveSessionReadCursors(cursors, accountID: accountID)
    }

    private func reconcileReadCursor(
        for session: SessionRecord,
        cursors: inout [String: SessionReadCursor]
    ) -> Bool {
        let sessionID = session.sessionId
        let cursor = sessionReadCursor(for: session)
        if selectedSessionID == sessionID, isChatVisible {
            unreadSessionIDs.remove(sessionID)
            guard cursors[sessionID] != cursor else { return false }
            cursors[sessionID] = cursor
            return true
        }
        if let readCursor = cursors[sessionID] {
            if session.activity.state == .idle,
               session.sequence > readCursor.sequence || readCursor.wasActive {
                unreadSessionIDs.insert(sessionID)
            }
            return false
        }
        guard session.activity.state == .idle else { return false }
        if session.sequence > 0 || session.activity.lastOutcome != nil {
            unreadSessionIDs.insert(sessionID)
            return false
        }
        cursors[sessionID] = cursor
        return true
    }

    @discardableResult
    func applyBotSessions(_ records: [SessionRecord], botID: String) -> Bool {
        guard botSessionsBotID == botID,
              Set(records.map(\.sessionId)).count == records.count,
              records.allSatisfy({ $0.sessionContext.botId == botID })
        else {
            showToast("The gateway returned invalid Bot work.", tone: .error)
            return false
        }
        botSessions = records
        return true
    }

    func applyBots(_ records: [BotRecord]) {
        guard Set(records.map(\.id)).count == records.count,
              Set(records.map(\.handle)).count == records.count,
              records.allSatisfy({ record in
                  !record.id.isEmpty
                      && !record.handle.isEmpty
                      && !record.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                      && !record.description.trimmingCharacters(
                          in: .whitespacesAndNewlines
                      ).isEmpty
                      && record.handle == record.handle.trimmingCharacters(
                          in: .whitespacesAndNewlines
                      )
              })
        else {
            showToast("The gateway returned invalid Bot state.", tone: .error)
            return
        }
        let selectedHiddenBotID = selectedSessionIsHidden
            ? selectedSession?.sessionContext.botId ?? botSessionsBotID
            : nil
        bots = records
        let botIDs = Set(records.map(\.id))
        if let botSessionsBotID, !botIDs.contains(botSessionsBotID) {
            self.botSessionsBotID = nil
            botSessionsRequestID = nil
            pendingBotSessionResume = nil
            botSessions = []
            isLoadingBotSessions = false
        }
        if let selectedHiddenBotID, !botIDs.contains(selectedHiddenBotID) {
            clearSelectedSession()
        }
        chatBotFilterIDs.formIntersection(botIDs)
        backgroundApprovals.removeAll { !botIDs.contains($0.botId) }
        swarmAttentions.removeAll { !botIDs.contains($0.botId) }
        routines.removeAll { !botIDs.contains($0.botId) }
        routineRuns.removeAll { !botIDs.contains($0.botId) }
        if let botID = selectedSession?.sessionContext.botId,
           let bot = records.first(where: { $0.id == botID }) {
            agentDraft = refreshedAgentDraft(
                currentDraft: agentDraft,
                currentSnapshot: agentSnapshot,
                incomingSnapshot: bot.config
            )
            agentSnapshot = bot.config
            if let route = modelRoute(for: bot.config.config) {
                selectedModelRoute = route
            }
        }
        if let editingBotID, !records.contains(where: { $0.id == editingBotID }) {
            self.editingBotID = nil
            editingBotRevision = nil
            botDraft = nil
            botNameDraft = ""
            botDescriptionDraft = ""
            botTintDraft = .appDefault
            botApplyState = .idle
        }
        cacheChatCatalog()
    }

    @discardableResult
    func applySwarms(_ records: [SwarmRecord]) -> Bool {
        var claimedBotIDs = Set<String>()
        guard Set(records.map(\.id)).count == records.count,
              records.allSatisfy({ swarm in
                  let orderedMessages = zip(swarm.messages, swarm.messages.dropFirst())
                      .allSatisfy { pair in pair.0.sequence < pair.1.sequence }
                  return Set(swarm.members.map(\.botId)).count == swarm.members.count
                      && swarm.members.contains { $0.botId == swarm.leaderBotId }
                      && swarm.members.allSatisfy { member in
                          bots.contains { $0.id == member.botId }
                              && claimedBotIDs.insert(member.botId).inserted
                      }
                      && Set(swarm.messages.map(\.id)).count == swarm.messages.count
                      && orderedMessages
              })
        else {
            showToast("The gateway returned invalid swarm state.", tone: .error)
            return false
        }
        swarms = records
        let swarmIDs = Set(records.map(\.id))
        swarmAttentions.removeAll { !swarmIDs.contains($0.swarmId) }
        swarmContributions = swarmContributions.filter {
            swarmIDs.contains($0.key)
        }
        cacheChatCatalog()
        return true
    }

    private func applyExecutionStats(_ stats: ExecutionStats) {
        runStats.runCount = stats.runCount
        runStats.failedRunCount = stats.failedRunCount
        runStats.abortedRunCount = stats.abortedRunCount
        runStats.modelCalls = stats.modelCalls
        runStats.toolCalls = stats.toolCalls
        runStats.failedToolCalls = stats.failedToolCalls
        runStats.elapsedMs = stats.elapsedMs
        runStats.usage = stats.usage
    }

    @discardableResult
    func applyBackgroundApprovals(
        _ records: [BackgroundApproval],
        notifyingNew: Bool
    ) -> Bool {
        let botIDs = Set(bots.map(\.id))
        guard Set(records.map(\.sessionId)).count == records.count,
              Set(records.map(\.requestId)).count == records.count,
              records.allSatisfy({ approval in
                  !approval.sessionId.isEmpty
                      && !approval.botId.isEmpty
                      && !approval.turnId.isEmpty
                      && !approval.requestId.isEmpty
                      && botIDs.contains(approval.botId)
              })
        else {
            showToast("The gateway returned invalid background approval state.", tone: .error)
            return false
        }
        let previousRequestIDs = Set(backgroundApprovals.map(\.requestId))
        backgroundApprovals = records
        if notifyingNew {
            for approval in records where !previousRequestIDs.contains(approval.requestId) {
                presentSessionNotification(
                    .awaitingApproval,
                    sessionID: approval.sessionId,
                    approvalRequestID: approval.requestId
                )
            }
        }
        if connectionState.isReady { _ = openPendingRemoteNotification() }
        return true
    }

    @discardableResult
    func applySwarmAttentions(
        _ records: [SwarmAttention],
        notifyingNew: Bool
    ) -> Bool {
        let botIDs = Set(bots.map(\.id))
        let previousMessageIDs = Set(swarmAttentions.map(\.messageId))
        guard Set(records.map(\.messageId)).count == records.count,
              records.allSatisfy({ attention in
                  !attention.swarmId.isEmpty
                      && !attention.swarmTitle.trimmingCharacters(
                          in: .whitespacesAndNewlines
                      ).isEmpty
                      && !attention.messageId.isEmpty
                      && !attention.botId.isEmpty
                      && !attention.text.trimmingCharacters(
                          in: .whitespacesAndNewlines
                      ).isEmpty
                      && swarms.contains { swarm in
                          swarm.id == attention.swarmId
                      }
                      && botIDs.contains(attention.botId)
              })
        else {
            showToast("The gateway returned invalid Swarm attention state.", tone: .error)
            return false
        }
        swarmAttentions = records
        if notifyingNew {
            for attention in records where !previousMessageIDs.contains(attention.messageId) {
                presentSwarmAttention(attention)
            }
        }
        if connectionState.isReady { _ = openPendingRemoteNotification() }
        return true
    }

    private func applyActivityTransition(
        from previous: SessionRecord?,
        to session: SessionRecord
    ) {
        guard let previous, previous.activity != session.activity else { return }
        let activity = session.activity
        let sessionID = session.sessionId
        if activity.state == .awaitingApproval,
           let approvalRequestID = activity.approvalRequestId,
           previous.activity.approvalRequestId != approvalRequestID {
            presentSessionNotification(
                .awaitingApproval,
                sessionID: sessionID,
                approvalRequestID: approvalRequestID
            )
        }
        guard activity.state == .idle,
              previous.activity.state != .idle || session.sequence > previous.sequence
        else { return }

        let isActiveChat = selectedSessionID == sessionID && isChatVisible
        if isActiveChat {
            unreadSessionIDs.remove(sessionID)
        } else {
            unreadSessionIDs.insert(sessionID)
        }

        guard let outcome = activity.lastOutcome else { return }
        switch outcome {
        case .completed:
            presentSessionNotification(
                .completed,
                sessionID: sessionID,
                runCount: session.executionStats.runCount,
                detail: activity.message,
                canRefineCompletion: true
            )
        case .aborted:
            presentSessionNotification(
                .aborted,
                sessionID: sessionID,
                runCount: session.executionStats.runCount,
                detail: activity.message
            )
        case .failed:
            presentSessionNotification(
                .failed,
                sessionID: sessionID,
                runCount: session.executionStats.runCount,
                detail: activity.message
            )
        }
    }

    private func requestSessionData() {
        guard selectedSessionID != nil else { return }
        refreshWorkspaceChanges()
        refreshSessionFiles()
        startNextSessionFileThumbnailDownload()
    }

    func clearSelectedSession() {
        changeComposerDraftOwner(to: nil)
        latestSequence = nil
        sessionOpenCursor = nil
        sessionToRestoreID = nil
        selectedSessionID = nil
        navigationPath = []
        resetSessionState()
        connectionState = .ready
        cacheChatCatalog()
    }

    private func handleAccepted(_ requestID: String) {
        acceptSessionFileDeletionRequest(requestID)
        if pendingDrafts[requestID] != nil { flushComposerDraft() }
        if requestID == approvalRequestID {
            pendingApproval = nil
            approvalRequestID = nil
        }
        if requestID == sessionMutationRequestID {
            for sessionID in pendingDeletedSessionIDs {
                cancelChatTitle(sessionID)
                if let accountID = selectedAccountID {
                    let owner = ComposerDraftOwner(accountID: accountID, sessionID: sessionID)
                    invalidateComposerEditRecovery(for: owner)
                    enqueueComposerDraftSave(.empty, owner: owner)
                    enqueueComposerEditRecoveryRemoval(owner: owner)
                    if composerDraftOwner == owner { discardComposerDraft() }
                }
            }
            pendingDeletedSessionIDs = []
            pendingDeletedPresentedSessionID = nil
            transmit(.listSessions(requestID: requestID)) { [weak self] _ in
                if self?.sessionMutationRequestID == requestID {
                    self?.sessionMutationRequestID = nil
                }
            }
        }
        if requestID == gitBranchRequestID {
            gitBranchRequestID = nil
            showToast("Git branch changed.", tone: .success)
            refreshWorkspaceChanges()
        }
        if requestID == workspaceFileWriteRequestID {
            workspaceFileWriteRequestID = nil
            isSavingWorkspaceFile = false
            textFilePreview = nil
            showToast("File saved.", tone: .success)
            refreshWorkspaceFiles()
        }
        if routineRequestIDs.remove(requestID) != nil {
            refreshRoutines()
        }
    }

    private func handleRejected(_ rejection: GatewayRejection) {
        let rejectedAbandonedUpload = discardAbandonedSessionFileUploadRequest(
            rejection.requestId
        )
        let rejectedFileThumbnailDownload = sessionFileThumbnailDownload.flatMap { download in
            download.requestID == rejection.requestId ? download : nil
        }
        let rejectedDiscardedFileThumbnail =
            discardedSessionFileThumbnailRequestIDs.remove(rejection.requestId) != nil
        let rejectedFileThumbnail = rejectedFileThumbnailDownload != nil
            || rejectedDiscardedFileThumbnail
        let deletedPresentedSessionID = rejection.requestId == sessionMutationRequestID
            ? pendingDeletedPresentedSessionID
            : nil
        handleRejectedTranscript(rejection)
        if retryRejectedSessionReplay(rejection) { return }
        handleRejectedFiles(rejection, thumbnail: rejectedFileThumbnailDownload)
        handleRejectedConfiguration(rejection, deletedSessionID: deletedPresentedSessionID)
        handleRejectedWorkspace(rejection)
        handleRejectedCapabilities(rejection)
        if (!rejectedFileThumbnail && !rejectedAbandonedUpload) || rejection.fatal {
            showToast(
                verbatim: rejection.message,
                tone: rejection.code == "revision_conflict" || rejection.code == "agent_busy"
                    ? .warning
                    : .error
            )
        }
        if rejection.fatal {
            automaticReconnectBlocked = true
            cancelReconnect()
            connectionGeneration = UUID()
            eventTask?.cancel()
            eventTask = nil
            restorePendingDrafts()
            cancelExtensionAndCredentialRequests()
            sshIdentityError = rejection.message
            connectionState = .failed(rejection.message)
        }
    }

    private func handleRejectedTranscript(_ rejection: GatewayRejection) {
        if rejection.requestId == historyRequestID {
            finishHistoryLoad()
        }
        if rejection.requestId == previewPageRequestID {
            previewPageRequestID = nil
            isLoadingPreviewPage = false
        }
        if rejection.requestId == sessionMutationRequestID {
            pendingDeletedSessionIDs = []
            pendingDeletedPresentedSessionID = nil
            if let sessionID = pendingChatTitles.first(where: {
                $0.value.renameRequestID == rejection.requestId
            })?.key {
                cancelChatTitle(sessionID)
            }
        }
        cancelChatTitle(submissionID: rejection.requestId, rearm: true)
    }

    private func retryRejectedSessionReplay(_ rejection: GatewayRejection) -> Bool {
        guard rejection.requestId == sessionRequestID,
              rejection.code == "replay_unavailable",
              let sessionID = sessionOpeningID,
              sessionOpenCursor != nil
        else { return false }
        if let accountID = selectedAccountID {
            enqueueTranscriptIO { [store] in
                await store.removeTranscript(accountID: accountID, sessionID: sessionID)
            }
        }
        sessionRequestID = nil
        sessionOpenCursor = nil
        pendingCachedTranscript = nil
        pendingPresentedTranscript = nil
        if sessionID == selectedSessionID { resetSessionState() }
        requestSessionOpen(sessionID, lastSequence: nil)
        return true
    }

    private func handleRejectedFiles(
        _ rejection: GatewayRejection,
        thumbnail: SessionFileThumbnailDownload?
    ) {
        failSessionFileUploadRequest(
            rejection.requestId,
            message: rejection.message,
            showsToast: false
        )
        failSessionFileDeletionRequest(
            rejection.requestId,
            message: rejection.message,
            refreshesFiles: true,
            showsToast: false
        )
        if rejection.requestId == sessionFilesRequestID {
            sessionFilesRequestID = nil
            isLoadingSessionFiles = false
        }
        if rejection.requestId == sessionFileDownload?.requestID {
            sessionFileDownload = nil
            isLoadingFilePresentation = false
        }
        if let thumbnail {
            finishSessionFileThumbnailAttempt(thumbnail, startsNext: !rejection.fatal)
        }
        if rejection.requestId == workspaceFilePreviewDownload?.requestID {
            workspaceFilePreviewDownload = nil
            isLoadingFilePresentation = false
        }
        if rejection.requestId == workspaceFileWriteRequestID {
            workspaceFileWriteRequestID = nil
            isSavingWorkspaceFile = false
        }
        if pendingDrafts[rejection.requestId] != nil {
            restoreDraft(id: rejection.requestId)
        }
        rejectComposerEdit(requestID: rejection.requestId)
    }

    private func handleRejectedConfiguration(
        _ rejection: GatewayRejection,
        deletedSessionID: String?
    ) {
        if rejection.requestId == botDefaultsRequestID {
            botDefaultsApplyState = configurationApplyState(for: rejection)
            botDefaultsRequestID = nil
            submittedBotDefaultsDraft = nil
        }
        if rejection.requestId == approvalRequestID {
            approvalRequestID = nil
        }
        if rejection.requestId == sessionRequestID {
            cancelVoiceChatIntent()
            sessionRequestID = nil
            sessionOpeningID = nil
            sessionOpenCursor = nil
            pendingCachedTranscript = nil
            pendingPresentedTranscript = nil
            connectionState = .ready
            if isChangingWorkspace { workspaceError = rejection.message }
            isChangingWorkspace = false
        }
        if rejection.requestId == sessionMutationRequestID {
            sessionMutationRequestID = nil
            restoreDeletedPresentedSession(deletedSessionID)
        }
    }

    private func handleRejectedWorkspace(_ rejection: GatewayRejection) {
        if rejection.requestId == directoryRequestID {
            directoryError = rejection.message
            directoryRequestID = nil
            isLoadingDirectories = false
        }
        for scope in GitDiffScope.allCases where gitDiffs[scope]?.requestID == rejection.requestId {
            gitDiffs[scope]?.requestID = nil
        }
        if rejection.requestId == workspaceFilesRequestID {
            workspaceFilesRequestID = nil
            isLoadingWorkspaceFiles = false
        }
        if rejection.requestId == gitBranchRequestID {
            gitBranchRequestID = nil
        }
        if rejection.requestId == gitCredentialRequestID {
            gitCredentialRequestID = nil
            isApprovingGitCredential = false
            isCheckingGitCredential = false
            gitCredentialError = rejection.message
        }
        if rejection.requestId == sshIdentityRequestID {
            sshIdentityRequestID = nil
            isLoadingSshIdentities = false
            isGeneratingSshIdentity = false
            sshIdentityError = rejection.message
        }
    }

    private func handleRejectedCapabilities(_ rejection: GatewayRejection) {
        if rejection.requestId == swarmMutationRequestID {
            swarmMutationRequestID = nil
        }
        if rejection.requestId == swarmMessageRequestID {
            swarmMessageRequestID = nil
        }
        if rejection.requestId == botSessionsRequestID {
            botSessionsRequestID = nil
            pendingBotSessionResume = nil
            isLoadingBotSessions = false
        }
        if rejection.requestId == botMutationRequestID {
            botMutationRequestID = nil
            botMutationSuccessMessage = nil
            botApplyState = configurationApplyState(for: rejection)
        }
        if rejection.requestId == pendingProviderCredential?.requestID {
            providerActionState = .failed(rejection.message)
            pendingProviderCredential = nil
        }
        if rejection.requestId == providerLoginRequestID {
            providerActionState = .failed(rejection.message)
            providerLoginRequestID = nil
        }
        if rejection.requestId == providerRegistrationRequestID {
            providerActionState = .failed(rejection.message)
            providerRegistrationRequestID = nil
        }
        if rejection.requestId == pendingProviderRemoval?.requestID {
            providerActionState = .failed(rejection.message)
            pendingProviderRemoval = nil
        }
        rejectExtensionAction(requestID: rejection.requestId)
        if rejection.requestId == pairingCodeRequestID {
            pairingCodeRequestID = nil
        }
        if routineRequestIDs.remove(rejection.requestId) != nil {
            routineError = rejection.message
        }
        if rejection.requestId == routineRunPreviewRequestID {
            routineRunPreviewRequestID = nil
            routineRunPreviewRequestBeforeSequence = nil
            isLoadingRoutineRunPreview = false
            routineRunPreviewError = rejection.message
        }
    }

    private func configurationApplyState(for rejection: GatewayRejection) -> ApplyState {
        switch rejection.code {
        case "revision_conflict": .conflict(rejection.message)
        case "agent_busy": .busy(rejection.message)
        case "invalid_config": .invalid(rejection.message)
        default: .failed(rejection.message)
        }
    }

}
