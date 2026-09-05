import Foundation

extension AppModel {
    func connectionEnded(generation: UUID, error: Error) {
        guard connectionGeneration == generation else { return }
        if case .unsupportedVersion(let version) = error as? GatewayWireError,
           version > gatewayProtocolVersion {
            automaticReconnectBlocked = true
            showsAppUpdateAlert = true
            connectionEnded(
                generation: generation,
                message: "Update möbius to connect to this gateway."
            )
            return
        }
        connectionEnded(generation: generation, message: error.localizedDescription)
    }

    func connectionEnded(generation: UUID, message: String) {
        guard connectionGeneration == generation else { return }
        cancelVoiceChatIntent()
        stopRealtimeVoice()
        cancelReconnect()
        connectionGeneration = UUID()
        transcriptLoadGeneration = UUID()
        eventTask = nil
        connectionState = .failed(message)
        sessionFileUploadRequests.removeAll()
        abandonedSessionFileUploadRequests.removeAll()
        sessionFileDeleteRequests.removeAll()
        activeSessionFileUpload = nil
        sessionFilesRequestID = nil
        isLoadingSessionFiles = false
        for scope in GitDiffScope.allCases { gitDiffs[scope]?.requestID = nil }
        cancelExtensionAndCredentialRequests()
        workspaceFilesRequestID = nil
        workspaceFileWriteRequestID = nil
        isLoadingWorkspaceFiles = false
        isSavingWorkspaceFile = false
        discardPendingComposerAttachments()
        discardFilePresentation(preservingWorkspaceTextDraft: true)
        cancelSessionFileThumbnailDownloads()
        restorePendingDrafts()
        if pendingPairingAccount != nil { pairingError = message }
        if cloudPairingContinuation != nil {
            completeCloudPairing(.failure(MobiusCloudError.provisioningFailed))
        }
        if reconnectAttempt == 0 { showToast(verbatim: message, tone: .error) }
        scheduleReconnect()
    }

    func scheduleReconnect() {
        guard reconnectTask == nil,
              !automaticReconnectBlocked,
              pendingPairingAccount == nil,
              let account = selectedAccount
        else { return }
        guard !appIsInBackground else {
            reconnectsOnActivation = true
            return
        }
        let attempt = reconnectAttempt
        reconnectAttempt += 1
        let generation = connectionGeneration
        reconnectTask = Task { [weak self] in
            guard let self else { return }
            do {
                try await Task.sleep(for: reconnectDelay(attempt))
            } catch {
                return
            }
            guard !Task.isCancelled,
                  generation == connectionGeneration,
                  selectedAccountID == account.id
            else { return }
            reconnectTask = nil
            connect(to: account, retrying: true)
        }
    }

    func cancelReconnect() {
        reconnectTask?.cancel()
        reconnectTask = nil
    }

    @discardableResult
    func resetGatewayState(
        preservingDrafts: Bool,
        preservingSession: Bool = false
    ) -> UUID {
        cancelVoiceChatIntent()
        stopRealtimeVoice()
        if cloudPairingContinuation != nil {
            completeCloudPairing(.failure(CancellationError()))
        }
        if !preservingSession { changeComposerDraftOwner(to: nil) }
        if preservingSession { flushStreamDeltas() }
        connectionGeneration = UUID()
        abandonedSessionFileUploadRequests.removeAll()
        sessionFileDeleteRequests.removeAll()
        transcriptLoadGeneration = UUID()
        eventTask?.cancel()
        eventTask = nil
        if !preservingSession {
            latestSequence = nil
        }
        sessionOpenCursor = nil
        replayRequestID = nil
        replaySnapshotSequence = nil
        finishHistoryLoad()
        if !preservingSession {
            nextHistoryBeforeSequence = nil
            transcriptWindowAnchor = .tail
            awaitingInitialMessageTurnID = nil
        }
        if !preservingSession { replayPresentedTranscript = nil }
        if preservingDrafts {
            discardPendingComposerAttachments()
        } else {
            pendingDrafts.removeAll()
            composer = ""
            discardComposerAttachments()
        }
        pendingPairingAccount = nil
        connectionState = .disconnected
        dismissToast()
        sessionRequestID = nil
        sessionOpeningID = nil
        pendingCachedTranscript = nil
        pendingPresentedTranscript = nil
        botSessionsRequestID = nil
        pendingBotSessionResume = nil
        botSessions = []
        botSessionsBotID = nil
        isLoadingBotSessions = false
        sessionMutationRequestID = nil
        swarmMutationRequestID = nil
        swarmMessageRequestID = nil
        completedSwarmMessageRequestID = nil
        botMutationRequestID = nil
        botMutationSuccessMessage = nil
        pendingDeletedSessionIDs = []
        pendingDeletedPresentedSessionID = nil
        if preservingSession {
            for sessionID in Array(pendingChatTitles.keys) {
                pendingChatTitles[sessionID]?.renameRequestID = nil
            }
        }
        sessionToRestoreID = nil
        botDefaultsRequestID = nil
        submittedBotDefaultsDraft = nil
        botApplyState = .idle
        botDefaultsApplyState = .idle
        workspaceError = nil
        isChangingWorkspace = false
        pendingNewChatWorkspace = nil
        pendingNewChatBotID = nil
        showsWorkspaceBrowser = false
        directoryListing = nil
        directoryError = nil
        directoryRequestID = nil
        isLoadingDirectories = false
        if preservingSession {
            for scope in GitDiffScope.allCases { gitDiffs[scope]?.requestID = nil }
            workspaceFilesRequestID = nil
            isLoadingWorkspaceFiles = false
            sessionFilesRequestID = nil
            isLoadingSessionFiles = false
            sessionFileUploadRequests.removeAll()
            activeSessionFileUpload = nil
            discardFilePresentation(preservingWorkspaceTextDraft: true)
            cancelSessionFileThumbnailDownloads()
        }
        if !preservingSession {
            chatTitleTasks.values.forEach { $0.cancel() }
            chatTitleTasks.removeAll()
            titleEligibleSessionIDs.removeAll()
            pendingChatTitles.removeAll()
            sessions = []
            backgroundApprovals = []
            swarmAttentions = []
            chatBotFilterIDs.removeAll()
            bots = []
            swarms = []
            gatewayMachineName = ""
            selectedSessionID = nil
            navigationPath = []
            sessionToRename = nil
            sessionRenameDraft = ""
            sessionToDelete = nil
            unreadSessionIDs.removeAll()
            profile = nil
            modelChoices = []
            modelProviders = [:]
            middlewareFeatures = []
            extensions = []
            gatewayContributions = []
            swarmContributions = [:]
            providerStatuses = []
            providerInstances = []
            sessionFileLimits = nil
            botDefaultsSnapshot = nil
            botDefaultsDraft = nil
            editingBotID = nil
            editingBotRevision = nil
            botDraft = nil
            botNameDraft = ""
            botDescriptionDraft = ""
            botTintDraft = .appDefault
            providerDraft = nil
        }
        providerAPIKey = ""
        providerModelIDsText = ""
        providerReasoningEffortsText = ""
        providerActionState = .idle
        pendingProviderCredential = nil
        providerLoginRequestID = nil
        providerRegistrationRequestID = nil
        pendingProviderRemoval = nil
        gitCredentialAvailable = nil
        gitCredentialUsername = nil
        gitCredentialError = nil
        sshIdentities = nil
        sshIdentityError = nil
        cancelExtensionAndCredentialRequests()
        generatedSshIdentity = nil
        pairingCodeRequestID = nil
        pairingCodeExpiryTask?.cancel()
        pairingCodeExpiryTask = nil
        pairingCodeInfo = nil
        pairingCode = ""
        pairingError = nil
        if !preservingSession {
            discardFileThumbnails()
            resetSessionState()
        }
        if preservingDrafts { restorePendingDrafts() }
        return connectionGeneration
    }

    func cancelExtensionAndCredentialRequests() {
        extensionAction = nil
        extensionRequestID = nil
        gitCredentialRequestID = nil
        isApprovingGitCredential = false
        isCheckingGitCredential = false
        sshIdentityRequestID = nil
        isLoadingSshIdentities = false
        isGeneratingSshIdentity = false
    }

    func resetSessionState(preservingComposerAttachments: Bool = false) {
        stopRealtimeVoice()
        composerReply = nil
        messageNavigationRequest = nil
        workspace = nil
        gitStatus = nil
        for scope in GitDiffScope.allCases {
            gitDiffs[scope, default: GitDiffState()].text = ""
            gitDiffs[scope]?.requestID = nil
        }
        workspaceFiles = []
        workspaceFilesTruncated = false
        workspaceFilesRequestID = nil
        workspaceFileWriteRequestID = nil
        isLoadingWorkspaceFiles = false
        isSavingWorkspaceFile = false
        filesInspectorTab = .modified
        modifiedFilesScope = .unstaged
        gitBranchRequestID = nil
        if !preservingComposerAttachments { discardComposerAttachments() }
        cancelSessionFileThumbnailDownloads()
        sessionFiles = []
        sessionFilesRequestID = nil
        isLoadingSessionFiles = false
        sessionFileUploadRequests.removeAll()
        activeSessionFileUpload = nil
        discardFilePresentation()
        selectedModelRoute = ""
        contributions = []
        agentSnapshot = nil
        agentDraft = nil
        routines = []
        routineRuns = []
        routineError = nil
        routineRequestIDs.removeAll()
        routineRunPreviewPollingTask?.cancel()
        routineRunPreviewPollingTask = nil
        routineRunPreviewRequestID = nil
        routineRunPreviewRequestBeforeSequence = nil
        presentedRoutineRun = nil
        routineRunPreview = nil
        routineRunPreviewEntries = []
        routineRunPreviewNextBeforeSequence = nil
        isLoadingRoutineRunPreview = false
        routineRunPreviewError = nil
        transcript = []
        deltaFlushTask?.cancel()
        deltaFlushTask = nil
        bufferedDeltas.removeAll()
        replayRequestID = nil
        replaySnapshotSequence = nil
        replayPresentedTranscript = nil
        transcriptRecordBase = []
        transcriptRecordBaseSequence = nil
        transcriptRecords.removeAll(keepingCapacity: true)
        replayCompletionSubmissionIDs.removeAll(keepingCapacity: true)
        replayUserMessages.removeAll(keepingCapacity: true)
        completedComposerEditReplay = false
        finishHistoryLoad()
        nextHistoryBeforeSequence = nil
        transcriptWindowAnchor = .tail
        activeTurnID = nil
        awaitingInitialMessageTurnID = nil
        runStats = RunStats()
        contextTokens = 0
        sessionCompactionCount = 0
        modelContextWindow = nil
        contextLimitTokens = nil
        pendingApproval = nil
        approvalRequestID = nil
        pendingPicker = nil
        mountedWidgets = []
        previews = []
        presentedPreview = nil
        previewSelections.removeAll()
        previewWidgetRequestID = nil
        previewPageRequestID = nil
        isLoadingPreviewPage = false
        showsInspector = false
        currentUsage = TokenUsage()
        lastUsage = TokenUsage()
    }
}
