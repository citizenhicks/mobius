import Foundation
import Observation
import XCTest

@MainActor
extension AppModelTests {
    func testTranscriptStartsWithABoundedTurnTail() throws {
        let model = try model()
        model.transcript = (0..<2).map { index in
            TranscriptEntry(
                id: "entry-\(index)",
                text: "\(index)",
                kind: .assistant,
                format: "plain_text",
                pending: false,
                turnID: "turn-\(index)",
                startsTurn: true
            )
        }
        model.connectionState = .ready

        XCTAssertEqual(model.displayedTranscript.count, 1)
        XCTAssertEqual(model.displayedTranscript.first?.text, "1")
        XCTAssertEqual(TranscriptProjection.turnCount(in: model.displayedTranscript), 1)
        XCTAssertTrue(model.hasEarlierHistory)

        model.loadEarlierHistory()

        XCTAssertEqual(model.displayedTranscript.count, 2)
        XCTAssertFalse(model.hasEarlierHistory)
    }

    func testTranscriptPaginationKeepsCompletedTurnsWhole() throws {
        let model = try model()
        model.transcript = (0..<2).flatMap { index in
            let turnID = "turn-\(index)"
            return [
                TranscriptEntry(
                    id: "user-\(index)",
                    text: "Question \(index)",
                    kind: .user,
                    format: "plain_text",
                    pending: false,
                    turnID: turnID,
                    startsTurn: true
                ),
                TranscriptEntry(
                    id: "work-\(index)",
                    text: "Work \(index)",
                    kind: .commentary,
                    format: "plain_text",
                    pending: false,
                    turnID: turnID
                ),
                TranscriptEntry(
                    id: "final-\(index)",
                    text: "Answer \(index)",
                    kind: .assistant,
                    format: "plain_text",
                    pending: false,
                    turnID: turnID,
                    turnTerminal: true
                ),
            ]
        }
        model.connectionState = .ready

        XCTAssertEqual(model.displayedTranscript.count, 3)
        XCTAssertEqual(model.displayedTranscript.first?.id, "user-1")
        XCTAssertEqual(model.displayedTranscript.prefix(3).map(\.turnID), [
            "turn-1", "turn-1", "turn-1",
        ])
        XCTAssertEqual(
            model.transcriptProjection(breakBefore: nil).rows.prefix(3).map(\.kind),
            [.user, .workedGroup, .narrative]
        )
        XCTAssertTrue(model.hasEarlierHistory)

        model.loadEarlierHistory()

        XCTAssertEqual(model.displayedTranscript.count, 6)
        XCTAssertEqual(model.displayedTranscript.first?.id, "user-0")
        XCTAssertFalse(model.hasEarlierHistory)
    }

    func testLiveGrowthKeepsTheVisibleTranscriptStartStable() throws {
        let model = try model()
        model.transcript = [TranscriptEntry(
            id: "entry-0",
            text: "0",
            kind: .event,
            format: "plain_text",
            pending: false,
            turnID: "turn-0",
            startsTurn: true
        )]
        model.activeTurnID = "turn-0"
        let before = model.transcriptProjection(breakBefore: nil)

        model.reduce(record: RecordedEvent(
            sequence: 1,
            recordedAtMs: 1_000,
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("tool_call_begin")
            ])),
            streamMetrics: [],
            blocks: (0..<2).map { index in
                RenderedBlock(capability: "tools", block: FrontendBlock(
                    id: "tail-event-\(index)",
                    group: nil,
                    update: .replace,
                    state: .pending,
                    role: .tool,
                    title: "Run command",
                    text: "Arguments",
                    symbol: nil,
                    format: "plain_text",
                    tone: "neutral",
                    files: []
                ))
            },
            preview: nil
        ))
        let after = model.transcriptProjection(breakBefore: nil)

        XCTAssertEqual(model.displayedTranscript.count, 3)
        XCTAssertEqual(model.displayedTranscript.first?.presentationID, "entry-0")
        XCTAssertEqual(after.rows.last?.id, before.rows.last?.id)
        XCTAssertEqual(after.structuralRevision, before.structuralRevision)
    }

    func testWarmTranscriptWindowPublishesStructuralGrowth() async throws {
        let model = try model()
        model.transcript = [TranscriptEntry(
            id: "first",
            text: "First",
            kind: .assistant,
            format: "plain_text",
            pending: false,
            turnID: "turn-1"
        )]
        XCTAssertEqual(model.displayedTranscript.count, 1)

        let changed = expectation(description: "Observed the appended transcript row")
        withObservationTracking {
            _ = model.displayedTranscript.count
        } onChange: {
            changed.fulfill()
        }
        model.transcript.append(TranscriptEntry(
            id: "second",
            text: "Second",
            kind: .commentary,
            format: "plain_text",
            pending: false,
            turnID: "turn-1"
        ))

        await fulfillment(of: [changed], timeout: 1)
        XCTAssertEqual(model.displayedTranscript.count, 2)
    }

    func testStructuralTranscriptGrowthInvalidatesThePinnedWindow() throws {
        let model = try model()
        let original = (0..<2).map { index in
            TranscriptEntry(
                id: "entry-\(index)",
                text: "\(index)",
                kind: .assistant,
                format: "plain_text",
                pending: false,
                turnID: "turn-\(index)",
                startsTurn: true
            )
        }
        model.transcript = original
        XCTAssertEqual(model.displayedTranscript.first?.id, "entry-1")
        model.activeTurnID = "turn-1"

        model.reduce(record: RecordedEvent(
            sequence: 1,
            recordedAtMs: 100,
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("web_search_begin")
            ])),
            streamMetrics: [],
            blocks: [],
            preview: nil
        ))

        var rewritten = model.transcript
        rewritten[1] = TranscriptEntry(
            id: "replacement-1",
            text: "Replacement",
            kind: .assistant,
            format: "plain_text",
            pending: false,
            turnID: "turn-1",
            startsTurn: true
        )
        rewritten.append(TranscriptEntry(
            id: "appended",
            text: "Appended",
            kind: .assistant,
            format: "plain_text",
            pending: false,
            turnID: "turn-1"
        ))
        model.transcript = rewritten

        XCTAssertTrue(model.displayedTranscript.contains { $0.id == "replacement-1" })
        XCTAssertFalse(model.displayedTranscript.contains { $0.id == "entry-1" })
        XCTAssertEqual(model.displayedTranscript.last?.id, "appended")
    }

    func testCachedTranscriptSuppliesTheOpenCursorAndRestoresOnce() async throws {
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
        var currentUsage = TokenUsage()
        currentUsage.totalTokens = 55
        var lastUsage = TokenUsage()
        lastUsage.inputTokens = 30
        lastUsage.outputTokens = 12
        await store.saveTranscript(
            accountID: account.id,
            sessionID: "chat-1",
            sequence: 7,
            nextBeforeSequence: 40,
            transcript: [TranscriptEntry(
                id: "answer-1",
                text: "Already rendered",
                kind: .assistant,
                format: "plain_text",
                pending: false,
                turnID: "turn-cached",
                startsTurn: true,
                turnTerminal: true,
                turnElapsedMs: 1_250
            )],
            currentUsage: currentUsage,
            lastUsage: lastUsage
        )
        let model = AppModel(
            client: GatewayClient(),
            store: store,
            requestSender: { request in await recorder.record(request) }
        )
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready

        model.openSession("chat-1")
        try await Task.sleep(for: .milliseconds(20))
        let requests = await recorder.requests()
        let request = try XCTUnwrap(requests.first)
        guard case .openSession(let requestID, let sessionID, let cursor) = request else {
            return XCTFail("Expected a cached session open")
        }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(cursor, 7)
        XCTAssertFalse(model.isLoadingTranscript)
        XCTAssertEqual(model.displayedTranscript.map(\.text), ["Already rendered"])

        model.handle(.sessionOpened(requestID: requestID, payload: sessionReady(latestSequence: 7)))
        XCTAssertEqual(model.transcript.map(\.text), ["Already rendered"])
        XCTAssertEqual(model.transcript.first?.turnID, "turn-cached")
        XCTAssertEqual(model.transcript.first?.startsTurn, true)
        XCTAssertEqual(model.transcript.first?.turnTerminal, true)
        XCTAssertEqual(model.transcript.first?.turnElapsedMs, 1_250)
        XCTAssertFalse(model.isLoadingTranscript)
        XCTAssertEqual(model.currentUsage.totalTokens, 55)
        XCTAssertEqual(model.contextTokens, 42)
        XCTAssertTrue(model.hasEarlierHistory)
        model.handle(.sessionReplayComplete(requestID: requestID, sessionID: "chat-1"))

        model.handle(.agentEvent(
            sessionID: "chat-1",
            sequence: 7,
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("error"),
                "message": .string("Duplicate")
            ])),
            blocks: [],
            history: nil,
            preview: nil
        ))
        XCTAssertEqual(model.transcript.map(\.text), ["Already rendered"])
    }

    func testLegacyTranscriptCacheIsRejectedBeforeReplay() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: directory)
        }
        let store = GatewayStore(defaults: defaults, transcriptDirectory: directory)
        let accountID = UUID()
        await store.saveTranscript(
            accountID: accountID,
            sessionID: "chat-legacy",
            sequence: 7,
            transcript: [TranscriptEntry(
                id: "answer-1",
                text: "Already rendered",
                kind: .assistant,
                format: "plain_text",
                pending: false,
                turnID: "turn-legacy",
                turnTerminal: true
            )],
            currentUsage: TokenUsage(),
            lastUsage: TokenUsage()
        )

        let accountDirectory = directory.appendingPathComponent(
            accountID.uuidString,
            isDirectory: true
        )
        let cacheURL = try XCTUnwrap(
            FileManager.default.contentsOfDirectory(
                at: accountDirectory,
                includingPropertiesForKeys: nil
            ).first
        )
        var legacy = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(contentsOf: cacheURL))
                as? [String: Any]
        )
        legacy.removeValue(forKey: "schemaVersion")
        try JSONSerialization.data(withJSONObject: legacy).write(to: cacheURL, options: .atomic)

        let restored = await store.loadTranscript(
            accountID: accountID,
            sessionID: "chat-legacy"
        )
        XCTAssertNil(restored)
        XCTAssertFalse(FileManager.default.fileExists(atPath: cacheURL.path))
    }

    func testReconnectKeepsTheRetainedTranscriptVisibleWhileReplayLoads() throws {
        let model = try model()
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.transcript = [TranscriptEntry(
            id: "answer-1",
            text: "Already rendered",
            kind: .assistant,
            format: "plain_text",
            pending: false
        )]

        model.restoreSession("chat-1")

        XCTAssertEqual(model.connectionState, .loading)
        XCTAssertFalse(model.isLoadingTranscript)
        XCTAssertEqual(model.displayedTranscript.map(\.text), ["Already rendered"])
    }

    func testOpeningAnotherSessionStillHidesThePreviousTranscript() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.transcript = [TranscriptEntry(
            id: "answer-1",
            text: "Previous chat",
            kind: .assistant,
            format: "plain_text",
            pending: false
        )]

        let requestCount = await recorder.requestCount()
        model.openSession("chat-2")
        let request = await recorder.firstRequest(after: requestCount) { request in
            guard case .openSession(_, "chat-2", _) = request else { return false }
            return true
        }

        XCTAssertNotNil(request)
        XCTAssertTrue(model.isLoadingTranscript)
        XCTAssertEqual(model.displayedTranscript.map(\.text), ["Previous chat"])
    }

    func testCanonicalReplayCompletesAfterTheFrozenCachedTranscript() async throws {
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
                id: "model-stream:8:answer-1final_answer",
                text: "Cached",
                kind: .assistant,
                format: "plain_text",
                pending: false,
                modelStepID: "answer-1",
                turnID: "turn-1"
            )],
            currentUsage: TokenUsage(),
            lastUsage: TokenUsage()
        )
        let model = AppModel(
            client: GatewayClient(),
            store: store,
            requestSender: { request in await recorder.record(request) }
        )
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready

        model.openSession("chat-1")
        try await Task.sleep(for: .milliseconds(20))
        let requests = await recorder.requests()
        guard case .openSession(let requestID, _, _) = try XCTUnwrap(requests.first) else {
            return XCTFail("Expected a session open")
        }
        model.handle(.sessionOpened(
            requestID: requestID,
            payload: sessionReady(latestSequence: 9)
        ))
        model.handle(.agentEvent(
            sessionID: "chat-1",
            sequence: 8,
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("assistant_content_delta"),
                "sessionId": .string("chat-1"),
                "turnId": .string("turn-1"),
                "modelStepId": .string("answer-2"),
                "phase": .string("final_answer"),
                "delta": .string(" updated")
            ])),
            blocks: [],
            history: nil,
            preview: nil
        ))
        XCTAssertEqual(model.displayedTranscript.map(\.text), ["Cached"])
        model.handle(.agentEvent(
            sessionID: "chat-1",
            sequence: 9,
            event: AgentEventRecord(
                submissionId: nil,
                msg: testAssistantMessage(
                    turnID: "turn-1",
                    modelStepID: "answer-2",
                    text: "Canonical"
                )
            ),
            blocks: [],
            history: nil,
            preview: nil
        ))
        XCTAssertEqual(model.displayedTranscript.map(\.text), ["Cached"])

        model.handle(.sessionReplayComplete(requestID: requestID, sessionID: "chat-1"))

        XCTAssertEqual(model.displayedTranscript.map(\.text), ["Cached", "Canonical"])
    }

    func testCursorRestoreKeepsTheReplayBaseAndFrozenPresentation() async throws {
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
                id: "model-stream:8:answer-1final_answer",
                text: "Cached",
                kind: .assistant,
                format: "plain_text",
                pending: false,
                modelStepID: "answer-1"
            )],
            currentUsage: TokenUsage(),
            lastUsage: TokenUsage()
        )
        let model = AppModel(
            client: GatewayClient(),
            store: store,
            requestSender: { request in await recorder.record(request) }
        )
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready

        model.openSession("chat-1")
        try await Task.sleep(for: .milliseconds(20))
        var requests = await recorder.requests()
        guard case .openSession(let firstRequestID, _, _) = try XCTUnwrap(requests.first) else {
            return XCTFail("Expected the first session open")
        }
        model.handle(.sessionOpened(
            requestID: firstRequestID,
            payload: sessionReady(latestSequence: 9)
        ))
        model.handle(.agentEvent(
            sessionID: "chat-1",
            sequence: 8,
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("assistant_content_delta"),
                "sessionId": .string("chat-1"),
                "turnId": .string("turn-1"),
                "modelStepId": .string("answer-1"),
                "phase": .string("final_answer"),
                "delta": .string(" updated")
            ])),
            blocks: [],
            history: nil,
            preview: nil
        ))

        model.restoreSession("chat-1")
        try await Task.sleep(for: .milliseconds(20))
        requests = await recorder.requests()
        guard case .openSession(let secondRequestID, _, 8) = try XCTUnwrap(
            requests.last
        ) else { return XCTFail("Expected the replay cursor to resume at sequence 8") }
        model.handle(.sessionOpened(
            requestID: secondRequestID,
            payload: sessionReady(latestSequence: 9)
        ))
        XCTAssertEqual(model.displayedTranscript.map(\.text), ["Cached"])
        model.handle(.agentEvent(
            sessionID: "chat-1",
            sequence: 9,
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("assistant_content_delta"),
                "sessionId": .string("chat-1"),
                "turnId": .string("turn-1"),
                "modelStepId": .string("answer-1"),
                "phase": .string("final_answer"),
                "delta": .string(" again")
            ])),
            blocks: [],
            history: nil,
            preview: nil
        ))
        model.handle(.sessionReplayComplete(
            requestID: secondRequestID,
            sessionID: "chat-1"
        ))

        XCTAssertEqual(model.displayedTranscript.map(\.text), ["Cached updated again"])
    }

    func testTranscriptCacheIsProtectedAndEvictsByRecency() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: directory)
        }
        let store = GatewayStore(defaults: defaults, transcriptDirectory: directory)
        let accountID = UUID()
        for index in 0..<20 {
            await store.saveTranscript(
                accountID: accountID,
                sessionID: "chat-\(index)",
                sequence: UInt64(index),
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
        }
        let accountDirectory = directory.appendingPathComponent(
            accountID.uuidString,
            isDirectory: true
        )
        for file in try FileManager.default.contentsOfDirectory(
            at: accountDirectory,
            includingPropertiesForKeys: nil
        ) {
            let cached = try JSONDecoder().decode(
                CachedTranscript.self,
                from: Data(contentsOf: file)
            )
            let date = cached.sequence == 0
                ? Date().addingTimeInterval(3600)
                : Date(timeIntervalSinceReferenceDate: TimeInterval(cached.sequence))
            try FileManager.default.setAttributes(
                [.modificationDate: date],
                ofItemAtPath: file.path
            )
        }
        await store.saveTranscript(
            accountID: accountID,
            sessionID: "chat-20",
            sequence: 20,
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
        let files = try FileManager.default.contentsOfDirectory(
            at: accountDirectory,
            includingPropertiesForKeys: nil
        )
        XCTAssertEqual(files.count, 20)
        let newestOldCache = await store.loadTranscript(
            accountID: accountID,
            sessionID: "chat-0"
        )
        let oldestCache = await store.loadTranscript(
            accountID: accountID,
            sessionID: "chat-1"
        )
        let newCache = await store.loadTranscript(
            accountID: accountID,
            sessionID: "chat-20"
        )
        XCTAssertNotNil(newestOldCache)
        XCTAssertNil(oldestCache)
        XCTAssertNotNil(newCache)
        #if !targetEnvironment(simulator)
        let attributes = try FileManager.default.attributesOfItem(atPath: XCTUnwrap(files.first).path)
        XCTAssertEqual(attributes[.protectionKey] as? FileProtectionType, .complete)
        #endif

        let oversizedAccountID = UUID()
        await store.saveTranscript(
            accountID: oversizedAccountID,
            sessionID: "large",
            sequence: 1,
            transcript: [TranscriptEntry(
                id: "answer-1",
                text: String(repeating: "x", count: 3 * 1024 * 1024 + 1),
                kind: .assistant,
                format: "plain_text",
                pending: false
            )],
            currentUsage: TokenUsage(),
            lastUsage: TokenUsage()
        )
        let oversized = await store.loadTranscript(
            accountID: oversizedAccountID,
            sessionID: "large"
        )
        XCTAssertNil(oversized)

        await store.saveTranscript(
            accountID: oversizedAccountID,
            sessionID: "corrupt",
            sequence: 1,
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
        let oversizedDirectory = directory.appendingPathComponent(
            oversizedAccountID.uuidString,
            isDirectory: true
        )
        let oversizedURL = try XCTUnwrap(
            FileManager.default.contentsOfDirectory(
                at: oversizedDirectory,
                includingPropertiesForKeys: nil
            ).first
        )
        try Data(count: 4 * 1024 * 1024 + 1).write(to: oversizedURL, options: .atomic)
        let corrupt = await store.loadTranscript(
            accountID: oversizedAccountID,
            sessionID: "corrupt"
        )
        XCTAssertNil(corrupt)
        XCTAssertFalse(FileManager.default.fileExists(atPath: oversizedURL.path))
    }


    func testTranscriptReplayDoesNotShowStaleErrorToast() throws {
        let model = try model()
        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("error"),
                "message": .string("Old error")
            ])),
            blocks: [],
            preview: nil
        )

        XCTAssertNil(model.toast)
        XCTAssertTrue(model.transcript.isEmpty)
    }

}
