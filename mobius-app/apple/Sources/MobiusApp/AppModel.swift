import Foundation
import Observation

@MainActor
@Observable
final class AppModel {
    var accounts: [GatewayAccount]
    var selectedAccountID: UUID?
    var connectionState: ConnectionState = .disconnected
    var destination: AppDestination? = .chats
    var navigationPath: [AppRoute] = []
    var workspace: WorkspaceInfo?
    var gitStatus: GitStatus?
    var gitCredentialAvailable: Bool?
    var gitCredentialUsername: String?
    var gitCredentialError: String?
    var isCheckingGitCredential = false
    var sshIdentities: [SshIdentityRecord]?
    var sshIdentityError: String?
    var isLoadingSshIdentities = false
    var isGeneratingSshIdentity = false
    var generatedSshIdentity: GeneratedSshIdentity?
    private(set) var gitDiffRevision = 0
    var gitDiff = "" {
        didSet { gitDiffRevision &+= 1 }
    }
    private(set) var stagedGitDiffRevision = 0
    var stagedGitDiff = "" {
        didSet { stagedGitDiffRevision &+= 1 }
    }
    private(set) var committedGitDiffRevision = 0
    var committedGitDiff = "" {
        didSet { committedGitDiffRevision &+= 1 }
    }
    var sessions: [SessionRecord] = []
    var cloudSession: MobiusCloudSession?
    var cloudAccount: MobiusCloudAccount?
    var cloudAction: MobiusCloudAction = .idle
    var cloudError: String?
    var isUpdatingCloudDiagnostics = false
    var hasCloudAccount: Bool { cloudSession != nil }
    var isLoadingCloudAccount: Bool {
        hasCloudAccount && cloudAccount == nil && cloudError == nil
    }

    var gatewayMachineName = ""
    @ObservationIgnored let titleWriter: ChatTitleWriter
    @ObservationIgnored var chatTitleTasks: [String: Task<Void, Never>] = [:]
    @ObservationIgnored var titleEligibleSessionIDs: Set<String> = []
    var pendingChatTitles: [String: PendingChatTitle] = [:]
    var selectedSessionID: String?
    var sessionToRename: SessionRecord?
    var sessionRenameDraft = ""
    var sessionToDelete: SessionRecord?
    var unreadSessionIDs: Set<String> = []
    var transcript: [TranscriptEntry] = [] {
        didSet { updateTranscriptWindow(after: oldValue) }
    }
    var replayPresentedTranscript: [TranscriptEntry]? {
        didSet { transcriptWindowAnchor = .tail }
    }
    var transcriptWindowAnchor = TranscriptWindowAnchor.tail {
        didSet { invalidateTranscriptProjection() }
    }
    var displayedTranscript: [TranscriptEntry] {
        transcriptWindow.entries
    }
    var transcriptWindow: TranscriptWindowCache {
        let source = replayPresentedTranscript ?? transcript
        if let transcriptWindowCache { return transcriptWindowCache }
        let maximumTurns = switch transcriptWindowAnchor {
        case .tail: transcriptTurnsPerPage
        case .visibleTurns(let count): count
        }
        let window = TranscriptProjection.turnWindow(
            from: source,
            maximumTurns: maximumTurns
        )
        let cached = TranscriptWindowCache(
            entries: window.entries,
            turnCount: window.turnCount,
            hasEarlierEntries: window.hasEarlierEntries
        )
        transcriptWindowCache = cached
        return cached
    }
    /// The one visible activity label that represents the live turn's current step.
    var activeTranscriptStepID: String? {
        guard activeTurnID != nil,
              let latest = displayedTranscript.last,
              latest.pending,
              [.reasoning, .event, .error].contains(latest.kind)
        else { return nil }
        return latest.presentationID
    }
    /// The turn is running with nothing pending, so no row is shimmering and the transcript
    /// would otherwise sit still while the model decides what to do next.
    var isWaitingForModel: Bool {
        TranscriptWaitingNote.isWaiting(
            hasActiveTurn: activeTurnID != nil,
            lastEntryIsPending: displayedTranscript.last?.pending == true,
            connectionIsReady: connectionState.isReady,
            hasPendingApproval: pendingApproval != nil,
            hasPendingPicker: pendingPicker != nil
        )
    }

    var isLoadingTranscript: Bool {
        guard connectionState == .loading,
              sessionRequestID != nil || replayRequestID != nil
        else { return false }

        let opensAnotherSession = sessionOpeningID.map { $0 != selectedSessionID } ?? false
        return opensAnotherSession || (replayPresentedTranscript ?? transcript).isEmpty
    }
    var isLoadingEarlierHistory = false
    var historyLoadCompletionRevision = 0
    var hasEarlierHistory: Bool {
        transcriptWindow.hasEarlierEntries
            || nextHistoryBeforeSequence != nil
            || isLoadingEarlierHistory
    }
    var canLoadEarlierHistory: Bool {
        hasEarlierHistory
            && connectionState.isReady
            && historyRequestID == nil
    }
    var composer = "" {
        didSet { scheduleComposerDraftSave() }
    }
    @ObservationIgnored var transcriptProjectionCache:
        (key: TranscriptProjectionKey, projection: TranscriptProjection)?
    @ObservationIgnored var transcriptWindowCache: TranscriptWindowCache?
    @ObservationIgnored var transcriptProjectionVersion = 0
    @ObservationIgnored var transcriptMutationPreservesPrefix = false
    var composerFocusRequest = 0
    /// Counterpart to `composerFocusRequest`: the composer owns the focus state, so anything
    /// outside it that needs the keyboard gone asks rather than reaching in.
    private(set) var composerBlurRequest = 0
    var composerAttachments: [ComposerAttachment] = []
    var sessionFiles: [SessionFileRecord] = []
    var isLoadingSessionFiles = false
    var previewURL: URL?
    var textFilePreview: TextFilePreview?
    var sessionFileShareItem: SessionFileShareItem?
    var isLoadingFilePresentation = false
    var toast: AppToast?
    var activeTurnID: String?
    var activeOperation: String?
    var steeringDeliveryRevision = 0
    var contextTokens = 0
    var sessionCompactionCount: UInt64 = 0
    var modelContextWindow: Int64?
    var contextLimitTokens: Int64?
    var pendingApproval: PendingApproval?
    var modelChoices: [ModelChoice] = []
    var modelProviders: [String: String] = [:]
    var middlewareFeatures: [MiddlewareFeature] = []
    var extensions: [ExtensionRecord] = []
    var availableExtensions: [MobiusCloudExtensionCatalogItem] = []
    var extensionCatalogError: String?
    var isLoadingExtensionCatalog = false
    var gatewayContributions: [FrontendContribution] = []
    var extensionInstallSource = ""
    var selectedModelRoute = ""
    private(set) var contributionsRevision = 0
    var contributions: [FrontendContribution] = [] {
        didSet { contributionsRevision &+= 1 }
    }
    var mountedWidgets: [MountedWidget] = []
    var pendingPicker: FrontendPickerPrompt?
    var previews: [TranscriptPreview] = []
    var presentedPreview: TranscriptPreview?
    var isLoadingPreviewPage = false
    var showsInspector = false
    var filesInspectorTab: FilesInspectorTab = .modified
    var modifiedFilesScope: ModifiedFilesScope = .unstaged
    var lastTurnDiff: String {
        guard let final = transcript.last(where: {
            $0.turnTerminal && $0.kind == .assistant
        })?.turnID else { return "" }
        return turnDiff(for: final)
    }
    func turnDiff(for entry: TranscriptEntry) -> String {
        guard entry.kind == .assistant,
              entry.turnTerminal,
              let turnID = entry.turnID,
              transcript.last(where: {
                  $0.turnID == turnID && $0.turnTerminal && $0.kind == .assistant
              })?.id == entry.id
        else { return "" }
        return turnDiff(for: turnID)
    }
    private func turnDiff(for turnID: String) -> String {
        return transcript.lazy
            .filter {
                $0.turnID == turnID
                    && $0.role == .tool
                    && $0.format == "unified_diff"
                    && !$0.pending
            }
            .map(\.text)
            .joined(separator: "\ndiff --git a/turn-change b/turn-change\n")
    }
    var lastTurnDiffRevision: Int {
        transcript.lastIndex(where: {
            $0.turnTerminal && $0.kind == .assistant
        }) ?? -1
    }
    private(set) var workspaceFilesRevision = 0
    var workspaceFiles: [WorkspaceFileRecord] = [] {
        didSet { workspaceFilesRevision &+= 1 }
    }
    var workspaceFilesTruncated = false
    var isLoadingGitDiff = false
    var isLoadingStagedGitDiff = false
    var isLoadingCommittedGitDiff = false
    var isLoadingWorkspaceFiles = false
    var profile: ProfileSnapshot?
    var runStats = RunStats()
    var currentUsage = TokenUsage()
    var lastUsage = TokenUsage()
    var cronTasks: [CronTask] = []
    var cronRuns: [CronRun] = []
    var cronTaskDraft = ""
    var cronError: String?
    var workspaceError: String?
    var isChangingWorkspace = false
    var showsWorkspaceBrowser = false
    var directoryListing: DirectoryListing?
    var directoryError: String?
    var isLoadingDirectories = false

    var agentSnapshot: VersionedAgentConfig?
    var defaultAgentSnapshot: VersionedAgentConfig?
    var agentDraft: AgentComposition?
    var defaultAgentDraft: AgentComposition?
    var providerDraft: ProviderConfig?
    var chatAgentApplyState: ApplyState = .idle
    var defaultAgentApplyState: ApplyState = .idle
    var providerStatuses: [ProviderStatus] = []
    var providerInstances: [ProviderInstance] = []
    var providerAPIKey = ""
    var providerLabelDraft = ""
    var providerTintDraft: ProviderTint = .blue
    var providerModelIDsText = ""
    var providerReasoningEffortsText = ""
    var providerActionState: ProviderActionState = .idle
    var extensionAction: ExtensionAction?
    var pairingCodeInfo: PairingCodeInfo?

    var showsPairing = false
    var pairingEndpoint = "wss://"
    var pairingCode = ""
    var pairingError: String?
    var theme: ThemePreference
    var appLockEnabled: Bool
    var isAppLocked: Bool
    var isAppLockAuthenticating = false
    var appLockAuthenticationMethod: AppLockAuthenticationMethod
    var appLockError: String?

    @ObservationIgnored let client: GatewayClient
    @ObservationIgnored let store: GatewayStore
    @ObservationIgnored let cloudClient: MobiusCloudClient
    @ObservationIgnored let settingsDefaults: UserDefaults
    @ObservationIgnored let appLockAuthenticator: AppLockAuthenticator
    @ObservationIgnored let requestSender:
        @MainActor @Sendable (GatewayRequest) async throws -> Void
    @ObservationIgnored let connectionOpener:
        @MainActor @Sendable (GatewayEndpoint) async throws -> AsyncThrowingStream<GatewayEnvelope, Error>
    @ObservationIgnored let reconnectDelay: @Sendable (Int) -> Duration
    @ObservationIgnored var eventTask: Task<Void, Never>?
    @ObservationIgnored var reconnectTask: Task<Void, Never>?
    @ObservationIgnored var reconnectAttempt = 0
    @ObservationIgnored var automaticReconnectBlocked = false
    @ObservationIgnored var cloudPairingContinuation: CheckedContinuation<Void, Error>?
    @ObservationIgnored var deltaFlushTask: Task<Void, Never>?
    @ObservationIgnored var awaitingInitialUserTurnID: String?
    @ObservationIgnored var bufferedDeltas:
        [(
            id: String,
            delta: String,
            kind: TranscriptEntry.Kind,
            modelStepID: String,
            turnID: String?,
            sourceSequence: UInt64,
            recordedAtMs: Int64
        )] = []
    @ObservationIgnored var connectionGeneration = UUID()
    @ObservationIgnored var reconnectsOnActivation = false
    @ObservationIgnored var pendingPairingAccount: GatewayAccount?
    @ObservationIgnored var pendingDrafts: [String: PendingComposerDraft] = [:]
    var pendingWidgetEdit: PendingWidgetEdit?
    var stashedComposerDraft: String?
    var isLoadingComposerEditRecovery = false
    @ObservationIgnored var composerEditRecoveryGeneration = UUID()
    @ObservationIgnored var replayCompletionSubmissionIDs: Set<String> = []
    @ObservationIgnored var replayUserMessages: [ReplayUserMessage] = []
    @ObservationIgnored var completedComposerEditReplay = false
    @ObservationIgnored var awaitsSteeringDelivery = false
    @ObservationIgnored var composerDraftOwner: ComposerDraftOwner?
    @ObservationIgnored var composerDraftGeneration = UUID()
    @ObservationIgnored var composerDraftSaveTask: Task<Void, Never>?
    @ObservationIgnored var composerDraftIOTask: Task<Void, Never>?
    @ObservationIgnored var isLoadingComposerDraft = false
    @ObservationIgnored var suppressesComposerDraftSave = false
    @ObservationIgnored var transcriptIOTask: Task<Void, Never>?
    @ObservationIgnored var transcriptLoadGeneration = UUID()
    @ObservationIgnored var sessionRequestID: String?
    @ObservationIgnored var sessionOpeningID: String?
    @ObservationIgnored var pendingCachedTranscript: CachedTranscript?
    @ObservationIgnored var pendingPresentedTranscript: [TranscriptEntry]?
    var sessionMutationRequestID: String?
    @ObservationIgnored var pendingDeletedSessionID: String?
    @ObservationIgnored var pendingDeletedPresentedSessionID: String?
    @ObservationIgnored var sessionToRestoreID: String?
    @ObservationIgnored var configRequestID: String?
    @ObservationIgnored var defaultConfigRequestID: String?
    @ObservationIgnored var submittedDefaultAgentDraft: AgentComposition?
    @ObservationIgnored var approvalRequestID: String?
    @ObservationIgnored var directoryRequestID: String?
    @ObservationIgnored var gitDiffRequestID: String?
    @ObservationIgnored var stagedGitDiffRequestID: String?
    @ObservationIgnored var committedGitDiffRequestID: String?
    @ObservationIgnored var gitCredentialRequestID: String?
    @ObservationIgnored var isApprovingGitCredential = false
    @ObservationIgnored var sshIdentityRequestID: String?
    @ObservationIgnored var workspaceFilesRequestID: String?
    @ObservationIgnored var sessionFilesRequestID: String?
    @ObservationIgnored var sessionFileUploadRequests: [String: SessionFileUploadRequest] = [:]
    @ObservationIgnored var sessionFileData: [UUID: Data] = [:]
    @ObservationIgnored var attachmentImportReservations = 0
    @ObservationIgnored var attachmentImportGeneration = UUID()
    @ObservationIgnored var activeSessionFileUpload: ActiveSessionFileUpload?
    @ObservationIgnored var sessionFileDownload: SessionFileDownload?
    @ObservationIgnored var workspaceFilePreviewDownload: WorkspaceFilePreviewDownload?
    @ObservationIgnored var filePresentationGeneration = UUID()
    @ObservationIgnored var previewTemporaryDirectory: URL?
    var gitBranchRequestID: String?
    @ObservationIgnored var pendingProviderCredential: (
        requestID: String,
        instance: String,
        provider: String
    )?
    @ObservationIgnored var pairingCodeRequestID: String?
    @ObservationIgnored var pairingCodeExpiryTask: Task<Void, Never>?
    @ObservationIgnored var providerLoginRequestID: String?
    @ObservationIgnored var providerRegistrationRequestID: String?
    var pendingProviderRemoval: (requestID: String, instance: String)?
    @ObservationIgnored var extensionRequestID: String?
    @ObservationIgnored var cronRequestIDs: Set<String> = []
    @ObservationIgnored var toastDismissTask: Task<Void, Never>?
    @ObservationIgnored var isChatVisible = false
    @ObservationIgnored var latestSequence: UInt64?
    @ObservationIgnored var sessionOpenCursor: UInt64?
    @ObservationIgnored var replayRequestID: String?
    @ObservationIgnored var replaySnapshotSequence: UInt64?
    @ObservationIgnored var transcriptRecordBase: [TranscriptEntry] = []
    @ObservationIgnored var transcriptRecordBaseSequence: UInt64?
    @ObservationIgnored var transcriptRecords: [UInt64: RecordedEvent] = [:]
    @ObservationIgnored var historyRequestID: String?
    @ObservationIgnored var nextHistoryBeforeSequence: UInt64?
    @ObservationIgnored var previewSelections: [String: FrontendPickerOption] = [:]
    @ObservationIgnored var previewPageRequestID: String?
    @ObservationIgnored var appIsInBackground = true

    init(
        client: GatewayClient? = nil,
        store: GatewayStore? = nil,
        settingsDefaults: UserDefaults = .standard,
        appLockAuthenticator: AppLockAuthenticator? = nil,
        requestSender: (@MainActor @Sendable (GatewayRequest) async throws -> Void)? = nil,
        connectionOpener: (
            @MainActor @Sendable (GatewayEndpoint) async throws
                -> AsyncThrowingStream<GatewayEnvelope, Error>
        )? = nil,
        reconnectDelay: (@Sendable (Int) -> Duration)? = nil,
        titleWriter: ChatTitleWriter? = nil,
        cloudClient: MobiusCloudClient? = nil
    ) {
        let client = client ?? GatewayClient()
        let store = store ?? GatewayStore()
        let cloudClient = cloudClient ?? MobiusCloudClient()
        let appLockAuthenticator = appLockAuthenticator ?? AppLockAuthenticator()
        let appLockEnabled = settingsDefaults.bool(forKey: appLockEnabledKey)
        self.client = client
        self.store = store
        self.cloudClient = cloudClient
        self.cloudSession = try? cloudClient.loadSession()
        self.settingsDefaults = settingsDefaults
        self.appLockAuthenticator = appLockAuthenticator
        self.titleWriter = titleWriter ?? ChatTitleWriter()
        self.requestSender = requestSender ?? { request in
            try await client.send(request)
        }
        self.connectionOpener = connectionOpener ?? { endpoint in
            try await client.connect(to: endpoint)
        }
        self.reconnectDelay = reconnectDelay ?? { attempt in
            let seconds = min(
                8,
                0.5 * pow(2, Double(min(attempt, 4))) * Double.random(in: 0.75...1.25)
            )
            return .milliseconds(Int64(seconds * 1_000))
        }
        self.accounts = store.loadAccounts()
        self.selectedAccountID = store.selectedAccountID()
        self.theme = ThemePreference(rawValue: settingsDefaults.string(forKey: "theme") ?? "") ?? .system
        self.appLockEnabled = appLockEnabled
        self.isAppLocked = appLockEnabled
        self.appLockAuthenticationMethod = appLockAuthenticator.method
        if selectedAccountID == nil { selectedAccountID = accounts.first?.id }
        if cloudSession != nil,
           let selectedAccountID,
           let index = accounts.firstIndex(where: { $0.id == selectedAccountID }),
           isMobiusCloudSpriteEndpoint(accounts[index].endpoint) {
            if accounts[index].displayName != mobiusCloudGatewayDisplayName,
               let renamed = try? store.rename(accounts[index], to: mobiusCloudGatewayDisplayName) {
                accounts[index] = renamed
            }
            if accounts[index].machineName != mobiusCloudGatewayDisplayName {
                try? store.recordMachineName(mobiusCloudGatewayDisplayName, for: accounts[index])
                accounts[index].machineName = mobiusCloudGatewayDisplayName
            }
        }
        showsPairing = accounts.isEmpty
        #if DEBUG
        let environment = ProcessInfo.processInfo.environment
        if accounts.isEmpty,
           let endpoint = environment["MOBIUS_PAIR_ENDPOINT"],
           let code = environment["MOBIUS_PAIR_CODE"] {
            pairingEndpoint = endpoint
            pairingCode = code
        }
        switch ProcessInfo.processInfo.environment["MOBIUS_PAGE"] {
        case "gateway": destination = .gateway
        case "providers": destination = .providers
        case "agent": destination = .agent
        case "extensions": destination = .extensions
        case "cron": destination = .cron
        case "profile": destination = .profile
        default: break
        }
        #endif
    }

    deinit {
        eventTask?.cancel()
        reconnectTask?.cancel()
        deltaFlushTask?.cancel()
        composerDraftSaveTask?.cancel()
        pairingCodeExpiryTask?.cancel()
        toastDismissTask?.cancel()
        chatTitleTasks.values.forEach { $0.cancel() }
    }

    var selectedAccount: GatewayAccount? {
        accounts.first { $0.id == selectedAccountID }
    }

    var mobiusCloudGateway: GatewayAccount? {
        guard let userID = cloudSession?.userID else { return nil }
        if let exact = accounts.first(where: { $0.cloudUserID == userID }) {
            return exact
        }
        let legacy = accounts.filter {
            $0.cloudUserID == nil
                && ($0.displayName == mobiusCloudGatewayDisplayName
                    || $0.machineName == mobiusCloudGatewayDisplayName
                    || isMobiusCloudSpriteEndpoint($0.endpoint))
        }
        return legacy.count == 1 ? legacy[0] : nil
    }

    var selectedGatewayIsMobiusCloud: Bool {
        guard let cloudGatewayID = mobiusCloudGateway?.id else { return false }
        return selectedAccountID == cloudGatewayID
    }

    var presentedChatSessionID: String? {
        guard destination == .chats,
              case .chat(let route) = navigationPath.last
        else { return nil }
        return route.sessionID
    }

    var canOpenSession: Bool {
        connectionState.isReady
            && pendingDrafts.isEmpty
            && sessionRequestID == nil
            && sessionMutationRequestID == nil
            && gitBranchRequestID == nil
            && sessionFileUploadRequests.isEmpty
            && pendingWidgetEdit == nil
            && !isLoadingComposerEditRecovery
            && !isApplyingConfiguration
    }

    var canCreateSession: Bool { canOpenSession }

    var canRenameSession: Bool {
        connectionState.isReady && sessionMutationRequestID == nil
    }

    var canModifySelectedSession: Bool {
        canOpenSession && activeTurnID == nil && pendingApproval == nil
    }

    func isCapabilityEnabled(_ capability: String) -> Bool {
        guard let snapshot = agentSnapshot else { return false }
        guard let feature = middlewareFeatures.first(where: { $0.id == capability }) else {
            return snapshot.config.middleware.enabled.contains(capability)
                || contributions.contains { $0.capability == capability }
        }
        return feature.required
            || snapshot.config.middleware.enabled.contains(capability)
    }

    var isSchedulingEnabled: Bool { isCapabilityEnabled("cron") }

    var canStartCronSetup: Bool {
        canModifySelectedSession
            && selectedSessionID != nil
            && isSchedulingEnabled
    }

    var isSwitchingGitBranch: Bool { gitBranchRequestID != nil }

    var attachmentsEnabled: Bool {
        contributions.contains { $0.acceptsFileAttachments }
    }

    var selectedRouteSupportsImageInput: Bool {
        modelChoices.first(where: { $0.route == selectedModelRoute })?
            .supportsImageInput == true
    }

    var canSubmitAttachments: Bool {
        attachmentsEnabled
            && (selectedRouteSupportsImageInput || !uploadedComposerAttachments.contains {
                $0.mediaType.hasPrefix("image/")
            })
    }

    var attachmentSubmissionUnavailableMessage: String {
        attachmentsEnabled
            ? "The selected model does not accept image attachments."
            : "File attachments are not enabled for this chat."
    }

    var canImportAttachments: Bool {
        attachmentsEnabled
            && connectionState.isReady
            && selectedSessionID != nil
            && pendingWidgetEdit == nil
    }

    var canSendComposer: Bool {
        guard connectionState.isReady,
              let sessionID = selectedSessionID,
              sessionRequestID == nil,
              !isLoadingComposerDraft,
              !isLoadingComposerEditRecovery
        else { return false }
        if let pending = pendingWidgetEdit {
            guard let accountID = selectedAccountID,
                  pending.owner == ComposerDraftOwner(accountID: accountID, sessionID: sessionID),
                  pending.recovery.phase == .editing
            else { return false }
        }
        let hasText = !composer.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        let uploaded = uploadedComposerAttachments
        guard uploaded.isEmpty || canSubmitAttachments else { return false }
        return hasText || !uploaded.isEmpty
    }

    var uploadedComposerAttachments: [SessionFileReference] {
        composerAttachments.compactMap { item in
            guard case .uploaded(let attachment) = item.state else { return nil }
            return attachment
        }
    }

    var composerHasUnfinishedAttachments: Bool {
        composerAttachments.contains { item in
            switch item.state {
            case .uploaded: false
            case .queued, .uploading, .failed: true
            }
        }
    }

    var runningSessionIDs: Set<String> {
        Set(sessions.lazy.filter { $0.activity.state != .idle }.map(\.sessionId))
    }

    var attentionSessionIDs: Set<String> {
        runningSessionIDs.union(unreadSessionIDs)
    }

    var isApplyingConfiguration: Bool {
        configRequestID != nil
            || defaultConfigRequestID != nil
            || providerRegistrationRequestID != nil
            || pendingProviderRemoval != nil
            || chatAgentApplyState == .applying
            || chatAgentApplyState == .restarting
            || defaultAgentApplyState == .applying
            || defaultAgentApplyState == .restarting
    }

    var contextFillFraction: Double {
        guard let contextLimitTokens, contextLimitTokens > 0 else { return 0 }
        return min(max(Double(contextTokens) / Double(contextLimitTokens), 0), 1)
    }

    var contextFillPercent: Int {
        Int((contextFillFraction * 100).rounded())
    }

    /// Completed execution time plus the live turn, when one is running.
    func sessionElapsed(at date: Date) -> TimeInterval {
        let completed = TimeInterval(runStats.elapsedMs) / 1_000
        if let active = runStats.active {
            let live = max(
                TimeInterval(active.elapsedMs) / 1_000,
                date.timeIntervalSince1970 - TimeInterval(active.startedAtMs) / 1_000
            )
            return completed + max(0, live)
        }
        guard let session = sessions.first(where: { $0.sessionId == selectedSessionID }),
              session.activity.state != .idle
        else { return completed }
        guard let startedAt = session.activity.startedAt else { return completed }
        return completed + max(0, date.timeIntervalSince1970 - TimeInterval(startedAt))
    }

    var sessionRunCount: UInt64 { runStats.runCount + (runStats.active == nil ? 0 : 1) }
    var sessionModelCalls: UInt64 { runStats.modelCalls + (runStats.active?.modelCalls ?? 0) }
    var sessionToolCalls: UInt64 { runStats.toolCalls + (runStats.active?.toolCalls ?? 0) }
    var sessionFailedToolCalls: UInt64 {
        runStats.failedToolCalls + (runStats.active?.failedToolCalls ?? 0)
    }

    func showToast(
        _ message: String,
        tone: ToastTone = .info,
        sessionID: String? = nil
    ) {
        let toast = AppToast(message: message, tone: tone, sessionID: sessionID)
        toastDismissTask?.cancel()
        self.toast = toast
        let duration: Duration = tone == .error || tone == .warning ? .seconds(7) : .seconds(4)
        toastDismissTask = Task { [weak self] in
            try? await Task.sleep(for: duration)
            guard !Task.isCancelled, self?.toast?.id == toast.id else { return }
            self?.toast = nil
            self?.toastDismissTask = nil
        }
    }

    func dismissToast() {
        toastDismissTask?.cancel()
        toastDismissTask = nil
        toast = nil
    }

    func setChatVisible(_ visible: Bool) {
        isChatVisible = visible
        if visible, let selectedSessionID {
            unreadSessionIDs.remove(selectedSessionID)
        }
    }

    /// Asks the composer to give up the keyboard. Leaving it up while the drawer slides means
    /// the page animates against a keyboard that belongs to a screen the reader just left.
    func dismissComposerFocus() {
        composerBlurRequest &+= 1
    }

    /// The stable presentation consumed by ChatView. Wire identity and reduction stay below
    /// this boundary; text deltas keep the cached row objects, while structural changes are
    /// projected once and receive one revision.
    func transcriptProjection(
        breakBefore boundaryID: TranscriptPresentationID?,
        waitingPhrase: TranscriptWaitingPhrase? = nil
    ) -> TranscriptProjection {
        let source = displayedTranscript
        let key = TranscriptProjectionKey(
            version: transcriptProjectionVersion,
            count: source.count,
            boundaryID: boundaryID,
            firstID: source.first?.presentationID,
            lastID: source.last?.presentationID,
            waitingPhrase: waitingPhrase
        )
        if let cached = transcriptProjectionCache, cached.key == key {
            return cached.projection
        }
        let projection = TranscriptProjection(
            entries: source,
            breakBefore: boundaryID,
            waitingPhrase: waitingPhrase,
            previous: transcriptProjectionCache?.projection
        )
        transcriptProjectionCache = (key, projection)
        return projection
    }

    func invalidateTranscriptProjection() {
        transcriptProjectionVersion &+= 1
        transcriptWindowCache = nil
    }

    private func updateTranscriptWindow(after previous: [TranscriptEntry]) {
        guard transcriptMutationPreservesPrefix,
              replayPresentedTranscript == nil,
              case .visibleTurns = transcriptWindowAnchor,
              let cached = transcriptWindowCache,
              transcript.count > previous.count,
              previous.isEmpty
                || (transcript.first === previous.first
                    && transcript[previous.count - 1] === previous.last)
        else {
            invalidateTranscriptProjection()
            return
        }
        let entries = cached.entries + transcript.dropFirst(previous.count)
        let updated = TranscriptWindowCache(
            entries: entries,
            turnCount: TranscriptProjection.turnCount(in: entries),
            hasEarlierEntries: cached.hasEarlierEntries
        )
        transcriptWindowAnchor = .visibleTurns(updated.turnCount)
        transcriptWindowCache = updated
    }

    func mutateTranscriptPreservingPrefix(
        _ mutation: (inout [TranscriptEntry]) -> Void
    ) {
        let wasPreservingPrefix = transcriptMutationPreservesPrefix
        transcriptMutationPreservesPrefix = true
        defer { transcriptMutationPreservesPrefix = wasPreservingPrefix }
        mutation(&transcript)
    }

    func pinTranscriptWindowIfNeeded() {
        guard replayRequestID == nil,
              historyRequestID == nil,
              let cached = transcriptWindowCache
        else { return }
        switch transcriptWindowAnchor {
        case .visibleTurns:
            return
        case .tail:
            break
        }
        transcriptWindowAnchor = .visibleTurns(cached.turnCount)
        transcriptWindowCache = cached
    }

    /// Starts the on-device rewrite with the submitted first message. The task is stored,
    /// but deliberately not awaited, so the gateway turn and Foundation Models run together.
    func startChatTitle(
        prompt submittedPrompt: String,
        submissionID: String,
        sessionID: String
    ) {
        let prompt = submittedPrompt.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let previewTitle = ChatTitleWriter.preview(for: prompt),
              titleEligibleSessionIDs.contains(sessionID)
                  || (pendingChatTitles[sessionID] == nil && sessions.contains(where: {
                      $0.sessionId == sessionID
                          && $0.explicitTitle == nil
                          && ($0.firstUserMessage ?? "")
                              .trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                  })),
              let accountID = selectedAccountID
        else { return }
        guard sessions.first(where: { $0.sessionId == sessionID })?.explicitTitle == nil
        else {
            titleEligibleSessionIDs.remove(sessionID)
            return
        }
        let attempt = ChatTitleAttempt(
            accountID: accountID,
            sessionID: sessionID,
            submissionID: submissionID,
            prompt: prompt
        )
        pendingChatTitles[sessionID] = PendingChatTitle(
            attempt: attempt,
            previewTitle: previewTitle,
            generatedTitle: nil,
            renameRequestID: nil,
            submissionConfirmed: false
        )
        titleEligibleSessionIDs.remove(sessionID)
        let titleWriter = titleWriter
        chatTitleTasks[sessionID] = Task { [weak self] in
            let outcome = await titleWriter.title(for: prompt) { [weak self] message in
                self?.showToast(message, tone: .warning)
            }
            guard let self else { return }
            self.finishChatTitle(outcome, attempt: attempt)
        }
    }

    private func finishChatTitle(_ outcome: ChatTitleWriter.Outcome, attempt: ChatTitleAttempt) {
        guard pendingChatTitles[attempt.sessionID]?.attempt == attempt else { return }
        chatTitleTasks.removeValue(forKey: attempt.sessionID)
        guard !Task.isCancelled, selectedAccountID == attempt.accountID
        else {
            pendingChatTitles.removeValue(forKey: attempt.sessionID)
            return
        }
        switch outcome {
        case .title(let title):
            pendingChatTitles[attempt.sessionID]?.generatedTitle = title
        case .failed(let message):
            showToast(message, tone: .warning)
        case .cancelled:
            break
        }
        reconcileChatTitles()
    }

    func confirmChatTitle(submissionID: String) {
        guard let sessionID = pendingChatTitles.first(where: {
            $0.value.attempt.submissionID == submissionID
        })?.key else { return }
        confirmChatTitle(sessionID: sessionID)
    }

    func confirmChatTitle(sessionID: String) {
        guard pendingChatTitles[sessionID] != nil else { return }
        pendingChatTitles[sessionID]?.submissionConfirmed = true
        persistGeneratedChatTitles()
    }

    func reconcileChatTitles() {
        for sessionID in Array(pendingChatTitles.keys) {
            guard let pending = pendingChatTitles[sessionID] else { continue }
            guard pending.attempt.accountID == selectedAccountID else {
                cancelChatTitle(sessionID)
                continue
            }
            guard let session = sessions.first(where: { $0.sessionId == sessionID }) else {
                continue
            }
            if let durableTitle = session.explicitTitle {
                if durableTitle == pending.generatedTitle {
                    completeChatTitle(sessionID)
                } else {
                    // An explicit user or another client always wins.
                    cancelChatTitle(sessionID)
                }
                continue
            }

            let catalogPrompt = (session.firstUserMessage ?? "")
                .trimmingCharacters(in: .whitespacesAndNewlines)
            if !catalogPrompt.isEmpty {
                guard pending.attempt.prompt.hasPrefix(catalogPrompt) else {
                    cancelChatTitle(sessionID)
                    continue
                }
                if !pending.submissionConfirmed {
                    pendingChatTitles[sessionID]?.submissionConfirmed = true
                }
                if pending.generatedTitle == nil, chatTitleTasks[sessionID] == nil {
                    // The catalog now owns the same deterministic preview, so the temporary
                    // override is no longer needed.
                    completeChatTitle(sessionID)
                }
            } else if let requestID = pending.renameRequestID,
                      requestID != sessionMutationRequestID {
                // The mutation slot cleared without the generated title reaching the catalog.
                cancelChatTitle(sessionID)
            }
        }
        persistGeneratedChatTitles()
    }

    func persistGeneratedChatTitles() {
        guard connectionState.isReady,
              sessionMutationRequestID == nil,
              let accountID = selectedAccountID
        else { return }

        for sessionID in pendingChatTitles.keys.sorted() {
            guard var pending = pendingChatTitles[sessionID],
                  pending.attempt.accountID == accountID,
                  let title = pending.generatedTitle,
                  pending.submissionConfirmed,
                  pending.renameRequestID == nil
            else { continue }
            if sessions.first(where: { $0.sessionId == sessionID })?.explicitTitle != nil {
                cancelChatTitle(sessionID)
                continue
            }
            guard let requestID = requestSessionRename(
                sessionID: sessionID,
                title: title,
                generatedTitleSessionID: sessionID
            ) else { return }
            pending.renameRequestID = requestID
            pendingChatTitles[sessionID] = pending
            return
        }
    }

    func cancelChatTitle(_ sessionID: String, rearm: Bool = false) {
        chatTitleTasks.removeValue(forKey: sessionID)?.cancel()
        pendingChatTitles.removeValue(forKey: sessionID)
        if rearm { titleEligibleSessionIDs.insert(sessionID) }
    }

    func cancelChatTitle(submissionID: String, rearm: Bool) {
        guard let sessionID = pendingChatTitles.first(where: {
            $0.value.attempt.submissionID == submissionID
        })?.key else { return }
        cancelChatTitle(sessionID, rearm: rearm)
    }

    private func completeChatTitle(_ sessionID: String) {
        chatTitleTasks.removeValue(forKey: sessionID)?.cancel()
        pendingChatTitles.removeValue(forKey: sessionID)
        titleEligibleSessionIDs.remove(sessionID)
    }

    func prepareChatTitle(for sessionID: String) {
        cancelChatTitle(sessionID)
        titleEligibleSessionIDs.insert(sessionID)
    }

    var capabilityReferences: [MountedReference] {
        contributions.flatMap { contribution in
            contribution.references.map {
                MountedReference(capability: contribution.capability, reference: $0)
            }
        }
    }

    var extensionSkillReferences: [FrontendReference] {
        let selected = agentSnapshot?.config.extensions
            ?? defaultAgentSnapshot?.config.extensions
            ?? []
        var seen = Set(extensions
            .filter { selected.contains($0.id) }
            .flatMap(\.skills))
        let references = (gatewayContributions + contributions)
            .flatMap(\.references)
            .filter { $0.trigger == "$" }
        return references.filter {
            seen.insert($0.value).inserted
        }
    }

    var currentSessionTitle: String {
        selectedSessionID.map(sessionTitle) ?? SessionRecord.untitledDisplayTitle
    }

    var selectedSession: SessionRecord? {
        guard let selectedSessionID else { return nil }
        return sessions.first { $0.sessionId == selectedSessionID }
    }

    func beginRenamingSession(_ session: SessionRecord) {
        sessionRenameDraft = displayedTitle(for: session)
        sessionToRename = session
    }

    func beginDeletingSession(_ session: SessionRecord) {
        sessionToDelete = session
    }

    func displayedTitle(for session: SessionRecord) -> String {
        pendingChatTitles[session.sessionId]?.displayTitle ?? session.displayTitle
    }

    func sessionTitle(_ sessionID: String) -> String {
        if let pendingTitle = pendingChatTitles[sessionID]?.displayTitle {
            return pendingTitle
        }
        let session = sessions.first(where: { $0.sessionId == sessionID })
        return session.map { String($0.displayTitle.prefix(72)) }
            ?? SessionRecord.untitledDisplayTitle
    }

    var headerWidgets: [MountedWidget] { widgets(in: .header) }
    var transcriptTailWidgets: [MountedWidget] { widgets(in: .transcriptTail) }
    var composerHeaderWidgets: [MountedWidget] { widgets(in: .composerHeader) }
    var composerFooterWidgets: [MountedWidget] { widgets(in: .composerFooter) }
    var messageActionWidgets: [MountedWidget] {
        widgets(in: .messageActions).filter { $0.widget.action != nil }
    }
    var navigationWidgets: [MountedWidget] { widgets(in: .navigation) }
    var chatMenuWidgets: [MountedWidget] { widgets(in: .chatMenu) }

    func referenceSuggestions(in text: String, cursor: String.Index) -> ReferenceSuggestions? {
        guard text.indices.contains(cursor) || cursor == text.endIndex else { return nil }
        return Self.referenceSuggestions(
            in: text,
            cursorOffset: text.distance(from: text.startIndex, to: cursor),
            capabilityReferences: capabilityReferences,
            workspaceFiles: workspaceFiles
        )
    }

    nonisolated static func referenceSuggestions(
        in text: String,
        cursorOffset: Int,
        capabilityReferences: [MountedReference],
        workspaceFiles: [WorkspaceFileRecord]
    ) -> ReferenceSuggestions? {
        guard cursorOffset >= 0, cursorOffset <= text.count else { return nil }
        let cursor = text.index(text.startIndex, offsetBy: cursorOffset)
        let start = text[..<cursor].lastIndex(where: { $0.isWhitespace })
            .map { text.index(after: $0) } ?? text.startIndex
        guard start < cursor, let trigger = text[start..<cursor].first else { return nil }
        let end = text[cursor...].firstIndex(where: { $0.isWhitespace }) ?? text.endIndex
        let queryStart = text.index(after: start)
        let query = String(text[queryStart..<end]).lowercased()
        let capabilityMatches = capabilityReferences.filter { $0.reference.trigger == trigger }
        var matches: [MountedReference]

        if query.isEmpty {
            matches = Array(capabilityMatches.prefix(8))
            if trigger == "@", matches.count < 8 {
                matches.append(contentsOf: workspaceFiles.prefix(8 - matches.count).map {
                    Self.workspaceReference($0)
                })
            }
        } else {
            var ranked: [(score: ReferenceMatchScore, reference: MountedReference)] = []
            func consider(_ reference: MountedReference) {
                guard let score = referenceScore(reference.reference.value, query: query) else {
                    return
                }
                let index = ranked.firstIndex {
                    score < $0.score
                        || (score == $0.score
                            && reference.reference.value < $0.reference.reference.value)
                } ?? ranked.endIndex
                guard index < 8 else { return }
                ranked.insert((score, reference), at: index)
                if ranked.count > 8 { ranked.removeLast() }
            }
            capabilityMatches.forEach(consider)
            if trigger == "@" {
                workspaceFiles.lazy.map(Self.workspaceReference).forEach(consider)
            }
            matches = ranked.map { $0.reference }
        }
        guard !matches.isEmpty else { return nil }
        return ReferenceSuggestions(source: text, range: start..<end, matches: matches)
    }

    nonisolated static func workspaceReference(
        _ file: WorkspaceFileRecord
    ) -> MountedReference {
        MountedReference(
            capability: "workspace-files",
            reference: FrontendReference(trigger: "@", value: file.path, description: "file"),
            replacement: file.path.contains(where: \Character.isWhitespace)
                && !file.path.contains("\"")
                ? "\"\(file.path)\""
                : file.path
        )
    }

    nonisolated static func referenceScore(
        _ value: String,
        query: String
    ) -> ReferenceMatchScore? {
        let value = value.lowercased()
        let name = value.split(separator: "/").last.map(String.init) ?? value
        let length = value.count
        if name == query { return ReferenceMatchScore(tier: 0, gaps: 0, length: length) }
        if name.hasPrefix(query) { return ReferenceMatchScore(tier: 1, gaps: 0, length: length) }
        if value.hasPrefix(query) { return ReferenceMatchScore(tier: 2, gaps: 0, length: length) }
        if let range = name.range(of: query) {
            return ReferenceMatchScore(
                tier: 3,
                gaps: name.distance(from: name.startIndex, to: range.lowerBound),
                length: length
            )
        }
        if let range = value.range(of: query) {
            return ReferenceMatchScore(
                tier: 4,
                gaps: value.distance(from: value.startIndex, to: range.lowerBound),
                length: length
            )
        }
        if let gaps = subsequenceGaps(in: name, query: query) {
            return ReferenceMatchScore(tier: 5, gaps: gaps, length: length)
        }
        return subsequenceGaps(in: value, query: query).map {
            ReferenceMatchScore(tier: 6, gaps: $0, length: length)
        }
    }

    nonisolated static func subsequenceGaps(in value: String, query: String) -> Int? {
        var searchStart = value.startIndex
        var firstOffset: Int?
        var lastOffset = 0
        var count = 0
        for wanted in query {
            guard let index = value[searchStart...].firstIndex(of: wanted) else { return nil }
            let offset = value.distance(from: value.startIndex, to: index)
            if firstOffset == nil { firstOffset = offset }
            lastOffset = offset
            count += 1
            searchStart = value.index(after: index)
        }
        return lastOffset + 1 - (firstOffset ?? 0) - count
    }

}
extension TokenUsage {
    init?(json: JSONValue) {
        guard let inputTokens = json["inputTokens"]?.intValue,
              let cachedInputTokens = json["cachedInputTokens"]?.intValue,
              let cacheWriteInputTokens = json["cacheWriteInputTokens"]?.intValue,
              let outputTokens = json["outputTokens"]?.intValue,
              let reasoningOutputTokens = json["reasoningOutputTokens"]?.intValue,
              let totalTokens = json["totalTokens"]?.intValue
        else { return nil }
        self.inputTokens = inputTokens
        self.cachedInputTokens = cachedInputTokens
        self.cacheWriteInputTokens = cacheWriteInputTokens
        self.outputTokens = outputTokens
        self.reasoningOutputTokens = reasoningOutputTokens
        self.totalTokens = totalTokens
    }
}

extension JSONValue {
    var prettyPrinted: String {
        guard let data = try? JSONEncoder().encode(self),
              let object = try? JSONSerialization.jsonObject(with: data),
              let pretty = try? JSONSerialization.data(withJSONObject: object, options: [.prettyPrinted, .sortedKeys]),
              let text = String(data: pretty, encoding: .utf8)
        else { return "{}" }
        return text
    }
}
