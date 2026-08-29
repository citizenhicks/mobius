import Foundation
import XCTest

@MainActor
extension AppModelTests {
    func testSessionElapsedTimesTheRunningTurn() throws {
        let model = try model()
        model.selectedSessionID = "chat-1"
        model.sessions = [session(state: .idle, createdAt: 100, updatedAt: 160)]

        XCTAssertEqual(model.sessionElapsed(at: Date(timeIntervalSince1970: 200)), 0)

        // `session(state:)` starts a running turn at 100, not at the chat's creation time.
        model.sessions = [session(state: .running, createdAt: 20, updatedAt: 160)]
        XCTAssertEqual(model.sessionElapsed(at: Date(timeIntervalSince1970: 200)), 100)
    }

    func testSessionCompactionCountRestoresAndAdvancesOnlyFromLiveEvents() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready

        model.openSession("chat-1")
        let request = await recorder.firstRequest(after: 0) {
            guard case .openSession(_, "chat-1", nil) = $0 else { return false }
            return true
        }
        guard case .openSession(let requestID, _, _) = try XCTUnwrap(request)
        else { return XCTFail("Expected session open") }

        model.handle(.sessionOpened(
            requestID: requestID,
            payload: sessionReady(latestSequence: 8, compactionCount: 2)
        ))
        model.handle(.agentEvent(
            sessionID: "chat-1",
            sequence: 8,
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("context_compacted")
            ])),
            blocks: [],
            preview: nil
        ))
        XCTAssertEqual(model.sessionCompactionCount, 2)

        model.handle(.sessionReplayComplete(requestID: requestID, sessionID: "chat-1"))
        model.handle(.agentEvent(
            sessionID: "chat-1",
            sequence: 9,
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("context_compacted")
            ])),
            blocks: [],
            preview: nil
        ))

        XCTAssertEqual(model.sessionCompactionCount, 3)
    }

    func testLiveRunStatsStartImmediatelyAndTrackToolCalls() throws {
        let model = try model()
        model.selectedSessionID = "chat-1"

        model.reduce(
            event: AgentEventRecord(submissionId: "input-1", msg: .object([
                "type": .string("turn_started"),
                "turnId": .string("turn-1")
            ])),
            blocks: [],
            preview: nil
        )
        let active = try XCTUnwrap(model.runStats.active)
        XCTAssertEqual(model.sessionRunCount, 1)
        XCTAssertGreaterThan(
            model.sessionElapsed(at: Date(timeIntervalSince1970: TimeInterval(active.startedAtMs) / 1_000 + 2)),
            1.9
        )

        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("tool_call_begin")
            ])),
            blocks: [],
            preview: nil
        )
        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("tool_call_end"),
                "isError": .bool(true)
            ])),
            blocks: [],
            preview: nil
        )
        XCTAssertEqual(model.sessionToolCalls, 1)
        XCTAssertEqual(model.sessionFailedToolCalls, 1)

        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("turn_complete")
            ])),
            blocks: [],
            preview: nil
        )
        XCTAssertNil(model.runStats.active)
    }

    func testSessionSnapshotRestoresActiveTurnInterrupt() async throws {
        let recorder = GatewayRequestRecorder()
        let interruptSent = expectation(description: "Interrupt sent")
        let model = try model { request in
            await recorder.record(request)
            guard case .submit(_, let submission) = request,
                  case .interrupt = submission.op
            else { return }
            interruptSent.fulfill()
        }
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        var stats = RunStats()
        stats.active = RunSummary(
            sessionId: "chat-1",
            submissionId: "submission-1",
            turnId: "turn-1",
            startedAtMs: 1_000,
            finishedAtMs: nil,
            elapsedMs: 500,
            outcome: nil,
            modelCalls: 1,
            toolCalls: 0,
            failedToolCalls: 0,
            usage: TokenUsage()
        )

        model.handle(.sessionChanged(sessionReady(latestSequence: 8, runStats: stats)))

        XCTAssertEqual(model.activeTurnID, "turn-1")
        model.interrupt()
        await fulfillment(of: [interruptSent], timeout: 1)
        let requests = await recorder.requests()
        guard case .submit(let sessionID, let submission) = try XCTUnwrap(requests.last),
              case .interrupt(let turnID) = submission.op
        else { return XCTFail("Expected active-turn interrupt") }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(turnID, "turn-1")
    }

    func testStreamDeltasStayBatchedUntilTheCanonicalMessage() async throws {
        let model = try model()

        for _ in 0..<100 {
            model.reduce(
                event: AgentEventRecord(submissionId: nil, msg: .object([
                    "type": .string("assistant_content_delta"),
                    "sessionId": .string("chat-1"),
                    "turnId": .string("turn-1"),
                    "modelStepId": .string("answer-1"),
                    "phase": .string("final_answer"),
                    "delta": .string("x")
                ])),
                blocks: [],
                preview: nil
            )
        }
        XCTAssertTrue(model.transcript.isEmpty)

        let expected = String(repeating: "x", count: 100)
        let deltasFlushed = await eventually {
            model.transcript.map(\.text) == [expected]
        }
        XCTAssertTrue(deltasFlushed)
        XCTAssertEqual(model.transcript.map(\.text), [expected])
        XCTAssertTrue(try XCTUnwrap(model.transcript.first).pending)

        model.reduce(
            event: AgentEventRecord(
                submissionId: nil,
                msg: testAssistantMessage(
                    turnID: "turn-1",
                    modelStepID: "answer-1",
                    text: "Canonical **Markdown**"
                )
            ),
            blocks: [],
            preview: nil
        )

        XCTAssertEqual(model.transcript.map(\.text), ["Canonical **Markdown**"])
        XCTAssertFalse(try XCTUnwrap(model.transcript.first).pending)
    }

    func testCommentaryAndFinalAnswerRemainSeparateAssistantMessages() throws {
        let model = try model()

        for (phase, delta) in [("commentary", "Checking **the workspace**"), ("final_answer", "Done")] {
            model.reduce(
                event: AgentEventRecord(submissionId: nil, msg: .object([
                    "type": .string("assistant_content_delta"),
                    "sessionId": .string("chat-1"),
                    "turnId": .string("turn-1"),
                    "modelStepId": .string("response-1"),
                    "phase": .string(phase),
                    "delta": .string(delta)
                ])),
                blocks: [],
                preview: nil
            )
        }

        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("assistant_message"),
                "sessionId": .string("chat-1"),
                "turnId": .string("turn-1"),
                "modelStepId": .string("response-1"),
                "content": .array([
                    .object([
                        "outputIndex": .number(0),
                        "partIndex": .number(0),
                        "phase": .string("commentary"),
                        "text": .string("Checking **the workspace**"),
                        "annotations": .array([]),
                    ]),
                    .object([
                        "outputIndex": .number(1),
                        "partIndex": .number(0),
                        "phase": .string("final_answer"),
                        "text": .string("Done"),
                        "annotations": .array([]),
                    ]),
                ]),
                "messageTarget": .null,
            ])),
            blocks: [],
            preview: nil
        )

        XCTAssertEqual(model.transcript.map(\.text), ["Checking **the workspace**", "Done"])
        XCTAssertEqual(model.transcript.map(\.kind), [.commentary, .assistant])
        XCTAssertTrue(model.transcript.allSatisfy { !$0.pending })
    }

    func testCompletedModelStepSnapshotMatchesReplayWithoutDeltas() throws {
        let live = try model()
        let replay = try model()
        let stepID = "step-1"
        let deltas = [
            ("reasoning", "Reason"),
            ("commentary", "Checking"),
            ("final_answer", "Done"),
        ]
        for (offset, delta) in deltas.enumerated() {
            let fields: [String: JSONValue] = [
                "type": .string("assistant_content_delta"),
                "sessionId": .string("chat-1"),
                "turnId": .string("turn-1"),
                "modelStepId": .string(stepID),
                "phase": .string(delta.0),
                "delta": .string(delta.1),
            ]
            live.reduce(record: recorded(UInt64(offset + 1), .object(fields)))
        }
        let completion = recorded(4, .object([
            "type": .string("model_step_completed"),
            "sessionId": .string("chat-1"),
            "turnId": .string("turn-1"),
            "modelStepId": .string(stepID),
            "stepIndex": .number(0),
            "startedAtMs": .number(100),
            "completedAtMs": .number(400),
            "outcome": .object([
                "status": .string("completed"),
                "endTurn": .bool(true),
                "toolCallIds": .array([]),
                "usage": .object([
                    "inputTokens": .number(10),
                    "cachedInputTokens": .number(2),
                    "cacheWriteInputTokens": .number(0),
                    "outputTokens": .number(3),
                    "reasoningOutputTokens": .number(1),
                    "totalTokens": .number(13),
                ]),
            ]),
        ]))
        let snapshot = recorded(5, .object([
            "type": .string("assistant_message"),
            "sessionId": .string("chat-1"),
            "turnId": .string("turn-1"),
            "modelStepId": .string(stepID),
            "content": .array([
                .object([
                    "outputIndex": .number(0),
                    "partIndex": .number(0),
                    "phase": .string("reasoning"),
                    "text": .string("Reason"),
                    "annotations": .array([]),
                ]),
                .object([
                    "outputIndex": .number(1),
                    "partIndex": .number(0),
                    "phase": .string("commentary"),
                    "text": .string("Checking"),
                    "annotations": .array([]),
                ]),
                .object([
                    "outputIndex": .number(1),
                    "partIndex": .number(1),
                    "phase": .string("final_answer"),
                    "text": .string("Done"),
                    "annotations": .array([]),
                ]),
            ]),
            "messageTarget": .null,
        ]))

        live.reduce(record: completion)
        replay.reduce(record: completion)
        live.reduce(record: snapshot)
        replay.reduce(record: snapshot)

        let liveProjection = live.transcript.map {
            [$0.id, $0.kind.rawValue, $0.text, String($0.pending), $0.modelStepID ?? ""]
        }
        let replayProjection = replay.transcript.map {
            [$0.id, $0.kind.rawValue, $0.text, String($0.pending), $0.modelStepID ?? ""]
        }
        XCTAssertEqual(liveProjection, replayProjection)
        XCTAssertEqual(live.transcript.map(\.text), ["Reason", "Checking", "Done"])
        XCTAssertEqual(
            live.transcript.map(\.presentationID),
            [
                "step-1:reasoning:0",
                "step-1:commentary:0",
                "step-1:final_answer:0",
            ]
        )
        XCTAssertEqual(live.transcript.map(\.presentationID), replay.transcript.map(\.presentationID))
    }

    func testCompletedModelStepAssignsDeterministicPresentationOrdinalsWithinEachPhase() async throws {
        let live = try model()
        let replay = try model()
        let stepID = "step-1"

        live.reduce(record: recorded(1, .object([
            "type": .string("assistant_content_delta"),
            "sessionId": .string("chat-1"),
            "turnId": .string("turn-1"),
            "modelStepId": .string(stepID),
            "phase": .string("commentary"),
            "delta": .string("First"),
        ])))
        // Deltas are batched, so the streamed row exists a flush later, not on the record.
        let streamed = await eventually {
            live.transcript.map(\.presentationID) == ["step-1:commentary:0"]
        }
        XCTAssertTrue(streamed)

        let completion = recorded(2, .object([
            "type": .string("model_step_completed"),
            "sessionId": .string("chat-1"),
            "turnId": .string("turn-1"),
            "modelStepId": .string(stepID),
            "stepIndex": .number(0),
            "startedAtMs": .number(100),
            "completedAtMs": .number(200),
            "outcome": .object([
                "status": .string("completed"),
                "endTurn": .bool(false),
                "toolCallIds": .array([]),
                "usage": .object([
                    "inputTokens": .number(1),
                    "cachedInputTokens": .number(0),
                    "cacheWriteInputTokens": .number(0),
                    "outputTokens": .number(2),
                    "reasoningOutputTokens": .number(0),
                    "totalTokens": .number(3),
                ]),
            ]),
        ]))
        let snapshot = recorded(3, .object([
            "type": .string("assistant_message"),
            "sessionId": .string("chat-1"),
            "turnId": .string("turn-1"),
            "modelStepId": .string(stepID),
            "content": .array([
                .object([
                    "outputIndex": .number(0),
                    "partIndex": .number(0),
                    "phase": .string("commentary"),
                    "text": .string("First"),
                    "annotations": .array([]),
                ]),
                .object([
                    "outputIndex": .number(0),
                    "partIndex": .number(1),
                    "phase": .string("commentary"),
                    "text": .string("Second"),
                    "annotations": .array([]),
                ]),
                .object([
                    "outputIndex": .number(1),
                    "partIndex": .number(0),
                    "phase": .string("final_answer"),
                    "text": .string("Done"),
                    "annotations": .array([]),
                ]),
            ]),
            "messageTarget": .null,
        ]))

        live.reduce(record: completion)
        replay.reduce(record: completion)
        live.reduce(record: snapshot)
        replay.reduce(record: snapshot)

        let expected = [
            "step-1:commentary:0",
            "step-1:commentary:1",
            "step-1:final_answer:0",
        ]
        XCTAssertEqual(live.transcript.map(\.presentationID), expected)
        XCTAssertEqual(replay.transcript.map(\.presentationID), expected)
        XCTAssertEqual(Set(expected).count, expected.count)
    }

    func testNonNarrativePresentationIdentityDefaultsToRecordIdentity() {
        let entry = TranscriptEntry(
            id: "event:1",
            text: "Compiled",
            kind: .event,
            format: "plain_text",
            pending: false
        )

        XCTAssertEqual(entry.presentationID, entry.id)
    }

    func testFailedModelStepKeepsItsPartialDelta() throws {
        let model = try model()
        model.reduce(record: recorded(1, .object([
            "type": .string("assistant_content_delta"),
            "sessionId": .string("chat-1"),
            "turnId": .string("turn-1"),
            "modelStepId": .string("step-1"),
            "phase": .string("reasoning"),
            "delta": .string("Partial reasoning"),
        ])))
        model.reduce(record: recorded(2, .object([
            "type": .string("model_step_completed"),
            "sessionId": .string("chat-1"),
            "turnId": .string("turn-1"),
            "modelStepId": .string("step-1"),
            "stepIndex": .number(0),
            "startedAtMs": .number(100),
            "completedAtMs": .number(200),
            "outcome": .object(["status": .string("failed")]),
        ])))

        XCTAssertEqual(model.transcript.map(\.text), ["Partial reasoning"])
        XCTAssertFalse(try XCTUnwrap(model.transcript.first).pending)
    }

    func testRetryingModelStepSeparatesPartialOutputAndClosesSearch() throws {
        let model = try model()
        model.reduce(record: recorded(1, .object([
            "type": .string("assistant_content_delta"),
            "sessionId": .string("chat-1"),
            "turnId": .string("turn-1"),
            "modelStepId": .string("step-1"),
            "delta": .string("Partial answer"),
            "phase": .string("final_answer"),
        ])))
        model.reduce(record: recorded(2, .object([
            "type": .string("web_search_begin"),
            "sessionId": .string("chat-1"),
            "turnId": .string("turn-1"),
            "modelStepId": .string("step-1"),
            "callId": .string("search-1"),
        ]), blocks: [RenderedBlock(capability: "web_search", block: FrontendBlock(
            id: "step-1/search-1",
            group: "turn-1",
            update: .replace,
            state: .pending,
            role: .webSearch,
            title: "Searching the web",
            text: "",
            symbol: "search",
            format: "plain_text",
            tone: "neutral",
            files: []
        ))]))
        model.reduce(record: recorded(3, .object([
            "type": .string("web_search_end"),
            "sessionId": .string("chat-1"),
            "turnId": .string("turn-1"),
            "modelStepId": .string("step-1"),
            "callId": .string("search-1"),
            "action": .object(["type": .string("interrupted")]),
        ]), blocks: [RenderedBlock(capability: "web_search", block: FrontendBlock(
            id: "step-1/search-1",
            group: "turn-1",
            update: .replace,
            state: .complete,
            role: .webSearch,
            title: "Web search interrupted",
            text: "",
            symbol: "search",
            format: "plain_text",
            tone: "warning",
            files: []
        ))]))
        model.reduce(record: recorded(4, .object([
            "type": .string("model_step_completed"),
            "sessionId": .string("chat-1"),
            "turnId": .string("turn-1"),
            "modelStepId": .string("step-1"),
            "stepIndex": .number(0),
            "startedAtMs": .number(100),
            "completedAtMs": .number(300),
            "outcome": .object(["status": .string("retrying")]),
        ]), blocks: [RenderedBlock(capability: "agent", block: FrontendBlock(
            id: "step-1/retry",
            group: "turn-1",
            update: .replace,
            state: .complete,
            role: .notice,
            title: "Reconnecting…",
            text: "",
            symbol: nil,
            format: "plain_text",
            tone: "warning",
            files: []
        ))]))

        let partial = try XCTUnwrap(model.transcript.first(where: {
            $0.modelStepID == "step-1" && $0.kind == .assistant
        }))
        let search = try XCTUnwrap(model.transcript.first(where: {
            $0.capability == "web_search"
        }))
        let reconnecting = try XCTUnwrap(model.transcript.first(where: {
            $0.title == "Reconnecting…"
        }))
        XCTAssertFalse(partial.pending)
        XCTAssertEqual(partial.tone, "warning")
        XCTAssertFalse(search.pending)
        XCTAssertEqual(search.title, "Web search interrupted")
        XCTAssertEqual(search.tone, "warning")
        XCTAssertLessThan(
            try XCTUnwrap(model.transcript.firstIndex(where: { $0 === partial })),
            try XCTUnwrap(model.transcript.firstIndex(where: { $0 === reconnecting }))
        )
    }

}
