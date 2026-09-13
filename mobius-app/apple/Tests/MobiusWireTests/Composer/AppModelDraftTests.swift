import Foundation
@testable import Mobius
import XCTest

@MainActor
extension AppModelTests {
    func testChatRecipientsSurviveCreationDraftPersistenceAndRejection() async throws {
        let recorder = GatewayRequestRecorder()
        let app = try model { await recorder.record($0) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        app.gateway.accounts = [account]
        app.gateway.selectedAccountID = account.id
        app.gateway.connectionState = .ready
        let helper = bot()
        let reviewer = bot(id: "bot-2", handle: "reviewer", name: "Reviewer")
        let outsider = bot(id: "bot-3", handle: "outsider", name: "Outsider")
        app.bots = [helper, reviewer, outsider]
        app.chooseWorkspace("/srv/project")
        app.selectBotForNewChat(reviewer)
        app.selectBotForNewChat(helper)
        app.selectPrimaryBotForNewChat(helper)
        app.addComposerRecipient(reviewer)
        app.addComposerRecipient(reviewer)
        app.addComposerRecipient(outsider)
        XCTAssertEqual(app.chat.composerRecipientBotIDs, [reviewer.id])
        app.chat.composer = "Read @README.md and compare the result."
        XCTAssertTrue(app.sendMessage())
        let create = await recorder.firstRequest(after: 0) {
            if case .createSession = $0 { return true }
            return false
        }
        guard
            case .createSession(let createID, _, let memberIDs, let primaryID) = try XCTUnwrap(
                create)
        else { return XCTFail("Expected group creation") }
        XCTAssertEqual(memberIDs, [reviewer.id, helper.id])
        XCTAssertEqual(primaryID, helper.id)
        XCTAssertTrue(app.chat.composerRecipientBotIDs.isEmpty)
        app.chat.composer = "Next draft"
        app.addComposerRecipient(helper)
        app.gateway.handle(
            .sessionOpened(
                requestID: createID,
                payload: sessionReady(
                    latestSequence: 0, sessionID: "group-1", memberBotIDs: memberIDs,
                    primaryBotID: primaryID)))
        app.gateway.handle(.sessionReplayComplete(requestID: createID, sessionID: "group-1"))
        let sent = await recorder.firstRequest(after: 0) {
            if case .submit = $0 { return true }
            return false
        }
        guard case .submit("group-1", let submission, let recipients) = try XCTUnwrap(sent),
            case .message(let message) = submission.op
        else { return XCTFail("Expected group message") }
        XCTAssertEqual(message.text, "Read @README.md and compare the result.")
        XCTAssertEqual(recipients, [reviewer.id])
        XCTAssertNil(message.targetTurnId)
        XCTAssertEqual(app.chat.composer, "Next draft")
        XCTAssertEqual(app.chat.composerRecipientBotIDs, [helper.id])
        app.gateway.handle(.accepted(requestID: submission.id))
        app.chat.flushComposerDraft()
        await app.chat.composerDraftIOTask?.value
        let saved = await app.chat.store.loadComposerDraft(
            accountID: account.id, sessionID: "group-1")
        XCTAssertEqual(saved, ComposerDraft(text: "Next draft", recipientBotIDs: [helper.id]))

        let nextIndex = await recorder.requestCount()
        XCTAssertTrue(app.sendMessage())
        let next = await recorder.firstRequest(after: nextIndex) {
            if case .submit = $0 { return true }
            return false
        }
        guard case .submit(_, let nextSubmission, let nextRecipients) = try XCTUnwrap(next) else {
            return XCTFail("Expected next message")
        }
        XCTAssertEqual(nextRecipients, [helper.id])
        app.gateway.handle(
            .rejected(
                GatewayRejection(
                    requestId: nextSubmission.id, code: "busy", message: "Try again", fatal: false))
        )
        XCTAssertEqual(app.chat.composer, "Next draft")
        XCTAssertEqual(app.chat.composerRecipientBotIDs, [helper.id])
    }

    func testSwitchingSessionsFlushesAndRestoresTextDrafts() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: root)
        }
        let store = GatewayStore(
            defaults: defaults,
            transcriptDirectory: root.appendingPathComponent("Transcripts", isDirectory: true),
            draftDirectory: root.appendingPathComponent("Drafts", isDirectory: true)
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        let firstReply = MessageReply(
            target: MessageTarget(checkpointSequence: 3, batchItemCount: 1),
            text: "First original"
        )
        let secondReply = MessageReply(
            target: MessageTarget(checkpointSequence: 5, batchItemCount: 2),
            text: "Second original"
        )
        await store.saveComposerDraft(
            ComposerDraft(text: "Draft two", reply: secondReply),
            accountID: account.id,
            sessionID: "chat-2"
        )
        let recorder = GatewayRequestRecorder()
        let model = AppModel(
            client: GatewayClient(),
            store: store,
            settingsDefaults: defaults,
            requestSender: { request in await recorder.record(request) }
        )
        model.bots = [bot()]
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready

        var requestCount = await recorder.requestCount()
        model.chat.openSession("chat-1")
        let firstRequest = await recorder.firstRequest(after: requestCount) { request in
            guard case .openSession(_, "chat-1", _) = request else { return false }
            return true
        }
        guard case .openSession(let firstID, "chat-1", _) = try XCTUnwrap(firstRequest)
        else { return XCTFail("Expected first session open") }
        model.chat.composer = "Typed while opening"
        model.gateway.handle(
            .sessionOpened(
                requestID: firstID,
                payload: sessionReady(latestSequence: 1, sessionID: "chat-1")
            ))
        model.gateway.handle(.sessionReplayComplete(requestID: firstID, sessionID: "chat-1"))
        let firstSessionReady = await eventually { model.canCreateSession }
        XCTAssertTrue(firstSessionReady)
        XCTAssertEqual(model.chat.composer, "Typed while opening")
        model.chat.composer = "Draft one"
        model.chat.composerReply = firstReply

        requestCount = await recorder.requestCount()
        model.chat.openSession("chat-2")
        let secondRequest = await recorder.firstRequest(after: requestCount) { request in
            guard case .openSession(_, "chat-2", _) = request else { return false }
            return true
        }
        let secondOpen = try XCTUnwrap(secondRequest)
        guard case .openSession(let secondID, _, _) = secondOpen else {
            return XCTFail("Expected second session open")
        }
        model.gateway.handle(
            .sessionOpened(
                requestID: secondID,
                payload: sessionReady(latestSequence: 1, sessionID: "chat-2")
            ))
        model.gateway.handle(.sessionReplayComplete(requestID: secondID, sessionID: "chat-2"))
        let secondSessionReady = await eventually {
            model.canCreateSession
                && model.chat.composer == "Draft two"
                && model.chat.composerReply == secondReply
        }
        XCTAssertTrue(secondSessionReady)

        let firstDraftSaved = await eventually {
            await store.loadComposerDraft(
                accountID: account.id,
                sessionID: "chat-1"
            ) == ComposerDraft(text: "Draft one", reply: firstReply)
        }
        XCTAssertTrue(firstDraftSaved)
        let firstDraft = await store.loadComposerDraft(
            accountID: account.id,
            sessionID: "chat-1"
        )
        XCTAssertEqual(firstDraft, ComposerDraft(text: "Draft one", reply: firstReply))
        XCTAssertEqual(model.chat.composer, "Draft two")
        XCTAssertEqual(model.chat.composerReply, secondReply)

        requestCount = await recorder.requestCount()
        model.sendMessage()
        model.chat.composer = "Next draft"
        let submitRequest = await recorder.firstRequest(after: requestCount) { request in
            if case .submit = request { return true }
            return false
        }
        let submittedDraft = await store.loadComposerDraft(
            accountID: account.id,
            sessionID: "chat-2"
        )
        XCTAssertEqual(submittedDraft, ComposerDraft(text: "Draft two", reply: secondReply))
        guard case .submit(_, let submission, _) = try XCTUnwrap(submitRequest) else {
            return XCTFail("Expected submitted draft")
        }
        model.gateway.handle(.accepted(requestID: submission.id))
        let nextDraftSaved = await eventually {
            await store.loadComposerDraft(
                accountID: account.id,
                sessionID: "chat-2"
            ) == ComposerDraft(text: "Next draft")
        }
        XCTAssertTrue(nextDraftSaved)
        let nextDraft = await store.loadComposerDraft(
            accountID: account.id,
            sessionID: "chat-2"
        )
        XCTAssertEqual(nextDraft, ComposerDraft(text: "Next draft"))

        requestCount = await recorder.requestCount()
        model.deleteSession(session(sessionID: "chat-2", state: .idle))
        let deleteRequest = await recorder.firstRequest(after: requestCount) { request in
            guard case .deleteSessions(_, let ids) = request else { return false }
            return ids == ["chat-2"]
        }
        guard case .deleteSessions(let deleteID, let ids) = try XCTUnwrap(deleteRequest),
            ids == ["chat-2"]
        else { return XCTFail("Expected session delete") }
        model.gateway.handle(.accepted(requestID: deleteID))
        model.gateway.handle(.sessions(requestID: deleteID, sessions: []))
        let deletedDraftRemoved = await eventually {
            await store.loadComposerDraft(
                accountID: account.id,
                sessionID: "chat-2"
            ).isEmpty
        }
        XCTAssertTrue(deletedDraftRemoved)
        let deletedDraft = await store.loadComposerDraft(
            accountID: account.id,
            sessionID: "chat-2"
        )
        XCTAssertTrue(model.chat.composer.isEmpty)
        XCTAssertTrue(deletedDraft.isEmpty)
    }

    func testComposerDraftsAreDurableScopedAndBounded() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        let transcriptDirectory = root.appendingPathComponent("Transcripts", isDirectory: true)
        let draftDirectory = root.appendingPathComponent("Drafts", isDirectory: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: root)
        }
        let firstAccount = GatewayAccount(
            endpoint: try GatewayEndpoint("tcp://localhost:9191")
        )
        let secondAccount = GatewayAccount(
            endpoint: try GatewayEndpoint("tcp://localhost:9192")
        )
        var store = GatewayStore(
            defaults: defaults,
            transcriptDirectory: transcriptDirectory,
            draftDirectory: draftDirectory
        )
        await store.saveComposerDraft(
            ComposerDraft(text: "First account"),
            accountID: firstAccount.id,
            sessionID: "chat-1"
        )
        await store.saveComposerDraft(
            ComposerDraft(text: "Second account"),
            accountID: secondAccount.id,
            sessionID: "chat-1"
        )

        store = GatewayStore(
            defaults: defaults,
            transcriptDirectory: transcriptDirectory,
            draftDirectory: draftDirectory
        )
        let restoredFirst = await store.loadComposerDraft(
            accountID: firstAccount.id,
            sessionID: "chat-1"
        )
        let restoredSecond = await store.loadComposerDraft(
            accountID: secondAccount.id,
            sessionID: "chat-1"
        )
        XCTAssertEqual(restoredFirst, ComposerDraft(text: "First account"))
        XCTAssertEqual(restoredSecond, ComposerDraft(text: "Second account"))

        await store.saveComposerDraft(
            .empty,
            accountID: firstAccount.id,
            sessionID: "chat-1"
        )
        await store.saveComposerDraft(
            ComposerDraft(text: "Existing"),
            accountID: firstAccount.id,
            sessionID: "oversized"
        )
        await store.saveComposerDraft(
            ComposerDraft(text: String(repeating: "x", count: maximumComposerBytes + 1)),
            accountID: firstAccount.id,
            sessionID: "oversized"
        )
        let removedEmpty = await store.loadComposerDraft(
            accountID: firstAccount.id,
            sessionID: "chat-1"
        )
        let removedOversized = await store.loadComposerDraft(
            accountID: firstAccount.id,
            sessionID: "oversized"
        )
        XCTAssertEqual(removedEmpty, .empty)
        XCTAssertEqual(removedOversized, .empty)

        for recipients in [[""], ["bot-1", "bot-1"], [String(repeating: "x", count: 257)]] {
            await store.saveComposerDraft(
                ComposerDraft(text: "Invalid recipients", recipientBotIDs: recipients),
                accountID: firstAccount.id,
                sessionID: "invalid-recipients"
            )
            let invalid = await store.loadComposerDraft(
                accountID: firstAccount.id, sessionID: "invalid-recipients")
            XCTAssertEqual(invalid, .empty)
        }

        await store.saveComposerDraft(
            ComposerDraft(text: "Will corrupt"),
            accountID: firstAccount.id,
            sessionID: "corrupt"
        )
        let corruptFilename = Data("corrupt".utf8).base64EncodedString()
        let corruptURL =
            draftDirectory
            .appendingPathComponent(firstAccount.id.uuidString, isDirectory: true)
            .appendingPathComponent(corruptFilename)
            .appendingPathExtension("txt")
        try Data([0xFF]).write(to: corruptURL, options: .atomic)
        let corrupt = await store.loadComposerDraft(
            accountID: firstAccount.id,
            sessionID: "corrupt"
        )
        XCTAssertEqual(corrupt, .empty)
        XCTAssertFalse(FileManager.default.fileExists(atPath: corruptURL.path))

        await store.saveComposerDraft(
            ComposerDraft(text: "Remove with account"),
            accountID: firstAccount.id,
            sessionID: "chat-2"
        )
        try await store.remove(firstAccount)
        let removedAccountDraft = await store.loadComposerDraft(
            accountID: firstAccount.id,
            sessionID: "chat-2"
        )
        let preservedAccountDraft = await store.loadComposerDraft(
            accountID: secondAccount.id,
            sessionID: "chat-1"
        )
        XCTAssertEqual(removedAccountDraft, .empty)
        XCTAssertEqual(preservedAccountDraft, ComposerDraft(text: "Second account"))
    }

    func testUnavailableCachedCursorRetriesTheOpenWithoutIt() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: directory)
        }
        let recorder = GatewayRequestRecorder()
        let store = GatewayStore(defaults: defaults, transcriptDirectory: directory)
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        await store.saveTranscript(
            accountID: account.id,
            sessionID: "chat-1",
            sequence: 7,
            transcript: [
                TranscriptEntry(
                    id: "answer-1",
                    text: "Cached",
                    kind: .assistant,
                    format: "plain_text",
                    pending: false
                )
            ],
            currentUsage: TokenUsage(),
            lastUsage: TokenUsage()
        )
        let model = AppModel(
            client: GatewayClient(),
            store: store,
            requestSender: { request in await recorder.record(request) }
        )
        model.bots = [bot()]
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready

        model.chat.openSession("chat-1")
        let firstRequest = await recorder.firstRequest(after: 0) { request in
            guard case .openSession(_, "chat-1", 7) = request else {
                return false
            }
            return true
        }
        let first = try XCTUnwrap(firstRequest)
        guard case .openSession(let requestID, _, 7) = first else {
            return XCTFail("Expected the cached cursor")
        }
        let requestCount = await recorder.requestCount()
        model.gateway.handle(
            .rejected(
                GatewayRejection(
                    requestId: requestID,
                    code: "replay_unavailable",
                    message: "Reload",
                    fatal: false
                )))
        let retryRequest = await recorder.firstRequest(after: requestCount) { request in
            guard case .openSession(_, "chat-1", nil) = request else { return false }
            return true
        }
        _ = try XCTUnwrap(retryRequest)

        let opens = await recorder.requests().compactMap { request -> (String, UInt64?)? in
            guard case .openSession(_, let sessionID, let cursor) = request else { return nil }
            return (sessionID, cursor)
        }
        XCTAssertEqual(opens.count, 2)
        XCTAssertEqual(opens.last?.0, "chat-1")
        XCTAssertNil(opens.last?.1)
        let removed = await eventually {
            await store.loadTranscript(accountID: account.id, sessionID: "chat-1") == nil
        }
        XCTAssertTrue(removed)
    }

}
