import Foundation
import Observation

struct BotConversationState {
    var botID: String?
    var conversations: [BotConversation] = []
    var nextCursor: BotConversationCursor?
    var request: (id: String, cursor: BotConversationCursor?)?
    var error: String?
    var presented: BotConversation?
    var entries: [TranscriptEntry] = []
    var nextBeforeSequence: UInt64?
    var historyRequest: (id: String, before: UInt64?)?
    var historyError: String?
}

extension AppModel {
    func openBotConversations(_ botID: String) {
        guard bots.contains(where: { $0.id == botID }) else { return }
        destination = .bots
        if navigationPath.last != .botConversations(botID) {
            navigationPath.append(.botConversations(botID))
        }
        refreshBotConversations(botID)
    }

    func refreshBotConversations(_ botID: String) {
        guard bots.contains(where: { $0.id == botID }) else { return }
        if botConversationState.botID != botID {
            closeBotConversation()
            botConversationState = BotConversationState(botID: botID)
        }
        loadBotConversations(cursor: nil)
    }

    func loadMoreBotConversations() {
        guard let cursor = botConversationState.nextCursor else { return }
        loadBotConversations(cursor: cursor)
    }

    private func loadBotConversations(cursor: BotConversationCursor?) {
        guard gateway.connectionState.isReady,
            botConversationState.request == nil,
            let botID = botConversationState.botID
        else { return }
        let id = requestID("bot-conversations")
        botConversationState.request = (id, cursor)
        botConversationState.error = nil
        gateway.transmit(.listBotConversations(requestID: id, botID: botID, cursor: cursor)) {
            [weak self] message in
            guard self?.botConversationState.request?.id == id else { return }
            self?.botConversationState.request = nil
            self?.botConversationState.error = message
        }
    }

    func applyBotConversations(requestID: String, botID: String, page: BotConversationPage) {
        guard let request = botConversationState.request, request.id == requestID,
            botConversationState.botID == botID
        else { return }
        botConversationState.request = nil
        guard page.conversations.allSatisfy({ $0.botId == botID }),
            page.nextCursor == nil || page.nextCursor != request.cursor
        else {
            botConversationState.error = localizedString(
                "The gateway returned invalid private conversations.")
            return
        }
        if request.cursor == nil {
            botConversationState.conversations = page.conversations
        } else {
            let existing = Set(botConversationState.conversations.map(\.id))
            botConversationState.conversations.append(
                contentsOf: page.conversations.filter { !existing.contains($0.id) })
        }
        botConversationState.nextCursor = page.nextCursor
        botConversationState.error = nil
    }

    func presentBotConversation(_ conversation: BotConversation) {
        guard gateway.connectionState.isReady,
            conversation.botId == botConversationState.botID,
            botConversationState.conversations.contains(where: { $0.id == conversation.id })
        else { return }
        closeBotConversation()
        botConversationState.presented = conversation
        chat.previewFileSource = .botConversation(
            botID: conversation.botId, conversationID: conversation.id)
        loadBotConversationHistory()
    }

    func closeBotConversation() {
        chat.cancelSessionFileThumbnailDownloads()
        chat.previewFileSource = nil
        botConversationState.presented = nil
        botConversationState.entries = []
        botConversationState.nextBeforeSequence = nil
        botConversationState.historyRequest = nil
        botConversationState.historyError = nil
    }

    func loadEarlierBotConversationHistoryAndWait() async {
        guard !Task.isCancelled, botConversationState.historyRequest == nil,
            let before = botConversationState.nextBeforeSequence
        else { return }
        loadBotConversationHistory(beforeSequence: before)
        guard botConversationState.historyRequest != nil else { return }
        for await loading in Observations({ self.botConversationState.historyRequest != nil }) {
            if !loading { return }
        }
    }

    func loadBotConversationHistory(beforeSequence: UInt64? = nil) {
        guard gateway.connectionState.isReady,
            botConversationState.historyRequest == nil,
            let conversation = botConversationState.presented
        else { return }
        let id = requestID("bot-conversation-history")
        botConversationState.historyRequest = (id, beforeSequence)
        botConversationState.historyError = nil
        gateway.transmit(
            .getBotConversationHistory(
                requestID: id, botID: conversation.botId,
                conversationID: conversation.id, beforeSequence: beforeSequence)
        ) { [weak self] message in
            guard self?.botConversationState.historyRequest?.id == id else { return }
            self?.botConversationState.historyRequest = nil
            self?.botConversationState.historyError = message
        }
    }

    func applyBotConversationHistory(
        requestID: String, botID: String, conversationID: String,
        records: [RecordedEvent], nextBeforeSequence: UInt64?
    ) {
        guard let request = botConversationState.historyRequest, request.id == requestID,
            let conversation = botConversationState.presented,
            conversation.botId == botID, conversation.id == conversationID
        else { return }
        botConversationState.historyRequest = nil
        guard nextBeforeSequence.map({ next in request.before.map { next < $0 } ?? true }) ?? true
        else {
            botConversationState.historyError = localizedString(
                "The gateway returned an invalid history cursor.")
            return
        }
        var entries: [TranscriptEntry] = []
        var turnState = TranscriptHistoryTurnState()
        for record in records {
            chat.reduceHistory(record, into: &entries, turnState: &turnState)
        }
        botConversationState.entries =
            if request.before == nil {
                entries
            } else {
                chat.mergePreviewPages(older: entries, newer: botConversationState.entries)
            }
        botConversationState.nextBeforeSequence = nextBeforeSequence
        botConversationState.historyError = nil
    }

    func rejectBotConversationRequest(_ rejection: GatewayRejection) {
        if botConversationState.request?.id == rejection.requestId {
            botConversationState.request = nil
            botConversationState.error = rejection.message
        }
        if botConversationState.historyRequest?.id == rejection.requestId {
            botConversationState.historyRequest = nil
            botConversationState.historyError = rejection.message
        }
    }
}
