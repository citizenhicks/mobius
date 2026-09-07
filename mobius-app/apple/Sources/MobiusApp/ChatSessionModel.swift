import CoreGraphics
import Foundation
import Observation

@MainActor
@Observable
final class ChatSessionModel {
    @ObservationIgnored let gateway: GatewayConnectionModel
    @ObservationIgnored let store: GatewayStore
    @ObservationIgnored let titleWriter: ChatTitleWriter
    @ObservationIgnored let dictation: ComposerDictation
    @ObservationIgnored let messageSpeaker: MessageSpeaker
    var locale: Locale
    var onToast: (@MainActor (String, ToastTone) -> Void)?
    var onWorkspaceRefresh: (@MainActor () -> Void)?
    var onSessionFilesRefresh: (@MainActor () -> Void)?
    var onDiscardFilePresentation: (@MainActor () -> Void)?
    var onOpenChat: (@MainActor (String) -> Void)?

    var sessions: [SessionRecord] = []
    var botSessions: [SessionRecord] = []
    var botSessionsBotID: String?
    var isLoadingBotSessions = false
    var chatBotFilterIDs: Set<String> = []
    var chatCatalogSessions: [SessionRecord] {
        guard !chatBotFilterIDs.isEmpty else { return sessions }
        return sessions.filter { chatBotFilterIDs.contains($0.sessionContext.botId) }
    }
    var chatPresentationRevision = 0
    var sessionToRename: SessionRecord?
    var sessionRenameDraft = ""
    var sessionToDelete: [SessionRecord]?
    var sessionMutationRequestID: String?
    var unreadSessionIDs: Set<String> = []
    @ObservationIgnored var sessionReadCursors: [String: SessionReadCursor]?

    var selectedSessionID: String? {
        didSet {
            if oldValue != selectedSessionID { stopRealtimeVoice() }
        }
    }
    var transcript: [TranscriptEntry] = [] {
        didSet { updateTranscriptWindow(after: oldValue) }
    }
    var replayPresentedTranscript: [TranscriptEntry]? {
        didSet { invalidateTranscriptProjection() }
    }
    var pendingPresentedTranscript: [TranscriptEntry]? {
        didSet { invalidateTranscriptProjection() }
    }
    var transcriptWindowAnchor = TranscriptWindowAnchor.tail {
        didSet { invalidateTranscriptProjection() }
    }
    var isLoadingEarlierHistory = false
    var historyLoadCompletionRevision = 0
    var historyLoadSuccessRevision = 0
    var historyLoadFailureRevision = 0

    var composer = "" {
        didSet { scheduleComposerDraftSave() }
    }
    var composerReply: MessageReply? {
        didSet { scheduleComposerDraftSave() }
    }
    var messageNavigationRequest: MessageNavigationRequest?
    @ObservationIgnored var transcriptProjectionCache:
        (key: TranscriptProjectionKey, projection: TranscriptProjection)?
    @ObservationIgnored var transcriptWindowCache: TranscriptWindowCache?
    @ObservationIgnored var transcriptProjectionVersion = 0
    @ObservationIgnored var transcriptMutationPreservesPrefix = false
    var composerFocusRequest = 0
    private(set) var composerBlurRequest = 0
    var composerAttachments: [ComposerAttachment] = []
    var fileThumbnails: [FileThumbnailKey: CGImage] = [:]
    var sessionFiles: [SessionFileRecord] = []
    var isLoadingSessionFiles = false
    var activeTurnID: String?
    var steeringDeliveryRevision = 0
    var contextTokens = 0
    var sessionCompactionCount: UInt64 = 0
    var modelContextWindow: Int64?
    var contextLimitTokens: Int64?
    var pendingApproval: PendingApproval?
    var selectedModelRoute = "" {
        didSet {
            if oldValue != selectedModelRoute { stopRealtimeVoice() }
        }
    }
    var contributions: [FrontendContribution] = [] {
        didSet { contributionsRevision &+= 1 }
    }
    private(set) var contributionsRevision = 0
    var mountedWidgets: [MountedWidget] = []
    var pendingPicker: FrontendPickerPrompt?
    var previews: [TranscriptPreview] = []
    var presentedPreview: TranscriptPreview?
    var isLoadingPreviewPage = false
    var runStats = RunStats()
    var currentUsage = TokenUsage()
    var lastUsage = TokenUsage()
    var pendingNewChatWorkspace: String?
    var pendingNewChatBotID: String?
    var pendingWidgetEdit: PendingWidgetEdit?
    var stashedComposerDraft: String?
    var isLoadingComposerEditRecovery = false
    var pendingChatTitles: [String: PendingChatTitle] = [:]
    @ObservationIgnored var chatTitleTasks: [String: Task<Void, Never>] = [:]
    @ObservationIgnored var titleEligibleSessionIDs: Set<String> = []
    var isClearingLocalData = false
    var attachmentReferenceLimit: Int {
        min(
            sessionFileLimits?.maxAttachmentReferences ?? 0,
            maximumWireSessionFileReferences
        )
    }
    var sessionFileLimits: SessionFileLimits?

    var uploadChunkByteLimit: Int {
        min(sessionFileLimits?.maxUploadChunkBytes ?? 0, maximumClientUploadChunkBytes)
    }

    @ObservationIgnored var pendingDrafts: [String: PendingComposerDraft] = [:]
    @ObservationIgnored var composerEditRecoveryGeneration = UUID()
    @ObservationIgnored var replayCompletionSubmissionIDs: Set<String> = []
    @ObservationIgnored var replayUserMessages: [ReplayUserMessage] = []
    @ObservationIgnored var completedComposerEditReplay = false
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
    @ObservationIgnored var botSessionsRequestID: String?
    @ObservationIgnored var pendingBotSessionResume: (botID: String, sessionID: String)?
    @ObservationIgnored var pendingDeletedSessionIDs: [String] = []
    @ObservationIgnored var pendingDeletedPresentedSessionID: String?
    @ObservationIgnored var sessionToRestoreID: String?
    @ObservationIgnored var approvalRequestID: String?
    @ObservationIgnored var sessionFilesRequestID: String?
    @ObservationIgnored var sessionFileUploadRequests: [String: SessionFileUploadRequest] = [:]
    @ObservationIgnored var abandonedSessionFileUploadRequests: [String: RemovedComposerAttachment] = [:]
    @ObservationIgnored var sessionFileDeleteRequests: [String: RemovedComposerAttachment] = [:]
    @ObservationIgnored var sessionFileData: [UUID: Data] = [:]
    @ObservationIgnored var activeSessionFileUpload: ActiveSessionFileUpload?
    @ObservationIgnored var sessionFileDownload: SessionFileDownload?
    @ObservationIgnored var fileThumbnailOrder: [FileThumbnailKey] = []
    @ObservationIgnored var requestedSessionFileThumbnailKeys: Set<FileThumbnailKey> = []
    @ObservationIgnored var discardedSessionFileThumbnailRequestIDs: Set<String> = []
    @ObservationIgnored var queuedSessionFileThumbnails:
        [(sessionID: String, file: SessionFileReference)] = []
    @ObservationIgnored var sessionFileThumbnailDownload: SessionFileThumbnailDownload?
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
    @ObservationIgnored var previewWidgetRequestID: String?
    @ObservationIgnored var previewPageRequestID: String?
    @ObservationIgnored var visibleChatWindowTokens: Set<UUID> = []
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
    @ObservationIgnored var realtimeVoiceTask: Task<Void, Never>?
    var realtimeVoice = RealtimeVoiceSession()
    var realtimeVoiceCall: RealtimeVoiceCall?

    var hasEarlierHistory: Bool {
        transcriptWindow.hasEarlierEntries
            || nextHistoryBeforeSequence != nil
            || isLoadingEarlierHistory
    }

    var composerHasUnfinishedAttachments: Bool {
        composerAttachments.contains { item in
            switch item.state {
            case .uploaded: false
            case .preparing, .queued, .uploading, .failed: true
            }
        }
    }

    @discardableResult
    func finishSessionReplay() -> String? {
        let completedRequestID = replayRequestID
        flushStreamDeltas()
        if let replaySnapshotSequence {
            latestSequence = replaySnapshotSequence
        }
        replayRequestID = nil
        self.replaySnapshotSequence = nil
        replayPresentedTranscript = nil
        gateway.connectionState = .ready
        completedComposerEditReplay = true
        reconcileChatTitleAfterReplay()
        reconcileComposerEditRecovery()
        onWorkspaceRefresh?()
        refreshSessionFiles()
        startNextSessionFileThumbnailDownload()
        cacheSelectedTranscript()
        return completedRequestID
    }

    func takePendingNewChatDraft(requestID: String?) async -> PendingComposerDraft? {
        guard let requestID,
              let sessionID = selectedSessionID
        else { return nil }
        await composerDraftIOTask?.value
        guard gateway.connectionState.isReady,
              selectedSessionID == sessionID
        else { return nil }
        return pendingDrafts.removeValue(forKey: requestID)
    }

    var canBeginReply: Bool {
        gateway.connectionState.isReady
            && selectedSessionID != nil
            && sessionRequestID == nil
            && sessionMutationRequestID == nil
            && !botSessions.contains(where: { $0.sessionId == selectedSessionID })
            && !isLoadingComposerDraft
            && !isLoadingComposerEditRecovery
            && pendingWidgetEdit == nil
    }

    var isLoadingTranscript: Bool {
        if !gateway.connectionState.isReady,
           let selectedSessionID,
           sessionToRestoreID == selectedSessionID,
           latestSequence == nil,
           transcript.isEmpty {
            return true
        }
        guard gateway.connectionState == .loading,
              sessionRequestID != nil || replayRequestID != nil
        else { return false }
        let opensAnotherSessionWithoutCache =
            (sessionOpeningID.map { $0 != selectedSessionID } ?? false)
            && pendingPresentedTranscript == nil
        if opensAnotherSessionWithoutCache { return true }
        return (pendingPresentedTranscript ?? replayPresentedTranscript ?? transcript).isEmpty
    }

    var canLoadEarlierHistory: Bool {
        hasEarlierHistory
            && (gateway.connectionState.isReady || transcriptWindow.hasEarlierEntries)
            && historyRequestID == nil
    }

    func turnDiff(for entry: TranscriptEntry) -> String {
        transcriptTurnDiff(for: entry, in: transcript)
    }

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
        if let cached = transcriptProjectionCache, cached.key == key { return cached.projection }
        let projection = TranscriptProjection(
            entries: source,
            breakBefore: boundaryID,
            waitingPhrase: waitingPhrase,
            previous: transcriptProjectionCache?.projection
        )
        transcriptProjectionCache = (key, projection)
        return projection
    }

    func widgets(in slot: FrontendSlot) -> [MountedWidget] {
        mountedWidgets.filter { $0.widget.slot == slot }
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
    func dismissComposerFocus() {
        composerBlurRequest &+= 1
    }

    func pinTranscriptWindowIfNeeded() {
        guard replayRequestID == nil,
              historyRequestID == nil,
              let cached = transcriptWindowCache,
              cached.turnCount > 0
        else { return }
        switch transcriptWindowAnchor {
        case .visibleTurns:
            return
        case .tail:
            transcriptWindowAnchor = .visibleTurns(cached.turnCount)
            transcriptWindowCache = cached
        }
    }

    func finishHistoryLoad(succeeded: Bool = false) {
        let wasLoading = historyRequestID != nil || isLoadingEarlierHistory
        historyRequestID = nil
        isLoadingEarlierHistory = false
        guard wasLoading else { return }
        historyLoadCompletionRevision &+= 1
        if succeeded {
            historyLoadSuccessRevision &+= 1
        } else {
            historyLoadFailureRevision &+= 1
        }
    }

    func invalidateTranscriptProjection() {
        transcriptProjectionVersion &+= 1
        transcriptWindowCache = nil
    }

    func updateTranscriptWindow(after previous: [TranscriptEntry]) {
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

    var displayedTranscript: [TranscriptEntry] { transcriptWindow.entries }

    var transcriptWindow: TranscriptWindowCache {
        let source = pendingPresentedTranscript ?? replayPresentedTranscript ?? transcript
        if let transcriptWindowCache { return transcriptWindowCache }
        let maximumTurns = switch transcriptWindowAnchor {
        case .tail: transcriptTurnsPerPage
        case .visibleTurns(let count): count
        }
        let window = TranscriptProjection.turnWindow(from: source, maximumTurns: maximumTurns)
        let cached = TranscriptWindowCache(
            entries: window.entries,
            turnCount: window.turnCount,
            hasEarlierEntries: window.hasEarlierEntries
        )
        transcriptWindowCache = cached
        return cached
    }

    var activeTranscriptStepID: String? {
        activeStepID(in: displayedTranscript, isRunning: activeTurnID != nil)
    }

    var isWaitingForModel: Bool {
        TranscriptWaitingNote.isWaiting(
            hasActiveTurn: activeTurnID != nil,
            lastEntryIsPending: displayedTranscript.last?.pending == true,
            connectionIsReady: gateway.connectionState.isReady,
            hasPendingApproval: pendingApproval != nil,
            hasPendingPicker: pendingPicker != nil
        )
    }

    init(
        gateway: GatewayConnectionModel,
        store: GatewayStore,
        titleWriter: ChatTitleWriter,
        dictation: ComposerDictation,
        messageSpeaker: MessageSpeaker,
        locale: Locale = .current
    ) {
        self.gateway = gateway
        self.store = store
        self.titleWriter = titleWriter
        self.dictation = dictation
        self.messageSpeaker = messageSpeaker
        self.locale = locale
    }

    isolated deinit {
        composerDraftSaveTask?.cancel()
        transcriptIOTask?.cancel()
        realtimeVoiceTask?.cancel()
        realtimeVoice.close()
    }

    func showToast(_ resource: LocalizedStringResource, tone: ToastTone = .info) {
        var resource = resource
        resource.locale = locale
        onToast?(String(localized: resource), tone)
    }

    func showToast(verbatim message: String, tone: ToastTone = .info) {
        onToast?(message, tone)
    }

    func localizedErrorDescription(_ error: Error) -> String {
        if let resource = (error as? GatewayWireError)?.localizedDescriptionResource {
            var resource = resource
            resource.locale = locale
            return String(localized: resource)
        }
        if let resource = (error as? GatewayStore.StoreError)?.localizedDescriptionResource {
            var resource = resource
            resource.locale = locale
            return String(localized: resource)
        }
        return error.localizedDescription
    }

    func requestID(_ prefix: String) -> String {
        "\(prefix)-\(UUID().uuidString.lowercased())"
    }

    func cacheSelectedTranscript() {
        guard !isClearingLocalData,
              let accountID = gateway.selectedAccountID,
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
            await store.saveTranscript(snapshot, accountID: accountID, sessionID: sessionID)
        }
    }

    func decodeApproval(_ value: JSONValue) -> PendingApproval? {
        guard let id = value["id"]?.stringValue else { return nil }
        let calls = value["calls"]?.arrayValue?.compactMap { call -> ApprovalCall? in
            guard let callID = call["callId"]?.stringValue,
                  let name = call["name"]?.stringValue
            else { return nil }
            return ApprovalCall(
                id: callID,
                name: name,
                arguments: call["arguments"]?.prettyPrinted ?? "{}"
            )
        } ?? []
        return PendingApproval(
            id: id,
            reason: value["reason"]?.stringValue ?? "möbius needs permission to continue.",
            calls: calls
        )
    }

    func refreshSessionFiles() {
        guard gateway.connectionState.isReady, let sessionID = selectedSessionID else { return }
        let id = requestID("session-files")
        sessionFilesRequestID = id
        isLoadingSessionFiles = true
        gateway.transmit(.listSessionFiles(requestID: id, sessionID: sessionID)) { [weak self] _ in
            guard self?.sessionFilesRequestID == id else { return }
            self?.sessionFilesRequestID = nil
            self?.isLoadingSessionFiles = false
        }
    }

    func resetSessionState(preservingComposerAttachments: Bool = false) {
        stopRealtimeVoice()
        composerReply = nil
        messageNavigationRequest = nil
        if !preservingComposerAttachments { discardComposerAttachments() }
        cancelSessionFileThumbnailDownloads()
        sessionFiles = []
        sessionFilesRequestID = nil
        isLoadingSessionFiles = false
        sessionFileUploadRequests.removeAll()
        activeSessionFileUpload = nil
        selectedModelRoute = ""
        contributions = []
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
        currentUsage = TokenUsage()
        lastUsage = TokenUsage()
    }

    func quiesce() {
        selectedSessionID = nil
        sessionRequestID = nil
        sessionOpeningID = nil
        pendingDrafts.removeAll()
        pendingWidgetEdit = nil
        stashedComposerDraft = nil
        discardComposerDraft()
        discardComposerAttachments()
    }

    func drainIO() async {
        let draftIO = composerDraftIOTask
        let transcriptIO = transcriptIOTask
        await draftIO?.value
        await transcriptIO?.value
    }

    func markSessionRead(_ sessionID: String) {
        unreadSessionIDs.remove(sessionID)
        saveSessionReadCursor(sessionID, unread: false)
    }

    private func saveSessionReadCursor(_ sessionID: String, unread: Bool) {
        guard let accountID = gateway.selectedAccountID,
              let session = sessions.first(where: { $0.sessionId == sessionID })
        else { return }
        let cursor = unread
            ? SessionReadCursor(sequence: nil, wasActive: session.activity.state != .idle)
            : SessionReadCursor(sequence: session.sequence, wasActive: session.activity.state != .idle)
        guard sessionReadCursors?[sessionID] != cursor else { return }
        var cursors = sessionReadCursors ?? [:]
        cursors[sessionID] = cursor
        sessionReadCursors = cursors
        store.saveSessionReadCursors(cursors, accountID: accountID)
    }
}
