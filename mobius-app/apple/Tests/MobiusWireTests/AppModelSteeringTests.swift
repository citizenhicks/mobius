import Foundation
import XCTest

@MainActor
extension AppModelTests {
    func testSteeringDraftSettlesOnSuccessAndRestoresOnSubmissionRejection() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.activeTurnID = "turn-1"
        model.composer = "Use the smaller patch"

        var requestCount = await recorder.requestCount()
        model.sendMessage()
        let firstRequest = await recorder.firstRequest(after: requestCount) { request in
            if case .submit = request { return true }
            return false
        }
        let first = try XCTUnwrap(firstRequest.flatMap { request -> Submission? in
            guard case .submit(_, let submission) = request else { return nil }
            return submission
        })
        model.reduce(
            event: AgentEventRecord(submissionId: first.id, msg: .object([
                "type": .string("frontend"),
                "frontendType": .string("widget"),
                "capability": .string("messages"),
                "item": .object([
                    "id": .string(first.id),
                    "slot": .string("transcript_tail"),
                    "text": .string("Use the smaller patch"),
                    "tone": .string("neutral"),
                    "symbol": .null,
                    "iconOnly": .bool(false),
                    "progress": .null,
                    "content": .null,
                    "action": .object([
                        "type": .string("capability_command"),
                        "capability": .string("messages"),
                        "command": .string("edit"),
                        "arguments": .string(first.id),
                        "input": .string("Use the smaller patch"),
                        "target": .null
                    ])
                ])
            ])),
            blocks: [],
            preview: nil
        )
        model.handle(.rejected(GatewayRejection(
            requestId: "unrelated",
            code: "connection_failed",
            message: "Disconnected",
            fatal: true
        )))

        XCTAssertEqual(model.composer, "")
        XCTAssertEqual(model.transcriptTailWidgets.first?.widget.text, "Use the smaller patch")
        XCTAssertEqual(
            model.transcriptTailWidgets.first?.widget.action?.capabilityInput,
            "Use the smaller patch"
        )

        model.connectionState = .ready
        model.composer = "Retry this steering"
        requestCount = await recorder.requestCount()
        model.sendMessage()
        let secondRequest = await recorder.firstRequest(after: requestCount) { request in
            if case .submit = request { return true }
            return false
        }
        let second = try XCTUnwrap(secondRequest.flatMap { request -> Submission? in
            guard case .submit(_, let submission) = request else { return nil }
            return submission
        })
        model.reduce(
            event: AgentEventRecord(submissionId: second.id, msg: .object([
                "type": .string("submission_rejected"),
                "message": .string("Steering queue is full")
            ])),
            blocks: [],
            preview: nil
        )

        XCTAssertEqual(model.composer, "Retry this steering")
    }

    func testQueuedSteeringKeepsOneBubblePerMessageAndRemovesOnlyTheTarget() throws {
        let model = try model()
        for (id, text) in [("steer-1", "First"), ("steer-2", "Second")] {
            model.reduce(
                event: AgentEventRecord(submissionId: id, msg: .object([
                    "type": .string("frontend"),
                    "frontendType": .string("widget"),
                    "capability": .string("messages"),
                    "item": .object([
                        "id": .string(id),
                        "slot": .string("transcript_tail"),
                        "text": .string(text),
                        "tone": .string("neutral"),
                        "symbol": .null,
                        "iconOnly": .bool(false),
                        "progress": .null,
                        "content": .null,
                        "action": .null
                    ])
                ])),
                blocks: [],
                preview: nil
            )
        }

        XCTAssertEqual(model.transcriptTailWidgets.map(\.widget.text), ["First", "Second"])

        model.reduce(
            event: AgentEventRecord(submissionId: "input-1", msg: .object([
                "type": .string("frontend"),
                "frontendType": .string("remove_widget"),
                "capability": .string("messages"),
                "id": .string("steer-1")
            ])),
            blocks: [],
            preview: nil
        )

        XCTAssertEqual(model.transcriptTailWidgets.map(\.widget.text), ["Second"])

        model.reduce(
            event: AgentEventRecord(submissionId: "input-1", msg: .object([
                "type": .string("frontend"),
                "frontendType": .string("remove_widget"),
                "capability": .string("messages"),
                "id": .string("steer-2")
            ])),
            blocks: [],
            preview: nil
        )

        XCTAssertTrue(model.transcriptTailWidgets.isEmpty)
    }

    func testSteeringFeedbackFiresWhenTheMessageReachesModelInput() throws {
        let model = try model()
        model.reduce(
            event: AgentEventRecord(submissionId: "input-1", msg: .object([
                "type": .string("turn_started"),
                "turnId": .string("turn-1")
            ])),
            blocks: [],
            preview: nil
        )
        model.reduce(
            event: AgentEventRecord(
                submissionId: "input-1",
                msg: testMessageEvent(
                    text: "Start",
                    messageTarget: MessageTarget(checkpointSequence: 1, batchItemCount: 1)
                )
            ),
            blocks: [],
            preview: nil
        )
        model.reduce(
            event: AgentEventRecord(submissionId: "input-1", msg: .object([
                "type": .string("frontend"),
                "frontendType": .string("remove_widget"),
                "capability": .string("messages"),
                "id": .string("steering-1")
            ])),
            blocks: [],
            preview: nil
        )

        XCTAssertEqual(model.steeringDeliveryRevision, 0)

        model.reduce(
            event: AgentEventRecord(
                submissionId: "input-1",
                msg: testMessageEvent(
                    delivery: .steer,
                    text: "Use the smaller patch",
                    messageTarget: MessageTarget(checkpointSequence: 2, batchItemCount: 1)
                )
            ),
            blocks: [],
            preview: nil
        )

        XCTAssertEqual(model.steeringDeliveryRevision, 1)
    }

    func testActiveMessageOmitsMiddlewareDefaultAndAllowsTheOppositeOverride() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        var composition = composition()
        composition.middleware.settings["messages"] = ["delivery": .string("queue")]
        model.agentDraft = composition
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.activeTurnID = "turn-1"

        model.composer = "Handle this next"
        var requestCount = await recorder.requestCount()
        XCTAssertTrue(model.sendMessage())
        let queuedRequest = await recorder.firstRequest(after: requestCount) { request in
            if case .submit = request { return true }
            return false
        }
        guard case .submit(_, let queuedSubmission) = try XCTUnwrap(queuedRequest),
              case .message(let queuedMessage) = queuedSubmission.op
        else {
            return XCTFail("Expected a queued message submission")
        }
        XCTAssertEqual(queuedMessage.author, .user)
        XCTAssertNil(queuedMessage.requestedDelivery)
        XCTAssertEqual(queuedMessage.targetTurnId, "turn-1")

        model.composer = "Use this immediately"
        requestCount = await recorder.requestCount()
        XCTAssertTrue(model.sendMessage(delivery: .steer))
        let steeringRequest = await recorder.firstRequest(after: requestCount) { request in
            if case .submit = request { return true }
            return false
        }
        guard case .submit(_, let steeringSubmission) = try XCTUnwrap(steeringRequest),
              case .message(let steeringMessage) = steeringSubmission.op
        else {
            return XCTFail("Expected a steering message submission")
        }
        XCTAssertEqual(steeringMessage.requestedDelivery, .steer)
        XCTAssertEqual(steeringMessage.targetTurnId, "turn-1")

        model.activeTurnID = nil
        model.composer = "Start another turn"
        requestCount = await recorder.requestCount()
        XCTAssertTrue(model.sendMessage(delivery: .queue))
        let turnRequest = await recorder.firstRequest(after: requestCount) { request in
            if case .submit = request { return true }
            return false
        }
        guard case .submit(_, let turnSubmission) = try XCTUnwrap(turnRequest),
              case .message(let turnMessage) = turnSubmission.op
        else {
            return XCTFail("Expected a new-turn message submission")
        }
        XCTAssertNil(turnMessage.requestedDelivery)
        XCTAssertNil(turnMessage.targetTurnId)
    }

}
