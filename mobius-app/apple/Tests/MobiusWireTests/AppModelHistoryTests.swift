import Foundation
import XCTest

@MainActor
extension AppModelTests {
    func testHistoricalReplayAppearsOnlyWhenTheSnapshotIsComplete() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready

        let openRequestCount = await recorder.requestCount()
        model.openSession("chat-1")
        let openRequest = await recorder.firstRequest(after: openRequestCount) {
            guard case .openSession(_, "chat-1", nil) = $0 else { return false }
            return true
        }
        let request = try XCTUnwrap(openRequest)
        guard case .openSession(let requestID, _, nil) = request else {
            return XCTFail("Expected an uncached session open")
        }
        XCTAssertTrue(model.isLoadingTranscript)
        model.handle(.sessionOpened(requestID: requestID, payload: sessionReady(latestSequence: 2)))
        XCTAssertTrue(model.isLoadingTranscript)
        model.showFiles(.unstaged)
        model.handle(.agentEvent(
            sessionID: "chat-1",
            sequence: 1,
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("assistant_content_delta"),
                "sessionId": .string("chat-1"),
                "turnId": .string("turn-1"),
                "modelStepId": .string("answer-1"),
                "phase": .string("final_answer"),
                "delta": .string("Hel")
            ])),
            blocks: [],
            history: nil,
            preview: nil
        ))
        XCTAssertTrue(model.displayedTranscript.isEmpty)

        model.handle(.agentEvent(
            sessionID: "chat-1",
            sequence: 2,
            event: AgentEventRecord(
                submissionId: nil,
                msg: testAssistantMessage(
                    turnID: "turn-1",
                    modelStepID: "answer-1",
                    text: "Hello"
                )
            ),
            blocks: [],
            history: nil,
            preview: nil
        ))
        XCTAssertEqual(model.transcript.map(\.text), ["Hello"])
        XCTAssertTrue(model.displayedTranscript.isEmpty)
        let refreshRequestCount = await recorder.requestCount()
        model.handle(.sessionReplayComplete(requestID: requestID, sessionID: "chat-1"))
        XCTAssertFalse(model.isLoadingTranscript)
        XCTAssertEqual(model.displayedTranscript.map(\.text), ["Hello"])
        let gitDiffRequest = await recorder.firstRequest(after: refreshRequestCount) { request in
            guard case .getGitDiff(_, "chat-1", .unstaged) = request else { return false }
            return true
        }
        let workspaceFilesRequest = await recorder.firstRequest(after: refreshRequestCount) { request in
            guard case .listWorkspaceFiles(_, "chat-1", .all) = request else { return false }
            return true
        }
        XCTAssertNotNil(gitDiffRequest)
        XCTAssertNotNil(workspaceFilesRequest)
    }

    func testEarlierHistoryUsesTheReadyCursorAndPrependsOnlyTranscriptState() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready

        let openRequestCount = await recorder.requestCount()
        model.openSession("chat-1")
        let openRequest = await recorder.firstRequest(after: openRequestCount) { request in
            guard case .openSession(_, "chat-1", _) = request else { return false }
            return true
        }
        guard case .openSession(let openID, _, _) = try XCTUnwrap(openRequest)
        else { return XCTFail("Expected session open") }
        model.handle(.sessionOpened(
            requestID: openID,
            payload: sessionReady(latestSequence: 8, nextBeforeSequence: 40)
        ))
        model.handle(.agentEvent(
            sessionID: "chat-1",
            sequence: 8,
            event: AgentEventRecord(
                submissionId: nil,
                msg: testAssistantMessage(
                    turnID: "turn-live",
                    modelStepID: "step-current",
                    text: "Current"
                )
            ),
            blocks: [],
            history: nil,
            preview: nil
        ))
        model.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))
        model.selectedModelRoute = "current-route"

        let initialRequests = await recorder.requests()
        let historyRequestCount = initialRequests.filter {
            if case .getSessionHistory = $0 { return true }
            return false
        }.count
        model.connectionState = .disconnected
        model.loadEarlierHistory()
        let disconnectedRequests = await recorder.requests()
        XCTAssertEqual(
            disconnectedRequests.filter {
                if case .getSessionHistory = $0 { return true }
                return false
            }.count,
            historyRequestCount
        )

        model.connectionState = .ready
        model.activeTurnID = "turn-live"
        XCTAssertTrue(model.canLoadEarlierHistory)
        let readyRequestCount = await recorder.requestCount()
        model.loadEarlierHistory()
        model.loadEarlierHistory()
        let historyRequest = await recorder.firstRequest(after: readyRequestCount) { request in
            if case .getSessionHistory = request { return true }
            return false
        }
        let requests = await recorder.requests()
        XCTAssertEqual(
            requests.filter {
                if case .getSessionHistory = $0 { return true }
                return false
            }.count,
            1
        )
        guard case .getSessionHistory(
            let historyID,
            "chat-1",
            40
        ) = try XCTUnwrap(historyRequest) else {
            return XCTFail("Expected paged history request")
        }

        model.handle(.agentEvent(
            sessionID: "chat-1",
            record: recorded(9, testAssistantMessage(
                turnID: "turn-live",
                modelStepID: "step-live",
                phase: "commentary",
                text: "Still working"
            ))
        ))

        let events = [
            RenderedEventRecord(
                event: testMessageEvent(text: "Oldest question"),
                blocks: []
            ),
            RenderedEventRecord(
                event: testAssistantMessage(
                    turnID: "turn-oldest",
                    modelStepID: "step-oldest",
                    text: "Oldest answer"
                ),
                blocks: []
            ),
            RenderedEventRecord(event: .object([
                "type": .string("turn_started"),
                "turnId": .string("turn-older")
            ]), blocks: []),
            RenderedEventRecord(
                event: testMessageEvent(text: "Older question"),
                blocks: []
            ),
            RenderedEventRecord(event: .object([
                "type": .string("model_changed"),
                "route": .string("historical-route")
            ]), blocks: []),
            RenderedEventRecord(
                event: testAssistantMessage(
                    turnID: "turn-older",
                    modelStepID: "step-older-commentary",
                    phase: "commentary",
                    text: "Earlier update"
                ),
                blocks: []
            ),
            RenderedEventRecord(
                event: testAssistantMessage(
                    turnID: "turn-older",
                    modelStepID: "step-older-final",
                    text: "Older answer"
                ),
                blocks: []
            ),
        ]
        let records = events.enumerated().map { index, rendered in
            RecordedEvent(
                sequence: UInt64(index + 1),
                recordedAtMs: Int64(1_000 + index),
                event: AgentEventRecord(submissionId: nil, msg: rendered.event),
                streamMetrics: [],
                blocks: rendered.blocks,
                preview: nil
            )
        }
        model.handle(.sessionHistory(
            requestID: "stale",
            sessionID: "chat-1",
            records: records,
            nextBeforeSequence: nil
        ))
        XCTAssertEqual(model.displayedTranscript.map(\.text), ["Current", "Still working"])

        model.handle(.sessionHistory(
            requestID: historyID,
            sessionID: "chat-1",
            records: Array(records.dropFirst(2)),
            nextBeforeSequence: 3
        ))

        XCTAssertEqual(
            model.displayedTranscript.map(\.text),
            ["Older question", "Earlier update", "Older answer", "Current", "Still working"]
        )
        XCTAssertEqual(
            model.displayedTranscript.map(\.kind),
            [.user, .commentary, .assistant, .assistant, .commentary]
        )
        XCTAssertEqual(model.selectedModelRoute, "current-route")
        XCTAssertTrue(model.hasEarlierHistory)

        let olderRequestCount = await recorder.requestCount()
        model.loadEarlierHistory()
        let olderRequest = await recorder.firstRequest(after: olderRequestCount) { request in
            if case .getSessionHistory = request { return true }
            return false
        }
        guard case .getSessionHistory(
            let olderHistoryID,
            "chat-1",
            3
        ) = try XCTUnwrap(olderRequest) else {
            return XCTFail("Expected the next history page")
        }
        model.handle(.sessionHistory(
            requestID: olderHistoryID,
            sessionID: "chat-1",
            records: Array(records.prefix(2)),
            nextBeforeSequence: nil
        ))
        XCTAssertEqual(
            model.displayedTranscript.map(\.text),
            [
                "Oldest question",
                "Oldest answer",
                "Older question",
                "Earlier update",
                "Older answer",
                "Current",
                "Still working",
            ]
        )
        XCTAssertFalse(model.hasEarlierHistory)

        model.handle(.agentEvent(
            sessionID: "chat-1",
            record: recorded(10, testAssistantMessage(
                turnID: "turn-live",
                modelStepID: "step-more",
                phase: "commentary",
                text: "More work"
            ))
        ))
        XCTAssertEqual(
            model.displayedTranscript.map(\.text),
            [
                "Oldest question",
                "Oldest answer",
                "Older question",
                "Earlier update",
                "Older answer",
                "Current",
                "Still working",
                "More work",
            ]
        )

        model.restoreSession("chat-1")
        try await Task.sleep(for: .milliseconds(30))
        let reconnectRequests = await recorder.requests()
        guard case .openSession(let reconnectID, _, _) = try XCTUnwrap(
            reconnectRequests.last
        ) else { return XCTFail("Expected reconnect session open") }
        model.handle(.sessionOpened(
            requestID: reconnectID,
            payload: sessionReady(latestSequence: 8, nextBeforeSequence: 40)
        ))
        model.handle(.sessionReplayComplete(requestID: reconnectID, sessionID: "chat-1"))
        XCTAssertEqual(
            model.displayedTranscript.map(\.text),
            ["Older question", "Earlier update", "Older answer", "Current", "Still working", "More work"]
        )
        XCTAssertTrue(model.hasEarlierHistory)
    }

    func testHistoryMergeDoesNotReplayABufferedDeltaTwice() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready

        model.openSession("chat-1")
        let openRequest = await recorder.firstRequest(after: 0) {
            guard case .openSession(_, "chat-1", _) = $0 else { return false }
            return true
        }
        guard case .openSession(let openID, _, _) = try XCTUnwrap(openRequest) else {
            return XCTFail("Expected session open")
        }
        model.handle(.sessionOpened(
            requestID: openID,
            payload: sessionReady(latestSequence: 8, nextBeforeSequence: 40)
        ))
        model.handle(.agentEvent(
            sessionID: "chat-1",
            record: recorded(8, testAssistantMessage(
                turnID: "turn-current",
                modelStepID: "step-current",
                text: "Current"
            ))
        ))
        model.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))
        model.activeTurnID = "turn-live"

        let requestCount = await recorder.requestCount()
        model.loadEarlierHistory()
        let historyRequest = await recorder.firstRequest(after: requestCount) {
            guard case .getSessionHistory = $0 else { return false }
            return true
        }
        guard case .getSessionHistory(let historyID, _, _) = try XCTUnwrap(historyRequest)
        else { return XCTFail("Expected history request") }

        model.handle(.agentEvent(
            sessionID: "chat-1",
            record: recorded(9, .object([
                "type": .string("assistant_content_delta"),
                "sessionId": .string("chat-1"),
                "turnId": .string("turn-live"),
                "modelStepId": .string("step-live"),
                "phase": .string("commentary"),
                "delta": .string("Still working"),
            ]))
        ))
        XCTAssertEqual(model.transcript.map(\.text), ["Current"])

        model.handle(.sessionHistory(
            requestID: historyID,
            sessionID: "chat-1",
            records: [recorded(1, testMessageEvent(text: "Older question"))],
            nextBeforeSequence: nil
        ))
        try await Task.sleep(for: .milliseconds(80))

        XCTAssertEqual(
            model.transcript.map(\.text),
            ["Older question", "Current", "Still working"]
        )
    }

    func testHistoryPagesReconnectTurnMetadataAcrossAPageBoundary() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready

        model.openSession("chat-1")
        let openRequest = await recorder.firstRequest(after: 0) {
            guard case .openSession(_, "chat-1", _) = $0 else { return false }
            return true
        }
        guard case .openSession(let openID, _, _) = try XCTUnwrap(openRequest) else {
            return XCTFail("Expected session open")
        }
        model.handle(.sessionOpened(
            requestID: openID,
            payload: sessionReady(latestSequence: 8, nextBeforeSequence: 9)
        ))
        model.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))

        model.loadEarlierHistory()
        let firstRequest = await recorder.firstRequest(after: 1) {
            guard case .getSessionHistory = $0 else { return false }
            return true
        }
        guard case .getSessionHistory(let firstID, _, 9) = try XCTUnwrap(firstRequest) else {
            return XCTFail("Expected first history page")
        }
        let turnID = "turn-1"
        model.handle(.sessionHistory(
            requestID: firstID,
            sessionID: "chat-1",
            records: [
                recorded(5, testMessageEvent(
                    delivery: .steer,
                    text: "Use the smaller patch"
                )),
                recorded(6, testAssistantMessage(
                    turnID: turnID,
                    modelStepID: "step-2",
                    phase: "commentary",
                    text: "After steering"
                )),
                recorded(7, testAssistantMessage(
                    turnID: turnID,
                    modelStepID: "step-3",
                    text: "Done"
                )),
                recorded(8, .object([
                    "type": .string("turn_complete"),
                    "turnId": .string(turnID),
                ])),
            ],
            nextBeforeSequence: 5
        ))

        XCTAssertEqual(model.transcript.map(\.turnID), Array(repeating: turnID, count: 3))
        XCTAssertEqual(
            model.transcriptProjection(breakBefore: nil).rows.map(\.kind),
            [.workedGroup, .narrative]
        )

        let requestCount = await recorder.requestCount()
        model.loadEarlierHistory()
        let secondRequest = await recorder.firstRequest(after: requestCount) {
            guard case .getSessionHistory = $0 else { return false }
            return true
        }
        guard case .getSessionHistory(let secondID, _, 5) = try XCTUnwrap(secondRequest) else {
            return XCTFail("Expected second history page")
        }
        model.handle(.sessionHistory(
            requestID: secondID,
            sessionID: "chat-1",
            records: [
                recorded(1, .object([
                    "type": .string("turn_started"),
                    "turnId": .string(turnID),
                ])),
                recorded(2, testMessageEvent(text: "Start")),
                recorded(3, testAssistantMessage(
                    turnID: turnID,
                    modelStepID: "step-1",
                    phase: "commentary",
                    text: "Before steering"
                )),
                recorded(4, testMessageEvent(
                    author: .peer(
                        messageID: "message-1",
                        sessionID: "chat-reviewer",
                        handle: "@reviewer",
                        symbol: nil
                    ),
                    delivery: .steer,
                    text: "The parser boundary is covered."
                )),
            ],
            nextBeforeSequence: nil
        ))

        XCTAssertEqual(
            model.transcript.map(\.text),
            [
                "Start",
                "Before steering",
                "The parser boundary is covered.",
                "Use the smaller patch",
                "After steering",
                "Done",
            ]
        )
        XCTAssertEqual(model.transcript.map(\.turnID), Array(repeating: turnID, count: 6))
        XCTAssertEqual(
            model.transcript.map(\.startsTurn),
            [true, false, false, false, false, false]
        )
        XCTAssertEqual(
            model.transcript.compactMap { $0.messageMetadata?.delivery },
            [.turn, .steer, .steer]
        )
        let projection = model.transcriptProjection(breakBefore: nil)
        XCTAssertEqual(projection.rows.map(\.kind), [.user, .workedGroup, .narrative])
        XCTAssertEqual(
            projection.rows[1].records.map(\.text),
            [
                "Before steering",
                "The parser boundary is covered.",
                "Use the smaller patch",
                "After steering",
            ]
        )
    }

    func testPeerHistoryReconnectsTurnMetadataAcrossAPageBoundary() throws {
        let model = try model()
        let turnID = "peer-turn"
        model.mergeHistory([
            recorded(5, testMessageEvent(
                author: .peer(
                    messageID: "message-1",
                    sessionID: "chat-reviewer",
                    handle: "@reviewer",
                    symbol: nil
                ),
                text: "Review the parser boundary."
            )),
            recorded(6, testAssistantMessage(
                turnID: turnID,
                modelStepID: "step-1",
                phase: "commentary",
                text: "Checking"
            )),
            recorded(7, testAssistantMessage(
                turnID: turnID,
                modelStepID: "step-2",
                text: "Done"
            )),
            recorded(8, .object([
                "type": .string("turn_complete"),
                "turnId": .string(turnID),
            ])),
        ])

        XCTAssertEqual(model.transcript.map(\.turnID), Array(repeating: turnID, count: 3))
        XCTAssertEqual(model.transcript.map(\.startsTurn), [true, false, false])

        model.mergeHistory([recorded(1, .object([
            "type": .string("turn_started"),
            "turnId": .string(turnID),
        ]))])

        XCTAssertEqual(
            model.transcript.map(\.text),
            ["Review the parser boundary.", "Checking", "Done"]
        )
        XCTAssertEqual(model.transcript.map(\.turnID), Array(repeating: turnID, count: 3))
        XCTAssertEqual(model.transcript.map(\.startsTurn), [true, false, false])
        XCTAssertEqual(
            model.transcriptProjection(breakBefore: nil).rows.map(\.kind),
            [.peer, .workedGroup, .narrative]
        )
    }

    func testHistoricalInterruptedTurnCollapsesAroundAbortNotice() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready

        model.openSession("chat-1")
        let openRequest = await recorder.firstRequest(after: 0) {
            guard case .openSession(_, "chat-1", _) = $0 else { return false }
            return true
        }
        guard case .openSession(let openID, _, _) = try XCTUnwrap(openRequest) else {
            return XCTFail("Expected session open")
        }
        model.handle(.sessionOpened(
            requestID: openID,
            payload: sessionReady(latestSequence: 4, nextBeforeSequence: 5)
        ))
        model.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))

        let requestCount = await recorder.requestCount()
        model.loadEarlierHistory()
        let historyRequest = await recorder.firstRequest(after: requestCount) {
            if case .getSessionHistory = $0 { return true }
            return false
        }
        guard case .getSessionHistory(let historyID, _, 5) = try XCTUnwrap(historyRequest)
        else { return XCTFail("Expected history request") }

        let turnID = "turn-1"
        model.handle(.sessionHistory(
            requestID: historyID,
            sessionID: "chat-1",
            records: [
                recorded(1, .object([
                    "type": .string("turn_started"),
                    "turnId": .string(turnID),
                ])),
                recorded(2, testMessageEvent(text: "Start")),
                recorded(3, testAssistantMessage(
                    turnID: turnID,
                    modelStepID: "step-1",
                    phase: "commentary",
                    text: "Checking"
                )),
                recorded(4, .object([
                    "type": .string("turn_aborted"),
                    "turnId": .string(turnID),
                    "reason": .string("Stopped"),
                ]), blocks: [RenderedBlock(capability: "agent", block: FrontendBlock(
                    id: nil,
                    group: turnID,
                    update: .replace,
                    state: .complete,
                    role: .notice,
                    title: "Turn aborted",
                    text: "Stopped",
                    symbol: nil,
                    format: "plain_text",
                    tone: "warning",
                    files: []
                ))]),
            ],
            nextBeforeSequence: nil
        ))

        XCTAssertEqual(model.displayedTranscript.map(\.turnID), Array(repeating: turnID, count: 3))
        XCTAssertEqual(model.displayedTranscript.map(\.turnTerminal), [false, false, true])
        let projection = model.transcriptProjection(breakBefore: nil)
        XCTAssertEqual(projection.rows.map(\.kind), [.user, .workedGroup, .activityGroup])
        XCTAssertEqual(projection.rows[1].records.map(\.text), ["Checking"])
        XCTAssertEqual(projection.rows[2].records.map(\.title), ["Turn aborted"])
        XCTAssertEqual(projection.rows[1].elapsedMs, 200)
        XCTAssertFalse(model.hasEarlierHistory)
    }

    func testHistoryCompletionRevisionCoversRejectedAndEmptyPages() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready

        model.openSession("chat-1")
        let openRequest = await recorder.firstRequest(after: 0) {
            guard case .openSession(_, "chat-1", _) = $0 else { return false }
            return true
        }
        guard case .openSession(let openID, _, _) = try XCTUnwrap(openRequest) else {
            return XCTFail("Expected session open")
        }
        model.handle(.sessionOpened(
            requestID: openID,
            payload: sessionReady(latestSequence: 8, nextBeforeSequence: 40)
        ))
        model.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))

        let initialRevision = model.historyLoadCompletionRevision
        let initialSuccessRevision = model.historyLoadSuccessRevision
        let initialFailureRevision = model.historyLoadFailureRevision
        var requestCount = await recorder.requestCount()
        model.loadEarlierHistory()
        let rejectedRequest = await recorder.firstRequest(after: requestCount) {
            if case .getSessionHistory = $0 { return true }
            return false
        }
        guard case .getSessionHistory(let rejectedID, _, _) = try XCTUnwrap(rejectedRequest)
        else { return XCTFail("Expected rejected history request") }

        model.handle(.rejected(GatewayRejection(
            requestId: rejectedID,
            code: "unavailable",
            message: "Try again",
            fatal: false
        )))

        XCTAssertFalse(model.isLoadingEarlierHistory)
        XCTAssertEqual(model.historyLoadCompletionRevision, initialRevision + 1)
        XCTAssertEqual(model.historyLoadSuccessRevision, initialSuccessRevision)
        XCTAssertEqual(model.historyLoadFailureRevision, initialFailureRevision + 1)

        requestCount = await recorder.requestCount()
        model.loadEarlierHistory()
        let emptyRequest = await recorder.firstRequest(after: requestCount) {
            if case .getSessionHistory = $0 { return true }
            return false
        }
        guard case .getSessionHistory(let emptyID, _, _) = try XCTUnwrap(emptyRequest)
        else { return XCTFail("Expected empty history request") }

        model.handle(.sessionHistory(
            requestID: emptyID,
            sessionID: "chat-1",
            records: [],
            nextBeforeSequence: nil
        ))

        XCTAssertFalse(model.isLoadingEarlierHistory)
        XCTAssertEqual(model.historyLoadCompletionRevision, initialRevision + 2)
        XCTAssertEqual(model.historyLoadSuccessRevision, initialSuccessRevision + 1)
        XCTAssertEqual(model.historyLoadFailureRevision, initialFailureRevision + 1)
        XCTAssertFalse(model.hasEarlierHistory)
    }

    func testHistoryPagesRebuildCrossPageAppendsAndFailedStepDeltas() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready

        let openRequestCount = await recorder.requestCount()
        model.openSession("chat-1")
        let open = await recorder.firstRequest(after: openRequestCount) {
            guard case .openSession(_, "chat-1", nil) = $0 else { return false }
            return true
        }
        guard case .openSession(let openID, _, _) = try XCTUnwrap(open) else {
            return XCTFail("Expected session open")
        }
        model.handle(.sessionOpened(
            requestID: openID,
            payload: sessionReady(latestSequence: 8, nextBeforeSequence: 7)
        ))

        let toolEnd = recorded(7, .object([
            "type": .string("tool_call_end"),
            "turnId": .string("turn-1"),
            "callId": .string("call-1"),
            "name": .string("shell"),
            "output": .string("Output"),
            "isError": .bool(false),
        ]), blocks: [RenderedBlock(capability: "tools", block: FrontendBlock(
            id: "turn-1/call-1",
            group: nil,
            update: .append,
            state: .complete,
            role: .tool,
            title: "Run command",
            text: "\nOutput",
            symbol: nil,
            format: "plain_text",
            tone: "success",
            files: []
        ))])
        let failed = recorded(8, .object([
            "type": .string("model_step_completed"),
            "sessionId": .string("chat-1"),
            "turnId": .string("turn-1"),
            "modelStepId": .string("step-1"),
            "stepIndex": .number(0),
            "startedAtMs": .number(100),
            "completedAtMs": .number(200),
            "outcome": .object(["status": .string("failed")]),
        ]))
        model.handle(.agentEvent(sessionID: "chat-1", record: toolEnd))
        model.handle(.agentEvent(sessionID: "chat-1", record: failed))
        model.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))
        XCTAssertEqual(model.transcript.map(\.text), ["\nOutput"])

        var requestCount = await recorder.requestCount()
        model.loadEarlierHistory()
        let firstHistory = await recorder.firstRequest(after: requestCount) {
            guard case .getSessionHistory = $0 else { return false }
            return true
        }
        guard case .getSessionHistory(let firstID, _, 7) = try XCTUnwrap(firstHistory) else {
            return XCTFail("Expected first history page")
        }
        let partial = recorded(6, .object([
            "type": .string("assistant_content_delta"),
            "sessionId": .string("chat-1"),
            "turnId": .string("turn-1"),
            "modelStepId": .string("step-1"),
            "phase": .string("reasoning"),
            "delta": .string("Partial reasoning"),
        ]))
        model.handle(.sessionHistory(
            requestID: firstID,
            sessionID: "chat-1",
            records: [partial],
            nextBeforeSequence: 6
        ))
        XCTAssertEqual(model.transcript.map(\.text), ["Partial reasoning", "\nOutput"])
        XCTAssertFalse(try XCTUnwrap(model.transcript.first).pending)

        requestCount = await recorder.requestCount()
        model.loadEarlierHistory()
        let secondHistory = await recorder.firstRequest(after: requestCount) {
            guard case .getSessionHistory = $0 else { return false }
            return true
        }
        guard case .getSessionHistory(let secondID, _, 6) = try XCTUnwrap(secondHistory) else {
            return XCTFail("Expected second history page")
        }
        let toolBegin = recorded(5, .object([
            "type": .string("tool_call_begin"),
            "turnId": .string("turn-1"),
            "callId": .string("call-1"),
            "name": .string("shell"),
            "arguments": .object(["cmd": .string("pwd")]),
        ]), blocks: [RenderedBlock(capability: "tools", block: FrontendBlock(
            id: "turn-1/call-1",
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
        ))])
        model.handle(.sessionHistory(
            requestID: secondID,
            sessionID: "chat-1",
            records: [toolBegin],
            nextBeforeSequence: nil
        ))

        XCTAssertEqual(model.transcript.map(\.text), ["Arguments\nOutput", "Partial reasoning"])
        XCTAssertEqual(model.transcript.map(\.role), [.tool, nil])
        XCTAssertTrue(model.transcript.allSatisfy { !$0.pending })
    }

}
