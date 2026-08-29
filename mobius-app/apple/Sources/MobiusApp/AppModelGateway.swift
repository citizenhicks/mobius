import Foundation

extension AppModel {
    func connect(to account: GatewayAccount, retrying: Bool = false) {
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
        case .paired, .authenticated, .ready:
            handleConnectionEnvelope(envelope)
        case .sessionOpened, .sessionReplayComplete, .sessionHistory, .sessionChanged:
            handleSessionEnvelope(envelope)
        case .gatewayConfigured, .globalScratchpadChanged, .accepted, .rejected,
             .agentEvent, .sessions, .swarms, .clients:
            handleGatewayUpdateEnvelope(envelope)
        case .providerCredentialSaved, .pairingCode, .providerLoginStarted,
             .providerLoginFinished, .gitCredentialStatus, .sshIdentities,
             .sshIdentityGenerated, .profile:
            handleCredentialEnvelope(envelope)
        case .gitDiff, .workspaceFiles, .workspaceFileChunk, .sessionFileUploadReady,
             .sessionFileUploadChunkAccepted, .sessionFileUploadCompleted,
             .sessionFiles, .sessionFileChunk, .directories:
            handleFileEnvelope(envelope)
        case .cronTasks, .cronHistory, .cronRunPreview, .error:
            handleCronOrFailureEnvelope(envelope)
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
            finishHistoryLoad()
        case .sessionChanged(let payload):
            guard payload.session.sessionId == selectedSessionID,
                  payload.config.revision >= (agentSnapshot?.revision ?? 0)
            else { break }
            applySessionReady(payload, opened: false)
        default:
            break
        }
    }

    private func handleGatewayUpdateEnvelope(_ envelope: GatewayEnvelope) {
        switch envelope {
        case .gatewayConfigured(let requestID, let payload):
            applyGatewayConfigurationResponse(requestID: requestID, payload: payload)
        case .globalScratchpadChanged(_, let contribution):
            guard contribution.capability == "scratchpad" else { break }
            if let index = gatewayContributions.firstIndex(where: {
                $0.capability == contribution.capability
            }) {
                gatewayContributions[index] = contribution
            } else {
                gatewayContributions.append(contribution)
            }
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
            if requestID == sessionMutationRequestID {
                sessionMutationRequestID = nil
                pendingDeletedPresentedSessionID = nil
            }
            applySessions(sessions)
        case .swarms(let requestID, let swarms):
            if requestID == swarmMutationRequestID { swarmMutationRequestID = nil }
            applySwarms(swarms)
        case .clients:
            break
        default:
            break
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
            guard sessionID == selectedSessionID else { break }
            if scope == .unstaged, requestID == gitDiffRequestID {
                gitDiffRequestID = nil
                isLoadingGitDiff = false
                gitDiff = diff
            } else if scope == .staged, requestID == stagedGitDiffRequestID {
                stagedGitDiffRequestID = nil
                isLoadingStagedGitDiff = false
                stagedGitDiff = diff
            } else if scope == .committed, requestID == committedGitDiffRequestID {
                committedGitDiffRequestID = nil
                isLoadingCommittedGitDiff = false
                committedGitDiff = diff
            }
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

    private func handleCronOrFailureEnvelope(_ envelope: GatewayEnvelope) {
        switch envelope {
        case .cronTasks(let requestID, let tasks):
            cronRequestIDs.remove(requestID)
            cronTasks = tasks
        case .cronHistory(let requestID, let runs):
            cronRequestIDs.remove(requestID)
            cronRuns = runs
        case .cronRunPreview(let preview):
            applyCronRunPreview(preview)
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
        guard let accountID = selectedAccountID,
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
        applySessions(payload.sessions)
        refreshProfile()
        guard sessionRequestID == nil else { return }
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
        let editedDefaultDraft = requestID == defaultConfigRequestID
            ? defaultAgentDraft
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
        } else if requestID == defaultConfigRequestID {
            defaultConfigRequestID = nil
            if let editedDefaultDraft,
               let submittedDefaultAgentDraft,
               editedDefaultDraft != submittedDefaultAgentDraft {
                defaultAgentDraft = editedDefaultDraft
            }
            submittedDefaultAgentDraft = nil
            defaultAgentApplyState = .applied
            showToast("Default agent saved for new chats.", tone: .success)
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
        let previousDefault = defaultAgentSnapshot
        let pendingDefaultDraft: AgentComposition? = if defaultConfigRequestID != nil {
            defaultAgentDraft
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
        applySwarms(payload.swarms)
        defaultAgentSnapshot = payload.defaultConfig
        defaultAgentDraft = payload.defaultConfig.map { incomingSnapshot in
            pendingDefaultDraft ?? refreshedAgentDraft(
                currentDraft: defaultAgentDraft,
                currentSnapshot: previousDefault,
                incomingSnapshot: incomingSnapshot
            )
        }
        if providerDraft == nil, let instance = providerInstances.first {
            editProviderInstance(instance)
        }
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
        let createdByThisClient = opened && isChangingWorkspace
        let cursor = sessionOpenCursor
        let cached = opened && sessionOpeningID == payload.session.sessionId
            ? pendingCachedTranscript
            : nil
        let presented = opened && sessionOpeningID == payload.session.sessionId
            ? pendingPresentedTranscript
            : nil
        if selectedSessionID != payload.session.sessionId {
            restorePendingDrafts()
            changeComposerDraftOwner(to: selectedAccountID.map {
                ComposerDraftOwner(accountID: $0, sessionID: payload.session.sessionId)
            })
            resetSessionState()
        }
        if opened {
            latestSequence = cursor
            self.replayRequestID = replayRequestID
            replaySnapshotSequence = payload.latestSequence
            sessionOpenCursor = nil
            sessionOpeningID = nil
            pendingCachedTranscript = nil
            pendingPresentedTranscript = nil
            replayPresentedTranscript = presented ?? []
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
        selectedSessionID = payload.session.sessionId
        if createdByThisClient {
            destination = .chats
            navigationPath = [.chat(.session(payload.session.sessionId))]
            prepareChatTitle(for: payload.session.sessionId)
        }
        if isChatVisible {
            unreadSessionIDs.remove(payload.session.sessionId)
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
            incomingSnapshot: payload.config
        )
        agentSnapshot = payload.config
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

    func applySessions(_ records: [SessionRecord]) {
        guard Set(records.map(\.sessionId)).count == records.count else {
            showToast("The gateway returned duplicate chat identifiers.", tone: .error)
            return
        }
        if sessions != records {
            let previous = Dictionary(
                sessions.map { ($0.sessionId, $0.activity) },
                uniquingKeysWith: { _, latest in latest }
            )
            sessions = records
            for session in sessions {
                applyActivityTransition(
                    from: previous[session.sessionId],
                    to: session.activity,
                    sessionID: session.sessionId
                )
            }
        }
        if let selected = sessions.first(where: { $0.sessionId == selectedSessionID }) {
            applyExecutionStats(selected.executionStats)
            if selected.activity.state == .idle { runStats.active = nil }
        }
        let visible = Set(sessions.map(\.sessionId))
        unreadSessionIDs.formIntersection(visible)
        reconcileChatTitles()
        guard let selectedSessionID,
              !sessions.contains(where: { $0.sessionId == selectedSessionID }),
              sessionRequestID == nil
        else { return }
        clearSelectedSession()
    }

    func applySwarms(_ records: [SwarmRecord]) {
        guard Set(records.map(\.id)).count == records.count,
              records.allSatisfy({ swarm in
                  let orderedMessages = zip(swarm.messages, swarm.messages.dropFirst())
                      .allSatisfy { pair in pair.0.sequence < pair.1.sequence }
                  return Set(swarm.members.map(\.sessionId)).count == swarm.members.count
                      && swarm.members.contains { $0.sessionId == swarm.leaderSessionId }
                      && Set(swarm.messages.map(\.id)).count == swarm.messages.count
                      && orderedMessages
              })
        else {
            showToast("The gateway returned invalid swarm state.", tone: .error)
            return
        }
        swarms = records
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

    private func applyActivityTransition(
        from previous: SessionActivity?,
        to activity: SessionActivity,
        sessionID: String
    ) {
        guard let previous, previous != activity else { return }
        if activity.state == .awaitingApproval,
           previous.state != .awaitingApproval {
            showToast("\(sessionTitle(sessionID)) needs approval.", tone: .warning)
        }
        guard activity.state == .idle,
              previous.state != .idle
                || previous.lastOutcome != activity.lastOutcome
                || previous.message != activity.message
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
            guard !isActiveChat else { return }
            showToast("\(sessionTitle(sessionID)) is ready.", tone: .success, sessionID: sessionID)
        case .aborted:
            guard !isActiveChat else { return }
            let title = sessionTitle(sessionID)
            if let message = activity.message {
                showToast("\(title) stopped: \(message).", tone: .warning)
            } else {
                showToast("\(title) stopped.", tone: .warning)
            }
        case .failed:
            let title = sessionTitle(sessionID)
            if let message = activity.message {
                showToast("\(title) failed: \(message).", tone: .error)
            } else {
                showToast("\(title) failed.", tone: .error)
            }
        }
    }

    private func requestSessionData() {
        guard selectedSessionID != nil else { return }
        refreshWorkspaceChanges()
        refreshSessionFiles()
        refreshCron()
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
    }

    private func handleAccepted(_ requestID: String) {
        if pendingDrafts[requestID] != nil { flushComposerDraft() }
        if requestID == approvalRequestID {
            pendingApproval = nil
            approvalRequestID = nil
        }
        if requestID == configRequestID {
            configRequestID = nil
            chatAgentApplyState = .applied
            showToast("Agent configuration applied.", tone: .success)
        }
        if requestID == sessionMutationRequestID {
            if let sessionID = pendingDeletedSessionID {
                cancelChatTitle(sessionID)
                if let accountID = selectedAccountID {
                    let owner = ComposerDraftOwner(accountID: accountID, sessionID: sessionID)
                    invalidateComposerEditRecovery(for: owner)
                    enqueueComposerDraftSave("", owner: owner)
                    enqueueComposerEditRecoveryRemoval(owner: owner)
                    if composerDraftOwner == owner { discardComposerDraft() }
                }
            }
            pendingDeletedSessionID = nil
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
        if cronRequestIDs.remove(requestID) != nil {
            refreshCron()
        }
    }

    private func handleRejected(_ rejection: GatewayRejection) {
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
        if !rejectedFileThumbnail || rejection.fatal {
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
            pendingDeletedSessionID = nil
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
        if rejection.requestId == configRequestID
            || rejection.requestId == defaultConfigRequestID {
            let state: ApplyState = switch rejection.code {
            case "revision_conflict": .conflict(rejection.message)
            case "agent_busy": .busy(rejection.message)
            case "invalid_config": .invalid(rejection.message)
            default: .failed(rejection.message)
            }
            if rejection.requestId == configRequestID {
                chatAgentApplyState = state
                configRequestID = nil
            }
            if rejection.requestId == defaultConfigRequestID {
                defaultAgentApplyState = state
                defaultConfigRequestID = nil
                submittedDefaultAgentDraft = nil
            }
        }
        if rejection.requestId == approvalRequestID {
            approvalRequestID = nil
        }
        if rejection.requestId == sessionRequestID {
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
        if rejection.requestId == gitDiffRequestID {
            gitDiffRequestID = nil
            isLoadingGitDiff = false
        }
        if rejection.requestId == stagedGitDiffRequestID {
            stagedGitDiffRequestID = nil
            isLoadingStagedGitDiff = false
        }
        if rejection.requestId == committedGitDiffRequestID {
            committedGitDiffRequestID = nil
            isLoadingCommittedGitDiff = false
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
        if cronRequestIDs.remove(rejection.requestId) != nil {
            cronError = rejection.message
        }
        if rejection.requestId == cronRunPreviewRequestID {
            cronRunPreviewRequestID = nil
            cronRunPreviewRequestBeforeSequence = nil
            isLoadingCronRunPreview = false
            cronRunPreviewError = rejection.message
        }
    }

}
