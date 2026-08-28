import Foundation
import XCTest

@MainActor
extension AppModelTests {
    func testActiveRunAllowsSessionNavigationButNotSelectedSessionMutation() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.activeTurnID = "turn-1"
        model.pendingApproval = PendingApproval(
            id: "approval-1",
            reason: "Approve this tool?",
            calls: []
        )
        model.gitStatus = GitStatus(currentBranch: "main", branches: ["feature", "main"])

        XCTAssertTrue(model.canOpenSession)
        XCTAssertTrue(model.canCreateSession)
        XCTAssertFalse(model.canModifySelectedSession)

        model.switchGitBranch(to: "feature")
        model.openSession("chat-2")
        try await Task.sleep(for: .milliseconds(30))

        let requests = await recorder.requests()
        XCTAssertFalse(requests.contains { request in
            if case .switchGitBranch = request { return true }
            return false
        })
        XCTAssertTrue(requests.contains { request in
            guard case .openSession(_, "chat-2", _) = request else { return false }
            return true
        })
    }

    func testActiveRunAllowsCreatingAnotherSession() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.activeTurnID = "turn-1"

        model.chooseWorkspace("/srv/another-project")
        try await Task.sleep(for: .milliseconds(20))

        let requests = await recorder.requests()
        XCTAssertTrue(requests.contains { request in
            guard case .createSession(_, "/srv/another-project") = request else { return false }
            return true
        })
    }

    func testNewSessionInCurrentWorkspaceUsesWorkspacePath() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.connectionState = .ready
        model.workspace = WorkspaceInfo(
            id: "workspace-1",
            path: "/srv/current-project"
        )

        let requestCount = await recorder.requestCount()
        model.openNewSessionInCurrentWorkspace()

        let request = await recorder.firstRequest(after: requestCount) { request in
            guard case .createSession(_, "/srv/current-project") = request else { return false }
            return true
        }
        XCTAssertNotNil(request)
    }

    func testCreatingSwarmUsesEligibleChatsInSameFolderAndWaitsForCatalog() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let leader = session(sessionID: "chat-1", state: .idle)
        let coworker = session(sessionID: "chat-2", state: .running)
        let elsewhere = session(
            sessionID: "chat-3",
            state: .idle,
            workspaceID: "workspace-2",
            workspaceLabel: "/srv/another-project"
        )
        model.sessions = [leader, coworker, elsewhere]
        model.connectionState = .ready

        XCTAssertEqual(model.swarmCreationCandidates(for: leader).map(\.sessionId), ["chat-2"])

        model.createSwarm(
            leaderSessionID: leader.sessionId,
            memberSessionIDs: [coworker.sessionId]
        )
        let request = await recorder.firstRequest(after: 0) { request in
            if case .createSwarm = request { return true }
            return false
        }
        guard case .createSwarm(
            let requestID,
            let leaderSessionID,
            let memberSessionIDs
        ) = try XCTUnwrap(request) else {
            return XCTFail("Expected swarm creation")
        }
        XCTAssertEqual(leaderSessionID, "chat-1")
        XCTAssertEqual(memberSessionIDs, ["chat-1", "chat-2"])
        XCTAssertEqual(model.swarmMutationRequestID, requestID)
        XCTAssertFalse(model.canMutateSwarm)

        let swarm = SwarmRecord(
            id: "swarm-1",
            title: "Quiet Foxes",
            leaderSessionId: "chat-1",
            members: [
                SwarmMemberRecord(sessionId: "chat-1", handle: "@leader"),
                SwarmMemberRecord(sessionId: "chat-2", handle: "@builder"),
            ],
            messages: [],
            updatedAtMs: 200
        )
        model.handle(.swarms(requestID: requestID, swarms: [swarm]))

        XCTAssertNil(model.swarmMutationRequestID)
        XCTAssertTrue(model.canMutateSwarm)
        XCTAssertEqual(model.swarm(containing: "chat-2")?.title, "Quiet Foxes")
        XCTAssertTrue(model.swarmCreationCandidates(for: leader).isEmpty)
    }

    func testApplyingSwarmsRejectsMissingLeadersAndUnorderedMessages() throws {
        let model = try model { _ in }
        let leader = SwarmMemberRecord(sessionId: "chat-1", handle: "leader")
        let message = { (id: String, sequence: UInt64) in
            SwarmMessageRecord(
                id: id,
                sequence: sequence,
                authorSessionId: leader.sessionId,
                authorHandle: leader.handle,
                body: id,
                createdAtMs: Int64(sequence)
            )
        }
        let valid = SwarmRecord(
            id: "swarm-1",
            title: "Quiet Foxes",
            leaderSessionId: leader.sessionId,
            members: [leader],
            messages: [message("one", 1), message("two", 2)],
            updatedAtMs: 2
        )
        model.applySwarms([valid])

        model.applySwarms([SwarmRecord(
            id: "swarm-1",
            title: valid.title,
            leaderSessionId: leader.sessionId,
            members: [],
            messages: [],
            updatedAtMs: 3
        )])
        XCTAssertEqual(model.swarms, [valid])

        model.applySwarms([SwarmRecord(
            id: "swarm-1",
            title: valid.title,
            leaderSessionId: leader.sessionId,
            members: [leader],
            messages: [message("two", 2), message("one", 1)],
            updatedAtMs: 3
        )])
        XCTAssertEqual(model.swarms, [valid])
        XCTAssertEqual(model.toast?.message, "The gateway returned invalid swarm state.")
    }

    func testNewWorkspaceBrowserUsesCloudWorkingDirectoryOnly() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.connectionState = .ready

        let requestCount = await recorder.requestCount()
        model.openWorkspaceBrowser()

        let localRequest = await recorder.firstRequest(after: requestCount) { request in
            guard case .listDirectories(_, "/", false) = request else { return false }
            return true
        }
        XCTAssertNotNil(localRequest)

        let userID = UUID()
        let cloudAccount = GatewayAccount(
            endpoint: try GatewayEndpoint("wss://cloud-test.sprites.app"),
            displayName: "möbius Cloud",
            cloudUserID: userID
        )
        model.accounts = [cloudAccount]
        model.selectedAccountID = cloudAccount.id
        model.cloudSession = MobiusCloudSession(userID: userID, expiresAt: .distantFuture)

        let cloudRequestCount = await recorder.requestCount()
        model.openWorkspaceBrowser()

        let cloudRequest = await recorder.firstRequest(after: cloudRequestCount) { request in
            guard case .listDirectories(_, ".", false) = request else { return false }
            return true
        }
        XCTAssertNotNil(cloudRequest)
    }

    func testCreatingWorkspaceDirectoryUsesCurrentListingAndEntersCreatedFolder() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.connectionState = .ready
        model.directoryListing = DirectoryListing(
            path: "/srv",
            parent: "/",
            entries: []
        )

        let requestCount = await recorder.requestCount()
        model.createWorkspaceDirectory(named: "  New Project  ")

        let request = await recorder.firstRequest(after: requestCount) { request in
            if case .createWorkspaceDirectory = request { return true }
            return false
        }
        guard case .createWorkspaceDirectory(let requestID, let parent, let name) = try XCTUnwrap(request) else {
            return XCTFail("Expected a create-workspace-directory request")
        }
        XCTAssertEqual(parent, "/srv")
        XCTAssertEqual(name, "New Project")
        XCTAssertTrue(model.isLoadingDirectories)

        let created = DirectoryListing(
            path: "/srv/New Project",
            parent: "/srv",
            entries: []
        )
        model.handle(.directories(requestID: requestID, listing: created))

        XCTAssertEqual(model.directoryListing, created)
        XCTAssertFalse(model.isLoadingDirectories)
        XCTAssertNil(model.directoryError)
        let requests = await recorder.requests()
        XCTAssertFalse(requests.contains { request in
            if case .createSession = request { return true }
            return false
        })
    }

    func testCreatingWorkspaceDirectoryRejectsNestedNameBeforeSending() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.connectionState = .ready
        model.directoryListing = DirectoryListing(
            path: "/srv",
            parent: "/",
            entries: []
        )

        model.createWorkspaceDirectory(named: "../escape")
        try await Task.sleep(for: .milliseconds(20))

        XCTAssertEqual(model.directoryError, "Enter a single folder name.")
        let requests = await recorder.requests()
        XCTAssertFalse(requests.contains { request in
            if case .createWorkspaceDirectory = request { return true }
            return false
        })
    }

    func testGatewayReadyPopulatesChatCatalogWithoutOpeningSession() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }

        model.handle(.ready(ready(
            defaultConfig: VersionedAgentConfig(revision: 1, config: composition()),
            sessions: [
                session(sessionID: "chat-1", state: .idle),
                session(sessionID: "chat-2", state: .idle),
            ]
        )))
        try await Task.sleep(for: .milliseconds(30))

        XCTAssertEqual(model.sessions.map(\.sessionId), ["chat-1", "chat-2"])
        XCTAssertNil(model.selectedSessionID)
        XCTAssertTrue(model.navigationPath.isEmpty)
        let requests = await recorder.requests()
        XCTAssertFalse(requests.contains { request in
            if case .openSession = request { return true }
            return false
        })
    }

    func testOpenChatSetsRouteAndRequestsSessionOnce() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.connectionState = .ready
        model.sessions = [session(sessionID: "chat-2", state: .idle)]

        let requestCount = await recorder.requestCount()
        model.openChat("chat-2")

        XCTAssertEqual(model.destination, .chats)
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-2"))])
        let request = await recorder.firstRequest(after: requestCount) { request in
            guard case .openSession(_, "chat-2", _) = request else { return false }
            return true
        }
        guard case .openSession(let requestID, _, _) = try XCTUnwrap(request) else {
            return XCTFail("Expected the chat to open")
        }
        model.handle(.sessionOpened(
            requestID: requestID,
            payload: sessionReady(latestSequence: 0, sessionID: "chat-2")
        ))
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-2"))])
        try await Task.sleep(for: .milliseconds(30))
        let requests = await recorder.requests()
        let opens = requests.dropFirst(requestCount).filter { request in
            if case .openSession = request { return true }
            return false
        }
        XCTAssertEqual(opens.count, 1)
    }

    func testPoppingNavigationPathClearsPresentedChat() throws {
        let model = try model()
        model.selectedSessionID = "chat-1"
        model.navigationPath = [.chat(.session("chat-1"))]

        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-1"))])

        model.navigationPath = []

        XCTAssertNil(model.presentedChatSessionID)
    }

    func testCreatedSessionPresentsChatOnlyAfterGatewayOpensIt() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.connectionState = .ready

        let requestCount = await recorder.requestCount()
        model.chooseWorkspace("/srv/mobius")
        let request = await recorder.firstRequest(after: requestCount) { request in
            if case .createSession = request { return true }
            return false
        }
        guard case .createSession(let requestID, let path) = try XCTUnwrap(request) else {
            return XCTFail("Expected a create-session request")
        }
        XCTAssertEqual(path, "/srv/mobius")
        XCTAssertTrue(model.navigationPath.isEmpty)

        model.handle(.sessionOpened(
            requestID: requestID,
            payload: sessionReady(latestSequence: 0, sessionID: "chat-created")
        ))

        XCTAssertEqual(model.destination, .chats)
        XCTAssertEqual(model.selectedSessionID, "chat-created")
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-created"))])
    }

    func testDeletingPresentedChatReturnsToCatalogWithoutOpeningAnother() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let selected = session(sessionID: "chat-1", state: .idle)
        let remaining = session(sessionID: "chat-2", state: .idle)
        model.connectionState = .ready
        model.sessions = [selected, remaining]
        model.selectedSessionID = selected.sessionId
        model.destination = .chats
        model.navigationPath = [.chat(.session(selected.sessionId))]

        let requestCount = await recorder.requestCount()
        model.deleteSession(selected)

        XCTAssertNil(model.selectedSessionID)
        XCTAssertTrue(model.navigationPath.isEmpty)
        let request = await recorder.firstRequest(after: requestCount) { request in
            guard case .deleteSession(_, "chat-1") = request else { return false }
            return true
        }
        guard case .deleteSession(let requestID, _) = try XCTUnwrap(request) else {
            return XCTFail("Expected a delete-session request")
        }
        model.handle(.accepted(requestID: requestID))
        model.handle(.sessions(requestID: requestID, sessions: [remaining]))
        try await Task.sleep(for: .milliseconds(30))

        XCTAssertEqual(model.sessions.map(\.sessionId), ["chat-2"])
        let requests = await recorder.requests()
        XCTAssertFalse(requests.dropFirst(requestCount).contains { request in
            if case .openSession = request { return true }
            return false
        })
    }

    func testRejectedDeleteRestoresPresentedChat() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let selected = session(sessionID: "chat-1", state: .idle)
        model.connectionState = .ready
        model.sessions = [selected]
        model.selectedSessionID = selected.sessionId
        model.destination = .chats
        model.navigationPath = [.chat(.session(selected.sessionId))]

        model.deleteSession(selected)
        let deleteRequest = await recorder.firstRequest(after: 0) { request in
            guard case .deleteSession(_, "chat-1") = request else { return false }
            return true
        }
        guard case .deleteSession(let requestID, _) = try XCTUnwrap(deleteRequest) else {
            return XCTFail("Expected a delete-session request")
        }
        let requestCount = await recorder.requestCount()

        model.handle(.rejected(GatewayRejection(
            requestId: requestID,
            code: "delete_failed",
            message: "Chat could not be deleted",
            fatal: false
        )))

        XCTAssertEqual(model.destination, .chats)
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-1"))])
        let openRequest = await recorder.firstRequest(after: requestCount) { request in
            guard case .openSession(_, "chat-1", _) = request else { return false }
            return true
        }
        XCTAssertNotNil(openRequest)
    }

    func testDeleteSendFailureRestoresPresentedChatForReconnect() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in
            await recorder.record(request)
            if case .deleteSession = request { throw URLError(.cannotConnectToHost) }
        }
        let selected = session(sessionID: "chat-1", state: .running, turnID: "turn-1")
        model.connectionState = .ready
        model.sessions = [selected]
        model.selectedSessionID = selected.sessionId
        model.destination = .chats
        model.navigationPath = [.chat(.session(selected.sessionId))]
        model.activeTurnID = "turn-1"
        model.activeOperation = "steer"
        model.composer = "Keep working"

        model.sendMessage()
        let submission = await recorder.firstRequest(after: 0) { request in
            if case .submit = request { return true }
            return false
        }
        XCTAssertNotNil(submission)
        XCTAssertFalse(model.canOpenSession)

        let requestCount = await recorder.requestCount()
        model.deleteSession(selected)

        XCTAssertNil(model.selectedSessionID)
        XCTAssertTrue(model.navigationPath.isEmpty)
        let disconnected = await eventually {
            if case .failed = model.connectionState { return true }
            return false
        }
        XCTAssertTrue(disconnected)

        let requests = await recorder.requests()
        XCTAssertFalse(requests.dropFirst(requestCount).contains { request in
            if case .openSession(_, "chat-1", _) = request { return true }
            return false
        })
        XCTAssertEqual(model.destination, .chats)
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-1"))])
    }

    func testTaskCompleteFlushesPendingReasoning() throws {
        let model = try model()

        for delta in ["think", "ing"] {
            model.reduce(
                event: AgentEventRecord(submissionId: nil, msg: .object([
                    "type": .string("agent_reasoning_content_delta"),
                    "modelStepId": .string("reasoning-1"),
                    "delta": .string(delta)
                ])),
                blocks: [],
                preview: nil
            )
        }
        XCTAssertTrue(model.transcript.isEmpty)

        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("task_complete")
            ])),
            blocks: [],
            preview: nil
        )

        XCTAssertEqual(model.transcript.map(\.text), ["thinking"])
        XCTAssertFalse(try XCTUnwrap(model.transcript.first).pending)
    }

    func testTaskCompleteCollapsesCompactionIntoFinishedTurnWork() throws {
        let model = try model()
        let turnID = "turn-1"
        model.reduce(record: recorded(1, .object([
            "type": .string("task_started"),
            "turnId": .string(turnID),
        ])))
        model.reduce(record: recorded(2, .object([
            "type": .string("user_message"),
            "message": .string("Start"),
        ])))
        model.reduce(record: recorded(3, .object([
            "type": .string("agent_message"),
            "turnId": .string(turnID),
            "modelStepId": .string("step-1"),
            "phase": .string("commentary"),
            "message": .string("Checking"),
        ])))
        model.reduce(record: recorded(4, .object([
            "type": .string("context_compacted"),
        ]), blocks: [RenderedBlock(capability: "compaction", block: FrontendBlock(
            id: nil,
            group: nil,
            update: .replace,
            state: .complete,
            role: .notice,
            title: "context compacted",
            text: "",
            symbol: nil,
            format: "plain_text",
            tone: "neutral",
            files: []
        ))]))
        model.reduce(record: recorded(5, .object([
            "type": .string("user_message"),
            "message": .string("Also check tests"),
        ])))
        model.reduce(record: recorded(6, .object([
            "type": .string("agent_message"),
            "turnId": .string(turnID),
            "modelStepId": .string("step-2"),
            "phase": .string("final_answer"),
            "message": .string("Done"),
        ])))

        XCTAssertEqual(
            model.transcriptProjection(breakBefore: nil).rows.map(\.kind),
            [.user, .narrative, .activityGroup, .user, .narrative]
        )

        model.reduce(record: recorded(7, .object([
            "type": .string("task_complete"),
            "turnId": .string(turnID),
        ])))

        let projection = model.transcriptProjection(breakBefore: nil)
        XCTAssertEqual(projection.rows.map(\.kind), [.user, .workedGroup, .narrative])
        XCTAssertEqual(
            projection.rows[1].records.map(\.text),
            ["Checking", "", "Also check tests"]
        )
        XCTAssertEqual(projection.rows[1].records.map(\.title), ["", "context compacted", ""])
        XCTAssertEqual(projection.rows[1].elapsedMs, 500)
        XCTAssertEqual(model.transcript.map(\.turnID), Array(repeating: turnID, count: 5))
        XCTAssertEqual(model.transcript.map(\.startsTurn), [true, false, false, false, false])
        XCTAssertEqual(TranscriptProjection.turnCount(in: model.transcript), 1)
        XCTAssertEqual(model.transcript.last?.turnElapsedMs, 500)
    }

    func testPeerMessageStartsAndCollapsesACompletedTurnLikeUserInput() throws {
        let model = try model()
        let turnID = "peer-turn"
        model.reduce(record: recorded(1, .object([
            "type": .string("task_started"),
            "turnId": .string(turnID),
        ])))
        model.reduce(record: recorded(2, .object([
            "type": .string("peer_message"),
            "messageId": .string("message-1"),
            "sourceSessionId": .string("chat-reviewer"),
            "sourceHandle": .string("@reviewer"),
            "message": .string("Review the parser boundary."),
        ])))
        model.reduce(record: recorded(3, .object([
            "type": .string("agent_message"),
            "turnId": .string(turnID),
            "modelStepId": .string("step-1"),
            "phase": .string("commentary"),
            "message": .string("Checking"),
        ])))
        model.reduce(record: recorded(4, .object([
            "type": .string("agent_message"),
            "turnId": .string(turnID),
            "modelStepId": .string("step-2"),
            "phase": .string("final_answer"),
            "message": .string("Done"),
        ])))
        model.reduce(record: recorded(5, .object([
            "type": .string("task_complete"),
            "turnId": .string(turnID),
        ])))

        XCTAssertEqual(model.transcript.map(\.turnID), Array(repeating: turnID, count: 3))
        XCTAssertEqual(model.transcript.map(\.startsTurn), [true, false, false])
        XCTAssertEqual(
            model.transcriptProjection(breakBefore: nil).rows.map(\.kind),
            [.peer, .workedGroup, .narrative]
        )
    }

    func testOnlyLatestActivityStepIsActiveDuringTurn() throws {
        let model = try model()
        model.activeTurnID = "turn-1"
        model.transcript = [
            TranscriptEntry(
                id: "reasoning-1",
                text: "Considering the request",
                kind: .reasoning,
                format: "plain_text",
                pending: true
            ),
            TranscriptEntry(
                id: "tools/turn-1/call-1",
                text: "Read the file",
                kind: .event,
                group: "tools/turn-1",
                format: "plain_text",
                pending: false
            ),
            TranscriptEntry(
                id: "tools/turn-1/call-2",
                text: "Run the tests",
                kind: .event,
                group: "tools/turn-1",
                format: "plain_text",
                pending: true
            ),
        ]

        XCTAssertEqual(model.activeTranscriptStepID, "tools/turn-1/call-2")

        model.transcript.append(TranscriptEntry(
            id: "answer-1",
            text: "Here is the answer",
            kind: .assistant,
            format: "plain_text",
            pending: true
        ))
        XCTAssertNil(model.activeTranscriptStepID)

        model.transcript.removeLast()
        model.activeTurnID = nil
        XCTAssertNil(model.activeTranscriptStepID)
    }

}
