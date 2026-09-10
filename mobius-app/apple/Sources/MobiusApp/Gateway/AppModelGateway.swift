import Foundation

extension AppModel {
    func reduceGatewayEnvelope(_ envelope: GatewayEnvelope) {
        switch envelope {
        case .paired:
            restoreSessionReadState(for: gateway.selectedAccountID)
            showsPairing = false
            showToast("Gateway paired.", tone: .success)
            cloud.completeCloudPairing(.success(()))
        case .authenticated:
            break
        case .ready(let payload):
            applyGatewayReady(payload)
        case .realtimeVoiceStarted, .realtimeVoiceEnded, .realtimeVoiceFailed:
            chat.handleRealtimeVoiceEnvelope(
                envelope,
                eligible: selectedRouteSupportsRealtimeVoice
            )
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

    private func handleSessionEnvelope(_ envelope: GatewayEnvelope) {
        switch envelope {
        case .sessionOpened(let requestID, let payload):
            guard requestID == chat.sessionRequestID else { break }
            applySessionReady(payload, opened: true, replayRequestID: requestID)
        case .sessionReplayComplete(let requestID, let sessionID):
            guard requestID == chat.replayRequestID, sessionID == chat.selectedSessionID else {
                break
            }
            let completedRequestID = chat.finishSessionReplay()
            completePendingVoiceChat(requestID: completedRequestID)
            chat.startNextSessionFileUpload()
            submitPendingNewChatDraft(requestID: completedRequestID)
        case .sessionHistory(
            let requestID,
            let sessionID,
            let records,
            let nextBeforeSequence
        ):
            guard requestID == chat.historyRequestID, sessionID == chat.selectedSessionID else {
                break
            }
            chat.flushStreamDeltas()
            chat.mergeHistory(records)
            chat.nextHistoryBeforeSequence = nextBeforeSequence
            if !records.isEmpty,
                case .visibleTurns(let count) = chat.transcriptWindowAnchor
            {
                chat.transcriptWindowAnchor = .visibleTurns(count + transcriptTurnsPerPage)
                _ = chat.transcriptWindow
            }
            chat.finishHistoryLoad(succeeded: true)
        case .sessionChanged(let payload):
            guard payload.session.sessionId == chat.selectedSessionID else { break }
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
            guard sessionID == chat.selectedSessionID else { break }
            let buffered = BufferedAgentEvent(record: record)
            applyAgentEvent(buffered)
            if chat.replayRequestID == nil, shouldCacheTranscript(after: record.event) {
                chat.cacheSelectedTranscript()
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
        if requestID == chat.sessionMutationRequestID {
            chat.sessionMutationRequestID = nil
            chat.pendingDeletedPresentedSessionID = nil
        }
        applySessionCatalog(sessions)
    }

    private func applyBotSessionsResponse(
        requestID: String?,
        botID: String,
        sessions: [SessionRecord]
    ) {
        guard requestID == chat.botSessionsRequestID, botID == chat.botSessionsBotID else { return }
        chat.botSessionsRequestID = nil
        chat.isLoadingBotSessions = false
        let valid = applyBotSessions(sessions, botID: botID)
        guard let resume = chat.pendingBotSessionResume, resume.botID == botID else { return }
        chat.pendingBotSessionResume = nil
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
            let bot = bots.first(where: { $0.id == editingBotID })
        {
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
        let completedMutation = requestID != nil && requestID == swarmMutationRequestID
        if completedMutation { swarmMutationRequestID = nil }
        let posted = requestID != nil && requestID == swarmMessageRequestID
        if posted { swarmMessageRequestID = nil }
        if applySwarms(swarms) {
            if completedMutation { swarmApplyState = .applied }
            if posted { completedSwarmMessageRequestID = requestID }
        } else if completedMutation {
            swarmApplyState = .failed(localizedString("The gateway returned invalid swarm state."))
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
            guard requestID == pendingProviderLogin?.requestID else { break }
            providerActionState = .deviceCode(
                provider: provider,
                url: url,
                code: code
            )
        case .providerLoginFinished(let requestID, _, let provider):
            if requestID == pendingProviderLogin?.requestID {
                pendingProviderLogin = nil
                providerActionState = .loginFinished(provider)
                showToast("Signed in to \(provider).", tone: .success)
            }
            // A browser login is shared by every setup of that provider.
            for index in providerInstances.indices
            where providerInstances[index].provider == provider {
                providerInstances[index].configured = true
            }
            invalidateProviderUsage()
            refreshProfile()
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
        case .profile(let requestID, let profile):
            guard requestID == profileRequestID else { break }
            profileRequestID = nil
            self.profile = profile
        default:
            break
        }
    }

    private func handleFileEnvelope(_ envelope: GatewayEnvelope) {
        switch envelope {
        case .gitDiff(let requestID, let sessionID, let scope, let diff):
            guard sessionID == chat.selectedSessionID, requestID == gitDiffs[scope]?.requestID
            else { break }
            gitDiffs[scope]?.requestID = nil
            gitDiffs[scope]?.text = diff
        case .workspaceFiles(let requestID, let sessionID, let files, let truncated):
            guard requestID == workspaceFilesRequestID,
                sessionID == chat.selectedSessionID
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
            chat.handleSessionFileUploadReady(
                requestID: requestID,
                sessionID: sessionID,
                uploadID: uploadID,
                maxChunkBytes: maxChunkBytes
            )
        case .sessionFileUploadChunkAccepted(
            let requestID, let sessionID, let uploadID, let nextOffset):
            chat.handleSessionFileUploadChunkAccepted(
                requestID: requestID,
                sessionID: sessionID,
                uploadID: uploadID,
                nextOffset: nextOffset
            )
        case .sessionFileUploadCompleted(let requestID, let sessionID, let file):
            chat.handleSessionFileUploadCompleted(
                requestID: requestID,
                sessionID: sessionID,
                file: file
            )
        case .sessionFiles(let requestID, let sessionID, let files):
            guard requestID == chat.sessionFilesRequestID, sessionID == chat.selectedSessionID
            else { break }
            chat.sessionFilesRequestID = nil
            chat.isLoadingSessionFiles = false
            chat.sessionFiles = files
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
            if chat.pendingNewChatWorkspace == ".", !showsWorkspaceBrowser {
                chat.pendingNewChatWorkspace = listing.path
            }
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
            let wasPairing = gateway.hasPendingPairing
            if wasPairing { gateway.pairingError = failure.message }
            cloud.completeCloudPairing(.failure(MobiusCloudError.provisioningFailed))
            showToast(verbatim: failure.message, tone: .error)
            if failure.fatal {
                cancelVoiceChatIntent()
                chat.stopRealtimeVoice()
                chat.restorePendingDrafts()
                cancelExtensionAndCredentialRequests()
                sshIdentityError = failure.message
            }
        default:
            break
        }
    }

    private func applyAgentEvent(_ buffered: BufferedAgentEvent) {
        guard chat.latestSequence.map({ buffered.record.sequence > $0 }) ?? true else { return }
        let isLiveEvent = chat.replayRequestID == nil
        chat.observeReplayCompletion(buffered)
        chat.latestSequence = buffered.record.sequence
        if isLiveEvent,
            buffered.record.event.msg["type"]?.stringValue == "context_compacted"
        {
            chat.sessionCompactionCount += 1
        }
        chat.transcriptRecords[buffered.record.sequence] = buffered.record
        chat.reduce(
            record: buffered.record
        )
    }

    private func shouldCacheTranscript(after event: AgentEventRecord) -> Bool {
        switch event.msg["type"]?.stringValue {
        case "turn_complete", "turn_aborted": true
        default: false
        }
    }

    func submitPendingNewChatDraft(requestID: String?) {
        guard let requestID,
            chat.pendingDrafts[requestID] != nil
        else { return }
        Task { [weak self] in
            guard let self,
                let draft = await self.chat.takePendingNewChatDraft(requestID: requestID)
            else { return }
            self.chat.pendingNewChatBotID = nil
            let nextDraft = self.chat.composer
            self.chat.suppressesComposerDraftSave = true
            self.chat.composer = draft.text
            self.chat.suppressesComposerDraftSave = false
            self.chat.stashedComposerDraft = nextDraft
            guard self.sendMessage() else {
                self.chat.stashedComposerDraft = nil
                self.chat.suppressesComposerDraftSave = true
                self.chat.composer = nextDraft
                self.chat.suppressesComposerDraftSave = false
                self.chat.restoreDraft(draft)
                return
            }
        }
    }

    private func applyGatewayReady(_ payload: ReadyPayload) {
        applyGatewayCatalog(payload)
        gateway.connectionState = chat.sessionRequestID == nil ? .ready : .loading
        resumeProviderLogin()
        applySessionCatalog(payload.sessions)
        refreshProfile()
        guard chat.sessionRequestID == nil else { return }
        if openPendingRemoteNotification() {
            chat.sessionToRestoreID = nil
            return
        }
        if let sessionToRestoreID = chat.sessionToRestoreID {
            guard presentedChatSessionID == sessionToRestoreID else {
                clearSelectedSession()
                return
            }
            if chat.sessions.contains(where: { $0.sessionId == sessionToRestoreID })
                || chat.botSessions.contains(where: { $0.sessionId == sessionToRestoreID })
            {
                chat.restoreSession(sessionToRestoreID)
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
        let editedBotDefaultsDraft =
            requestID == botDefaultsRequestID
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
                editedBotDefaultsDraft != submittedBotDefaultsDraft
            {
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
        let machineName =
            selectedGatewayIsMobiusCloud
            ? cloudGatewayDisplayName
            : payload.machineName
        gateway.updateMachineName(machineName)
        let previousBotDefaults = botDefaultsSnapshot
        let pendingBotDefaultsDraft: AgentComposition? =
            if botDefaultsRequestID != nil {
                botDefaultsDraft
            } else {
                nil
            }
        providerStatuses = payload.providers
        providerInstances = payload.providerInstances
        chat.sessionFileLimits = payload.sessionFileLimits
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
            pendingBotDefaultsDraft
                ?? refreshedAgentDraft(
                    currentDraft: botDefaultsDraft,
                    currentSnapshot: previousBotDefaults,
                    incomingSnapshot: incomingSnapshot
                )
        }
        if providerDraft == nil, let instance = providerInstances.first {
            editProviderInstance(instance)
        }
        if !selectedRouteSupportsRealtimeVoice { chat.stopRealtimeVoice() }
    }

    private func applySessionReady(
        _ payload: SessionReadyPayload,
        opened: Bool,
        replayRequestID: String? = nil
    ) {
        guard let bot = bots.first(where: { $0.id == payload.session.context.botId }) else {
            cancelVoiceChatIntent()
            chat.restorePendingDrafts()
            chat.sessionRequestID = nil
            chat.sessionOpeningID = nil
            chat.sessionOpenCursor = nil
            chat.pendingCachedTranscript = nil
            chat.pendingPresentedTranscript = nil
            isChangingWorkspace = false
            chat.pendingNewChatBotID = nil
            gateway.connectionState = .ready
            showToast("The gateway returned a chat with an unknown Bot.", tone: .error)
            return
        }
        let createdByThisClient = opened && isChangingWorkspace
        let createdWithPendingDraft =
            createdByThisClient
            && replayRequestID.map { chat.pendingDrafts[$0] != nil } == true
        let cursor = chat.sessionOpenCursor
        let cached =
            opened && chat.sessionOpeningID == payload.session.sessionId
            ? chat.pendingCachedTranscript
            : nil
        let presented =
            opened && chat.sessionOpeningID == payload.session.sessionId
            ? chat.pendingPresentedTranscript
            : nil
        if chat.selectedSessionID != payload.session.sessionId {
            if !createdWithPendingDraft { chat.restorePendingDrafts() }
            chat.changeComposerDraftOwner(
                to: gateway.selectedAccountID.map {
                    ComposerDraftOwner(accountID: $0, sessionID: payload.session.sessionId)
                })
            resetSessionState(preservingComposerAttachments: createdByThisClient)
        }
        if opened {
            chat.latestSequence = cursor
            chat.replayRequestID = replayRequestID
            chat.replaySnapshotSequence = payload.latestSequence
            chat.sessionOpenCursor = nil
            chat.sessionOpeningID = nil
            chat.pendingCachedTranscript = nil
            chat.replayPresentedTranscript = presented ?? []
            chat.pendingPresentedTranscript = nil
            chat.transcriptRecordBase = cached?.transcript ?? []
            chat.transcriptRecordBaseSequence = cursor
            chat.transcriptRecords.removeAll(keepingCapacity: true)
            chat.transcript = cached?.transcript ?? []
            if let cached {
                chat.nextHistoryBeforeSequence = cached.nextBeforeSequence
            } else {
                chat.nextHistoryBeforeSequence = payload.nextBeforeSequence
            }
            if let cached {
                chat.currentUsage = cached.currentUsage
                chat.lastUsage = cached.lastUsage
                chat.updateContextTokens()
            }
        }
        chat.sessionRequestID = nil
        workspace = payload.workspace
        chat.attachedFolders = payload.attachedFolders
        gitStatus = payload.git
        workspaceError = nil
        isChangingWorkspace = false
        showsWorkspaceBrowser = false
        chat.pendingNewChatWorkspace = nil
        if !createdWithPendingDraft, chat.pendingDrafts.isEmpty { chat.pendingNewChatBotID = nil }
        chat.selectedSessionID = payload.session.sessionId
        if createdByThisClient {
            destination = .chats
            navigationPath = [.chat(.session(payload.session.sessionId))]
            chat.prepareChatTitle(for: payload.session.sessionId)
        }
        if isChatVisible,
            chat.sessionReadCursors?[payload.session.sessionId]?.isMarkedUnread != true
        {
            markSessionRead(payload.session.sessionId)
        }
        chat.selectedModelRoute = payload.session.model.route
        chat.modelContextWindow = payload.session.model.modelContextWindow
        chat.contextLimitTokens = payload.contextLimitTokens ?? chat.modelContextWindow
        chat.contributions = payload.contributions
        chat.mountedWidgets = payload.contributions.flatMap { contribution in
            contribution.widgets.map {
                MountedWidget(capability: contribution.capability, widget: $0)
            }
        }
        for widget in payload.widgets {
            chat.upsertWidget(MountedWidget(capability: widget.capability, widget: widget.item))
        }
        chat.runStats = payload.runStats
        chat.sessionCompactionCount = payload.compactionCount
        chat.activeTurnID = payload.runStats.active?.turnId
        agentDraft = refreshedAgentDraft(
            currentDraft: agentDraft,
            currentSnapshot: agentSnapshot,
            incomingSnapshot: bot.config
        )
        agentSnapshot = bot.config
        if !opened { gateway.connectionState = .ready }
        if let accountID = gateway.selectedAccountID {
            chat.prepareComposerEditRecovery(
                for: ComposerDraftOwner(
                    accountID: accountID,
                    sessionID: payload.session.sessionId
                )
            )
        }
        chat.persistGeneratedChatTitles()
    }

    func applySessionCatalog(_ records: [SessionRecord]) {
        guard
            records.allSatisfy({ session in
                bots.contains { $0.id == session.sessionContext.botId }
            })
        else {
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
        if chat.sessions != records {
            let previous = Dictionary(
                chat.sessions.map { ($0.sessionId, $0) },
                uniquingKeysWith: { _, latest in latest }
            )
            chat.sessions = records
            for session in chat.sessions {
                applyActivityTransition(
                    from: previous[session.sessionId],
                    to: session
                )
            }
        }
        if let selected = chat.sessions.first(where: { $0.sessionId == chat.selectedSessionID }) {
            applyExecutionStats(selected.executionStats)
            if selected.activity.state == .idle { chat.runStats.active = nil }
        }
        if let accountID = gateway.selectedAccountID {
            reconcileSessionReadState(accountID: accountID)
        }
        let visible = Set(chat.sessions.map(\.sessionId))
        chat.unreadSessionIDs.formIntersection(visible)
        chat.reconcileChatTitles()
        cacheChatCatalog()
        if gateway.connectionState.isReady, openPendingRemoteNotification() { return }
        guard chat.selectedSessionID != nil,
            selectedSession == nil,
            chat.sessionRequestID == nil
        else { return }
        clearSelectedSession()
    }

    private func reconcileSessionReadState(accountID: UUID) {
        guard var cursors = chat.sessionReadCursors else {
            let cursors = Dictionary(
                uniqueKeysWithValues: chat.sessions.map { session in
                    (session.sessionId, sessionReadCursor(for: session))
                })
            chat.sessionReadCursors = cursors
            store.saveSessionReadCursors(cursors, accountID: accountID)
            return
        }
        var changed = false
        for session in chat.sessions {
            if reconcileReadCursor(for: session, cursors: &cursors) { changed = true }
        }
        guard changed else { return }
        chat.sessionReadCursors = cursors
        store.saveSessionReadCursors(cursors, accountID: accountID)
    }

    private func reconcileReadCursor(
        for session: SessionRecord,
        cursors: inout [String: SessionReadCursor]
    ) -> Bool {
        let sessionID = session.sessionId
        let cursor = sessionReadCursor(for: session)
        if cursors[sessionID]?.isMarkedUnread == true {
            chat.unreadSessionIDs.insert(sessionID)
            return false
        }
        if chat.selectedSessionID == sessionID, isChatVisible {
            chat.unreadSessionIDs.remove(sessionID)
            guard cursors[sessionID] != cursor else { return false }
            cursors[sessionID] = cursor
            return true
        }
        if let readCursor = cursors[sessionID], let readSequence = readCursor.sequence {
            if session.activity.state == .idle,
                session.sequence > readSequence || readCursor.wasActive
            {
                chat.unreadSessionIDs.insert(sessionID)
            }
            return false
        }
        guard session.activity.state == .idle else { return false }
        if session.sequence > 0 || session.activity.lastOutcome != nil {
            chat.unreadSessionIDs.insert(sessionID)
            return false
        }
        cursors[sessionID] = cursor
        return true
    }

    @discardableResult
    func applyBotSessions(_ records: [SessionRecord], botID: String) -> Bool {
        guard chat.botSessionsBotID == botID,
            Set(records.map(\.sessionId)).count == records.count,
            records.allSatisfy({ $0.sessionContext.botId == botID })
        else {
            showToast("The gateway returned invalid Bot work.", tone: .error)
            return false
        }
        chat.botSessions = records
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
                    && record.handle
                        == record.handle.trimmingCharacters(
                            in: .whitespacesAndNewlines
                        )
            })
        else {
            showToast("The gateway returned invalid Bot state.", tone: .error)
            return
        }
        let selectedHiddenBotID =
            selectedSessionIsHidden
            ? selectedSession?.sessionContext.botId ?? chat.botSessionsBotID
            : nil
        bots = records
        let botIDs = Set(records.map(\.id))
        if let botSessionsBotID = chat.botSessionsBotID, !botIDs.contains(botSessionsBotID) {
            self.chat.botSessionsBotID = nil
            chat.botSessionsRequestID = nil
            chat.pendingBotSessionResume = nil
            chat.botSessions = []
            chat.isLoadingBotSessions = false
        }
        if let selectedHiddenBotID, !botIDs.contains(selectedHiddenBotID) {
            clearSelectedSession()
        }
        chat.chatBotFilterIDs.formIntersection(botIDs)
        backgroundApprovals.removeAll { !botIDs.contains($0.botId) }
        swarmAttentions.removeAll { !botIDs.contains($0.botId) }
        routines.removeAll { !botIDs.contains($0.botId) }
        routineRuns.removeAll { !botIDs.contains($0.botId) }
        if let botID = selectedSession?.sessionContext.botId,
            let bot = records.first(where: { $0.id == botID })
        {
            agentDraft = refreshedAgentDraft(
                currentDraft: agentDraft,
                currentSnapshot: agentSnapshot,
                incomingSnapshot: bot.config
            )
            agentSnapshot = bot.config
            if let route = modelRoute(for: bot.config.config) {
                chat.selectedModelRoute = route
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
        chat.runStats.runCount = stats.runCount
        chat.runStats.failedRunCount = stats.failedRunCount
        chat.runStats.abortedRunCount = stats.abortedRunCount
        chat.runStats.modelCalls = stats.modelCalls
        chat.runStats.toolCalls = stats.toolCalls
        chat.runStats.failedToolCalls = stats.failedToolCalls
        chat.runStats.elapsedMs = stats.elapsedMs
        chat.runStats.usage = stats.usage
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
        if gateway.connectionState.isReady { _ = openPendingRemoteNotification() }
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
        if gateway.connectionState.isReady { _ = openPendingRemoteNotification() }
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
            previous.activity.approvalRequestId != approvalRequestID
        {
            presentSessionNotification(
                .awaitingApproval,
                sessionID: sessionID,
                approvalRequestID: approvalRequestID
            )
        }
        guard activity.state == .idle,
            previous.activity.state != .idle || session.sequence > previous.sequence
        else { return }

        let isActiveChat = chat.selectedSessionID == sessionID && isChatVisible
        if isActiveChat {
            chat.unreadSessionIDs.remove(sessionID)
        } else {
            chat.unreadSessionIDs.insert(sessionID)
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

    func clearSelectedSession() {
        chat.changeComposerDraftOwner(to: nil)
        chat.latestSequence = nil
        chat.sessionOpenCursor = nil
        chat.sessionToRestoreID = nil
        chat.selectedSessionID = nil
        if isPresentingChat { navigationPath = [] }
        resetSessionState()
        gateway.connectionState = .ready
        cacheChatCatalog()
    }

    private func handleAccepted(_ requestID: String) {
        chat.acceptSessionFileDeletionRequest(requestID)
        if chat.pendingDrafts[requestID] != nil { chat.flushComposerDraft() }
        if requestID == chat.approvalRequestID {
            chat.pendingApproval = nil
            chat.approvalRequestID = nil
        }
        if requestID == chat.sessionMutationRequestID {
            for sessionID in chat.pendingDeletedSessionIDs {
                chat.cancelChatTitle(sessionID)
                if let accountID = gateway.selectedAccountID {
                    let owner = ComposerDraftOwner(accountID: accountID, sessionID: sessionID)
                    chat.invalidateComposerEditRecovery(for: owner)
                    chat.enqueueComposerDraftSave(.empty, owner: owner)
                    chat.enqueueComposerEditRecoveryRemoval(owner: owner)
                    if chat.composerDraftOwner == owner { chat.discardComposerDraft() }
                }
            }
            chat.pendingDeletedSessionIDs = []
            chat.pendingDeletedPresentedSessionID = nil
            gateway.transmit(.listSessions(requestID: requestID)) { [weak self] _ in
                if self?.chat.sessionMutationRequestID == requestID {
                    self?.chat.sessionMutationRequestID = nil
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
        if rejection.code == "profile_superseded",
            rejection.requestId != profileRequestID, !rejection.fatal
        {
            return
        }
        if rejection.requestId == profileRequestID { invalidateProviderUsage() }
        let rejectedAbandonedUpload = chat.discardAbandonedSessionFileUploadRequest(
            rejection.requestId
        )
        let rejectedFileThumbnailDownload = chat.sessionFileThumbnailDownload.flatMap { download in
            download.requestID == rejection.requestId ? download : nil
        }
        let rejectedDiscardedFileThumbnail =
            chat.discardedSessionFileThumbnailRequestIDs.remove(rejection.requestId) != nil
        let rejectedFileThumbnail =
            rejectedFileThumbnailDownload != nil
            || rejectedDiscardedFileThumbnail
        let deletedPresentedSessionID =
            rejection.requestId == chat.sessionMutationRequestID
            ? chat.pendingDeletedPresentedSessionID
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
            chat.restorePendingDrafts()
            cancelExtensionAndCredentialRequests()
            sshIdentityError = rejection.message
        }
    }

    private func handleRejectedTranscript(_ rejection: GatewayRejection) {
        if rejection.requestId == chat.historyRequestID {
            chat.finishHistoryLoad()
        }
        if rejection.requestId == chat.previewPageRequestID {
            chat.previewPageRequestID = nil
            chat.isLoadingPreviewPage = false
        }
        if rejection.requestId == chat.sessionMutationRequestID {
            chat.pendingDeletedSessionIDs = []
            chat.pendingDeletedPresentedSessionID = nil
            if let sessionID = chat.pendingChatTitles.first(where: {
                $0.value.renameRequestID == rejection.requestId
            })?.key {
                chat.cancelChatTitle(sessionID)
            }
        }
        chat.cancelChatTitle(submissionID: rejection.requestId, rearm: true)
    }

    private func retryRejectedSessionReplay(_ rejection: GatewayRejection) -> Bool {
        guard rejection.requestId == chat.sessionRequestID,
            rejection.code == "replay_unavailable",
            let sessionID = chat.sessionOpeningID,
            chat.sessionOpenCursor != nil
        else { return false }
        if let accountID = gateway.selectedAccountID {
            chat.enqueueTranscriptIO { [store] in
                await store.removeTranscript(accountID: accountID, sessionID: sessionID)
            }
        }
        chat.sessionRequestID = nil
        chat.sessionOpenCursor = nil
        chat.pendingCachedTranscript = nil
        chat.pendingPresentedTranscript = nil
        if sessionID == chat.selectedSessionID { chat.resetSessionState() }
        chat.requestSessionOpen(sessionID, lastSequence: nil)
        return true
    }

    private func handleRejectedFiles(
        _ rejection: GatewayRejection,
        thumbnail: SessionFileThumbnailDownload?
    ) {
        chat.failSessionFileUploadRequest(
            rejection.requestId,
            message: rejection.message,
            showsToast: false
        )
        chat.failSessionFileDeletionRequest(
            rejection.requestId,
            message: rejection.message,
            refreshesFiles: true,
            showsToast: false
        )
        if rejection.requestId == chat.sessionFilesRequestID {
            chat.sessionFilesRequestID = nil
            chat.isLoadingSessionFiles = false
        }
        if rejection.requestId == chat.sessionFileDownload?.requestID {
            chat.sessionFileDownload = nil
            isLoadingFilePresentation = false
        }
        if let thumbnail {
            chat.finishSessionFileThumbnailAttempt(thumbnail, startsNext: !rejection.fatal)
        }
        if rejection.requestId == workspaceFilePreviewDownload?.requestID {
            workspaceFilePreviewDownload = nil
            isLoadingFilePresentation = false
        }
        if rejection.requestId == workspaceFileWriteRequestID {
            workspaceFileWriteRequestID = nil
            isSavingWorkspaceFile = false
        }
        if chat.pendingDrafts[rejection.requestId] != nil {
            chat.restoreDraft(id: rejection.requestId)
        }
        chat.rejectComposerEdit(requestID: rejection.requestId)
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
        if rejection.requestId == chat.approvalRequestID {
            chat.approvalRequestID = nil
        }
        if rejection.requestId == chat.sessionRequestID {
            cancelVoiceChatIntent()
            chat.sessionRequestID = nil
            chat.sessionOpeningID = nil
            chat.sessionOpenCursor = nil
            chat.pendingCachedTranscript = nil
            chat.pendingPresentedTranscript = nil
            gateway.connectionState = .ready
            if isChangingWorkspace { workspaceError = rejection.message }
            isChangingWorkspace = false
        }
        if rejection.requestId == chat.sessionMutationRequestID {
            chat.sessionMutationRequestID = nil
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
            swarmApplyState = configurationApplyState(for: rejection)
        }
        if rejection.requestId == swarmMessageRequestID {
            swarmMessageRequestID = nil
        }
        if rejection.requestId == chat.botSessionsRequestID {
            chat.botSessionsRequestID = nil
            chat.pendingBotSessionResume = nil
            chat.isLoadingBotSessions = false
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
        if rejection.requestId == pendingProviderLogin?.requestID {
            providerActionState = .failed(rejection.message)
            pendingProviderLogin = nil
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
