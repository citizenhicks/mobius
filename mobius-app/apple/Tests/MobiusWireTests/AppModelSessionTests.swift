import Foundation
@testable import Mobius
import XCTest

@MainActor
extension AppModelTests {
    func testReassigningChatUsesItsIDAndWaitsForConfirmedOwnership() async throws {
        let recorder = GatewayRequestRecorder()
        let app = try model { await recorder.record($0) }
        app.gateway.connectionState = .ready
        app.bots = [bot(), bot(id: "bot-2")]
        let original = session(state: .idle)
        let other = session(sessionID: "chat-2", state: .running)
        app.chat.sessions = [original, other]
        app.chat.selectedSessionID = other.sessionId
        XCTAssertFalse(app.canReassignSession(other))
        XCTAssertNil(app.reassignSession(original, to: "missing-bot"))
        let id = try XCTUnwrap(app.reassignSession(original, to: "bot-2"))
        let request = await recorder.firstRequest(after: 0) {
            if case .reassignSession = $0 { return true }
            return false
        }
        guard
            case .reassignSession(let requestID, let sessionID, let botID) = try XCTUnwrap(request)
        else { return XCTFail("Expected chat reassignment") }
        XCTAssertEqual(requestID, id)
        XCTAssertEqual(sessionID, original.sessionId)
        XCTAssertEqual(botID, "bot-2")
        let encoded = try XCTUnwrap(
            JSONSerialization.jsonObject(with: JSONEncoder().encode(request)) as? [String: Any])
        XCTAssertEqual(encoded["type"] as? String, "reassign_session")
        XCTAssertEqual(encoded["botId"] as? String, "bot-2")
        XCTAssertEqual(app.chat.sessions.first?.sessionContext.botId, "bot-1")
        XCTAssertFalse(app.canReassignSession(original))
        app.gateway.handle(.accepted(requestID: id))
        XCTAssertEqual(app.chat.sessions.first?.sessionContext.botId, "bot-1")
        app.gateway.handle(
            .sessions(requestID: id, sessions: [session(state: .idle, botID: "bot-2"), other]))
        XCTAssertNil(app.chat.sessionMutationRequestID)
        XCTAssertEqual(app.chat.sessions.first?.sessionContext.botId, "bot-2")
        XCTAssertEqual(app.chat.selectedSessionID, other.sessionId)
        XCTAssertNil(app.reassignSession(original, to: "bot-2"))
        XCTAssertFalse(app.canReassignSession(session(sessionID: "removed", state: .idle)))

        app.chat.selectedSessionID = original.sessionId
        app.gateway.handle(.sessionChanged(sessionReady(latestSequence: 1, botID: "bot-2")))
        XCTAssertEqual(app.selectedBot?.id, "bot-2")
        XCTAssertEqual(app.agentSnapshot, app.bots[1].config)
        XCTAssertEqual(app.workspace?.path, "/srv/mobius")
        app.chat.realtimeVoiceCall = RealtimeVoiceCall(
            requestID: "voice", sessionID: original.sessionId)
        XCTAssertFalse(app.canReassignSession(app.chat.sessions[0]))
    }

    func testActiveRunAllowsSessionNavigationButNotSelectedSessionMutation() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.gateway.connectionState = .ready
        model.chat.selectedSessionID = "chat-1"
        model.chat.activeTurnID = "turn-1"
        model.chat.pendingApproval = PendingApproval(
            id: "approval-1",
            reason: "Approve this tool?",
            calls: []
        )
        model.gitStatus = GitStatus(currentBranch: "main", branches: ["feature", "main"])

        XCTAssertTrue(model.canOpenSession)
        XCTAssertTrue(model.canCreateSession)
        XCTAssertFalse(model.canModifySelectedSession)

        model.switchGitBranch(to: "feature")
        model.attachFolder("/srv/other")
        model.chat.openSession("chat-2")
        try await Task.sleep(for: .milliseconds(30))

        let requests = await recorder.requests()
        XCTAssertFalse(
            requests.contains { request in
                if case .switchGitBranch = request { return true }
                return false
            })
        XCTAssertFalse(
            requests.contains { request in
                if case .attachSessionFolder = request { return true }
                return false
            })
        XCTAssertTrue(
            requests.contains { request in
                guard case .openSession(_, "chat-2", _) = request else { return false }
                return true
            })
    }

    func testAttachingFolderUsesSelectedSessionMutation() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.gateway.connectionState = .ready
        model.chat.selectedSessionID = "chat-1"

        model.attachFolder("  /srv/other  ")

        let request = await recorder.firstRequest(after: 0) { request in
            if case .attachSessionFolder = request { return true }
            return false
        }
        guard
            case .attachSessionFolder(
                let requestID,
                let sessionID,
                let folder
            ) = try XCTUnwrap(request)
        else {
            return XCTFail("Expected folder attachment")
        }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(folder, "/srv/other")
        XCTAssertEqual(model.chat.sessionMutationRequestID, requestID)
        XCTAssertNil(model.chat.attachedFolders)
        model.bots = [bot()]
        model.gateway.handle(
            .sessionChanged(
                sessionReady(
                    latestSequence: 1, attachedFolders: ["/srv/other"])))
        XCTAssertEqual(model.chat.attachedFolders, ["/srv/other"])
        model.gateway.handle(
            .sessionChanged(
                sessionReady(
                    latestSequence: 1, sessionID: "chat-2", attachedFolders: ["/srv/unrelated"])))
        XCTAssertEqual(model.chat.attachedFolders, ["/srv/other"])
        model.chat.resetSessionState()
        XCTAssertNil(model.chat.attachedFolders)
    }

    func testNewChatBotsRemainSelectedUntilFirstSendCreatesGroup() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.gateway.connectionState = .ready
        model.chat.selectedSessionID = "chat-1"
        model.chat.activeTurnID = "turn-1"
        model.workspace = WorkspaceInfo(id: "workspace-1", path: "/srv/old")
        model.gitStatus = GitStatus(currentBranch: "main", branches: ["main"])
        model.gitDiffs[.unstaged]?.text = "old diff"
        let helper = bot()
        let reviewer = bot(id: "bot-2", handle: "reviewer", name: "Reviewer")
        model.bots = [helper, reviewer]

        model.chooseWorkspace("/srv/another-project")
        try await Task.sleep(for: .milliseconds(20))

        XCTAssertEqual(model.navigationPath, [.chat(.new)])
        XCTAssertNil(model.workspace)
        XCTAssertNil(model.gitStatus)
        XCTAssertEqual(model.gitDiffs[.unstaged]?.text, "")
        let stagedRequests = await recorder.requests()
        XCTAssertFalse(
            stagedRequests.contains { request in
                if case .createSession = request { return true }
                return false
            })

        model.selectBotForNewChat(helper)
        model.selectBotForNewChat(reviewer)
        XCTAssertEqual(model.chat.pendingNewChatBotIDs, [helper.id, reviewer.id])
        let selectedRequests = await recorder.requests()
        XCTAssertFalse(
            selectedRequests.contains { request in
                if case .createSession = request { return true }
                return false
            })

        model.chat.composer = "Start here"
        model.openWorkspaceBrowser()
        model.chooseWorkspace("/srv/final-project")
        XCTAssertFalse(model.showsWorkspaceBrowser)
        XCTAssertEqual(model.chat.composer, "Start here")
        XCTAssertEqual(model.chat.pendingNewChatBotIDs, [helper.id, reviewer.id])
        XCTAssertTrue(model.sendMessage())
        let request = await recorder.firstRequest(after: 0) { request in
            guard case .createSession(_, "/srv/final-project", let botIDs) = request,
                botIDs == ["bot-1", "bot-2"]
            else {
                return false
            }
            return true
        }
        XCTAssertNotNil(request)
    }

    func testNewChatOpensImmediatelyAndListsDistinctProjectFolders() throws {
        let model = try model { _ in }
        model.gateway.connectionState = .ready
        model.bots = [bot()]
        model.workspace = WorkspaceInfo(id: "current", path: "/srv/current")
        model.chat.selectedSessionID = "chat-1"
        model.chat.sessions = [
            session(sessionID: "one", state: .idle, workspaceLabel: "/srv/project"),
            session(sessionID: "two", state: .idle, workspaceLabel: "/srv/project"),
            session(sessionID: "three", state: .idle, workspaceLabel: "/srv/other"),
        ]

        model.openNewSession()

        XCTAssertEqual(model.navigationPath, [.chat(.new)])
        XCTAssertFalse(model.showsWorkspaceBrowser)
        XCTAssertNil(model.chat.sessionRequestID)
        XCTAssertEqual(model.chat.pendingNewChatWorkspace, "/srv/current")
        XCTAssertEqual(model.chat.pendingNewChatBotIDs, ["bot-1"])
        XCTAssertEqual(model.newChatWorkspacePaths, ["/srv/current", "/srv/other", "/srv/project"])
    }

    func testNewChatResolvesGatewayDefaultFolderWithoutOpeningBrowser() throws {
        let model = try model { _ in }
        model.gateway.connectionState = .ready
        model.bots = [bot()]

        model.openNewSession()

        XCTAssertFalse(model.showsWorkspaceBrowser)
        XCTAssertEqual(model.chat.pendingNewChatWorkspace, ".")
        let requestID = try XCTUnwrap(model.directoryRequestID)
        model.gateway.handle(
            .directories(
                requestID: requestID,
                listing: DirectoryListing(
                    path: "/srv/default", parent: "/srv",
                    entries: [])))
        XCTAssertEqual(model.chat.pendingNewChatWorkspace, "/srv/default")
        XCTAssertEqual(model.newChatWorkspacePaths, ["/srv/default"])

        model.openNewSession()
        XCTAssertEqual(model.chat.pendingNewChatWorkspace, "/srv/default")
        model.loadDirectory(".")
        let staleRequestID = try XCTUnwrap(model.directoryRequestID)
        model.chooseWorkspace("/srv/project")
        model.gateway.handle(
            .directories(
                requestID: staleRequestID,
                listing: DirectoryListing(
                    path: "/srv/default", parent: "/srv",
                    entries: [])))
        XCTAssertEqual(model.chat.pendingNewChatWorkspace, "/srv/project")
    }

    func testNewSessionInOpenChatInheritsWorkspaceAndBotWithoutPickers() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.gateway.connectionState = .ready
        model.workspace = WorkspaceInfo(
            id: "workspace-1",
            path: "/srv/current-project"
        )
        let helper = bot()
        model.bots = [helper]
        model.chat.sessions = [
            session(
                sessionID: "chat-current",
                state: .idle,
                workspaceID: "workspace-1",
                workspaceLabel: "/srv/current-project",
                botID: helper.id
            )
        ]
        model.chat.selectedSessionID = "chat-current"
        model.navigationPath = [.chat(.session("chat-current"))]

        let requestCount = await recorder.requestCount()
        model.openNewSessionInCurrentWorkspace()
        XCTAssertEqual(model.navigationPath, [.chat(.new)])
        try await Task.sleep(for: .milliseconds(20))
        let requests = await recorder.requests()
        XCTAssertTrue(requests.dropFirst(requestCount).isEmpty)
        XCTAssertEqual(model.chat.pendingNewChatWorkspace, "/srv/current-project")
        XCTAssertEqual(model.chat.pendingNewChatBotIDs, [helper.id])
    }

    func testHiddenBotSessionsStayOutsideChatsAndRemainSelectable() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let helper = bot()
        let visible = session(sessionID: "chat-1", state: .idle, botID: helper.id)
        let hidden = session(
            sessionID: "work-1",
            state: .awaitingApproval,
            turnID: "turn-1",
            firstUserMessage: "Review the dependency update",
            originLabel: "group",
            botID: helper.id
        )
        model.bots = [helper]
        model.chat.sessions = [visible]
        model.gateway.connectionState = .ready

        model.openBotSessions(helper.id)
        let request = await recorder.firstRequest(after: 0) { request in
            if case .listBotSessions = request { return true }
            return false
        }
        guard case .listBotSessions(let requestID, let botID) = try XCTUnwrap(request) else {
            return XCTFail("Expected hidden Bot session listing")
        }
        XCTAssertEqual(botID, helper.id)
        XCTAssertEqual(model.navigationPath, [.botSessions(helper.id)])

        model.gateway.handle(
            .botSessions(
                requestID: requestID,
                botID: helper.id,
                sessions: [hidden]
            ))
        XCTAssertEqual(model.chat.botSessions, [hidden])
        XCTAssertEqual(model.chat.sessions, [visible])
        XCTAssertEqual(model.chat.chatCatalogSessions, [visible])
        XCTAssertFalse(model.chat.unreadSessionIDs.contains(hidden.sessionId))
        XCTAssertNil(model.toast)

        model.chat.selectedSessionID = hidden.sessionId
        model.applySessions([visible])
        XCTAssertEqual(model.selectedSession, hidden)
        XCTAssertTrue(model.selectedSessionIsHidden)

        model.navigationPath.append(.chat(.session(hidden.sessionId)))
        model.chat.botSessions = []
        XCTAssertTrue(model.selectedSessionIsHidden)
        model.chat.transcript = [
            TranscriptEntry(
                id: "hidden-message",
                text: "Hidden message",
                kind: .assistant,
                format: "plain_text",
                pending: false,
                messageTarget: MessageTarget(checkpointSequence: 1, batchItemCount: 1)
            )
        ]
        XCTAssertFalse(model.canBeginReply)
        model.beginReplying(to: model.chat.transcript[0])
        XCTAssertNil(model.chat.composerReply)
    }

    func testBackgroundApprovalSnapshotValidatesOwnershipAndNotifiesOncePerRequest() throws {
        let model = try model()
        model.gateway.connectionState = .ready
        let first = BackgroundApproval(
            sessionId: "work-1",
            botId: "bot-1",
            turnId: "turn-1",
            requestId: "approval-1"
        )

        model.gateway.handle(.backgroundApprovals([first]))
        let firstToastID = try XCTUnwrap(model.toast?.id)
        XCTAssertEqual(model.backgroundApprovals, [first])
        XCTAssertEqual(model.toast?.message, "Helper needs approval.")
        XCTAssertEqual(model.bot(forSessionID: first.sessionId)?.id, first.botId)

        model.gateway.handle(.backgroundApprovals([first]))
        XCTAssertEqual(model.toast?.id, firstToastID)

        let second = BackgroundApproval(
            sessionId: first.sessionId,
            botId: first.botId,
            turnId: first.turnId,
            requestId: "approval-2"
        )
        model.gateway.handle(.backgroundApprovals([second]))
        XCTAssertNotEqual(model.toast?.id, firstToastID)

        XCTAssertFalse(
            model.applyBackgroundApprovals(
                [
                    BackgroundApproval(
                        sessionId: "work-2",
                        botId: "missing-bot",
                        turnId: "turn-2",
                        requestId: "approval-3"
                    )
                ], notifyingNew: true))
        XCTAssertEqual(model.backgroundApprovals, [second])
    }

    func testStaleBackgroundApprovalToastCannotOpenHiddenWorkAsAChat() throws {
        let model = try model()
        model.gateway.connectionState = .ready
        model.backgroundApprovals = [
            BackgroundApproval(
                sessionId: "work-1",
                botId: "bot-1",
                turnId: "turn-1",
                requestId: "approval-1"
            )
        ]
        model.applyBackgroundApprovals([], notifyingNew: false)

        model.openNotificationTarget(.session("work-1"))

        XCTAssertNil(model.chat.selectedSessionID)
        XCTAssertTrue(model.navigationPath.isEmpty)
    }

    func testBotSessionResumeOpensOnlyTheValidatedHiddenSession() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let helper = bot()
        let visible = session(sessionID: "chat-1", state: .idle, botID: helper.id)
        let hidden = session(
            sessionID: "work-1",
            state: .awaitingApproval,
            turnID: "turn-1",
            originLabel: "group",
            botID: helper.id
        )
        model.bots = [helper]
        model.chat.sessions = [visible]
        model.gateway.connectionState = .ready
        model.destination = .bots
        model.navigationPath = [.chat(.session("group-1"))]

        model.resumeBotSession(botID: helper.id, sessionID: hidden.sessionId)
        let listing = await recorder.firstRequest(after: 0) { request in
            if case .listBotSessions = request { return true }
            return false
        }
        guard case .listBotSessions(let requestID, let botID) = try XCTUnwrap(listing) else {
            return XCTFail("Expected hidden Bot session discovery")
        }
        XCTAssertEqual(botID, helper.id)
        let requestsBeforeValidation = await recorder.requests()
        XCTAssertFalse(
            requestsBeforeValidation.contains { request in
                if case .openSession = request { return true }
                return false
            })

        model.gateway.handle(
            .botSessions(
                requestID: requestID,
                botID: helper.id,
                sessions: [hidden]
            ))
        let opening = await recorder.firstRequest(after: 1) { request in
            guard case .openSession(_, hidden.sessionId, _) = request else { return false }
            return true
        }

        XCTAssertNotNil(opening)
        XCTAssertEqual(model.chat.sessions, [visible])
        XCTAssertEqual(model.chat.botSessions, [hidden])
        XCTAssertFalse(model.chat.unreadSessionIDs.contains(hidden.sessionId))
        XCTAssertEqual(
            model.navigationPath,
            [.chat(.session("group-1")), .chat(.session(hidden.sessionId))]
        )
    }

    func testBotSessionResumeNeverOpensAStaleOrDifferentHiddenSession() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let helper = bot()
        let target = session(
            sessionID: "work-target",
            state: .awaitingApproval,
            turnID: "turn-target",
            originLabel: "group",
            botID: helper.id
        )
        let other = session(
            sessionID: "work-other",
            state: .awaitingApproval,
            turnID: "turn-other",
            originLabel: "group",
            botID: helper.id
        )
        model.bots = [helper]
        model.gateway.connectionState = .ready
        model.destination = .bots
        model.navigationPath = [.chat(.session("group-1"))]

        model.resumeBotSession(botID: helper.id, sessionID: target.sessionId)
        let listing = await recorder.firstRequest(after: 0) { request in
            if case .listBotSessions = request { return true }
            return false
        }
        guard case .listBotSessions(let requestID, _) = try XCTUnwrap(listing) else {
            return XCTFail("Expected hidden Bot session discovery")
        }

        model.gateway.handle(
            .botSessions(
                requestID: "stale-request",
                botID: helper.id,
                sessions: [target]
            ))
        XCTAssertEqual(model.chat.pendingBotSessionResume?.sessionID, target.sessionId)

        model.gateway.handle(
            .botSessions(
                requestID: requestID,
                botID: helper.id,
                sessions: [other]
            ))

        XCTAssertNil(model.chat.pendingBotSessionResume)
        XCTAssertEqual(model.chat.botSessions, [other])
        XCTAssertEqual(
            model.navigationPath,
            [.chat(.session("group-1"))]
        )
        XCTAssertEqual(model.toast?.tone, .warning)
        XCTAssertEqual(model.toast?.message, "That Bot work is no longer available.")
    }

    func testBotSessionResumeOpensAnExistingVisibleSourceWithoutHiddenDiscovery() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let helper = bot()
        let source = session(sessionID: "chat-source", state: .idle, botID: helper.id)
        model.bots = [helper]
        model.chat.sessions = [source]
        model.gateway.connectionState = .ready

        model.resumeBotSession(botID: helper.id, sessionID: source.sessionId)
        let opening = await recorder.firstRequest(after: 0) { request in
            guard case .openSession(_, source.sessionId, _) = request else { return false }
            return true
        }

        XCTAssertNotNil(opening)
        XCTAssertEqual(model.destination, .chats)
        XCTAssertEqual(model.navigationPath, [.chat(.session(source.sessionId))])
        let requests = await recorder.requests()
        XCTAssertFalse(
            requests.contains { request in
                if case .listBotSessions = request { return true }
                return false
            })
    }

    func testNewWorkspaceBrowserUsesGatewayWorkingDirectory() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.gateway.connectionState = .ready

        let requestCount = await recorder.requestCount()
        model.openWorkspaceBrowser()

        let localRequest = await recorder.firstRequest(after: requestCount) { request in
            guard case .listDirectories(_, ".", false) = request else { return false }
            return true
        }
        XCTAssertNotNil(localRequest)

        let userID = UUID()
        let cloudAccount = GatewayAccount(
            endpoint: try GatewayEndpoint("wss://cloud-test.sprites.app"),
            displayName: "möbius Cloud",
            cloudUserID: userID
        )
        model.gateway.accounts = [cloudAccount]
        model.gateway.selectedAccountID = cloudAccount.id
        model.cloud.cloudSession = MobiusCloudSession(userID: userID, expiresAt: .distantFuture)

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
        model.gateway.connectionState = .ready
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
        guard
            case .createWorkspaceDirectory(let requestID, let parent, let name) = try XCTUnwrap(
                request)
        else {
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
        model.gateway.handle(.directories(requestID: requestID, listing: created))

        XCTAssertEqual(model.directoryListing, created)
        XCTAssertFalse(model.isLoadingDirectories)
        XCTAssertNil(model.directoryError)
        let requests = await recorder.requests()
        XCTAssertFalse(
            requests.contains { request in
                if case .createSession = request { return true }
                return false
            })
    }

    func testCreatingWorkspaceDirectoryRejectsNestedNameBeforeSending() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.gateway.connectionState = .ready
        model.directoryListing = DirectoryListing(
            path: "/srv",
            parent: "/",
            entries: []
        )

        model.createWorkspaceDirectory(named: "../escape")
        try await Task.sleep(for: .milliseconds(20))

        XCTAssertEqual(model.directoryError, "Enter a single folder name.")
        let requests = await recorder.requests()
        XCTAssertFalse(
            requests.contains { request in
                if case .createWorkspaceDirectory = request { return true }
                return false
            })
    }

    func testGatewayReadyPopulatesChatCatalogWithoutOpeningSession() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }

        model.gateway.handle(
            .ready(
                ready(
                    botDefaults: VersionedAgentConfig(revision: 1, config: composition()),
                    sessions: [
                        session(sessionID: "chat-1", state: .idle),
                        session(sessionID: "chat-2", state: .idle),
                    ]
                )))
        try await Task.sleep(for: .milliseconds(30))

        XCTAssertEqual(model.chat.sessions.map(\.sessionId), ["chat-1", "chat-2"])
        XCTAssertNil(model.chat.selectedSessionID)
        XCTAssertTrue(model.navigationPath.isEmpty)
        let requests = await recorder.requests()
        XCTAssertFalse(
            requests.contains { request in
                if case .openSession = request { return true }
                return false
            })
    }

    func testOpenChatSetsRouteAndRequestsSessionOnce() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.gateway.connectionState = .ready
        model.chat.sessions = [session(sessionID: "chat-2", state: .idle)]

        let requestCount = await recorder.requestCount()
        let presentationRevision = model.chat.chatPresentationRevision
        model.openChat("chat-2")

        XCTAssertEqual(model.destination, .chats)
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-2"))])
        XCTAssertEqual(model.chat.chatPresentationRevision, presentationRevision + 1)
        let request = await recorder.firstRequest(after: requestCount) { request in
            guard case .openSession(_, "chat-2", _) = request else { return false }
            return true
        }
        guard case .openSession(let requestID, _, _) = try XCTUnwrap(request) else {
            return XCTFail("Expected the chat to open")
        }
        model.gateway.handle(
            .sessionOpened(
                requestID: requestID,
                payload: sessionReady(latestSequence: 0, sessionID: "chat-2")
            ))
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-2"))])
        model.navigationPath = []
        model.openChat("chat-2")
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-2"))])
        XCTAssertEqual(model.chat.chatPresentationRevision, presentationRevision + 2)
        try await Task.sleep(for: .milliseconds(30))
        let requests = await recorder.requests()
        let opens = requests.dropFirst(requestCount).filter { request in
            if case .openSession = request { return true }
            return false
        }
        XCTAssertEqual(opens.count, 1)
    }

    func testOpenBotChatsUsesOnePredefinedFilterAndReturnsToCatalog() throws {
        let model = try model()
        let alpha = bot(id: "bot-a", handle: "alpha", name: "Alpha")
        let beta = bot(id: "bot-b", handle: "beta", name: "Beta")
        model.bots = [alpha, beta]
        model.destination = .bots
        model.navigationPath = [.bot(beta.id)]
        model.chat.chatBotFilterIDs = [alpha.id]

        model.openBotChats("missing")
        XCTAssertEqual(model.destination, .bots)
        XCTAssertEqual(model.navigationPath, [.bot(beta.id)])
        XCTAssertEqual(model.chat.chatBotFilterIDs, [alpha.id])

        model.openBotChats(beta.id)
        XCTAssertEqual(model.chat.chatBotFilterIDs, [beta.id])
        XCTAssertEqual(model.destination, .chats)
        XCTAssertTrue(model.navigationPath.isEmpty)
    }

    func testPoppingNavigationPathClearsPresentedChat() throws {
        let model = try model()
        model.chat.selectedSessionID = "chat-1"
        model.navigationPath = [.chat(.session("chat-1"))]

        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-1"))])

        model.navigationPath = []

        XCTAssertNil(model.presentedChatSessionID)
    }

    func testSingleBotIsAutoSelectedAndPresentsChatAfterGatewayOpensIt() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.gateway.connectionState = .ready
        let helper = bot()
        model.bots = [helper]

        let requestCount = await recorder.requestCount()
        model.chooseWorkspace("/srv/mobius")
        XCTAssertEqual(model.navigationPath, [.chat(.new)])
        XCTAssertEqual(model.chat.pendingNewChatBotIDs, [helper.id])
        try await Task.sleep(for: .milliseconds(20))
        let stagedRequests = await recorder.requests()
        XCTAssertTrue(stagedRequests.dropFirst(requestCount).isEmpty)

        model.chat.composer = "Inspect the project"
        XCTAssertTrue(model.sendMessage())
        let request = await recorder.firstRequest(after: requestCount) { request in
            if case .createSession = request { return true }
            return false
        }
        guard case .createSession(let requestID, let path, let botID) = try XCTUnwrap(request)
        else {
            return XCTFail("Expected a create-session request")
        }
        XCTAssertEqual(path, "/srv/mobius")
        XCTAssertEqual(botID, ["bot-1"])
        XCTAssertEqual(model.navigationPath, [.chat(.new)])

        model.gateway.handle(
            .sessionOpened(
                requestID: requestID,
                payload: sessionReady(latestSequence: 0, sessionID: "chat-created")
            ))
        model.gateway.handle(
            .sessionReplayComplete(
                requestID: requestID,
                sessionID: "chat-created"
            ))

        let submission = await recorder.firstRequest(after: requestCount) { request in
            guard case .submit("chat-created", let submission) = request,
                case .message(let message) = submission.op
            else { return false }
            return message.text == "Inspect the project"
        }

        XCTAssertEqual(model.destination, .chats)
        XCTAssertEqual(model.chat.selectedSessionID, "chat-created")
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-created"))])
        XCTAssertTrue(model.chat.pendingNewChatBotIDs.isEmpty)
        XCTAssertNotNil(submission)
        let submissions = (await recorder.requests()).dropFirst(requestCount).filter {
            if case .submit("chat-created", _) = $0 { return true }
            return false
        }
        XCTAssertEqual(submissions.count, 1)
    }

    func testRejectedFirstSendKeepsBotSelectedAndRestoresDraft() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.gateway.connectionState = .ready
        let helper = bot()
        model.bots = [helper]
        model.chooseWorkspace("/srv/mobius")
        model.chat.composer = "Try again"

        XCTAssertTrue(model.sendMessage())
        let request = await recorder.firstRequest(after: 0) {
            if case .createSession = $0 { return true }
            return false
        }
        guard case .createSession(let requestID, _, _) = try XCTUnwrap(request) else {
            return XCTFail("Expected a create-session request")
        }

        model.gateway.handle(
            .rejected(
                GatewayRejection(
                    requestId: requestID,
                    code: "create_failed",
                    message: "Chat could not be created",
                    fatal: false
                )))

        XCTAssertEqual(model.gateway.connectionState, .ready)
        XCTAssertEqual(model.chat.pendingNewChatBotIDs, [helper.id])
        XCTAssertEqual(model.chat.composer, "Try again")
        XCTAssertNil(model.chat.selectedSessionID)
    }

    func testDeletingMultipleChatsUsesOneAtomicRequest() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let first = session(sessionID: "chat-1", state: .idle)
        let second = session(sessionID: "chat-2", state: .idle)
        model.gateway.connectionState = .ready
        model.chat.sessions = [first, second]

        model.deleteSessions([first, second, first])

        let request = await recorder.firstRequest(after: 0) { request in
            guard case .deleteSessions(_, let ids) = request else { return false }
            return ids == ["chat-1", "chat-2"]
        }
        guard case .deleteSessions(let requestID, _) = try XCTUnwrap(request) else {
            return XCTFail("Expected one multi-chat deletion")
        }
        model.gateway.handle(.accepted(requestID: requestID))
        model.gateway.handle(.sessions(requestID: requestID, sessions: []))

        XCTAssertTrue(model.chat.sessions.isEmpty)
        XCTAssertNil(model.chat.sessionMutationRequestID)
    }

    func testDeletingPresentedChatReturnsToCatalogWithoutOpeningAnother() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let selected = session(sessionID: "chat-1", state: .idle)
        let remaining = session(sessionID: "chat-2", state: .idle)
        model.gateway.connectionState = .ready
        model.chat.sessions = [selected, remaining]
        model.chat.selectedSessionID = selected.sessionId
        model.destination = .chats
        model.navigationPath = [.chat(.session(selected.sessionId))]

        let requestCount = await recorder.requestCount()
        model.deleteSession(selected)

        XCTAssertNil(model.chat.selectedSessionID)
        XCTAssertTrue(model.navigationPath.isEmpty)
        let request = await recorder.firstRequest(after: requestCount) { request in
            guard case .deleteSessions(_, let ids) = request else { return false }
            return ids == ["chat-1"]
        }
        guard case .deleteSessions(let requestID, _) = try XCTUnwrap(request) else {
            return XCTFail("Expected a delete-session request")
        }
        model.gateway.handle(.accepted(requestID: requestID))
        model.gateway.handle(.sessions(requestID: requestID, sessions: [remaining]))
        try await Task.sleep(for: .milliseconds(30))

        XCTAssertEqual(model.chat.sessions.map(\.sessionId), ["chat-2"])
        let requests = await recorder.requests()
        XCTAssertFalse(
            requests.dropFirst(requestCount).contains { request in
                if case .openSession = request { return true }
                return false
            })
    }

    func testRejectedDeleteRestoresPresentedChat() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let selected = session(sessionID: "chat-1", state: .idle)
        model.gateway.connectionState = .ready
        model.chat.sessions = [selected]
        model.chat.selectedSessionID = selected.sessionId
        model.destination = .chats
        model.navigationPath = [.chat(.session(selected.sessionId))]

        model.deleteSession(selected)
        let deleteRequest = await recorder.firstRequest(after: 0) { request in
            guard case .deleteSessions(_, let ids) = request else { return false }
            return ids == ["chat-1"]
        }
        guard case .deleteSessions(let requestID, _) = try XCTUnwrap(deleteRequest) else {
            return XCTFail("Expected a delete-session request")
        }
        let requestCount = await recorder.requestCount()

        model.gateway.handle(
            .rejected(
                GatewayRejection(
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
            if case .deleteSessions = request { throw URLError(.cannotConnectToHost) }
        }
        let selected = session(sessionID: "chat-1", state: .running, turnID: "turn-1")
        model.gateway.connectionState = .ready
        model.chat.sessions = [selected]
        model.chat.selectedSessionID = selected.sessionId
        model.destination = .chats
        model.navigationPath = [.chat(.session(selected.sessionId))]
        model.chat.activeTurnID = "turn-1"
        model.chat.composer = "Keep working"

        model.sendMessage()
        let submission = await recorder.firstRequest(after: 0) { request in
            if case .submit = request { return true }
            return false
        }
        XCTAssertNotNil(submission)
        XCTAssertFalse(model.canOpenSession)

        let requestCount = await recorder.requestCount()
        model.deleteSession(selected)

        XCTAssertNil(model.chat.selectedSessionID)
        XCTAssertTrue(model.navigationPath.isEmpty)
        let disconnected = await eventually {
            if case .failed = model.gateway.connectionState { return true }
            return false
        }
        XCTAssertTrue(disconnected)

        let requests = await recorder.requests()
        XCTAssertFalse(
            requests.dropFirst(requestCount).contains { request in
                if case .openSession(_, "chat-1", _) = request { return true }
                return false
            })
        XCTAssertEqual(model.destination, .chats)
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-1"))])
    }

    func testTurnCompleteFlushesPendingReasoning() throws {
        let model = try model()

        for delta in ["think", "ing"] {
            model.chat.reduce(
                event: AgentEventRecord(
                    submissionId: nil,
                    msg: .object([
                        "type": .string("assistant_content_delta"),
                        "sessionId": .string("chat-1"),
                        "turnId": .string("turn-1"),
                        "modelStepId": .string("reasoning-1"),
                        "phase": .string("reasoning"),
                        "delta": .string(delta),
                    ])),
                blocks: [],
                preview: nil
            )
        }
        XCTAssertTrue(model.chat.transcript.isEmpty)

        model.chat.reduce(
            event: AgentEventRecord(
                submissionId: nil,
                msg: .object([
                    "type": .string("turn_complete")
                ])),
            blocks: [],
            preview: nil
        )

        XCTAssertEqual(model.chat.transcript.map(\.text), ["thinking"])
        XCTAssertFalse(try XCTUnwrap(model.chat.transcript.first).pending)
    }

    func testTurnCompleteCollapsesCompactionAndSteeringIntoFinishedTurnWork() throws {
        let model = try model()
        let turnID = "turn-1"
        model.chat.reduce(
            record: recorded(
                1,
                .object([
                    "type": .string("turn_started"),
                    "turnId": .string(turnID),
                ])))
        model.chat.reduce(record: recorded(2, testMessageEvent(text: "Start")))
        model.chat.reduce(
            record: recorded(
                3,
                testAssistantMessage(
                    turnID: turnID,
                    modelStepID: "step-1",
                    phase: "commentary",
                    text: "Checking"
                )))
        model.chat.reduce(
            record: recorded(
                4,
                .object([
                    "type": .string("context_compacted")
                ]),
                blocks: [
                    RenderedBlock(
                        capability: "compaction",
                        block: FrontendBlock(
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
                        ))
                ]))
        model.chat.reduce(
            record: recorded(
                5,
                testMessageEvent(
                    delivery: .steer,
                    text: "Also check tests"
                )))
        model.chat.reduce(
            record: recordedPeerMessage(
                6,
                delivery: .steer,
                text: "The parser boundary is covered."
            ))
        model.chat.reduce(
            record: recorded(
                7,
                testAssistantMessage(
                    turnID: turnID,
                    modelStepID: "step-2",
                    text: "Done"
                )))

        XCTAssertEqual(
            model.chat.transcriptProjection(breakBefore: nil).rows.map(\.kind),
            [.user, .narrative, .activityGroup, .user, .activityGroup, .narrative]
        )

        model.chat.reduce(
            record: recorded(
                8,
                .object([
                    "type": .string("turn_complete"),
                    "turnId": .string(turnID),
                ])))

        let projection = model.chat.transcriptProjection(breakBefore: nil)
        XCTAssertEqual(projection.rows.map(\.kind), [.user, .workedGroup, .narrative])
        XCTAssertEqual(
            projection.rows[1].records.map(\.text),
            ["Checking", "", "Also check tests", "The parser boundary is covered."]
        )
        XCTAssertEqual(
            projection.rows[1].records.map(\.title),
            ["", "context compacted", "", "Message received from @reviewer"]
        )
        XCTAssertEqual(
            projection.rows[1].records.compactMap { $0.messageMetadata?.delivery },
            [.steer, .steer]
        )
        XCTAssertEqual(projection.rows[1].elapsedMs, 600)
        XCTAssertEqual(model.chat.transcript.map(\.turnID), Array(repeating: turnID, count: 6))
        XCTAssertEqual(
            model.chat.transcript.map(\.startsTurn),
            [true, false, false, false, false, false]
        )
        XCTAssertEqual(TranscriptProjection.turnCount(in: model.chat.transcript), 1)
        XCTAssertEqual(model.chat.transcript.last?.turnElapsedMs, 600)
    }

    func testQueuedMessageStartsTheNextTranscriptTurn() throws {
        let model = try model()
        for (sequence, turnID, delivery, text) in [
            (UInt64(1), "turn-1", MessageDelivery.turn, "Start"),
            (UInt64(5), "turn-2", MessageDelivery.queue, "Follow up"),
        ] {
            model.chat.reduce(
                record: recorded(
                    sequence,
                    .object([
                        "type": .string("turn_started"),
                        "turnId": .string(turnID),
                    ])))
            model.chat.reduce(
                record: recorded(
                    sequence + 1,
                    testMessageEvent(delivery: delivery, text: text)
                ))
            model.chat.reduce(
                record: recorded(
                    sequence + 2,
                    testAssistantMessage(
                        turnID: turnID,
                        modelStepID: "step-\(turnID)",
                        text: "Done \(turnID)"
                    )
                ))
            model.chat.reduce(
                record: recorded(
                    sequence + 3,
                    .object([
                        "type": .string("turn_complete"),
                        "turnId": .string(turnID),
                    ])))
        }

        XCTAssertEqual(
            model.chat.transcript
                .filter { $0.kind == .user }
                .compactMap { $0.messageMetadata?.delivery },
            [.turn, .queue]
        )
        XCTAssertEqual(
            model.chat.transcript.filter { $0.kind == .user }.map(\.startsTurn),
            [true, true]
        )
        XCTAssertEqual(TranscriptProjection.turnCount(in: model.chat.transcript), 2)
    }

    func testOnlyLatestActivityStepIsActiveDuringTurn() throws {
        let model = try model()
        model.chat.activeTurnID = "turn-1"
        model.chat.transcript = [
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

        XCTAssertEqual(model.chat.activeTranscriptStepID, "tools/turn-1/call-2")

        model.chat.transcript.append(
            TranscriptEntry(
                id: "answer-1",
                text: "Here is the answer",
                kind: .assistant,
                format: "plain_text",
                pending: true
            ))
        XCTAssertNil(model.chat.activeTranscriptStepID)

        model.chat.transcript.removeLast()
        model.chat.activeTurnID = nil
        XCTAssertNil(model.chat.activeTranscriptStepID)
    }

    func testOrdinaryReplayPreservesSnapshotApprovalAfterEarlierCompletedTurn() throws {
        let app = try model { _ in }
        let approval: JSONValue = .object([
            "id": .string("current-approval"), "turnId": .string("current-turn"),
            "reason": .string("Run checks"), "calls": .array([]),
        ])
        app.chat.sessionRequestID = "open"
        app.gateway.handle(
            .sessionOpened(
                requestID: "open",
                payload: sessionReady(
                    latestSequence: 100, activeTurnIDs: ["current-turn"],
                    pendingApprovals: [approval])))
        app.chat.reduce(
            record: recorded(
                90,
                .object([
                    "type": .string("turn_complete"), "turnId": .string("old-turn"),
                ])))
        app.chat.reduce(
            record: recorded(
                95,
                .object([
                    "type": .string("turn_started"), "turnId": .string("current-turn"),
                ])))
        app.chat.reduce(
            record: recorded(
                99,
                .object([
                    "type": .string("exec_approval_request"), "id": .string("current-approval"),
                    "turnId": .string("current-turn"), "calls": .array([]),
                ])))
        app.chat.finishSessionReplay()
        XCTAssertEqual(app.chat.activeTurnID, "current-turn")
        XCTAssertEqual(app.chat.pendingApproval?.id, "current-approval")
        app.chat.reduce(
            record: recorded(
                101,
                .object([
                    "type": .string("turn_complete"), "turnId": .string("current-turn"),
                ])))
        XCTAssertNil(app.chat.pendingApproval)
    }

    func testGroupReplayKeepsCurrentApprovalsAndPeerRepliesInTheNormalTranscript() throws {
        let app = try model { _ in }
        app.bots = [bot(), bot(id: "bot-2", handle: "reviewer")]
        let approvals: [JSONValue] = ["a", "b"].map { id in
            .object([
                "id": .string(id), "turnId": .string("turn-" + id),
                "reason": .string("Run checks"), "calls": .array([]),
            ])
        }
        app.chat.sessionRequestID = "open"
        app.gateway.handle(
            .sessionOpened(
                requestID: "open",
                payload:
                    sessionReady(
                        latestSequence: 100, botID: "", memberBotIDs: ["bot-1", "bot-2"],
                        activeTurnIDs: ["turn-a", "turn-b"], pendingApprovals: approvals)))
        XCTAssertTrue(app.selectedChatIsGroup)
        XCTAssertEqual(app.selectedChatBots.count, 2)
        app.chat.reduce(
            record: recorded(
                90,
                .object([
                    "type": .string("turn_started"), "turnId": .string("old-turn"),
                ])))
        app.chat.reduce(
            record: recorded(
                91,
                .object([
                    "type": .string("exec_approval_request"), "id": .string("old-approval"),
                    "turnId": .string("old-turn"), "calls": .array([]),
                ])))
        XCTAssertEqual(app.chat.activeTurnIDs, ["turn-a", "turn-b"])
        XCTAssertEqual(app.chat.pendingApprovals.map(\.id), ["a", "b"])
        app.chat.reduce(
            record: recorded(
                92,
                testMessageEvent(
                    author: .peer(
                        messageID: "reply", sessionID: "private-reviewer", handle: "reviewer",
                        symbol: nil),
                    delivery: .turn, text: "Review complete"
                )))
        XCTAssertEqual(app.chat.transcript.last?.text, "Review complete")
        XCTAssertEqual(
            app.chat.transcript.last?.messageMetadata?.author,
            .peer(
                messageID: "reply", sessionID: "private-reviewer", handle: "reviewer", symbol: nil))
        app.chat.reduce(
            record: recorded(
                101,
                .object([
                    "type": .string("turn_started"), "turnId": .string("live-turn"),
                ])))
        XCTAssertEqual(app.chat.activeTurnIDs, ["turn-a", "turn-b", "live-turn"])
        app.chat.reduce(
            record: recorded(
                102,
                .object([
                    "type": .string("turn_complete"), "turnId": .string("live-turn"),
                ])))
        app.chat.replayRequestID = nil
        app.chat.approvalRequestID = "decision-a"
        app.chat.reduce(
            record: recorded(
                103,
                .object([
                    "type": .string("turn_complete"), "turnId": .string("turn-a"),
                ])))
        XCTAssertEqual(app.chat.activeTurnIDs, ["turn-b"])
        XCTAssertEqual(app.chat.pendingApproval?.id, "b")
        XCTAssertNil(app.chat.approvalRequestID)
        app.gateway.handle(.accepted(requestID: "decision-a"))
        XCTAssertEqual(app.chat.pendingApproval?.id, "b")
    }

}
