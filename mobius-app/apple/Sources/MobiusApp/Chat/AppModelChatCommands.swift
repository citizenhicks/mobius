import Foundation
import Observation

extension AppModel {
    func selectBotForNewChat(_ bot: BotRecord, selected: Bool = true) {
        guard canCreateSession,
            bots.contains(where: { $0.id == bot.id }),
            chat.pendingNewChatWorkspace != nil,
            case .chat(.new)? = navigationPath.last
        else { return }
        if selected {
            chat.pendingNewChatBotIDs.insert(bot.id)
        } else {
            chat.pendingNewChatBotIDs.remove(bot.id)
        }
        workspaceError = nil
        createPendingVoiceChat()
    }

    @discardableResult
    func createPendingSession() -> String? {
        guard canCreateSession,
            let path = chat.pendingNewChatWorkspace,
            !chat.pendingNewChatBotIDs.isEmpty,
            chat.pendingNewChatBotIDs.allSatisfy({ id in bots.contains { $0.id == id } }),
            case .chat(.new)? = navigationPath.last
        else { return nil }
        let id = requestID("create")
        chat.sessionRequestID = id
        workspaceError = nil
        isChangingWorkspace = true
        gateway.connectionState = .loading
        gateway.transmit(
            .createSession(
                requestID: id, workspace: path, botIDs: chat.pendingNewChatBotIDs.sorted())
        ) {
            [weak self] message in
            guard let self, self.chat.sessionRequestID == id else { return }
            self.chat.restoreDraft(id: id)
            self.chat.sessionRequestID = nil
            self.isChangingWorkspace = false
            self.gateway.connectionState = .ready
            self.workspaceError = message
            self.cancelVoiceChatIntent()
        }
        return id
    }

    func openNewSession() {
        guard canCreateSession else { return }
        cancelVoiceChatIntent()
        chat.stopRealtimeVoice()
        destination = .chats
        navigationPath = []
        let path = workspace?.path ?? newChatWorkspacePaths.first ?? "."
        chooseWorkspace(path)
        if path == "." { loadDirectory(path) }
    }

    var newChatWorkspacePaths: [String] {
        let paths =
            chat.sessions.compactMap(\.sessionContext.workspaceLabel)
            + [chat.pendingNewChatWorkspace, workspace?.path].compactMap { $0 }
        return Set(paths.filter { !$0.isEmpty }).sorted {
            $0.localizedStandardCompare($1) == .orderedAscending
        }
    }

    func openNewSessionInCurrentWorkspace() {
        guard let path = workspace?.path, let selectedSession else { return }
        let ids = selectedSession.botIds
        chooseWorkspace(path)
        chat.pendingNewChatBotIDs = Set(ids)
    }

    func openChat(_ sessionID: String) {
        guard canBrowseSessions || sessionID == chat.selectedSessionID else { return }
        cancelVoiceChatIntent()
        chat.chatPresentationRevision &+= 1
        destination = .chats
        chat.openSession(sessionID)
        navigationPath = [.chat(.session(sessionID))]
        cacheChatCatalog(lastSessionID: sessionID)
    }

    func openBotChats(_ botID: String) {
        guard bots.contains(where: { $0.id == botID }) else { return }
        chat.chatBotFilterIDs = [botID]
        destination = .chats
        navigationPath = []
    }

    func openBotSessions(_ botID: String) {
        guard bots.contains(where: { $0.id == botID }) else { return }
        if chat.botSessionsBotID != botID {
            chat.botSessionsRequestID = nil
            chat.botSessions = []
            chat.isLoadingBotSessions = false
        }
        chat.botSessionsBotID = botID
        destination = .bots
        if navigationPath.last != .botSessions(botID) {
            navigationPath.append(.botSessions(botID))
        }
        refreshBotSessions(botID)
    }

    func refreshBotSessions(_ botID: String) {
        guard gateway.connectionState.isReady,
            chat.botSessionsRequestID == nil,
            bots.contains(where: { $0.id == botID })
        else { return }
        if chat.botSessionsBotID != botID { chat.botSessions = [] }
        chat.botSessionsBotID = botID
        let id = requestID("bot-sessions")
        chat.botSessionsRequestID = id
        chat.isLoadingBotSessions = true
        gateway.transmit(.listBotSessions(requestID: id, botID: botID)) { [weak self] _ in
            guard self?.chat.botSessionsRequestID == id else { return }
            self?.chat.botSessionsRequestID = nil
            self?.chat.isLoadingBotSessions = false
        }
    }

    func openBotSession(_ sessionID: String) {
        guard canBrowseSessions || sessionID == chat.selectedSessionID,
            chat.botSessions.contains(where: { $0.sessionId == sessionID })
        else { return }
        cancelVoiceChatIntent()
        chat.chatPresentationRevision &+= 1
        chat.openSession(sessionID)
        navigationPath.append(.chat(.session(sessionID)))
    }

    func resumeBotSession(botID: String, sessionID: String) {
        guard bots.contains(where: { $0.id == botID }) else { return }
        if let visible = chat.sessions.first(where: { $0.sessionId == sessionID }) {
            guard visible.sessionContext.botId == botID else {
                showToast("The source conversation belongs to another Bot.", tone: .error)
                return
            }
            chat.pendingBotSessionResume = nil
            openChat(sessionID)
            return
        }
        guard canOpenSession else { return }
        chat.pendingBotSessionResume = (botID, sessionID)
        if chat.botSessionsBotID != botID {
            chat.botSessionsRequestID = nil
            chat.botSessions = []
            chat.isLoadingBotSessions = false
        }
        chat.botSessionsBotID = botID
        refreshBotSessions(botID)
    }

    func canReassignSession(_ session: SessionRecord) -> Bool {
        guard let current = chat.sessions.first(where: { $0.sessionId == session.sessionId })
        else { return false }
        return !current.isGroup && canRenameSession && current.activity.state == .idle
            && (current.sessionId != chat.selectedSessionID
                || (canModifySelectedSession && chat.realtimeVoiceCall == nil))
    }

    @discardableResult
    func reassignSession(_ session: SessionRecord, to botID: String) -> String? {
        guard let current = chat.sessions.first(where: { $0.sessionId == session.sessionId }),
            canReassignSession(current), bots.contains(where: { $0.id == botID }),
            botID != current.sessionContext.botId
        else { return nil }
        let id = requestID("session-reassign")
        chat.sessionMutationRequestID = id
        gateway.transmit(
            .reassignSession(requestID: id, sessionID: session.sessionId, botID: botID)
        ) {
            [weak self] _ in
            if self?.chat.sessionMutationRequestID == id {
                self?.chat.sessionMutationRequestID = nil
            }
        }
        return id
    }

    // Renaming, pinning and deleting address a session by id, so they work on any chat in the
    // catalogue rather than only the open one.
    @discardableResult
    func renameSession(_ session: SessionRecord, title: String) -> String? {
        let title = title.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !title.isEmpty else { return nil }
        guard chat.sessionMutationRequestID == nil else {
            showToast("Another chat update is finishing.", tone: .info)
            return nil
        }
        chat.cancelChatTitle(session.sessionId)
        return chat.requestSessionRename(sessionID: session.sessionId, title: title)
    }

    func attachFolder(_ selectedPath: String) {
        let path = selectedPath.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canModifySelectedSession, !selectedChatIsGroup,
            let sessionID = chat.selectedSessionID,
            chat.sessionMutationRequestID == nil,
            !path.isEmpty
        else { return }
        let id = requestID("session-attach-folder")
        chat.sessionMutationRequestID = id
        gateway.transmit(
            .attachSessionFolder(
                requestID: id,
                sessionID: sessionID,
                folder: path
            )
        ) { [weak self] _ in
            if self?.chat.sessionMutationRequestID == id {
                self?.chat.sessionMutationRequestID = nil
            }
        }
    }

    func deleteSession(_ session: SessionRecord) {
        deleteSessions([session])
    }

    func deleteSessions(_ sessions: [SessionRecord]) {
        guard chat.sessionMutationRequestID == nil else { return }
        var seen = Set<String>()
        let sessionIDs = sessions.map(\.sessionId).filter { seen.insert($0).inserted }
        guard !sessionIDs.isEmpty else { return }
        let deletesSelectedSession = chat.selectedSessionID.map(sessionIDs.contains) == true
        let deletedPresentedSessionID = presentedChatSessionID.flatMap { presented in
            sessionIDs.contains(presented) ? presented : nil
        }
        if let accountID = gateway.selectedAccountID {
            chat.enqueueTranscriptIO { [store] in
                for sessionID in sessionIDs {
                    await store.removeTranscript(accountID: accountID, sessionID: sessionID)
                }
            }
        }
        let id = requestID("session-delete")
        chat.sessionMutationRequestID = id
        chat.pendingDeletedSessionIDs = sessionIDs
        chat.pendingDeletedPresentedSessionID = deletedPresentedSessionID
        gateway.transmit(
            .deleteSessions(
                requestID: id,
                sessionIDs: sessionIDs
            )
        ) { [weak self] _ in
            guard let self, self.chat.sessionMutationRequestID == id else { return }
            let sessionID = self.chat.pendingDeletedPresentedSessionID
            self.chat.sessionMutationRequestID = nil
            self.chat.pendingDeletedSessionIDs = []
            self.chat.pendingDeletedPresentedSessionID = nil
            self.restoreDeletedPresentedSession(sessionID)
        }
        if deletesSelectedSession { clearSelectedSession() }
    }

    func restoreDeletedPresentedSession(_ sessionID: String?) {
        guard let sessionID,
            destination == .chats,
            navigationPath.isEmpty
        else { return }
        navigationPath = [.chat(.session(sessionID))]
        chat.restoreSession(sessionID)
    }

    func interrupt() {
        guard let sessionID = chat.selectedSessionID, let activeTurnID = chat.activeTurnID else {
            return
        }
        gateway.transmit(
            .submit(
                sessionID: sessionID,
                submission: Submission(
                    id: requestID("interrupt"),
                    op: .interrupt(turnID: activeTurnID)
                )
            ))
    }

    func resolveApproval(_ decision: ReviewDecision) {
        guard let sessionID = chat.selectedSessionID,
            let approval = chat.pendingApproval,
            chat.approvalRequestID == nil
        else { return }
        let id = requestID("approval")
        chat.approvalRequestID = id
        gateway.transmit(
            .submit(
                sessionID: sessionID,
                submission: Submission(
                    id: id,
                    op: .execApproval(id: approval.id, decision: decision)
                )
            )
        ) { [weak self] _ in
            guard self?.chat.approvalRequestID == id else { return }
            self?.chat.approvalRequestID = nil
        }
    }
}
