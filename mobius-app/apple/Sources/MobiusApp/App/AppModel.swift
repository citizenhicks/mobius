import Foundation
import CoreGraphics
import Observation

@MainActor
@Observable
final class AppModel {
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
    var gitDiffs: [GitDiffScope: GitDiffState] = [:]
    var bots: [BotRecord] = []
    var backgroundApprovals: [BackgroundApproval] = []
    var swarmAttentions: [SwarmAttention] = []
    var swarms: [SwarmRecord] = []
    var swarmMessageRequestID: String?
    var completedSwarmMessageRequestID: String?
    var showsCloudOffer = false

    var modelChoices: [ModelChoice] = []
    var modelProviders: [String: String] = [:]
    var middlewareFeatures: [MiddlewareFeature] = []
    var extensions: [ExtensionRecord] = []
    var gatewayContributions: [FrontendContribution] = []
    var swarmContributions: [String: [FrontendContribution]] = [:]
    var extensionInstallSource = ""
    let dictation = ComposerDictation()
    @ObservationIgnored let messageSpeaker = MessageSpeaker()
    var newVoiceChatIntent: NewVoiceChatIntent?
    var previewURL: URL?
    var textFilePreview: TextFilePreview?
    var sessionFileShareItem: SessionFileShareItem?
    var isLoadingFilePresentation = false
    var isSavingWorkspaceFile = false
    var returnsToFilesAfterFilePresentation = false
    var toast: AppToast?
    var showsAppUpdateAlert = false
    var showsInspector = false
    var filesInspectorTab: FilesInspectorTab = .modified
    var modifiedFilesScope: ModifiedFilesScope = .unstaged
    var lastTurnDiff: String {
        guard
            let final = chat.transcript.last(where: {
                $0.turnTerminal && $0.kind == .assistant
            })?.turnID
        else { return "" }
        return transcriptTurnDiff(forTurn: final, in: chat.transcript)
    }
    var lastTurnDiffRevision: Int {
        chat.transcript.lastIndex(where: {
            $0.turnTerminal && $0.kind == .assistant
        }) ?? -1
    }
    private(set) var workspaceFilesRevision = 0
    var workspaceFiles: [WorkspaceFileRecord] = [] {
        didSet { workspaceFilesRevision &+= 1 }
    }
    var workspaceFilesTruncated = false
    var isLoadingWorkspaceFiles = false
    var profile: ProfileSnapshot?
    var profileRequestID: String?
    var routines: [Routine] = []
    var routineRuns: [RoutineRun] = []
    var routineError: String?
    var presentedRoutineRun: RoutineRun?
    var routineRunPreview: RoutineRunPreview?
    var routineRunPreviewEntries: [TranscriptEntry] = []
    var routineRunPreviewNextBeforeSequence: UInt64?
    var isLoadingRoutineRunPreview = false
    var routineRunPreviewError: String?
    var workspaceError: String?
    var isChangingWorkspace = false
    var showsWorkspaceBrowser = false {
        didSet {
            if !showsWorkspaceBrowser, newVoiceChatIntent == .selectingWorkspace {
                cancelVoiceChatIntent()
            }
        }
    }
    var directoryListing: DirectoryListing?
    var directoryError: String?
    var isLoadingDirectories = false

    var agentSnapshot: VersionedAgentConfig?
    var botDefaultsSnapshot: VersionedAgentConfig?
    var agentDraft: AgentComposition?
    var botDefaultsDraft: AgentComposition?
    var editingBotID: String?
    var editingBotRevision: UInt64?
    var botDraft: AgentComposition?
    var botNameDraft = ""
    var botDescriptionDraft = ""
    var botTintDraft: AccentTint = .appDefault
    var botApplyState: ApplyState = .idle
    var providerDraft: ProviderConfig?
    var botDefaultsApplyState: ApplyState = .idle
    var providerStatuses: [ProviderStatus] = []
    var providerInstances: [ProviderInstance] = []
    var providerAPIKey = ""
    var providerLabelDraft = ""
    var providerTintDraft: AccentTint = .appDefault
    var providerModelIDsText = ""
    var providerReasoningEffortsText = ""
    var providerActionState: ProviderActionState = .idle
    var extensionAction: ExtensionAction?
    var pairingCodeInfo: PairingCodeInfo?

    var showsPairing = false
    var theme: ThemePreference
    var language: AppLanguage {
        didSet {
            gateway.locale = language.locale
            chat.locale = language.locale
        }
    }
    var accentTint: AccentTint
    var appLockEnabled: Bool
    var isAppLocked: Bool
    var isAppLockAuthenticating = false
    var isClearingLocalData = false
    var appLockAuthenticationMethod: AppLockAuthenticationMethod
    var appLockError: String?

    @ObservationIgnored let gateway: GatewayConnectionModel
    @ObservationIgnored let store: GatewayStore
    @ObservationIgnored let chat: ChatSessionModel
    @ObservationIgnored let cloud: MobiusCloudModel
    @ObservationIgnored let settingsDefaults: UserDefaults
    @ObservationIgnored let appLockAuthenticator: AppLockAuthenticator
    @ObservationIgnored var appLockAuthenticationGeneration = UUID()
    @ObservationIgnored var deltaFlushTask: Task<Void, Never>?
    @ObservationIgnored var awaitingInitialMessageTurnID: String?
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
    @ObservationIgnored var startupTask: Task<Void, Never>?
    @ObservationIgnored var startupTaskID: UUID?
    @ObservationIgnored var startedAccountID: UUID?
    @ObservationIgnored var appActivationTask: Task<Void, Never>?
    var swarmMutationRequestID: String?
    var botMutationRequestID: String?
    var botMutationSuccessMessage: String?
    @ObservationIgnored var botDefaultsRequestID: String?
    @ObservationIgnored var submittedBotDefaultsDraft: AgentComposition?
    @ObservationIgnored var directoryRequestID: String?
    @ObservationIgnored var gitCredentialRequestID: String?
    @ObservationIgnored var isApprovingGitCredential = false
    @ObservationIgnored var sshIdentityRequestID: String?
    @ObservationIgnored var workspaceFilesRequestID: String?
    @ObservationIgnored var workspaceFileWriteRequestID: String?
    @ObservationIgnored var workspaceFilePreviewDownload: WorkspaceFilePreviewDownload?
    @ObservationIgnored var filePresentationGeneration = UUID()
    @ObservationIgnored var previewTemporaryDirectory: URL?
    var gitBranchRequestID: String?
    @ObservationIgnored var pendingProviderCredential:
        (
            requestID: String,
            instance: String,
            provider: String,
            credentialHint: String?
        )?
    @ObservationIgnored var pairingCodeRequestID: String?
    @ObservationIgnored var pairingCodeExpiryTask: Task<Void, Never>?
    var pendingProviderLogin: (requestID: String, provider: String)?
    @ObservationIgnored var providerRegistrationRequestID: String?
    var pendingProviderRemoval: (requestID: String, instance: String)?
    @ObservationIgnored var extensionRequestID: String?
    @ObservationIgnored var routineRequestIDs: Set<String> = []
    @ObservationIgnored var routineRunPreviewRequestID: String?
    @ObservationIgnored var routineRunPreviewRequestBeforeSequence: UInt64?
    @ObservationIgnored var routineRunPreviewPollingTask: Task<Void, Never>?
    @ObservationIgnored var toastDismissTask: Task<Void, Never>?
    @ObservationIgnored var appIsInBackground = true

    init(
        client: GatewayClient? = nil,
        store: GatewayStore? = nil,
        settingsDefaults: UserDefaults = .standard,
        appLockAuthenticator: AppLockAuthenticator? = nil,
        remoteNotifications: RemoteNotificationSystem? = nil,
        requestSender: (@MainActor @Sendable (GatewayRequest) async throws -> Void)? = nil,
        connectionOpener: (
            @MainActor @Sendable (GatewayEndpoint) async throws
                -> AsyncThrowingStream<GatewayEnvelope, Error>
        )? = nil,
        reconnectDelay: (@Sendable (Int) -> Duration)? = nil,
        titleWriter: ChatTitleWriter? = nil,
        cloudClient: MobiusCloudClient? = nil,
        cloudPurchases: MobiusCloudPurchases? = nil
    ) {
        let client = client ?? GatewayClient()
        let store = store ?? GatewayStore()
        let cloudClient = cloudClient ?? MobiusCloudClient()
        let appLockAuthenticator = appLockAuthenticator ?? AppLockAuthenticator()
        let appLockEnabled = settingsDefaults.bool(forKey: appLockEnabledKey)
        let language =
            AppLanguage(
                rawValue: settingsDefaults.string(forKey: "language") ?? ""
            ) ?? .system
        self.gateway = GatewayConnectionModel(
            client: client,
            store: store,
            requestSender: requestSender,
            connectionOpener: connectionOpener,
            reconnectDelay: reconnectDelay
        )
        self.store = store
        self.settingsDefaults = settingsDefaults
        self.appLockAuthenticator = appLockAuthenticator
        self.cloud = MobiusCloudModel(
            gateway: gateway,
            settingsDefaults: settingsDefaults,
            remoteNotifications: remoteNotifications ?? .live(),
            cloudClient: cloudClient,
            cloudPurchases: cloudPurchases ?? .live()
        )
        self.theme =
            ThemePreference(rawValue: settingsDefaults.string(forKey: "theme") ?? "") ?? .system
        self.language = language
        let titleWriter = titleWriter ?? ChatTitleWriter()
        self.chat = ChatSessionModel(
            gateway: gateway,
            store: store,
            titleWriter: titleWriter,
            dictation: dictation,
            messageSpeaker: messageSpeaker,
            locale: language.locale
        )
        self.accentTint =
            AccentTint(
                rawValue: settingsDefaults.string(forKey: "accent-tint") ?? ""
            ) ?? .appDefault
        self.appLockEnabled = appLockEnabled
        self.isAppLocked = appLockEnabled
        self.appLockAuthenticationMethod = appLockAuthenticator.method
        gateway.locale = language.locale
        restoreSessionReadState(for: gateway.selectedAccountID)
        showsPairing = gateway.accounts.isEmpty
        #if DEBUG
            let environment = ProcessInfo.processInfo.environment
            if gateway.accounts.isEmpty,
                let endpoint = environment["MOBIUS_PAIR_ENDPOINT"],
                let code = environment["MOBIUS_PAIR_CODE"]
            {
                gateway.pairingEndpoint = endpoint
                gateway.pairingCode = code
            }
            switch ProcessInfo.processInfo.environment["MOBIUS_PAGE"] {
            case "gateway": destination = .gateway
            case "providers": destination = .providers
            case "bot-defaults": destination = .botDefaults
            case "extensions": destination = .extensions
            case "bots": destination = .bots
            case "profile": destination = .profile
            default: break
            }
        #endif
        gateway.onConnectionReplacement = { [weak self] account, preserving in
            guard let self else { return }
            let sessionID = preserving ? self.presentedChatSessionID : nil
            self.resetGatewayDependentState(
                preservingDrafts: preserving,
                preservingSession: sessionID != nil
            )
            self.chat.sessionToRestoreID = sessionID
            self.restoreSessionReadState(for: account.id)
        }
        gateway.onEnvelope = { [weak self] envelope in
            self?.reduceGatewayEnvelope(envelope)
        }
        gateway.onDisconnected = { [weak self] message in
            self?.handleGatewayDisconnected(message)
        }
        gateway.onUpdateRequired = { [weak self] in
            self?.showsAppUpdateAlert = true
        }
        gateway.onPairingRepairRequired = { [weak self] in
            self?.showsPairing = true
        }
        chat.onToast = { [weak self] message, tone in
            self?.showToast(verbatim: message, tone: tone)
        }
        chat.onWorkspaceRefresh = { [weak self] in
            self?.refreshWorkspaceChanges()
        }
        chat.onSessionFilesRefresh = { [weak self] in
            guard let self, self.filesInspectorTab == .chatFiles else { return }
            self.chat.refreshSessionFiles()
        }
        chat.onDiscardFilePresentation = { [weak self] in
            self?.resetRootSessionState()
        }
        chat.onOpenChat = { [weak self] sessionID in
            self?.openChat(sessionID)
        }
        cloud.callbacks = MobiusCloudModelCallbacks(
            resetGatewayDependentState: { [weak self] preservingDrafts, preservingSession in
                self?.resetGatewayDependentState(
                    preservingDrafts: preservingDrafts,
                    preservingSession: preservingSession
                )
            },
            reconnectRecoveredGateway: { [weak self] in
                guard let self,
                    !self.isClearingLocalData,
                    self.selectedGatewayIsMobiusCloud,
                    !self.gateway.connectionState.isReady
                else { return }
                if self.appIsInBackground {
                    self.gateway.setSceneActive(false)
                } else {
                    self.reconnect()
                }
            },
            removeGateway: { [weak self] account in
                guard let self else { return false }
                return await self.removeGateway(account)
            },
            cloudPairingStarted: { [weak self] in
                self?.showsPairing = false
            },
            showToast: { [weak self] message, tone in
                self?.showToast(verbatim: message, tone: tone)
            },
            presentRemoteNotification: { [weak self] notification, agentName, detail in
                self?.receivedForegroundRemoteNotification(
                    notification,
                    agentName: agentName,
                    detail: detail
                )
            },
            openRemoteNotification: { [weak self] notification in
                self?.openPendingRemoteNotification()
            }
        )
        cloud.observeCloudPurchaseUpdates()
    }

    isolated deinit {
        gateway.shutdown()
        startupTask?.cancel()
        appActivationTask?.cancel()
        chat.deltaFlushTask?.cancel()
        chat.composerDraftSaveTask?.cancel()
        pairingCodeExpiryTask?.cancel()
        toastDismissTask?.cancel()
        chat.chatTitleTasks.values.forEach { $0.cancel() }
    }

    var selectedGatewayIsMobiusCloud: Bool {
        guard let cloudGatewayID = cloud.cloudGateway?.id else { return false }
        return gateway.selectedAccountID == cloudGatewayID
    }

    var presentedChatSessionID: String? {
        guard case .chat(let route) = navigationPath.last
        else { return nil }
        return route.sessionID
    }

    var isPresentingChat: Bool {
        navigationPath.last.map {
            if case .chat = $0 { true } else { false }
        } == true
    }

    private var canChangeSession: Bool {
        chat.pendingDrafts.isEmpty
            && chat.sessionRequestID == nil
            && chat.sessionMutationRequestID == nil
            && gitBranchRequestID == nil
            && chat.sessionFileUploadRequests.isEmpty
            && chat.abandonedSessionFileUploadRequests.isEmpty
            && chat.sessionFileDeleteRequests.isEmpty
            && chat.pendingWidgetEdit == nil
            && !chat.isLoadingComposerEditRecovery
            && !isApplyingConfiguration
    }

    var canBrowseSessions: Bool {
        (gateway.connectionState.isReady || gateway.selectedAccountID != nil)
            && canChangeSession && chat.composerAttachments.isEmpty
    }

    var canOpenSession: Bool {
        gateway.connectionState.isReady && canBrowseSessions
    }

    var canCreateSession: Bool {
        gateway.connectionState.isReady && canChangeSession
            && (chat.composerAttachments.isEmpty
                || (chat.selectedSessionID == nil
                    && chat.pendingNewChatWorkspace != nil
                    && chat.composerAttachments.allSatisfy {
                        if case .queued = $0.state { true } else { false }
                    }))
    }

    var canRenameSession: Bool {
        gateway.connectionState.isReady && chat.sessionMutationRequestID == nil
    }

    var canModifySelectedSession: Bool {
        canOpenSession
            && !selectedSessionIsHidden
            && chat.activeTurnID == nil
            && chat.pendingApproval == nil
    }

    var canBeginReply: Bool {
        chat.canBeginReply && !selectedSessionIsHidden
    }

    func isCapabilityEnabled(_ capability: String) -> Bool {
        guard let snapshot = agentSnapshot else { return false }
        guard let feature = middlewareFeatures.first(where: { $0.id == capability }) else {
            return snapshot.config.middleware.enabled.contains(capability)
                || chat.contributions.contains { $0.capability == capability }
        }
        return feature.required
            || snapshot.config.middleware.enabled.contains(capability)
    }

    var isSwitchingGitBranch: Bool { gitBranchRequestID != nil }

    var attachmentsEnabled: Bool {
        if chat.selectedSessionID == nil {
            return selectedBot?.config.config.middleware.enabled.contains("attachments") == true
        }
        return chat.contributions.contains { $0.acceptsFileAttachments }
    }

    var selectedRouteSupportsImageInput: Bool {
        let route =
            chat.selectedSessionID == nil
            ? modelRoute(for: selectedBot?.config.config)
            : chat.selectedModelRoute
        return modelChoices.first(where: { $0.route == route })?
            .supportsImageInput == true
    }

    var canSubmitAttachments: Bool {
        attachmentsEnabled
            && (selectedRouteSupportsImageInput
                || !chat.composerAttachments.contains {
                    $0.mediaType.hasPrefix("image/")
                })
    }

    var attachmentSubmissionUnavailableMessage: LocalizedStringResource {
        attachmentsEnabled
            ? "The selected model does not accept image attachments."
            : "File attachments are not enabled for this chat."
    }

    var canImportAttachments: Bool {
        attachmentsEnabled
            && gateway.connectionState.isReady
            && (chat.selectedSessionID != nil
                || chat.pendingNewChatWorkspace != nil && selectedBot != nil)
            && chat.sessionFileLimits != nil
            && chat.pendingWidgetEdit == nil
    }

    var attachmentReferenceLimit: Int {
        min(
            chat.sessionFileLimits?.maxAttachmentReferences ?? 0,
            maximumWireSessionFileReferences
        )
    }

    var attachmentFileByteLimit: Int {
        Int(
            min(
                chat.sessionFileLimits?.maxFileBytes ?? 0,
                UInt64(maximumClientAttachmentBytes)
            ))
    }

    var attachmentDraftByteLimit: Int64 {
        Int64(
            min(
                chat.sessionFileLimits?.maxSessionBytes ?? 0,
                UInt64(maximumClientComposerAttachmentBytes)
            ))
    }

    var uploadChunkByteLimit: Int {
        min(
            chat.sessionFileLimits?.maxUploadChunkBytes ?? 0,
            maximumClientUploadChunkBytes
        )
    }

    var canSendComposer: Bool {
        guard gateway.connectionState.isReady,
            chat.sessionRequestID == nil,
            !chat.isLoadingComposerDraft,
            !chat.isLoadingComposerEditRecovery
        else { return false }
        let sessionID = chat.selectedSessionID
        let hasPendingSession =
            sessionID == nil
            && chat.pendingNewChatWorkspace != nil
            && chat.pendingNewChatBotID.map { botID in bots.contains { $0.id == botID } } == true
        guard sessionID != nil || hasPendingSession else { return false }
        guard sessionID == nil || chat.pendingNewChatBotID == nil else { return false }
        guard
            chat.composerAttachments.allSatisfy({ attachment in
                switch attachment.state {
                case .uploaded: true
                case .queued: sessionID == nil
                case .preparing, .uploading, .failed: false
                }
            })
        else { return false }
        if let pending = chat.pendingWidgetEdit {
            guard let sessionID,
                let accountID = gateway.selectedAccountID,
                pending.owner == ComposerDraftOwner(accountID: accountID, sessionID: sessionID),
                pending.recovery.phase == .editing
            else { return false }
        }
        let hasText = !chat.composer.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        guard chat.composerAttachments.isEmpty || canSubmitAttachments else { return false }
        return hasText || !chat.composerAttachments.isEmpty
    }

    var uploadedComposerAttachments: [SessionFileReference] {
        chat.composerAttachments.compactMap { item in
            guard case .uploaded(let attachment) = item.state else { return nil }
            return attachment
        }
    }

    var composerHasUnfinishedAttachments: Bool {
        chat.composerAttachments.contains { item in
            switch item.state {
            case .uploaded: false
            case .preparing, .queued, .uploading, .failed: true
            }
        }
    }

    var runningSessionIDs: Set<String> {
        Set(chat.sessions.lazy.filter { $0.activity.state != .idle }.map(\.sessionId))
    }

    var attentionSessionIDs: Set<String> {
        runningSessionIDs.union(chat.unreadSessionIDs)
    }

    var canMutateSwarm: Bool {
        gateway.connectionState.isReady && swarmMutationRequestID == nil
    }

    var canPostSwarmMessage: Bool {
        gateway.connectionState.isReady && swarmMessageRequestID == nil
    }

    var canMutateBots: Bool {
        gateway.connectionState.isReady && botMutationRequestID == nil
    }

    func canMutateBot(_ botID: String) -> Bool {
        guard canMutateBots else { return false }
        if selectedSession?.sessionContext.botId == botID, chat.activeTurnID != nil {
            return false
        }
        return !chat.sessions.contains {
            $0.sessionContext.botId == botID && $0.activity.state != .idle
        }
    }

    var canMutateSelectedBot: Bool {
        guard let selectedBot else { return false }
        return canMutateBot(selectedBot.id)
    }

    var isApplyingConfiguration: Bool {
        botDefaultsRequestID != nil
            || botMutationRequestID != nil
            || providerRegistrationRequestID != nil
            || pendingProviderRemoval != nil
            || botApplyState == .applying
            || botDefaultsApplyState == .applying
            || botDefaultsApplyState == .restarting
    }

    var contextFillFraction: Double {
        guard let contextLimitTokens = chat.contextLimitTokens, contextLimitTokens > 0 else {
            return 0
        }
        return min(max(Double(chat.contextTokens) / Double(contextLimitTokens), 0), 1)
    }

    var contextFillPercent: Int {
        Int((contextFillFraction * 100).rounded())
    }

    /// Completed execution time plus the live turn, when one is running.
    func sessionElapsed(at date: Date) -> TimeInterval {
        let completed = TimeInterval(chat.runStats.elapsedMs) / 1_000
        if let active = chat.runStats.active {
            let live = max(
                TimeInterval(active.elapsedMs) / 1_000,
                date.timeIntervalSince1970 - TimeInterval(active.startedAtMs) / 1_000
            )
            return completed + max(0, live)
        }
        guard let session = chat.sessions.first(where: { $0.sessionId == chat.selectedSessionID }),
            session.activity.state != .idle
        else { return completed }
        guard let startedAt = session.activity.startedAt else { return completed }
        return completed + max(0, date.timeIntervalSince1970 - TimeInterval(startedAt))
    }

    var sessionRunCount: UInt64 { chat.runStats.runCount + (chat.runStats.active == nil ? 0 : 1) }
    var sessionModelCalls: UInt64 {
        chat.runStats.modelCalls + (chat.runStats.active?.modelCalls ?? 0)
    }
    var sessionToolCalls: UInt64 {
        chat.runStats.toolCalls + (chat.runStats.active?.toolCalls ?? 0)
    }
    var sessionFailedToolCalls: UInt64 {
        chat.runStats.failedToolCalls + (chat.runStats.active?.failedToolCalls ?? 0)
    }

    func showToast(
        _ message: LocalizedStringResource,
        tone: ToastTone = .info,
        target: AppNotificationTarget? = nil
    ) {
        showToast(verbatim: localizedString(message), tone: tone, target: target)
    }

    func localizedString(_ resource: LocalizedStringResource) -> String {
        var resource = resource
        resource.locale = language.locale
        return String(localized: resource)
    }

    func localizedErrorDescription(_ error: Error) -> String {
        switch error {
        case let error as AttachmentImportError:
            localizedString(error.localizedDescriptionResource)
        case let error as ComposerDictationError:
            localizedString(error.localizedDescriptionResource)
        case let error as GatewayWireError:
            localizedString(error.localizedDescriptionResource)
        case let error as GatewayStore.StoreError:
            error.localizedDescriptionResource.map(localizedString) ?? error.localizedDescription
        case let error as MobiusCloudError:
            localizedString(error.localizedDescriptionResource)
        case let error as MobiusCloudPurchaseError:
            error.localizedDescriptionResource.map(localizedString) ?? error.localizedDescription
        default:
            error.localizedDescription
        }
    }

    func showToast(
        verbatim message: String,
        tone: ToastTone = .info,
        target: AppNotificationTarget? = nil
    ) {
        let toast = AppToast(message: message, tone: tone, target: target)
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

    func accessibilityMessage(for toast: AppToast) -> String {
        guard let bot = bot(for: toast.target),
            !toast.message.hasPrefix("\(bot.name):")
        else { return toast.message }
        return "\(bot.name): \(toast.message)"
    }

    func dismissToast() {
        toastDismissTask?.cancel()
        toastDismissTask = nil
        toast = nil
    }

    var isChatVisible: Bool { !chat.visibleChatWindowTokens.isEmpty }

    func setChatVisible(_ visible: Bool, windowToken: UUID) {
        if visible {
            chat.visibleChatWindowTokens.insert(windowToken)
        } else {
            chat.visibleChatWindowTokens.remove(windowToken)
        }
        if visible, let selectedSessionID = chat.selectedSessionID {
            markSessionRead(selectedSessionID)
        }
    }

    func restoreSessionReadState(for accountID: UUID?) {
        guard let accountID else {
            chat.sessionReadCursors = nil
            chat.unreadSessionIDs.removeAll()
            return
        }
        chat.sessionReadCursors = store.loadSessionReadCursors(accountID: accountID)
        chat.unreadSessionIDs.removeAll()
    }

    func markSessionRead(_ sessionID: String) {
        chat.unreadSessionIDs.remove(sessionID)
        saveSessionReadCursor(sessionID, unread: false)
    }

    func markSessionUnread(_ sessionID: String) {
        chat.unreadSessionIDs.insert(sessionID)
        saveSessionReadCursor(sessionID, unread: true)
    }

    private func saveSessionReadCursor(_ sessionID: String, unread: Bool) {
        guard let accountID = gateway.selectedAccountID,
            let session = chat.sessions.first(where: { $0.sessionId == sessionID })
        else { return }
        let cursor =
            unread
            ? SessionReadCursor(sequence: nil, wasActive: session.activity.state != .idle)
            : sessionReadCursor(for: session)
        guard chat.sessionReadCursors?[sessionID] != cursor else { return }
        var cursors = chat.sessionReadCursors ?? [:]
        cursors[sessionID] = cursor
        chat.sessionReadCursors = cursors
        store.saveSessionReadCursors(cursors, accountID: accountID)
    }

    func sessionReadCursor(for session: SessionRecord) -> SessionReadCursor {
        SessionReadCursor(
            sequence: session.sequence,
            wasActive: session.activity.state != .idle
        )
    }

    var capabilityReferences: [MountedReference] {
        chat.contributions.flatMap { contribution in
            contribution.references.map {
                MountedReference(capability: contribution.capability, reference: $0)
            }
        }
    }

    func commandSuggestions(in text: String, cursorOffset: Int) -> ReferenceSuggestions? {
        guard text.hasPrefix("/"), chat.pendingWidgetEdit == nil else { return nil }
        let end = text.firstIndex(where: \.isWhitespace) ?? text.endIndex
        guard cursorOffset >= 0, cursorOffset <= text.distance(from: text.startIndex, to: end)
        else {
            return nil
        }
        let query = text[text.index(after: text.startIndex)..<end].lowercased()
        let matches = chat.contributions.flatMap { contribution in
            contribution.commands.filter { $0.name.hasPrefix(query) }.map { command in
                MountedReference(
                    capability: contribution.capability,
                    reference: FrontendReference(
                        trigger: "/",
                        value: command.name
                            + (command.arguments.isEmpty ? "" : " \(command.arguments)"),
                        description: command.description
                    ),
                    replacement: "/\(command.name) "
                )
            }
        }
        guard !matches.isEmpty else { return nil }
        let exact = "/\(query) "
        let ordered =
            matches.filter { $0.replacement == exact }
            + matches.filter { $0.replacement != exact }
        return ReferenceSuggestions(source: text, range: text.startIndex..<end, matches: ordered)
    }

    var extensionSkillReferences: [FrontendReference] {
        let selected =
            agentSnapshot?.config.extensions
            ?? botDefaultsSnapshot?.config.extensions
            ?? []
        var seen = Set(
            extensions
                .filter { selected.contains($0.id) }
                .flatMap(\.skills))
        let references = (gatewayContributions + chat.contributions)
            .flatMap(\.references)
            .filter { $0.trigger == "$" }
        return references.filter {
            seen.insert($0.value).inserted
        }
    }

    var currentSessionTitle: String {
        chat.selectedSessionID.map(sessionTitle) ?? localizedString("new conversation")
    }

    var selectedSession: SessionRecord? {
        guard let selectedSessionID = chat.selectedSessionID else { return nil }
        return chat.sessions.first { $0.sessionId == selectedSessionID }
            ?? chat.botSessions.first { $0.sessionId == selectedSessionID }
    }

    var selectedSessionIsHidden: Bool {
        if let selectedSessionID = chat.selectedSessionID,
            chat.botSessions.contains(where: { $0.sessionId == selectedSessionID })
        {
            return true
        }
        guard let route = navigationPath.last,
            case .chat(.session) = route
        else { return false }
        return navigationPath.dropLast().contains { route in
            if case .botSessions = route { return true }
            return false
        }
    }

    var selectedBot: BotRecord? {
        guard let botID = selectedSession?.sessionContext.botId ?? chat.pendingNewChatBotID else {
            return nil
        }
        return bots.first { $0.id == botID }
    }

    func bot(for session: SessionRecord) -> BotRecord? {
        bots.first { $0.id == session.sessionContext.botId }
    }

    func bot(forSessionID sessionID: String?) -> BotRecord? {
        guard let sessionID else { return nil }
        if let session = chat.sessions.first(where: { $0.sessionId == sessionID })
            ?? chat.botSessions.first(where: { $0.sessionId == sessionID })
        {
            return bot(for: session)
        }
        guard let approval = backgroundApproval(forSessionID: sessionID) else { return nil }
        return bots.first { $0.id == approval.botId }
    }

    func backgroundApproval(forSessionID sessionID: String?) -> BackgroundApproval? {
        guard let sessionID else { return nil }
        return backgroundApprovals.first { $0.sessionId == sessionID }
    }

    func hasBackgroundApproval(forBotID botID: String) -> Bool {
        backgroundApprovals.contains { $0.botId == botID }
    }

    func hasSwarmAttention(forSwarmID swarmID: String) -> Bool {
        swarmAttentions.contains { $0.swarmId == swarmID }
    }

    func bot(for target: AppNotificationTarget?) -> BotRecord? {
        switch target {
        case .session(let sessionID):
            bot(forSessionID: sessionID)
        case .swarm(_, let messageID):
            swarmAttentions.first { $0.messageId == messageID }
                .flatMap { attention in bots.first { $0.id == attention.botId } }
        case nil:
            nil
        }
    }

    var selectedBotSwarm: SwarmRecord? {
        selectedSession.flatMap { swarm(containingBot: $0.sessionContext.botId) }
    }

    func beginRenamingSession(_ session: SessionRecord) {
        chat.sessionRenameDraft = displayedTitle(for: session)
        chat.sessionToRename = session
    }

    func beginDeletingSession(_ session: SessionRecord) {
        beginDeletingSessions([session])
    }

    func beginDeletingSessions(_ sessions: [SessionRecord]) {
        var seen = Set<String>()
        let uniqueSessions = sessions.filter { seen.insert($0.sessionId).inserted }
        guard !uniqueSessions.isEmpty else { return }
        chat.sessionToDelete = uniqueSessions
    }

    func displayedTitle(for session: SessionRecord) -> String {
        if let title = chat.pendingChatTitles[session.sessionId]?.displayTitle
            ?? session.explicitTitle
            ?? ChatTitleWriter.preview(for: session.firstUserMessage)
        {
            return title
        }
        return localizedString("new conversation")
    }

    func sessionTitle(_ sessionID: String) -> String {
        if let pendingTitle = chat.pendingChatTitles[sessionID]?.displayTitle {
            return pendingTitle
        }
        let session =
            chat.sessions.first(where: { $0.sessionId == sessionID })
            ?? chat.botSessions.first(where: { $0.sessionId == sessionID })
        return session.map { String(displayedTitle(for: $0).prefix(72)) }
            ?? localizedString("new conversation")
    }

    func contributions(in scope: ContributionScope) -> [FrontendContribution] {
        switch scope {
        case .global: gatewayContributions
        case .swarm(let id): swarmContributions[id] ?? []
        }
    }

    func navigationWidgets(in scope: ContributionScope) -> [MountedWidget] {
        contributions(in: scope).flatMap { contribution in
            contribution.widgets.filter { $0.slot == .navigation }.map {
                MountedWidget(capability: contribution.capability, widget: $0)
            }
        }
    }

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
        let start =
            text[..<cursor].lastIndex(where: { $0.isWhitespace })
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
                matches.append(
                    contentsOf: workspaceFiles.prefix(8 - matches.count).map {
                        Self.workspaceReference($0)
                    })
            }
        } else {
            var ranked: [(score: ReferenceMatchScore, reference: MountedReference)] = []
            func consider(_ reference: MountedReference) {
                guard let score = referenceScore(reference.reference.value, query: query) else {
                    return
                }
                let index =
                    ranked.firstIndex {
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
            let pretty = try? JSONSerialization.data(
                withJSONObject: object, options: [.prettyPrinted, .sortedKeys]),
            let text = String(data: pretty, encoding: .utf8)
        else { return "{}" }
        return text
    }
}
