import Foundation

extension AppModel {
    func handleGatewayDisconnected(_ message: String) {
        cancelVoiceChatIntent()
        stopRealtimeVoice()
        transcriptLoadGeneration = UUID()
        finishHistoryLoad()
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
        gitBranchRequestID = nil
        routineRunPreviewRequestID = nil
        routineRunPreviewRequestBeforeSequence = nil
        isLoadingRoutineRunPreview = false
        isLoadingWorkspaceFiles = false
        isSavingWorkspaceFile = false
        discardPendingComposerAttachments()
        discardFilePresentation(preservingWorkspaceTextDraft: true)
        cancelSessionFileThumbnailDownloads()
        restorePendingDrafts()
        if cloudPairingContinuation != nil {
            completeCloudPairing(.failure(MobiusCloudError.provisioningFailed))
        }
        if gateway.reconnectAttempt == 0 { showToast(verbatim: message, tone: .error) }
    }

    func resetGatewayDependentState(
        preservingDrafts: Bool,
        preservingSession: Bool = false
    ) {
        cancelVoiceChatIntent()
        stopRealtimeVoice()
        if cloudPairingContinuation != nil {
            completeCloudPairing(.failure(CancellationError()))
        }
        if !preservingSession { changeComposerDraftOwner(to: nil) }
        if preservingSession { flushStreamDeltas() }
        abandonedSessionFileUploadRequests.removeAll()
        sessionFileDeleteRequests.removeAll()
        transcriptLoadGeneration = UUID()
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
            replayPresentedTranscript = nil
        }
        if preservingDrafts {
            discardPendingComposerAttachments()
        } else {
            pendingDrafts.removeAll()
            composer = ""
            discardComposerAttachments()
        }
        dismissToast()
        sessionRequestID = nil
        sessionOpeningID = nil
        pendingCachedTranscript = nil
        pendingPresentedTranscript = nil
        botSessionsRequestID = nil
        pendingBotSessionResume = nil
        isLoadingBotSessions = false
        sessionMutationRequestID = nil
        swarmMutationRequestID = nil
        swarmMessageRequestID = nil
        completedSwarmMessageRequestID = nil
        botMutationRequestID = nil
        botMutationSuccessMessage = nil
        pendingDeletedSessionIDs = []
        pendingDeletedPresentedSessionID = nil
        for sessionID in Array(pendingChatTitles.keys) {
            pendingChatTitles[sessionID]?.renameRequestID = nil
        }
        sessionToRestoreID = nil
        botDefaultsRequestID = nil
        submittedBotDefaultsDraft = nil
        botApplyState = .idle
        botDefaultsApplyState = .idle
        workspaceError = nil
        isChangingWorkspace = false
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
            selectedSessionID = nil
        }
        // A transport replacement must not replace the user's navigation or setup drafts.
        if !preservingDrafts {
            chatTitleTasks.values.forEach { $0.cancel() }
            chatTitleTasks.removeAll()
            titleEligibleSessionIDs.removeAll()
            pendingChatTitles.removeAll()
            botSessions = []
            botSessionsBotID = nil
            pendingNewChatWorkspace = nil
            pendingNewChatBotID = nil
            showsWorkspaceBrowser = false
            directoryListing = nil
            directoryError = nil
            routineRunPreviewPollingTask?.cancel()
            routineRunPreviewPollingTask = nil
            sessions = []
            backgroundApprovals = []
            swarmAttentions = []
            chatBotFilterIDs.removeAll()
            bots = []
            swarms = []
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
            routines = []
            routineRuns = []
            routineError = nil
            presentedRoutineRun = nil
            routineRunPreview = nil
            routineRunPreviewEntries = []
            routineRunPreviewNextBeforeSequence = nil
            routineRunPreviewError = nil
            providerDraft = nil
            providerLabelDraft = ""
            providerTintDraft = .appDefault
            providerAPIKey = ""
            providerModelIDsText = ""
            providerReasoningEffortsText = ""
            providerActionState = .idle
            pendingProviderLogin = nil
            gitCredentialAvailable = nil
            gitCredentialUsername = nil
            gitCredentialError = nil
            sshIdentities = nil
            sshIdentityError = nil
            generatedSshIdentity = nil
            pairingCodeExpiryTask?.cancel()
            pairingCodeExpiryTask = nil
            pairingCodeInfo = nil
        } else if pendingProviderCredential != nil {
            // A write may have reached the gateway without its response reaching us.
            // Keep the key available for an explicit retry; never resend it automatically.
            providerActionState = .failed(localizedString(
                "The gateway disconnected. Send the key again to confirm it was saved."
            ))
        }
        pendingProviderCredential = nil
        providerRegistrationRequestID = nil
        pendingProviderRemoval = nil
        cancelExtensionAndCredentialRequests()
        pairingCodeRequestID = nil
        gitBranchRequestID = nil
        workspaceFileWriteRequestID = nil
        isSavingWorkspaceFile = false
        routineRequestIDs.removeAll()
        routineRunPreviewRequestID = nil
        routineRunPreviewRequestBeforeSequence = nil
        isLoadingRoutineRunPreview = false
        if !preservingSession {
            discardFileThumbnails()
            resetSessionState()
        }
        if preservingDrafts { restorePendingDrafts() }
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
