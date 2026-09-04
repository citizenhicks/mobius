import Foundation
import Observation
import XCTest

@MainActor
extension AppModelTests {
    func testQueuedWidgetEditIsTakenBeforeTheComposerResubmitsFreshMessage() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        let target = MessageTarget(checkpointSequence: 12, batchItemCount: 3)
        let queued = MountedWidget(
            capability: "notes",
            widget: FrontendWidget(
                id: "queued",
                slot: .transcriptTail,
                text: "Queued note",
                tone: "neutral",
                symbol: nil,
                iconOnly: false,
                progress: nil,
                content: nil,
                action: .capabilityCommand(
                    capability: "notes",
                    command: "edit",
                    arguments: "note-1",
                    input: "Original input",
                    target: target
                )
            )
        )
        let sibling = MountedWidget(
            capability: "notes",
            widget: FrontendWidget(
                id: "sibling",
                slot: .transcriptTail,
                text: "Another queued note",
                tone: "neutral",
                symbol: nil,
                iconOnly: false,
                progress: nil,
                content: nil,
                action: nil
            )
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.activeTurnID = "turn-1"
        model.mountedWidgets = [queued, sibling]
        model.composer = "Keep this draft"
        let focusRequest = model.composerFocusRequest

        var requestCount = await recorder.requestCount()
        model.editWidgetInputInComposer(queued)
        let editRequest = await recorder.firstRequest(after: requestCount) { request in
            if case .submit = request { return true }
            return false
        }
        guard case .submit(let sessionID, let editSubmission) = try XCTUnwrap(editRequest),
              case .capabilityCommand(
                  let capability,
                  let command,
                  let arguments,
                  let input,
                  let submittedTarget
              ) = editSubmission.op
        else { return XCTFail("Expected the queued capability operation") }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(capability, "notes")
        XCTAssertEqual(command, "edit")
        XCTAssertEqual(arguments, "note-1")
        XCTAssertEqual(input, "Original input")
        XCTAssertEqual(submittedTarget, target)

        model.handle(.accepted(requestID: editSubmission.id))
        XCTAssertEqual(model.composer, "Keep this draft")
        XCTAssertFalse(model.canSendComposer)
        XCTAssertEqual(model.composerFocusRequest, focusRequest)
        XCTAssertEqual(model.transcriptTailWidgets.map(\.id), [queued.id, sibling.id])

        model.reduce(
            event: AgentEventRecord(submissionId: editSubmission.id, msg: .object([
                "type": .string("frontend"),
                "frontendType": .string("remove_widget"),
                "capability": .string("notes"),
                "id": .string("queued")
            ])),
            blocks: [],
            preview: nil
        )
        XCTAssertEqual(model.composer, "Original input")
        XCTAssertTrue(model.canSendComposer)
        XCTAssertEqual(model.composerFocusRequest, focusRequest + 1)
        XCTAssertEqual(model.transcriptTailWidgets.map(\.id), [sibling.id])

        model.composer = "Edited input"
        requestCount = await recorder.requestCount()
        model.sendMessage()
        let editedRequest = await recorder.firstRequest(after: requestCount) { request in
            if case .submit = request { return true }
            return false
        }
        let editedSubmission = try XCTUnwrap(editedRequest.flatMap { request -> Submission? in
            guard case .submit(_, let submission) = request else { return nil }
            return submission
        })
        guard case .message(let message) = editedSubmission.op
        else { return XCTFail("Expected fresh active message") }
        XCTAssertEqual(message.author, .user)
        XCTAssertNil(message.requestedDelivery)
        XCTAssertEqual(message.targetTurnId, "turn-1")
        XCTAssertEqual(message.text, "Edited input")
        XCTAssertEqual(model.composer, "Keep this draft")

        model.reduce(
            event: AgentEventRecord(submissionId: editedSubmission.id, msg: .object([
                "type": .string("turn_started"),
                "turnId": .string("turn-1")
            ])),
            blocks: [],
            preview: nil
        )
        XCTAssertFalse(model.canSendComposer)
        model.handle(.rejected(GatewayRejection(
            requestId: editedSubmission.id,
            code: "queue_full",
            message: "Try again",
            fatal: false
        )))
        XCTAssertEqual(model.composer, "Edited input")
        XCTAssertTrue(model.canSendComposer)
    }

    func testComposerEditRecoveryRestoresEditedTextAndDisplacedDraftAfterRelaunch() async throws {
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
            transcriptDirectory: root.appendingPathComponent("Transcripts"),
            draftDirectory: root.appendingPathComponent("Drafts")
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        await store.saveComposerDraft(
            ComposerDraft(text: "Displaced draft"),
            accountID: account.id,
            sessionID: "chat-1"
        )
        try await store.saveComposerEditRecovery(
            ComposerEditRecovery(
                capability: "notes",
                widgetID: "queued",
                originalInput: "Original input",
                displacedDraft: "Displaced draft",
                editedInput: "Edited after relaunch",
                requestID: "removed-input",
                submissionBaselineSequence: nil,
                phase: .editing
            ),
            accountID: account.id,
            sessionID: "chat-1"
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
        model.openSession("chat-1")
        try await Task.sleep(for: .milliseconds(30))
        let openRequests = await recorder.requests()
        let openRequest = try XCTUnwrap(openRequests.last)
        guard case .openSession(let openID, _, _) = openRequest else {
            return XCTFail("Expected session open")
        }
        model.handle(.sessionOpened(
            requestID: openID,
            payload: sessionReady(latestSequence: 0, sessionID: "chat-1")
        ))
        model.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))
        try await Task.sleep(for: .milliseconds(100))

        XCTAssertEqual(model.composer, "Edited after relaunch")
        XCTAssertTrue(model.canSendComposer)

        model.sendMessage()
        try await Task.sleep(for: .milliseconds(50))
        let submissions = await recorder.requests().compactMap { request -> Submission? in
            guard case .submit(_, let submission) = request else { return nil }
            return submission
        }
        guard case .message(let message) = try XCTUnwrap(submissions.last).op else {
            return XCTFail("Expected recovered user message")
        }
        XCTAssertEqual(message.text, "Edited after relaunch")
        XCTAssertEqual(model.composer, "Displaced draft")
    }

    func testComposerEditRecoveryRecognizesSubmissionReplayedBeforeItsDiskLoad() async throws {
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
            transcriptDirectory: root.appendingPathComponent("Transcripts"),
            draftDirectory: root.appendingPathComponent("Drafts")
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        await store.saveComposerDraft(
            ComposerDraft(text: "Displaced draft"),
            accountID: account.id,
            sessionID: "chat-1"
        )
        try await store.saveComposerEditRecovery(
            ComposerEditRecovery(
                capability: "notes",
                widgetID: "queued",
                originalInput: "Original input",
                displacedDraft: "Displaced draft",
                editedInput: "Edited input",
                requestID: "submitted-edit",
                submissionBaselineSequence: 10,
                phase: .submitting
            ),
            accountID: account.id,
            sessionID: "chat-1"
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
        let openRequestCount = await recorder.requestCount()
        model.openSession("chat-1")
        let openRequest = await recorder.firstRequest(after: openRequestCount) { request in
            guard case .openSession(_, "chat-1", _) = request else { return false }
            return true
        }
        guard case .openSession(let openID, _, _) = try XCTUnwrap(openRequest) else {
            return XCTFail("Expected session open")
        }

        model.handle(.sessionOpened(
            requestID: openID,
            payload: sessionReady(latestSequence: 11, sessionID: "chat-1")
        ))
        model.handle(.agentEvent(
            sessionID: "chat-1",
            sequence: 11,
            event: AgentEventRecord(
                submissionId: nil,
                msg: testMessageEvent(
                    text: "Edited input",
                    messageTarget: MessageTarget(checkpointSequence: 11, batchItemCount: 1)
                )
            ),
            blocks: [],
            history: nil,
            preview: nil
        ))
        model.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))
        try await Task.sleep(for: .milliseconds(150))

        XCTAssertEqual(model.composer, "Displaced draft")
        XCTAssertEqual(model.transcript.filter { $0.kind == .user }.map(\.text), ["Edited input"])
        XCTAssertTrue(model.canSendComposer)
        let replayedRecovery = await store.loadComposerEditRecovery(
            accountID: account.id,
            sessionID: "chat-1"
        )
        XCTAssertNil(replayedRecovery)
    }

    func testCompletedComposerEditTombstoneIsIgnoredAndOverwrittenByTheNextEdit() async throws {
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
            transcriptDirectory: root.appendingPathComponent("Transcripts"),
            draftDirectory: root.appendingPathComponent("Drafts")
        )
        let accountID = UUID()
        let completed = ComposerEditRecovery(
            capability: "notes",
            widgetID: "queued",
            originalInput: "Original",
            displacedDraft: "Draft",
            editedInput: "Edited",
            requestID: "submitted",
            submissionBaselineSequence: 7,
            phase: .completed
        )
        try await store.saveComposerEditRecovery(
            completed,
            accountID: accountID,
            sessionID: "chat-1"
        )
        let ignored = await store.loadComposerEditRecovery(
            accountID: accountID,
            sessionID: "chat-1"
        )
        XCTAssertNil(ignored)

        var next = completed
        next.requestID = "next-edit"
        next.submissionBaselineSequence = nil
        next.phase = .editing
        try await store.saveComposerEditRecovery(
            next,
            accountID: accountID,
            sessionID: "chat-1"
        )
        let restored = await store.loadComposerEditRecovery(
            accountID: accountID,
            sessionID: "chat-1"
        )
        XCTAssertEqual(restored, next)
    }

    func testForgettingGatewayInvalidatesItsInMemoryComposerEdit() async throws {
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
            transcriptDirectory: root.appendingPathComponent("Transcripts"),
            draftDirectory: root.appendingPathComponent("Drafts")
        )
        let recorder = GatewayRequestRecorder()
        let ordinaryMessageSent = expectation(description: "Ordinary message sent")
        let model = AppModel(
            client: GatewayClient(),
            store: store,
            settingsDefaults: defaults,
            requestSender: { request in
                await recorder.record(request)
                if case .submit(_, let submission) = request,
                   case .message(let message) = submission.op,
                   message.text == "New gateway message" {
                    ordinaryMessageSent.fulfill()
                }
            }
        )
        model.bots = [bot()]
        let first = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        let second = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9192"))
        try await beginComposerEdit(in: model, recorder: recorder, account: first)
        model.accounts = [first, second]

        let gatewayForgotten = expectation(description: "Gateway forgotten")
        withObservationTracking {
            _ = model.accounts
        } onChange: {
            gatewayForgotten.fulfill()
        }
        model.forgetGateway(first)
        await fulfillment(of: [gatewayForgotten], timeout: 1)
        model.selectedAccountID = second.id
        model.selectedSessionID = "chat-1"
        model.connectionState = .ready
        model.composer = "New gateway message"
        model.sendMessage()
        await fulfillment(of: [ordinaryMessageSent], timeout: 1)

        let submissions = await recorder.requests().compactMap { request -> Submission? in
            guard case .submit(_, let submission) = request else { return nil }
            return submission
        }
        guard case .message(let message) = try XCTUnwrap(submissions.last).op else {
            return XCTFail("Expected an ordinary new-gateway message")
        }
        XCTAssertEqual(message.text, "New gateway message")
    }

    func testDeletingSelectedSessionInvalidatesItsInMemoryComposerEdit() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in await recorder.record(request) })
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try await beginComposerEdit(in: model, recorder: recorder, account: account)
        let selected = session(sessionID: "chat-1", state: .idle)
        model.sessions = [selected]

        model.deleteSession(selected)
        try await Task.sleep(for: .milliseconds(30))
        let requests = await recorder.requests()
        guard case .deleteSessions(let deleteID, let ids) = try XCTUnwrap(
            requests.last(where: { if case .deleteSessions = $0 { true } else { false } })
        ), ids == ["chat-1"] else { return XCTFail("Expected session deletion") }
        model.handle(.accepted(requestID: deleteID))
        model.handle(.sessions(requestID: deleteID, sessions: []))

        model.selectedSessionID = "chat-1"
        model.connectionState = .ready
        model.composer = "Replacement message"
        model.sendMessage()
        try await Task.sleep(for: .milliseconds(30))

        let submissions = await recorder.requests().compactMap { request -> Submission? in
            guard case .submit(_, let submission) = request else { return nil }
            return submission
        }
        guard case .message(let message) = try XCTUnwrap(submissions.last).op else {
            return XCTFail("Expected an ordinary message after deletion")
        }
        XCTAssertEqual(message.text, "Replacement message")
    }

    func testSwitchingGatewayImmediatelyPersistsTheLatestComposerEdit() async throws {
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
            transcriptDirectory: root.appendingPathComponent("Transcripts"),
            draftDirectory: root.appendingPathComponent("Drafts")
        )
        let recorder = GatewayRequestRecorder()
        let model = AppModel(
            client: GatewayClient(),
            store: store,
            settingsDefaults: defaults,
            requestSender: { request in await recorder.record(request) }
        )
        model.bots = [bot()]
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        let second = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9192"))
        model.accounts = [account, second]
        model.selectedAccountID = account.id
        model.connectionState = .ready
        let openRequestCount = await recorder.requestCount()
        model.openSession("chat-1")
        let openRequest = await recorder.firstRequest(after: openRequestCount) { request in
            guard case .openSession(_, "chat-1", _) = request else { return false }
            return true
        }
        guard case .openSession(let openID, _, _) = try XCTUnwrap(openRequest) else {
            return XCTFail("Expected session open")
        }
        model.handle(.sessionOpened(
            requestID: openID,
            payload: sessionReady(latestSequence: 0, sessionID: "chat-1")
        ))
        model.handle(.sessionReplayComplete(requestID: openID, sessionID: "chat-1"))
        let sessionReady = await eventually { model.canCreateSession }
        XCTAssertTrue(sessionReady)
        try await beginComposerEdit(in: model, recorder: recorder, account: account)
        model.accounts = [account, second]

        model.composer = "Latest edit before switching"
        XCTAssertEqual(model.composer, "Latest edit before switching")
        XCTAssertTrue(model.canSendComposer)
        model.selectAccount(second.id)
        XCTAssertEqual(model.composer, "")

        let recoverySaved = await eventually {
            await store.loadComposerEditRecovery(
                accountID: account.id,
                sessionID: "chat-1"
            )?.editedInput == "Latest edit before switching"
        }
        XCTAssertTrue(recoverySaved)
        let recovery = await store.loadComposerEditRecovery(
            accountID: account.id,
            sessionID: "chat-1"
        )
        XCTAssertEqual(recovery?.editedInput, "Latest edit before switching")
    }

    func testSendMessageCannotBypassConnectionOrPendingWidgetEdit() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.selectedSessionID = "chat-1"
        model.composer = "Do not lose this"
        model.contributions = [fileAttachmentContribution()]

        model.sendMessage()
        try await Task.sleep(for: .milliseconds(20))
        let disconnectedRequests = await recorder.requests()
        XCTAssertTrue(disconnectedRequests.isEmpty)
        XCTAssertEqual(model.composer, "Do not lose this")

        model.connectionState = .ready
        XCTAssertTrue(model.canImportAttachments)
        model.editWidgetInputInComposer(editableWidget())
        XCTAssertFalse(model.canImportAttachments)
        let requestCount = await recorder.requestCount()
        model.sendMessage()
        let request = await recorder.firstRequest(after: requestCount) { request in
            guard case .submit(_, let submission) = request,
                  case .capabilityCommand = submission.op
            else { return false }
            return true
        }

        let requests = await recorder.requests()
        XCTAssertEqual(requests.count, requestCount + 1)
        guard case .submit(_, let submission) = try XCTUnwrap(request),
              case .capabilityCommand = submission.op
        else { return XCTFail("Expected only the edit-removal command") }
        XCTAssertEqual(model.composer, "Do not lose this")
    }

}
