import Foundation
import XCTest

@MainActor
extension AppModelTests {
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
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready

        var requestCount = await recorder.requestCount()
        model.openSession("chat-1")
        let firstRequest = await recorder.firstRequest(after: requestCount) { request in
            guard case .openSession(_, "chat-1", _) = request else { return false }
            return true
        }
        guard case .openSession(let firstID, "chat-1", _) = try XCTUnwrap(firstRequest)
        else { return XCTFail("Expected first session open") }
        model.composer = "Typed while opening"
        model.handle(.sessionOpened(
            requestID: firstID,
            payload: sessionReady(latestSequence: 1, sessionID: "chat-1")
        ))
        model.handle(.sessionReplayComplete(requestID: firstID, sessionID: "chat-1"))
        let firstSessionReady = await eventually { model.canCreateSession }
        XCTAssertTrue(firstSessionReady)
        XCTAssertEqual(model.composer, "Typed while opening")
        model.composer = "Draft one"
        model.composerReply = firstReply

        requestCount = await recorder.requestCount()
        model.openSession("chat-2")
        let secondRequest = await recorder.firstRequest(after: requestCount) { request in
            guard case .openSession(_, "chat-2", _) = request else { return false }
            return true
        }
        let secondOpen = try XCTUnwrap(secondRequest)
        guard case .openSession(let secondID, _, _) = secondOpen else {
            return XCTFail("Expected second session open")
        }
        model.handle(.sessionOpened(
            requestID: secondID,
            payload: sessionReady(latestSequence: 1, sessionID: "chat-2")
        ))
        model.handle(.sessionReplayComplete(requestID: secondID, sessionID: "chat-2"))
        let secondSessionReady = await eventually {
            model.canCreateSession
                && model.composer == "Draft two"
                && model.composerReply == secondReply
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
        XCTAssertEqual(model.composer, "Draft two")
        XCTAssertEqual(model.composerReply, secondReply)

        requestCount = await recorder.requestCount()
        model.sendMessage()
        model.composer = "Next draft"
        let submitRequest = await recorder.firstRequest(after: requestCount) { request in
            if case .submit = request { return true }
            return false
        }
        let submittedDraft = await store.loadComposerDraft(
            accountID: account.id,
            sessionID: "chat-2"
        )
        XCTAssertEqual(submittedDraft, ComposerDraft(text: "Draft two", reply: secondReply))
        guard case .submit(_, let submission) = try XCTUnwrap(submitRequest) else {
            return XCTFail("Expected submitted draft")
        }
        model.handle(.accepted(requestID: submission.id))
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
        model.handle(.accepted(requestID: deleteID))
        model.handle(.sessions(requestID: deleteID, sessions: []))
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
        XCTAssertTrue(model.composer.isEmpty)
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

        await store.saveComposerDraft(
            ComposerDraft(text: "Will corrupt"),
            accountID: firstAccount.id,
            sessionID: "corrupt"
        )
        let corruptFilename = Data("corrupt".utf8).base64EncodedString()
        let corruptURL = draftDirectory
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
            transcript: [TranscriptEntry(
                id: "answer-1",
                text: "Cached",
                kind: .assistant,
                format: "plain_text",
                pending: false
            )],
            currentUsage: TokenUsage(),
            lastUsage: TokenUsage()
        )
        let model = AppModel(
            client: GatewayClient(),
            store: store,
            requestSender: { request in await recorder.record(request) }
        )
        model.bots = [bot()]
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready

        model.openSession("chat-1")
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
        model.handle(.rejected(GatewayRejection(
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
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: .seconds(1))
        var cached = await store.loadTranscript(accountID: account.id, sessionID: "chat-1")
        while cached != nil, clock.now < deadline {
            try await Task.sleep(for: .milliseconds(5))
            cached = await store.loadTranscript(accountID: account.id, sessionID: "chat-1")
        }
        XCTAssertNil(cached)
    }

}
