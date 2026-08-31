import Foundation
import Observation
import XCTest

@MainActor
extension AppModelTests {
    func testDelayedGeneratedTitleDoesNotOverwriteAnExplicitRename() async throws {
        let recorder = GatewayRequestRecorder()
        let writer = ChatTitleWriter { _ in
            try? await Task.sleep(for: .milliseconds(100))
            return "Generated title"
        }
        let model = try model(
            requestSender: { request in await recorder.record(request) },
            titleWriter: writer
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try await openNewSession(in: model, recorder: recorder, account: account)

        XCTAssertEqual(model.currentSessionTitle, "new conversation")

        try await submitMessage("Review the gateway", in: model, recorder: recorder)
        model.applySessions([session(
            state: .running,
            firstUserMessage: "Review the gateway",
            title: "Manual title"
        )])
        try await Task.sleep(for: .milliseconds(150))

        let requests = await recorder.requests()
        XCTAssertFalse(requests.contains {
            if case .renameSession = $0 { return true }
            return false
        })
    }

    func testGeneratedTitleAppearsAndPersistsAfterTheDurableUserMessage() async throws {
        let recorder = GatewayRequestRecorder()
        let writer = ChatTitleWriter { _ in "Generated title" }
        let model = try model(
            requestSender: { request in await recorder.record(request) },
            titleWriter: writer
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try await openNewSession(in: model, recorder: recorder, account: account)

        let submission = try await submitMessage(
            "Review the gateway",
            in: model,
            recorder: recorder
        )

        XCTAssertEqual(model.currentSessionTitle, "Generated title")
        let earlyRequests = await recorder.requests()
        XCTAssertFalse(earlyRequests.contains {
            if case .renameSession = $0 { return true }
            return false
        })

        model.reduce(
            event: AgentEventRecord(
                submissionId: submission.id,
                msg: testMessageEvent(text: "Review the gateway")
            ),
            blocks: [],
            preview: nil
        )
        try await Task.sleep(for: .milliseconds(30))

        let rename = await recorder.requests().first { request in
            guard case .renameSession(_, "chat-1", "Generated title") = request else {
                return false
            }
            return true
        }
        XCTAssertNotNil(rename)

        model.applySessions([session(
            state: .running,
            firstUserMessage: "Review the gateway",
            title: "Generated title"
        )])

        XCTAssertEqual(model.currentSessionTitle, "Generated title")
    }

    func testRestoredUntitledDraftStillGeneratesTitleAfterNavigation() async throws {
        let recorder = GatewayRequestRecorder()
        var prompts: [String] = []
        let model = try model(
            requestSender: { request in await recorder.record(request) },
            titleWriter: ChatTitleWriter { prompt in
                prompts.append(prompt)
                return "Generated title"
            }
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try await openNewSession(in: model, recorder: recorder, account: account)
        model.applySessions([session(state: .idle, firstUserMessage: nil)])
        model.titleEligibleSessionIDs.removeAll()
        model.composer = "Review the gateway"
        model.destination = .profile
        model.navigationPath = []
        model.destination = .chats
        model.openChat("chat-1")

        try await submitMessage("Review the gateway", in: model, recorder: recorder)
        model.startChatTitle(
            prompt: "Use the second prompt instead",
            submissionID: "second-submission",
            sessionID: "chat-1"
        )

        XCTAssertEqual(prompts, ["Review the gateway"])
        XCTAssertEqual(model.currentSessionTitle, "Generated title")
    }

    func testGeneratedTitlePersistsWhenTheCatalogTruncatesALongFirstMessage() async throws {
        let recorder = GatewayRequestRecorder()
        let writer = ChatTitleWriter { _ in "Generated title" }
        let model = try model(
            requestSender: { request in await recorder.record(request) },
            titleWriter: writer
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try await openNewSession(in: model, recorder: recorder, account: account)
        let prompt = String(repeating: "review this code carefully ", count: 30)

        try await submitMessage(prompt, in: model, recorder: recorder)
        model.applySessions([session(
            state: .running,
            firstUserMessage: String(decoding: prompt.utf8.prefix(512), as: UTF8.self)
        )])
        try await Task.sleep(for: .milliseconds(30))

        let rename = await recorder.requests().first { request in
            guard case .renameSession(_, "chat-1", "Generated title") = request else {
                return false
            }
            return true
        }
        XCTAssertNotNil(rename)
        XCTAssertEqual(model.currentSessionTitle, "Generated title")
    }

    func testGeneratedTitleWaitsForItsOwnDurableUserMessage() async throws {
        let recorder = GatewayRequestRecorder()
        let writer = ChatTitleWriter { _ in "Generated title" }
        let model = try model(
            requestSender: { request in await recorder.record(request) },
            titleWriter: writer
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try await openNewSession(in: model, recorder: recorder, account: account)
        let submission = try await submitMessage(
            "Review the gateway",
            in: model,
            recorder: recorder
        )

        model.reduce(
            event: AgentEventRecord(
                submissionId: "another-submission",
                msg: testMessageEvent(text: "Another message")
            ),
            blocks: [],
            preview: nil
        )
        try await Task.sleep(for: .milliseconds(30))
        let requestsAfterWrongMessage = await recorder.requests()
        XCTAssertFalse(requestsAfterWrongMessage.contains {
            if case .renameSession = $0 { return true }
            return false
        })

        model.reduce(
            event: AgentEventRecord(
                submissionId: submission.id,
                msg: testMessageEvent(text: "Review the gateway")
            ),
            blocks: [],
            preview: nil
        )
        try await Task.sleep(for: .milliseconds(30))

        let requestsAfterMatchingMessage = await recorder.requests()
        XCTAssertTrue(requestsAfterMatchingMessage.contains {
            guard case .renameSession(_, "chat-1", "Generated title") = $0 else {
                return false
            }
            return true
        })
    }

    func testGeneratedTitlePersistsWhenUserMessageArrivesBeforeGeneration() async throws {
        let recorder = GatewayRequestRecorder()
        let titleStarted = expectation(description: "Title generation started")
        let titleGate = AsyncGate()
        let writer = ChatTitleWriter { _ in
            titleStarted.fulfill()
            await titleGate.wait()
            return "Generated title"
        }
        let model = try model(
            requestSender: { request in await recorder.record(request) },
            titleWriter: writer
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try await openNewSession(in: model, recorder: recorder, account: account)
        let submission = try await submitMessage(
            "Review the gateway",
            in: model,
            recorder: recorder
        )
        await fulfillment(of: [titleStarted], timeout: 1)
        XCTAssertEqual(model.currentSessionTitle, "Review the gateway")

        let requestCount = await recorder.requestCount()
        model.reduce(
            event: AgentEventRecord(
                submissionId: submission.id,
                msg: testMessageEvent(text: "Review the gateway")
            ),
            blocks: [],
            preview: nil
        )
        await titleGate.open()

        let rename = await recorder.firstRequest(after: requestCount) { request in
            guard case .renameSession(_, "chat-1", "Generated title") = request else {
                return false
            }
            return true
        }
        XCTAssertNotNil(rename)
    }

    func testReplayedFirstMessagePersistsThePendingTitle() async throws {
        let recorder = GatewayRequestRecorder()
        let titleStarted = expectation(description: "Title generation started")
        let titleGate = AsyncGate()
        let model = try model(
            requestSender: { request in await recorder.record(request) },
            titleWriter: ChatTitleWriter { _ in
                titleStarted.fulfill()
                await titleGate.wait()
                return "Generated title"
            }
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try await openNewSession(in: model, recorder: recorder, account: account)
        try await submitMessage("Review the gateway", in: model, recorder: recorder)
        await fulfillment(of: [titleStarted], timeout: 1)

        let titleGenerated = expectation(description: "Generated title applied")
        withObservationTracking {
            _ = model.currentSessionTitle
        } onChange: {
            titleGenerated.fulfill()
        }
        await titleGate.open()
        await fulfillment(of: [titleGenerated], timeout: 1)
        XCTAssertEqual(model.currentSessionTitle, "Generated title")

        let requestCount = await recorder.requestCount()
        model.restoreSession("chat-1")
        let open = await recorder.firstRequest(after: requestCount) { request in
            guard case .openSession(_, "chat-1", _) = request else { return false }
            return true
        }
        guard case .openSession(let requestID, _, _) = try XCTUnwrap(open) else {
            return XCTFail("Expected the selected chat to reopen")
        }
        model.handle(.sessionOpened(
            requestID: requestID,
            payload: sessionReady(latestSequence: 1)
        ))
        model.handle(.agentEvent(
            sessionID: "chat-1",
            sequence: 1,
            event: AgentEventRecord(
                submissionId: nil,
                msg: testMessageEvent(
                    text: "Review the gateway",
                    messageTarget: MessageTarget(checkpointSequence: 1, batchItemCount: 1)
                )
            ),
            blocks: [],
            history: nil,
            preview: nil
        ))
        let replayRequestCount = await recorder.requestCount()
        model.handle(.sessionReplayComplete(requestID: requestID, sessionID: "chat-1"))

        let rename = await recorder.firstRequest(after: replayRequestCount) { request in
            guard case .renameSession(_, "chat-1", "Generated title") = request else {
                return false
            }
            return true
        }
        XCTAssertNotNil(rename)
        XCTAssertEqual(model.currentSessionTitle, "Generated title")
    }

    func testReconnectRearmsTitleWhenTheSubmissionWasNotDurable() async throws {
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
        try store.save(account, token: "test-token")
        addTeardownBlock { try await store.remove(account) }
        let harness = GatewayConnectionHarness()
        let recorder = GatewayRequestRecorder()
        var prompts: [String] = []
        let firstTitleGenerated = expectation(description: "Initial title generated")
        let secondTitleGenerated = expectation(description: "Rearmed title generated")
        let model = AppModel(
            client: GatewayClient(),
            store: store,
            settingsDefaults: defaults,
            requestSender: { request in await recorder.record(request) },
            connectionOpener: { endpoint in try await harness.open(endpoint) },
            reconnectDelay: { _ in .zero },
            titleWriter: ChatTitleWriter { prompt in
                prompts.append(prompt)
                if prompts.count == 1 { firstTitleGenerated.fulfill() }
                if prompts.count == 2 { secondTitleGenerated.fulfill() }
                return "Generated title"
            }
        )
        await model.appDidBecomeActive()

        await model.start()
        let initialConnectionOpened = await eventually { await harness.attemptCount() >= 2 }
        XCTAssertTrue(initialConnectionOpened)
        await harness.yield(.authenticated)
        await harness.yield(.ready(ready(
            defaultConfig: VersionedAgentConfig(revision: 1, config: composition()),
            sessions: []
        )))
        let gatewayReady = await eventually { model.connectionState.isReady }
        XCTAssertTrue(gatewayReady)
        try await openNewSession(in: model, recorder: recorder, account: account)
        try await submitMessage("Review the gateway", in: model, recorder: recorder)
        await fulfillment(of: [firstTitleGenerated], timeout: 1)
        XCTAssertEqual(model.currentSessionTitle, "Generated title")

        let reconnectRequestCount = await recorder.requestCount()
        await harness.fail()
        let reconnectOpened = await eventually { await harness.attemptCount() >= 3 }
        XCTAssertTrue(reconnectOpened)
        let attemptCount = await harness.attemptCount()
        XCTAssertGreaterThanOrEqual(attemptCount, 3)
        await harness.yield(.authenticated)
        await harness.yield(.ready(ready(
            defaultConfig: VersionedAgentConfig(revision: 1, config: composition()),
            sessions: [session(state: .idle, firstUserMessage: nil)]
        )))
        let reconnectRequest = await recorder.firstRequest(
            after: reconnectRequestCount
        ) { request in
            guard case .openSession(_, "chat-1", _) = request else { return false }
            return true
        }
        let reconnectOpen = try XCTUnwrap(reconnectRequest)
        guard case .openSession(let requestID, _, _) = reconnectOpen else {
            return XCTFail("Expected the selected chat to reopen")
        }
        await harness.yield(.sessionOpened(
            requestID: requestID,
            payload: sessionReady(latestSequence: 0)
        ))
        await harness.yield(.sessionReplayComplete(requestID: requestID, sessionID: "chat-1"))
        let replayFinished = await eventually { model.canCreateSession }
        XCTAssertTrue(replayFinished)
        XCTAssertEqual(model.currentSessionTitle, "new conversation")
        XCTAssertEqual(model.composer, "Review the gateway")
        try await submitMessage("Review the gateway", in: model, recorder: recorder)
        await fulfillment(of: [secondTitleGenerated], timeout: 1)
        XCTAssertEqual(prompts, ["Review the gateway", "Review the gateway"])
        XCTAssertEqual(model.currentSessionTitle, "Generated title")
    }

    func testRejectedFirstMessageRearmsAutomaticTitleGeneration() async throws {
        let recorder = GatewayRequestRecorder()
        var prompts: [String] = []
        let writer = ChatTitleWriter { prompt in
            prompts.append(prompt)
            return "Generated title"
        }
        let model = try model(
            requestSender: { request in await recorder.record(request) },
            titleWriter: writer
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try await openNewSession(in: model, recorder: recorder, account: account)

        let firstSubmission = try await submitMessage(
            "Review the gateway",
            in: model,
            recorder: recorder
        )
        XCTAssertEqual(model.currentSessionTitle, "Generated title")

        model.handle(.rejected(GatewayRejection(
            requestId: firstSubmission.id,
            code: "rejected",
            message: "Try again",
            fatal: false
        )))
        XCTAssertEqual(model.currentSessionTitle, "new conversation")
        XCTAssertEqual(model.composer, "Review the gateway")

        try await submitMessage("Review the gateway again", in: model, recorder: recorder)

        XCTAssertEqual(prompts, ["Review the gateway", "Review the gateway again"])
        XCTAssertEqual(model.currentSessionTitle, "Generated title")
    }

    func testRejectedFirstMessageRearmsAfterTitleGenerationReturnsNil() async throws {
        let recorder = GatewayRequestRecorder()
        var prompts: [String] = []
        let writer = ChatTitleWriter { prompt in
            prompts.append(prompt)
            return nil
        }
        let model = try model(
            requestSender: { request in await recorder.record(request) },
            titleWriter: writer
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try await openNewSession(in: model, recorder: recorder, account: account)

        let firstTitleFinished = expectation(description: "First title generation finished")
        withObservationTracking {
            _ = model.toast
        } onChange: {
            firstTitleFinished.fulfill()
        }
        let firstSubmission = try await submitMessage(
            "Review the gateway",
            in: model,
            recorder: recorder
        )
        await fulfillment(of: [firstTitleFinished], timeout: 1)

        model.handle(.rejected(GatewayRejection(
            requestId: firstSubmission.id,
            code: "rejected",
            message: "Try again",
            fatal: false
        )))
        let secondTitleFinished = expectation(description: "Second title generation finished")
        withObservationTracking {
            _ = model.toast
        } onChange: {
            secondTitleFinished.fulfill()
        }
        try await submitMessage("Review the gateway again", in: model, recorder: recorder)
        await fulfillment(of: [secondTitleFinished], timeout: 1)

        XCTAssertEqual(prompts, ["Review the gateway", "Review the gateway again"])
        XCTAssertEqual(model.currentSessionTitle, "Review the gateway again")
    }

    func testPromptPreviewRemainsWhenAppleDoesNotProduceATitle() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(
            requestSender: { request in await recorder.record(request) },
            titleWriter: ChatTitleWriter { _ in nil }
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try await openNewSession(in: model, recorder: recorder, account: account)

        let submission = try await submitMessage(
            "Review the gateway retry behavior",
            in: model,
            recorder: recorder
        )
        XCTAssertEqual(model.currentSessionTitle, "Review the gateway retry behavior")
        XCTAssertEqual(model.toast?.message, "Apple did not produce a chat title.")
        XCTAssertEqual(model.toast?.tone, .warning)

        model.reduce(
            event: AgentEventRecord(
                submissionId: submission.id,
                msg: testMessageEvent(text: "Review the gateway retry behavior")
            ),
            blocks: [],
            preview: nil
        )
        model.applySessions([session(
            state: .running,
            firstUserMessage: "Review the gateway retry behavior"
        )])

        XCTAssertEqual(model.currentSessionTitle, "Review the gateway retry behavior")
        let requests = await recorder.requests()
        XCTAssertFalse(requests.contains {
            if case .renameSession = $0 { return true }
            return false
        })
    }

    func testAcceptedDeleteCancelsPendingTitleGeneration() async throws {
        let recorder = GatewayRequestRecorder()
        let deleteSent = expectation(description: "Delete request sent")
        let titleCancelled = expectation(description: "Title generation cancelled")
        let writer = ChatTitleWriter { _ in
            do {
                try await Task.sleep(for: .seconds(10))
            } catch is CancellationError {
                titleCancelled.fulfill()
            } catch {
                return nil
            }
            return "Generated title"
        }
        let model = try model(
            requestSender: { request in
                await recorder.record(request)
                if case .deleteSession = request { deleteSent.fulfill() }
            },
            titleWriter: writer
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try await openNewSession(in: model, recorder: recorder, account: account)

        try await submitMessage("Review the gateway", in: model, recorder: recorder)
        let chat = session(state: .running, firstUserMessage: "Review the gateway")
        model.applySessions([chat])
        model.deleteSession(chat)
        await fulfillment(of: [deleteSent], timeout: 1)
        let deleteIDs = await recorder.requests().compactMap { request -> String? in
            guard case .deleteSession(let requestID, "chat-1") = request else { return nil }
            return requestID
        }
        let deleteID = try XCTUnwrap(deleteIDs.last)

        model.handle(.accepted(requestID: deleteID))
        await fulfillment(of: [titleCancelled], timeout: 1)

        let requests = await recorder.requests()
        XCTAssertFalse(requests.contains {
            if case .renameSession = $0 { return true }
            return false
        })
    }

    func testOpeningAnotherNewChatDoesNotCancelTheFirstTitle() async throws {
        let recorder = GatewayRequestRecorder()
        let firstTitleStarted = expectation(description: "First title generation started")
        let secondTitleStarted = expectation(description: "Second title generation started")
        let firstTitleGate = AsyncGate()
        let secondTitleGate = AsyncGate()
        let writer = ChatTitleWriter { prompt in
            if prompt == "First prompt" {
                firstTitleStarted.fulfill()
                await firstTitleGate.wait()
                return "First title"
            }
            secondTitleStarted.fulfill()
            await secondTitleGate.wait()
            return "Second title"
        }
        let model = try model(
            requestSender: { request in await recorder.record(request) },
            titleWriter: writer
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try await openNewSession(
            in: model,
            recorder: recorder,
            account: account,
            sessionID: "chat-1"
        )

        let firstSubmission = try await submitMessage(
            "First prompt",
            in: model,
            recorder: recorder,
            sessionID: "chat-1"
        )
        await fulfillment(of: [firstTitleStarted], timeout: 1)
        model.reduce(
            event: AgentEventRecord(
                submissionId: firstSubmission.id,
                msg: testMessageEvent(text: "First prompt")
            ),
            blocks: [],
            preview: nil
        )
        try await openNewSession(
            in: model,
            recorder: recorder,
            account: account,
            sessionID: "chat-2"
        )

        try await submitMessage(
            "Second prompt",
            in: model,
            recorder: recorder,
            sessionID: "chat-2"
        )
        await fulfillment(of: [secondTitleStarted], timeout: 1)

        let secondTitleGenerated = expectation(description: "Second title applied")
        withObservationTracking {
            _ = model.currentSessionTitle
        } onChange: {
            secondTitleGenerated.fulfill()
        }
        await secondTitleGate.open()
        await fulfillment(of: [secondTitleGenerated], timeout: 1)
        XCTAssertEqual(model.currentSessionTitle, "Second title")

        let requestCount = await recorder.requestCount()
        await firstTitleGate.open()
        let firstRename = await recorder.firstRequest(after: requestCount) { request in
            guard case .renameSession(_, "chat-1", "First title") = request else {
                return false
            }
            return true
        }
        XCTAssertNotNil(firstRename)

        XCTAssertEqual(
            model.displayedTitle(for: session(
                sessionID: "chat-1",
                state: .running,
                firstUserMessage: "First prompt"
            )),
            "First title"
        )
    }
}
