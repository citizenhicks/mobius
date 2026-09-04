import Foundation
import XCTest

@MainActor
extension AppModelTests {
    func testReplySubmissionRestoresAndNavigatesToItsDurableTarget() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        let target = MessageTarget(checkpointSequence: 7, batchItemCount: 2)
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.reduce(
            event: AgentEventRecord(
                submissionId: "original",
                msg: testMessageEvent(text: "Earlier message", messageTarget: target)
            ),
            blocks: [],
            preview: nil
        )

        model.activeTurnID = "turn-active"
        model.composerAttachments = [ComposerAttachment(
            id: UUID(),
            name: "context.txt",
            size: 7,
            mediaType: "text/plain",
            state: .uploaded(SessionFileReference(
                id: "file-1",
                name: "context.txt",
                size: 7,
                mediaType: "text/plain"
            ))
        )]
        model.beginReplying(to: try XCTUnwrap(model.transcript.first))
        let reply = try XCTUnwrap(model.composerReply)
        model.composerAttachments = []
        model.composer = "Focused response"
        XCTAssertTrue(model.sendMessage())

        let request = await recorder.firstRequest(after: 0) { request in
            if case .submit = request { return true }
            return false
        }
        guard case .submit(_, let submission) = try XCTUnwrap(request),
              case .message(let message) = submission.op
        else { return XCTFail("Expected reply message submission") }
        XCTAssertEqual(message.reply, reply)
        XCTAssertNil(model.composerReply)

        model.openMessageReply(reply)
        let firstNavigationID = try XCTUnwrap(model.messageNavigationRequest?.id)
        model.openMessageReply(reply)
        XCTAssertNotEqual(model.messageNavigationRequest?.id, firstNavigationID)
        XCTAssertEqual(model.messageNavigationRequest?.target, target)

        model.composer = "New draft"
        model.composerReply = MessageReply(
            target: MessageTarget(checkpointSequence: 9, batchItemCount: 1),
            text: "Different original"
        )
        model.handle(.rejected(GatewayRejection(
            requestId: submission.id,
            code: "submission_rejected",
            message: "Try again",
            fatal: false
        )))
        XCTAssertEqual(model.composer, "Focused response\n\nNew draft")
        XCTAssertNil(model.composerReply)

        model.reduce(
            event: AgentEventRecord(
                submissionId: "reply",
                msg: testMessageEvent(
                    text: "Focused response",
                    reply: reply,
                    messageTarget: MessageTarget(checkpointSequence: 8, batchItemCount: 1)
                )
            ),
            blocks: [],
            preview: nil
        )
        XCTAssertEqual(model.transcript.last?.reply, reply)
    }

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

    func testActiveDeliveryDoesNotRequireAComposerControl() throws {
        let model = try model()
        var composition = composition()
        composition.middleware.settings["messages"] = ["delivery": .string("queue")]
        model.agentDraft = composition
        model.middlewareFeatures = [MiddlewareFeature(
            id: "messages",
            label: "Messages",
            description: "Message delivery",
            required: true,
            settings: [FrontendSetting(
                id: "delivery",
                label: "Delivery",
                description: "Active turn delivery",
                composer: false,
                kind: .select(options: [
                    FrontendSettingOption(
                        value: "steer",
                        label: "Steer",
                        description: "Steer now"
                    ),
                    FrontendSettingOption(
                        value: "queue",
                        label: "Queue",
                        description: "Run next"
                    ),
                ], unsetLabel: nil)
            )]
        )]

        XCTAssertEqual(model.activeMessageDelivery, .queue)
    }

}
