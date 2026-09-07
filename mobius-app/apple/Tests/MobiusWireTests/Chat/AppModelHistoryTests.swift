import Foundation
@testable import Mobius
import XCTest

@MainActor
extension AppModelTests {
    func testHistoricalReplayAppearsOnlyWhenTheSnapshotIsComplete() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready

        let openRequestCount = await recorder.requestCount()
        model.chat.openSession("chat-1")
        let openRequest = await recorder.firstRequest(after: openRequestCount) {
            guard case .openSession(_, "chat-1", nil) = $0 else { return false }
            return true
        }
        let request = try XCTUnwrap(openRequest)
        guard case .openSession(let requestID, _, nil) = request else {
            return XCTFail("Expected an uncached session open")
        }
        XCTAssertTrue(model.chat.isLoadingTranscript)
        model.gateway.handle(
            .sessionOpened(requestID: requestID, payload: sessionReady(latestSequence: 2)))
        XCTAssertTrue(model.chat.isLoadingTranscript)
        model.showFiles(.unstaged)
        model.gateway.handle(
            .agentEvent(
                sessionID: "chat-1",
                sequence: 1,
                event: AgentEventRecord(
                    submissionId: nil,
                    msg: .object([
                        "type": .string("assistant_content_delta"),
                        "sessionId": .string("chat-1"),
                        "turnId": .string("turn-1"),
                        "modelStepId": .string("answer-1"),
                        "phase": .string("final_answer"),
                        "delta": .string("Hel"),
                    ])),
                blocks: [],
                history: nil,
                preview: nil
            ))
        XCTAssertTrue(model.chat.displayedTranscript.isEmpty)

        model.gateway.handle(
            .agentEvent(
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
        XCTAssertEqual(model.chat.transcript.map(\.text), ["Hello"])
        XCTAssertTrue(model.chat.displayedTranscript.isEmpty)
        let refreshRequestCount = await recorder.requestCount()
        model.gateway.handle(.sessionReplayComplete(requestID: requestID, sessionID: "chat-1"))
        XCTAssertFalse(model.chat.isLoadingTranscript)
        XCTAssertEqual(model.chat.displayedTranscript.map(\.text), ["Hello"])
        let gitDiffRequest = await recorder.firstRequest(after: refreshRequestCount) { request in
            guard case .getGitDiff(_, "chat-1", .unstaged) = request else { return false }
            return true
        }
        let workspaceFilesRequest = await recorder.firstRequest(after: refreshRequestCount) {
            request in
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
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready

        let openRequestCount = await recorder.requestCount()
        model.chat.openSession("chat-1")
        let openRequest = await recorder.firstRequest(after: openRequestCount) { request in
            guard case .openSession(_, "chat-1", _) = request else { return false }
            return true
        }
        guard case .openSession(let openID, _, _) = try XCTUnwrap(openRequest)
        else { return XCTFail("Expected session open") }
        model.gateway.handle(
            .sessionOpened(
                requestID: openID,
                payload: sessionReady(latestSequence: 8, nextBeforeSequence: 40)
            ))
        model.gateway.handle(
            .agentEvent(
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
        model.gateway.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))
        model.chat.selectedModelRoute = "current-route"

        let initialRequests = await recorder.requests()
        let historyRequestCount = initialRequests.filter {
            if case .getSessionHistory = $0 { return true }
            return false
        }.count
        model.gateway.connectionState = .disconnected
        model.chat.requestEarlierHistory()
        let disconnectedRequests = await recorder.requests()
        XCTAssertEqual(
            disconnectedRequests.filter {
                if case .getSessionHistory = $0 { return true }
                return false
            }.count,
            historyRequestCount
        )

        model.gateway.connectionState = .ready
        model.chat.activeTurnID = "turn-live"
        XCTAssertTrue(model.chat.canLoadEarlierHistory)
        let readyRequestCount = await recorder.requestCount()
        model.chat.requestEarlierHistory()
        model.chat.requestEarlierHistory()
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
        guard
            case .getSessionHistory(
                let historyID,
                "chat-1",
                40
            ) = try XCTUnwrap(historyRequest)
        else {
            return XCTFail("Expected paged history request")
        }

        model.gateway.handle(
            .agentEvent(
                sessionID: "chat-1",
                record: recorded(
                    9,
                    testAssistantMessage(
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
            RenderedEventRecord(
                event: .object([
                    "type": .string("turn_started"),
                    "turnId": .string("turn-older"),
                ]), blocks: []),
            RenderedEventRecord(
                event: testMessageEvent(text: "Older question"),
                blocks: []
            ),
            RenderedEventRecord(
                event: .object([
                    "type": .string("model_changed"),
                    "route": .string("historical-route"),
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
        model.gateway.handle(
            .sessionHistory(
                requestID: "stale",
                sessionID: "chat-1",
                records: records,
                nextBeforeSequence: nil
            ))
        XCTAssertEqual(model.chat.displayedTranscript.map(\.text), ["Current", "Still working"])

        model.gateway.handle(
            .sessionHistory(
                requestID: historyID,
                sessionID: "chat-1",
                records: Array(records.dropFirst(2)),
                nextBeforeSequence: 3
            ))

        XCTAssertEqual(
            model.chat.displayedTranscript.map(\.text),
            ["Older question", "Earlier update", "Older answer", "Current", "Still working"]
        )
        XCTAssertEqual(
            model.chat.displayedTranscript.map(\.kind),
            [.user, .commentary, .assistant, .assistant, .commentary]
        )
        XCTAssertEqual(model.chat.selectedModelRoute, "current-route")
        XCTAssertTrue(model.chat.hasEarlierHistory)

        let olderRequestCount = await recorder.requestCount()
        model.chat.requestEarlierHistory()
        let olderRequest = await recorder.firstRequest(after: olderRequestCount) { request in
            if case .getSessionHistory = request { return true }
            return false
        }
        guard
            case .getSessionHistory(
                let olderHistoryID,
                "chat-1",
                3
            ) = try XCTUnwrap(olderRequest)
        else {
            return XCTFail("Expected the next history page")
        }
        model.gateway.handle(
            .sessionHistory(
                requestID: olderHistoryID,
                sessionID: "chat-1",
                records: Array(records.prefix(2)),
                nextBeforeSequence: nil
            ))
        XCTAssertEqual(
            model.chat.displayedTranscript.map(\.text),
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
        XCTAssertFalse(model.chat.hasEarlierHistory)

        model.gateway.handle(
            .agentEvent(
                sessionID: "chat-1",
                record: recorded(
                    10,
                    testAssistantMessage(
                        turnID: "turn-live",
                        modelStepID: "step-more",
                        phase: "commentary",
                        text: "More work"
                    ))
            ))
        XCTAssertEqual(
            model.chat.displayedTranscript.map(\.text),
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

        let visibleBeforeReconnect = model.chat.displayedTranscript.map(\.text)
        model.gateway.reset(preservingDrafts: true)
        model.resetGatewayDependentState(preservingDrafts: true, preservingSession: true)
        XCTAssertEqual(model.chat.displayedTranscript.map(\.text), visibleBeforeReconnect)
        model.gateway.connectionState = .ready
        model.chat.restoreSession("chat-1")
        try await Task.sleep(for: .milliseconds(30))
        let reconnectRequests = await recorder.requests()
        guard
            case .openSession(let reconnectID, _, _) = try XCTUnwrap(
                reconnectRequests.last
            )
        else { return XCTFail("Expected reconnect session open") }
        model.gateway.handle(
            .sessionOpened(
                requestID: reconnectID,
                payload: sessionReady(latestSequence: 8, nextBeforeSequence: 40)
            ))
        model.gateway.handle(.sessionReplayComplete(requestID: reconnectID, sessionID: "chat-1"))
        XCTAssertEqual(
            model.chat.displayedTranscript.map(\.text),
            visibleBeforeReconnect
        )
        XCTAssertFalse(model.chat.hasEarlierHistory)
    }

    func testHistoryMergeDoesNotReplayABufferedDeltaTwice() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready

        model.chat.openSession("chat-1")
        let openRequest = await recorder.firstRequest(after: 0) {
            guard case .openSession(_, "chat-1", _) = $0 else { return false }
            return true
        }
        guard case .openSession(let openID, _, _) = try XCTUnwrap(openRequest) else {
            return XCTFail("Expected session open")
        }
        model.gateway.handle(
            .sessionOpened(
                requestID: openID,
                payload: sessionReady(latestSequence: 8, nextBeforeSequence: 40)
            ))
        model.gateway.handle(
            .agentEvent(
                sessionID: "chat-1",
                record: recorded(
                    8,
                    testAssistantMessage(
                        turnID: "turn-current",
                        modelStepID: "step-current",
                        text: "Current"
                    ))
            ))
        model.gateway.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))
        model.chat.activeTurnID = "turn-live"

        let requestCount = await recorder.requestCount()
        model.chat.requestEarlierHistory()
        let historyRequest = await recorder.firstRequest(after: requestCount) {
            guard case .getSessionHistory = $0 else { return false }
            return true
        }
        guard case .getSessionHistory(let historyID, _, _) = try XCTUnwrap(historyRequest)
        else { return XCTFail("Expected history request") }

        model.gateway.handle(
            .agentEvent(
                sessionID: "chat-1",
                record: recorded(
                    9,
                    .object([
                        "type": .string("assistant_content_delta"),
                        "sessionId": .string("chat-1"),
                        "turnId": .string("turn-live"),
                        "modelStepId": .string("step-live"),
                        "phase": .string("commentary"),
                        "delta": .string("Still working"),
                    ]))
            ))
        XCTAssertEqual(model.chat.transcript.map(\.text), ["Current"])

        model.gateway.handle(
            .sessionHistory(
                requestID: historyID,
                sessionID: "chat-1",
                records: [recorded(1, testMessageEvent(text: "Older question"))],
                nextBeforeSequence: nil
            ))
        try await Task.sleep(for: .milliseconds(80))

        XCTAssertEqual(
            model.chat.transcript.map(\.text),
            ["Older question", "Current", "Still working"]
        )
    }

    func testHistoryPagesReconnectTurnMetadataAcrossAPageBoundary() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready

        model.chat.openSession("chat-1")
        let openRequest = await recorder.firstRequest(after: 0) {
            guard case .openSession(_, "chat-1", _) = $0 else { return false }
            return true
        }
        guard case .openSession(let openID, _, _) = try XCTUnwrap(openRequest) else {
            return XCTFail("Expected session open")
        }
        model.gateway.handle(
            .sessionOpened(
                requestID: openID,
                payload: sessionReady(latestSequence: 8, nextBeforeSequence: 9)
            ))
        model.gateway.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))

        model.chat.requestEarlierHistory()
        let firstRequest = await recorder.firstRequest(after: 1) {
            guard case .getSessionHistory = $0 else { return false }
            return true
        }
        guard case .getSessionHistory(let firstID, _, 9) = try XCTUnwrap(firstRequest) else {
            return XCTFail("Expected first history page")
        }
        let turnID = "turn-1"
        model.gateway.handle(
            .sessionHistory(
                requestID: firstID,
                sessionID: "chat-1",
                records: [
                    recorded(
                        5,
                        testMessageEvent(
                            delivery: .steer,
                            text: "Use the smaller patch"
                        )),
                    recorded(
                        6,
                        testAssistantMessage(
                            turnID: turnID,
                            modelStepID: "step-2",
                            phase: "commentary",
                            text: "After steering"
                        )),
                    recorded(
                        7,
                        testAssistantMessage(
                            turnID: turnID,
                            modelStepID: "step-3",
                            text: "Done"
                        )),
                    recorded(
                        8,
                        .object([
                            "type": .string("turn_complete"),
                            "turnId": .string(turnID),
                        ])),
                ],
                nextBeforeSequence: 5
            ))

        XCTAssertEqual(model.chat.transcript.map(\.turnID), Array(repeating: turnID, count: 3))
        XCTAssertEqual(
            model.chat.transcriptProjection(breakBefore: nil).rows.map(\.kind),
            [.workedGroup, .narrative]
        )

        let requestCount = await recorder.requestCount()
        model.chat.requestEarlierHistory()
        let secondRequest = await recorder.firstRequest(after: requestCount) {
            guard case .getSessionHistory = $0 else { return false }
            return true
        }
        guard case .getSessionHistory(let secondID, _, 5) = try XCTUnwrap(secondRequest) else {
            return XCTFail("Expected second history page")
        }
        model.gateway.handle(
            .sessionHistory(
                requestID: secondID,
                sessionID: "chat-1",
                records: [
                    recorded(
                        1,
                        .object([
                            "type": .string("turn_started"),
                            "turnId": .string(turnID),
                        ])),
                    recorded(2, testMessageEvent(text: "Start")),
                    recorded(
                        3,
                        testAssistantMessage(
                            turnID: turnID,
                            modelStepID: "step-1",
                            phase: "commentary",
                            text: "Before steering"
                        )),
                    recordedPeerMessage(
                        4,
                        delivery: .steer,
                        text: "The parser boundary is covered."
                    ),
                ],
                nextBeforeSequence: nil
            ))

        XCTAssertEqual(
            model.chat.transcript.map(\.text),
            [
                "Start",
                "Before steering",
                "The parser boundary is covered.",
                "Use the smaller patch",
                "After steering",
                "Done",
            ]
        )
        XCTAssertEqual(model.chat.transcript.map(\.turnID), Array(repeating: turnID, count: 6))
        XCTAssertEqual(
            model.chat.transcript.map(\.startsTurn),
            [true, false, false, false, false, false]
        )
        XCTAssertEqual(
            model.chat.transcript.compactMap { $0.messageMetadata?.delivery },
            [.turn, .steer, .steer]
        )
        let projection = model.chat.transcriptProjection(breakBefore: nil)
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
        model.chat.mergeHistory([
            recordedPeerMessage(
                5,
                text: "Review the parser boundary."
            ),
            recorded(
                6,
                testAssistantMessage(
                    turnID: turnID,
                    modelStepID: "step-1",
                    phase: "commentary",
                    text: "Checking"
                )),
            recorded(
                7,
                testAssistantMessage(
                    turnID: turnID,
                    modelStepID: "step-2",
                    text: "Done"
                )),
            recorded(
                8,
                .object([
                    "type": .string("turn_complete"),
                    "turnId": .string(turnID),
                ])),
        ])

        XCTAssertEqual(model.chat.transcript.map(\.turnID), Array(repeating: turnID, count: 3))
        XCTAssertEqual(model.chat.transcript.map(\.startsTurn), [true, false, false])

        model.chat.mergeHistory([
            recorded(
                1,
                .object([
                    "type": .string("turn_started"),
                    "turnId": .string(turnID),
                ]))
        ])

        XCTAssertEqual(
            model.chat.transcript.map(\.text),
            ["Review the parser boundary.", "Checking", "Done"]
        )
        XCTAssertEqual(model.chat.transcript.map(\.turnID), Array(repeating: turnID, count: 3))
        XCTAssertEqual(model.chat.transcript.map(\.startsTurn), [true, false, false])
        XCTAssertEqual(
            model.chat.transcriptProjection(breakBefore: nil).rows.map(\.kind),
            [.workedGroup, .narrative]
        )
    }

    func testHistoricalInterruptedTurnCollapsesAroundAbortNotice() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready

        model.chat.openSession("chat-1")
        let openRequest = await recorder.firstRequest(after: 0) {
            guard case .openSession(_, "chat-1", _) = $0 else { return false }
            return true
        }
        guard case .openSession(let openID, _, _) = try XCTUnwrap(openRequest) else {
            return XCTFail("Expected session open")
        }
        model.gateway.handle(
            .sessionOpened(
                requestID: openID,
                payload: sessionReady(latestSequence: 4, nextBeforeSequence: 5)
            ))
        model.gateway.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))

        let requestCount = await recorder.requestCount()
        model.chat.requestEarlierHistory()
        let historyRequest = await recorder.firstRequest(after: requestCount) {
            if case .getSessionHistory = $0 { return true }
            return false
        }
        guard case .getSessionHistory(let historyID, _, 5) = try XCTUnwrap(historyRequest)
        else { return XCTFail("Expected history request") }

        let turnID = "turn-1"
        model.gateway.handle(
            .sessionHistory(
                requestID: historyID,
                sessionID: "chat-1",
                records: [
                    recorded(
                        1,
                        .object([
                            "type": .string("turn_started"),
                            "turnId": .string(turnID),
                        ])),
                    recorded(2, testMessageEvent(text: "Start")),
                    recorded(
                        3,
                        testAssistantMessage(
                            turnID: turnID,
                            modelStepID: "step-1",
                            phase: "commentary",
                            text: "Checking"
                        )),
                    recorded(
                        4,
                        .object([
                            "type": .string("turn_aborted"),
                            "turnId": .string(turnID),
                            "reason": .string("Stopped"),
                        ]),
                        blocks: [
                            RenderedBlock(
                                capability: "agent",
                                block: FrontendBlock(
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
                                ))
                        ]),
                ],
                nextBeforeSequence: nil
            ))

        XCTAssertEqual(
            model.chat.displayedTranscript.map(\.turnID), Array(repeating: turnID, count: 3))
        XCTAssertEqual(model.chat.displayedTranscript.map(\.turnTerminal), [false, false, true])
        let projection = model.chat.transcriptProjection(breakBefore: nil)
        XCTAssertEqual(projection.rows.map(\.kind), [.user, .workedGroup, .activityGroup])
        XCTAssertEqual(projection.rows[1].records.map(\.text), ["Checking"])
        XCTAssertEqual(projection.rows[2].records.map(\.title), ["Turn aborted"])
        XCTAssertEqual(projection.rows[1].elapsedMs, 200)
        XCTAssertFalse(model.chat.hasEarlierHistory)
    }

    func testHistoryCompletionRevisionCoversRejectedAndEmptyPages() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready

        model.chat.openSession("chat-1")
        let openRequest = await recorder.firstRequest(after: 0) {
            guard case .openSession(_, "chat-1", _) = $0 else { return false }
            return true
        }
        guard case .openSession(let openID, _, _) = try XCTUnwrap(openRequest) else {
            return XCTFail("Expected session open")
        }
        model.gateway.handle(
            .sessionOpened(
                requestID: openID,
                payload: sessionReady(latestSequence: 8, nextBeforeSequence: 40)
            ))
        model.gateway.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))

        let initialRevision = model.chat.historyLoadCompletionRevision
        let initialSuccessRevision = model.chat.historyLoadSuccessRevision
        let initialFailureRevision = model.chat.historyLoadFailureRevision
        var requestCount = await recorder.requestCount()
        model.chat.requestEarlierHistory()
        let rejectedRequest = await recorder.firstRequest(after: requestCount) {
            if case .getSessionHistory = $0 { return true }
            return false
        }
        guard case .getSessionHistory(let rejectedID, _, _) = try XCTUnwrap(rejectedRequest)
        else { return XCTFail("Expected rejected history request") }

        model.gateway.handle(
            .rejected(
                GatewayRejection(
                    requestId: rejectedID,
                    code: "unavailable",
                    message: "Try again",
                    fatal: false
                )))

        XCTAssertFalse(model.chat.isLoadingEarlierHistory)
        XCTAssertEqual(model.chat.historyLoadCompletionRevision, initialRevision + 1)
        XCTAssertEqual(model.chat.historyLoadSuccessRevision, initialSuccessRevision)
        XCTAssertEqual(model.chat.historyLoadFailureRevision, initialFailureRevision + 1)

        requestCount = await recorder.requestCount()
        model.chat.requestEarlierHistory()
        let emptyRequest = await recorder.firstRequest(after: requestCount) {
            if case .getSessionHistory = $0 { return true }
            return false
        }
        guard case .getSessionHistory(let emptyID, _, _) = try XCTUnwrap(emptyRequest)
        else { return XCTFail("Expected empty history request") }

        model.gateway.handle(
            .sessionHistory(
                requestID: emptyID,
                sessionID: "chat-1",
                records: [],
                nextBeforeSequence: nil
            ))

        XCTAssertFalse(model.chat.isLoadingEarlierHistory)
        XCTAssertEqual(model.chat.historyLoadCompletionRevision, initialRevision + 2)
        XCTAssertEqual(model.chat.historyLoadSuccessRevision, initialSuccessRevision + 1)
        XCTAssertEqual(model.chat.historyLoadFailureRevision, initialFailureRevision + 1)
        XCTAssertFalse(model.chat.hasEarlierHistory)
    }

    func testAsyncHistoryLoadCompletesWhenTheGatewayFinishes() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready
        model.chat.openSession("chat-1")
        let openRequest = await recorder.firstRequest(after: 0) {
            guard case .openSession(_, "chat-1", _) = $0 else { return false }
            return true
        }
        guard case .openSession(let openID, _, _) = try XCTUnwrap(openRequest) else {
            return XCTFail("Expected session open")
        }
        model.gateway.handle(
            .sessionOpened(
                requestID: openID,
                payload: sessionReady(latestSequence: 8, nextBeforeSequence: 40)
            ))
        model.gateway.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))

        let load = Task { @MainActor in await model.chat.loadEarlierHistory() }
        let historyRequest = await recorder.firstRequest(after: 1) {
            if case .getSessionHistory = $0 { return true }
            return false
        }
        guard case .getSessionHistory(let historyID, _, _) = try XCTUnwrap(historyRequest)
        else { return XCTFail("Expected history request") }
        XCTAssertTrue(model.chat.isLoadingEarlierHistory)

        model.gateway.handle(
            .sessionHistory(
                requestID: historyID,
                sessionID: "chat-1",
                records: [],
                nextBeforeSequence: nil
            ))
        await load.value

        XCTAssertFalse(model.chat.isLoadingEarlierHistory)
        XCTAssertNil(model.chat.historyRequestID)
    }

    func testAsyncHistoryLoadCancellationDoesNotCancelTheGatewayRequest() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready
        model.chat.selectedSessionID = "chat-1"
        model.chat.nextHistoryBeforeSequence = 40
        model.chat.activeTurnID = "turn-live"

        let load = Task { @MainActor in await model.chat.loadEarlierHistory() }
        let historyRequest = await recorder.firstRequest(after: 0) {
            if case .getSessionHistory = $0 { return true }
            return false
        }
        guard case .getSessionHistory(let historyID, "chat-1", 40) = try XCTUnwrap(historyRequest)
        else { return XCTFail("Expected history request") }

        load.cancel()
        await load.value
        XCTAssertEqual(model.chat.historyRequestID, historyID)
        XCTAssertTrue(model.chat.isLoadingEarlierHistory)

        model.gateway.handle(
            .rejected(
                GatewayRejection(
                    requestId: historyID,
                    code: "unavailable",
                    message: "Try again",
                    fatal: false
                )))
        XCTAssertNil(model.chat.historyRequestID)
        XCTAssertFalse(model.chat.isLoadingEarlierHistory)

        model.gateway.connectionState = .ready
        let resetLoad = Task { @MainActor in await model.chat.loadEarlierHistory() }
        let resetRequest = await recorder.firstRequest(after: 1) {
            if case .getSessionHistory = $0 { return true }
            return false
        }
        _ = try XCTUnwrap(resetRequest)
        model.gateway.reset(preservingDrafts: true)
        model.resetGatewayDependentState(preservingDrafts: true)
        await resetLoad.value
        XCTAssertFalse(model.chat.isLoadingEarlierHistory)
        XCTAssertNil(model.chat.historyRequestID)
    }

    func testHistoryPagesRebuildCrossPageAppendsAndFailedStepDeltas() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready

        let openRequestCount = await recorder.requestCount()
        model.chat.openSession("chat-1")
        let open = await recorder.firstRequest(after: openRequestCount) {
            guard case .openSession(_, "chat-1", nil) = $0 else { return false }
            return true
        }
        guard case .openSession(let openID, _, _) = try XCTUnwrap(open) else {
            return XCTFail("Expected session open")
        }
        model.gateway.handle(
            .sessionOpened(
                requestID: openID,
                payload: sessionReady(latestSequence: 8, nextBeforeSequence: 7)
            ))

        let toolEnd = recorded(
            7,
            .object([
                "type": .string("tool_call_end"),
                "turnId": .string("turn-1"),
                "callId": .string("call-1"),
                "name": .string("shell"),
                "output": .string("Output"),
                "isError": .bool(false),
            ]),
            blocks: [
                RenderedBlock(
                    capability: "tools",
                    block: FrontendBlock(
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
                    ))
            ])
        let failed = recorded(
            8,
            .object([
                "type": .string("model_step_completed"),
                "sessionId": .string("chat-1"),
                "turnId": .string("turn-1"),
                "modelStepId": .string("step-1"),
                "stepIndex": .number(0),
                "startedAtMs": .number(100),
                "completedAtMs": .number(200),
                "outcome": .object(["status": .string("failed")]),
            ]))
        model.gateway.handle(.agentEvent(sessionID: "chat-1", record: toolEnd))
        model.gateway.handle(.agentEvent(sessionID: "chat-1", record: failed))
        model.gateway.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))
        XCTAssertEqual(model.chat.transcript.map(\.text), ["\nOutput"])

        var requestCount = await recorder.requestCount()
        model.chat.requestEarlierHistory()
        let firstHistory = await recorder.firstRequest(after: requestCount) {
            guard case .getSessionHistory = $0 else { return false }
            return true
        }
        guard case .getSessionHistory(let firstID, _, 7) = try XCTUnwrap(firstHistory) else {
            return XCTFail("Expected first history page")
        }
        let partial = recorded(
            6,
            .object([
                "type": .string("assistant_content_delta"),
                "sessionId": .string("chat-1"),
                "turnId": .string("turn-1"),
                "modelStepId": .string("step-1"),
                "phase": .string("reasoning"),
                "delta": .string("Partial reasoning"),
            ]))
        model.gateway.handle(
            .sessionHistory(
                requestID: firstID,
                sessionID: "chat-1",
                records: [partial],
                nextBeforeSequence: 6
            ))
        XCTAssertEqual(model.chat.transcript.map(\.text), ["Partial reasoning", "\nOutput"])
        XCTAssertFalse(try XCTUnwrap(model.chat.transcript.first).pending)

        requestCount = await recorder.requestCount()
        model.chat.requestEarlierHistory()
        let secondHistory = await recorder.firstRequest(after: requestCount) {
            guard case .getSessionHistory = $0 else { return false }
            return true
        }
        guard case .getSessionHistory(let secondID, _, 6) = try XCTUnwrap(secondHistory) else {
            return XCTFail("Expected second history page")
        }
        let toolBegin = recorded(
            5,
            .object([
                "type": .string("tool_call_begin"),
                "turnId": .string("turn-1"),
                "callId": .string("call-1"),
                "name": .string("shell"),
                "arguments": .object(["cmd": .string("pwd")]),
            ]),
            blocks: [
                RenderedBlock(
                    capability: "tools",
                    block: FrontendBlock(
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
                    ))
            ])
        model.gateway.handle(
            .sessionHistory(
                requestID: secondID,
                sessionID: "chat-1",
                records: [toolBegin],
                nextBeforeSequence: nil
            ))

        XCTAssertEqual(
            model.chat.transcript.map(\.text), ["Arguments\nOutput", "Partial reasoning"])
        XCTAssertEqual(model.chat.transcript.map(\.role), [.tool, nil])
        XCTAssertTrue(model.chat.transcript.allSatisfy { !$0.pending })
    }

}

@MainActor
extension AppModelTests {
    func testCachedChatsRemainBrowseableAndSyncOnlyTheCurrentChat() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { await recorder.record($0) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        let chats = [session(state: .idle), session(sessionID: "chat-2", state: .idle)]
        model.applySessions(chats)
        for (index, chat) in chats.enumerated() {
            await model.store.saveTranscript(
                accountID: account.id,
                sessionID: chat.sessionId,
                sequence: UInt64(index + 7),
                transcript: [
                    TranscriptEntry(
                        id: chat.sessionId,
                        text: "Cached \(chat.sessionId)",
                        kind: .assistant,
                        format: "plain_text",
                        pending: false
                    )
                ],
                currentUsage: TokenUsage(),
                lastUsage: TokenUsage()
            )
        }
        model.gateway.connectionState = .connecting
        XCTAssertTrue(model.canBrowseSessions)
        XCTAssertFalse(model.canOpenSession)
        model.openChat("chat-1")
        await model.chat.transcriptIOTask?.value
        XCTAssertEqual(model.chat.displayedTranscript.map(\.text), ["Cached chat-1"])
        await model.chat.composerDraftIOTask?.value
        model.chat.composer = "Keep this draft"
        model.openChat("chat-2")
        await model.chat.transcriptIOTask?.value
        await model.chat.composerDraftIOTask?.value
        XCTAssertEqual(model.chat.selectedSessionID, "chat-2")
        XCTAssertEqual(model.chat.sessionToRestoreID, "chat-2")
        XCTAssertEqual(model.chat.displayedTranscript.map(\.text), ["Cached chat-2"])
        XCTAssertEqual(model.chat.latestSequence, 8)
        XCTAssertFalse(model.canSendComposer)
        XCTAssertFalse(model.canModifySelectedSession)
        XCTAssertFalse(model.canCreateSession)
        let disconnectedRequests = await recorder.requests()
        XCTAssertTrue(disconnectedRequests.isEmpty)
        let draft = await model.store.loadComposerDraft(accountID: account.id, sessionID: "chat-1")
        XCTAssertEqual(draft.text, "Keep this draft")

        model.gateway.handle(
            .ready(
                ready(
                    botDefaults: VersionedAgentConfig(revision: 1, config: composition()),
                    sessions: chats
                )))
        let request = await recorder.firstRequest(after: 0) {
            if case .openSession = $0 { return true }
            return false
        }
        guard case .openSession(let requestID, "chat-2", 8) = try XCTUnwrap(request) else {
            return XCTFail(
                "Only the currently visible chat should synchronize from its cache cursor")
        }
        XCTAssertEqual(model.chat.displayedTranscript.map(\.text), ["Cached chat-2"])
        model.gateway.handle(
            .sessionOpened(
                requestID: requestID,
                payload: sessionReady(latestSequence: 8, sessionID: "chat-2")
            ))
        model.gateway.handle(.sessionReplayComplete(requestID: requestID, sessionID: "chat-2"))
        XCTAssertEqual(model.chat.displayedTranscript.map(\.text), ["Cached chat-2"])
        XCTAssertTrue(model.gateway.connectionState.isReady)
    }

    func testGatewayReadyRetiresAnUnfinishedOfflineCacheRead() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { await recorder.record($0) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.applySessions([session(state: .idle)])
        model.gateway.connectionState = .connecting
        let gate = AsyncGate()
        model.chat.transcriptIOTask = Task { await gate.wait() }
        model.openChat("chat-1")
        let cacheRead = model.chat.transcriptIOTask
        XCTAssertTrue(model.chat.isLoadingTranscript)
        XCTAssertTrue(model.chat.displayedTranscript.isEmpty)
        model.gateway.handle(
            .ready(
                ready(
                    botDefaults: VersionedAgentConfig(revision: 1, config: composition())
                )))
        let request = await recorder.firstRequest(after: 0) {
            if case .openSession = $0 { return true }
            return false
        }
        guard case .openSession(let requestID, "chat-1", nil) = try XCTUnwrap(request) else {
            return XCTFail("An uncached chat should open after Ready")
        }
        model.gateway.handle(
            .sessionOpened(requestID: requestID, payload: sessionReady(latestSequence: 1)))
        model.gateway.handle(
            .agentEvent(
                sessionID: "chat-1",
                record: recorded(
                    1,
                    testAssistantMessage(
                        turnID: "turn-1", modelStepID: "answer", text: "Fresh from replay"
                    ))
            ))
        XCTAssertEqual(
            model.chat.transcript.map(\.text), ["Fresh from replay"],
            "Replay must populate the live transcript")
        model.gateway.handle(.sessionReplayComplete(requestID: requestID, sessionID: "chat-1"))
        XCTAssertEqual(
            model.chat.displayedTranscript.map(\.text), ["Fresh from replay"],
            "Completed replay must be visible before the cache read resumes")
        await gate.open()
        await cacheRead?.value
        XCTAssertEqual(model.chat.displayedTranscript.map(\.text), ["Fresh from replay"])
        let requests = await recorder.requests()
        XCTAssertEqual(
            requests.filter {
                if case .openSession = $0 { return true }
                return false
            }.count, 1)
    }

    func testOfflineChatSelectionSupersedesEarlierCacheReads() async throws {
        let model = try model(requestSender: { _ in XCTFail("Browsing must not send a request") })
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.applySessions([session(state: .idle), session(sessionID: "chat-2", state: .idle)])
        let gate = AsyncGate()
        model.chat.transcriptIOTask = Task { await gate.wait() }
        model.openChat("chat-1")
        model.openChat("chat-2")
        await gate.open()
        await model.chat.transcriptIOTask?.value
        XCTAssertEqual(model.chat.selectedSessionID, "chat-2")
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-2"))])
        XCTAssertEqual(model.chat.sessionToRestoreID, "chat-2")
        XCTAssertTrue(model.chat.isLoadingTranscript)
    }

    func testVisibleBotSessionIsRestoredWithoutOpeningOtherBotWork() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { await recorder.record($0) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.chat.botSessionsBotID = "bot-1"
        model.chat.botSessions = [
            session(state: .idle), session(sessionID: "chat-2", state: .idle),
        ]
        model.destination = .bots
        model.navigationPath = [.botSessions("bot-1")]
        model.openBotSession("chat-1")
        await model.chat.transcriptIOTask?.value
        XCTAssertEqual(model.presentedChatSessionID, "chat-1")
        XCTAssertTrue(model.isPresentingChat)
        model.gateway.handle(
            .ready(
                ready(
                    botDefaults: VersionedAgentConfig(revision: 1, config: composition()),
                    sessions: []
                )))
        let request = await recorder.firstRequest(after: 0) {
            if case .openSession = $0 { return true }
            return false
        }
        guard case .openSession(_, "chat-1", nil) = try XCTUnwrap(request) else {
            return XCTFail("Only the visible Bot session should reopen")
        }
    }
}
