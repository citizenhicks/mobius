import Foundation

extension AppModel {
    func handleGatewayDisconnected(_ message: String) {
        cancelVoiceChatIntent()
        chat.stopRealtimeVoice()
        chat.transcriptLoadGeneration = UUID()
        chat.finishHistoryLoad()
        chat.sessionFileUploadRequests.removeAll()
        chat.abandonedSessionFileUploadRequests.removeAll()
        chat.sessionFileDeleteRequests.removeAll()
        chat.activeSessionFileUpload = nil
        chat.sessionFilesRequestID = nil
        chat.isLoadingSessionFiles = false
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
        chat.discardPendingComposerAttachments()
        discardFilePresentation(preservingWorkspaceTextDraft: true)
        chat.cancelSessionFileThumbnailDownloads()
        chat.restorePendingDrafts()
        cloud.completeCloudPairing(.failure(MobiusCloudError.provisioningFailed))
        if gateway.reconnectAttempt == 0 { showToast(verbatim: message, tone: .error) }
    }

    func resetGatewayDependentState(
        preservingDrafts: Bool,
        preservingSession: Bool = false
    ) {
        cancelVoiceChatIntent()
        chat.stopRealtimeVoice()
        cloud.completeCloudPairing(.failure(CancellationError()))
        if !preservingSession { chat.changeComposerDraftOwner(to: nil) }
        if preservingSession { chat.flushStreamDeltas() }
        chat.abandonedSessionFileUploadRequests.removeAll()
        chat.sessionFileDeleteRequests.removeAll()
        chat.transcriptLoadGeneration = UUID()
        if !preservingSession {
            chat.latestSequence = nil
        }
        chat.sessionOpenCursor = nil
        chat.replayRequestID = nil
        chat.replaySnapshotSequence = nil
        chat.finishHistoryLoad()
        if !preservingSession {
            chat.nextHistoryBeforeSequence = nil
            chat.transcriptWindowAnchor = .tail
            chat.awaitingInitialMessageTurnID = nil
            chat.replayPresentedTranscript = nil
        }
        if preservingDrafts {
            chat.discardPendingComposerAttachments()
        } else {
            chat.pendingDrafts.removeAll()
            chat.composer = ""
            chat.discardComposerAttachments()
        }
        dismissToast()
        chat.sessionRequestID = nil
        chat.sessionOpeningID = nil
        chat.pendingCachedTranscript = nil
        chat.pendingPresentedTranscript = nil
        chat.botSessionsRequestID = nil
        chat.pendingBotSessionResume = nil
        chat.isLoadingBotSessions = false
        chat.sessionMutationRequestID = nil
        swarmMutationRequestID = nil
        swarmMessageRequestID = nil
        completedSwarmMessageRequestID = nil
        botMutationRequestID = nil
        botMutationSuccessMessage = nil
        chat.pendingDeletedSessionIDs = []
        chat.pendingDeletedPresentedSessionID = nil
        for sessionID in Array(chat.pendingChatTitles.keys) {
            chat.pendingChatTitles[sessionID]?.renameRequestID = nil
        }
        chat.sessionToRestoreID = nil
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
            chat.sessionFilesRequestID = nil
            chat.isLoadingSessionFiles = false
            chat.sessionFileUploadRequests.removeAll()
            chat.activeSessionFileUpload = nil
            discardFilePresentation(preservingWorkspaceTextDraft: true)
            chat.cancelSessionFileThumbnailDownloads()
        }
        if !preservingSession {
            chat.selectedSessionID = nil
        }
        // A transport replacement must not replace the user's navigation or setup drafts.
        if !preservingDrafts {
            chat.sessionFileLimits = nil
            chat.chatTitleTasks.values.forEach { $0.cancel() }
            chat.chatTitleTasks.removeAll()
            chat.titleEligibleSessionIDs.removeAll()
            chat.pendingChatTitles.removeAll()
            chat.botSessions = []
            chat.botSessionsBotID = nil
            chat.pendingNewChatWorkspace = nil
            chat.pendingNewChatBotID = nil
            showsWorkspaceBrowser = false
            directoryListing = nil
            directoryError = nil
            routineRunPreviewPollingTask?.cancel()
            routineRunPreviewPollingTask = nil
            chat.sessions = []
            backgroundApprovals = []
            swarmAttentions = []
            chat.chatBotFilterIDs.removeAll()
            bots = []
            swarms = []
            navigationPath = []
            chat.sessionToRename = nil
            chat.sessionRenameDraft = ""
            chat.sessionToDelete = nil
            chat.unreadSessionIDs.removeAll()
            profile = nil
            modelChoices = []
            modelProviders = [:]
            middlewareFeatures = []
            extensions = []
            gatewayContributions = []
            swarmContributions = [:]
            providerStatuses = []
            providerInstances = []
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
            chat.discardFileThumbnails()
            resetSessionState()
        }
        if preservingDrafts { chat.restorePendingDrafts() }
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

    func resetRootSessionState() {
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
        agentSnapshot = nil
        agentDraft = nil
        showsInspector = false
        discardFilePresentation()
    }

    func resetSessionState(preservingComposerAttachments: Bool = false) {
        resetRootSessionState()
        chat.resetSessionState(preservingComposerAttachments: preservingComposerAttachments)
    }
}
