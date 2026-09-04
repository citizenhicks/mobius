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

    func testNewChatBotCanChangeUntilFirstSendCreatesSession() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.activeTurnID = "turn-1"
        let helper = bot()
        let reviewer = bot(id: "bot-2", handle: "reviewer", name: "Reviewer")
        model.bots = [helper, reviewer]

        model.chooseWorkspace("/srv/another-project")
        try await Task.sleep(for: .milliseconds(20))

        XCTAssertEqual(model.navigationPath, [.chat(.new)])
        let stagedRequests = await recorder.requests()
        XCTAssertFalse(stagedRequests.contains { request in
            if case .createSession = request { return true }
            return false
        })

        model.selectBotForNewChat(helper)
        model.selectBotForNewChat(reviewer)
        XCTAssertEqual(model.pendingNewChatBotID, reviewer.id)
        let selectedRequests = await recorder.requests()
        XCTAssertFalse(selectedRequests.contains { request in
            if case .createSession = request { return true }
            return false
        })

        model.composer = "Start here"
        XCTAssertTrue(model.sendMessage())
        let request = await recorder.firstRequest(after: 0) { request in
            guard case .createSession(_, "/srv/another-project", "bot-2") = request else {
                return false
            }
            return true
        }
        XCTAssertNotNil(request)
    }

    func testNewSessionInOpenChatInheritsWorkspaceAndBotWithoutPickers() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.connectionState = .ready
        model.workspace = WorkspaceInfo(
            id: "workspace-1",
            path: "/srv/current-project"
        )
        let helper = bot()
        model.bots = [helper]
        model.sessions = [session(
            sessionID: "chat-current",
            state: .idle,
            workspaceID: "workspace-1",
            workspaceLabel: "/srv/current-project",
            botID: helper.id
        )]
        model.selectedSessionID = "chat-current"
        model.navigationPath = [.chat(.session("chat-current"))]

        let requestCount = await recorder.requestCount()
        model.openNewSessionInCurrentWorkspace()
        XCTAssertEqual(model.navigationPath, [.chat(.new)])
        try await Task.sleep(for: .milliseconds(20))
        let requests = await recorder.requests()
        XCTAssertTrue(requests.dropFirst(requestCount).isEmpty)
        XCTAssertEqual(model.pendingNewChatWorkspace, "/srv/current-project")
        XCTAssertEqual(model.pendingNewChatBotID, helper.id)
    }

    func testCreatingSwarmUsesUnclaimedBotsAndWaitsForCatalog() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let leader = bot(id: "bot-1", handle: "leader", name: "Leader")
        let coworker = bot(id: "bot-2", handle: "builder", name: "Builder")
        let reviewer = bot(id: "bot-3", handle: "reviewer", name: "Reviewer")
        model.bots = [leader, coworker, reviewer]
        model.connectionState = .ready

        XCTAssertEqual(
            Set(model.availableBotsForSwarm(excluding: leader.id).map(\.id)),
            ["bot-2", "bot-3"]
        )

        model.createSwarm(
            title: "Quiet Foxes",
            leaderBotID: leader.id,
            memberBotIDs: [coworker.id]
        )
        let request = await recorder.firstRequest(after: 0) { request in
            if case .createSwarm = request { return true }
            return false
        }
        guard case .createSwarm(
            let requestID,
            let title,
            let leaderBotID,
            let memberBotIDs
        ) = try XCTUnwrap(request) else {
            return XCTFail("Expected swarm creation")
        }
        XCTAssertEqual(title, "Quiet Foxes")
        XCTAssertEqual(leaderBotID, "bot-1")
        XCTAssertEqual(memberBotIDs, ["bot-2"])
        XCTAssertEqual(model.swarmMutationRequestID, requestID)
        XCTAssertFalse(model.canMutateSwarm)

        let swarm = SwarmRecord(
            id: "swarm-1",
            title: "Quiet Foxes",
            leaderBotId: "bot-1",
            members: [
                SwarmMemberRecord(botId: "bot-1", handle: "leader"),
                SwarmMemberRecord(botId: "bot-2", handle: "builder"),
            ],
            messages: [],
            updatedAtMs: 200
        )
        model.handle(.swarms(requestID: requestID, swarms: [swarm]))

        XCTAssertNil(model.swarmMutationRequestID)
        XCTAssertTrue(model.canMutateSwarm)
        XCTAssertEqual(model.swarm(containingBot: "bot-2")?.title, "Quiet Foxes")
        XCTAssertEqual(model.availableBotsForSwarm().map(\.id), ["bot-3"])
    }

    func testSwarmPostClearsOnlyOnItsCorrelatedCatalog() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let leader = bot(id: "bot-1", handle: "leader", name: "Leader")
        model.bots = [leader]
        model.swarms = [SwarmRecord(
            id: "swarm-1",
            title: "Quiet Foxes",
            leaderBotId: leader.id,
            members: [SwarmMemberRecord(botId: leader.id, handle: leader.handle)],
            messages: [],
            updatedAtMs: 1
        )]
        model.connectionState = .ready

        let requestID = try XCTUnwrap(model.postSwarmMessage(
            to: "swarm-1",
            text: "  @leader check this  "
        ))
        let request = await recorder.firstRequest(after: 0) { request in
            if case .postSwarmMessage = request { return true }
            return false
        }
        guard case .postSwarmMessage(
            let sentID,
            let swarmID,
            let text
        ) = try XCTUnwrap(request) else {
            return XCTFail("Expected Swarm post")
        }
        XCTAssertEqual(sentID, requestID)
        XCTAssertEqual(swarmID, "swarm-1")
        XCTAssertEqual(text, "@leader check this")

        model.handle(.swarms(requestID: nil, swarms: model.swarms))
        XCTAssertEqual(model.swarmMessageRequestID, requestID)
        XCTAssertNil(model.completedSwarmMessageRequestID)

        model.handle(.swarms(requestID: requestID, swarms: model.swarms))
        XCTAssertNil(model.swarmMessageRequestID)
        XCTAssertEqual(model.completedSwarmMessageRequestID, requestID)
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
            originLabel: "swarm",
            botID: helper.id
        )
        model.bots = [helper]
        model.sessions = [visible]
        model.connectionState = .ready

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

        model.handle(.botSessions(
            requestID: requestID,
            botID: helper.id,
            sessions: [hidden]
        ))
        XCTAssertEqual(model.botSessions, [hidden])
        XCTAssertEqual(model.sessions, [visible])
        XCTAssertEqual(model.chatCatalogSessions, [visible])
        XCTAssertFalse(model.unreadSessionIDs.contains(hidden.sessionId))
        XCTAssertNil(model.toast)

        model.selectedSessionID = hidden.sessionId
        model.applySessions([visible])
        XCTAssertEqual(model.selectedSession, hidden)
        XCTAssertTrue(model.selectedSessionIsHidden)

        model.navigationPath.append(.chat(.session(hidden.sessionId)))
        model.botSessions = []
        XCTAssertTrue(model.selectedSessionIsHidden)
    }

    func testBackgroundApprovalSnapshotValidatesOwnershipAndNotifiesOncePerRequest() throws {
        let model = try model()
        model.connectionState = .ready
        let first = BackgroundApproval(
            sessionId: "work-1",
            botId: "bot-1",
            turnId: "turn-1",
            requestId: "approval-1"
        )

        model.handle(.backgroundApprovals([first]))
        let firstToastID = try XCTUnwrap(model.toast?.id)
        XCTAssertEqual(model.backgroundApprovals, [first])
        XCTAssertEqual(model.toast?.message, "Helper needs approval.")
        XCTAssertEqual(model.bot(forSessionID: first.sessionId)?.id, first.botId)

        model.handle(.backgroundApprovals([first]))
        XCTAssertEqual(model.toast?.id, firstToastID)

        let second = BackgroundApproval(
            sessionId: first.sessionId,
            botId: first.botId,
            turnId: first.turnId,
            requestId: "approval-2"
        )
        model.handle(.backgroundApprovals([second]))
        XCTAssertNotEqual(model.toast?.id, firstToastID)

        XCTAssertFalse(model.applyBackgroundApprovals([
            BackgroundApproval(
                sessionId: "work-2",
                botId: "missing-bot",
                turnId: "turn-2",
                requestId: "approval-3"
            )
        ], notifyingNew: true))
        XCTAssertEqual(model.backgroundApprovals, [second])
    }

    func testSwarmAttentionSnapshotAcceptsDepartedBotAndUsesSharedNotificationState() throws {
        let model = try model()
        let helper = bot()
        let leader = bot(id: "bot-2", handle: "leader", name: "Leader")
        let swarm = SwarmRecord(
            id: "swarm-1",
            title: "Quiet Foxes",
            leaderBotId: leader.id,
            members: [SwarmMemberRecord(botId: leader.id, handle: leader.handle)],
            messages: [],
            updatedAtMs: 1
        )
        let baseline = SwarmAttention(
            swarmId: swarm.id,
            swarmTitle: swarm.title,
            messageId: "message-1",
            botId: helper.id,
            text: "Already pending"
        )
        let live = SwarmAttention(
            swarmId: swarm.id,
            swarmTitle: swarm.title,
            messageId: "message-2",
            botId: helper.id,
            text: "  Please choose\nwhich path to take. "
        )
        model.bots = [helper, leader]
        model.swarms = [swarm]
        model.connectionState = .ready

        XCTAssertTrue(model.applySwarmAttentions([baseline], notifyingNew: false))
        XCTAssertNil(model.toast)
        XCTAssertTrue(model.hasSwarmAttention(forSwarmID: swarm.id))

        XCTAssertTrue(model.applySwarmAttentions([baseline, live], notifyingNew: true))
        let toastID = try XCTUnwrap(model.toast?.id)
        XCTAssertEqual(model.toast?.message, "Helper: Please choose which path to take.")
        XCTAssertEqual(model.toast?.tone, .warning)
        XCTAssertEqual(
            model.toast?.target,
            .swarm(swarmID: swarm.id, messageID: live.messageId)
        )
        XCTAssertEqual(model.bot(for: model.toast?.target)?.tint, helper.tint)

        XCTAssertTrue(model.applySwarmAttentions([baseline, live], notifyingNew: true))
        XCTAssertEqual(model.toast?.id, toastID)
        XCTAssertFalse(model.applySwarmAttentions([
            SwarmAttention(
                swarmId: swarm.id,
                swarmTitle: swarm.title,
                messageId: "message-3",
                botId: "missing-bot",
                text: "Invalid"
            )
        ], notifyingNew: true))
        XCTAssertEqual(model.swarmAttentions, [baseline, live])

        XCTAssertTrue(model.applySwarmAttentions([], notifyingNew: false))
        XCTAssertFalse(model.hasSwarmAttention(forSwarmID: swarm.id))
    }

    func testStaleBackgroundApprovalToastCannotOpenHiddenWorkAsAChat() throws {
        let model = try model()
        model.connectionState = .ready
        model.backgroundApprovals = [BackgroundApproval(
            sessionId: "work-1",
            botId: "bot-1",
            turnId: "turn-1",
            requestId: "approval-1"
        )]
        model.applyBackgroundApprovals([], notifyingNew: false)

        model.openNotificationTarget(.session("work-1"))

        XCTAssertNil(model.selectedSessionID)
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
            originLabel: "swarm",
            botID: helper.id
        )
        model.bots = [helper]
        model.sessions = [visible]
        model.connectionState = .ready
        model.destination = .bots
        model.navigationPath = [.swarm("swarm-1"), .swarmChat("swarm-1")]

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
        XCTAssertFalse(requestsBeforeValidation.contains { request in
            if case .openSession = request { return true }
            return false
        })

        model.handle(.botSessions(
            requestID: requestID,
            botID: helper.id,
            sessions: [hidden]
        ))
        let opening = await recorder.firstRequest(after: 1) { request in
            guard case .openSession(_, hidden.sessionId, _) = request else { return false }
            return true
        }

        XCTAssertNotNil(opening)
        XCTAssertEqual(model.sessions, [visible])
        XCTAssertEqual(model.botSessions, [hidden])
        XCTAssertFalse(model.unreadSessionIDs.contains(hidden.sessionId))
        XCTAssertEqual(
            model.navigationPath,
            [.swarm("swarm-1"), .swarmChat("swarm-1"), .chat(.session(hidden.sessionId))]
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
            originLabel: "swarm",
            botID: helper.id
        )
        let other = session(
            sessionID: "work-other",
            state: .awaitingApproval,
            turnID: "turn-other",
            originLabel: "swarm",
            botID: helper.id
        )
        model.bots = [helper]
        model.connectionState = .ready
        model.destination = .bots
        model.navigationPath = [.swarm("swarm-1"), .swarmChat("swarm-1")]

        model.resumeBotSession(botID: helper.id, sessionID: target.sessionId)
        let listing = await recorder.firstRequest(after: 0) { request in
            if case .listBotSessions = request { return true }
            return false
        }
        guard case .listBotSessions(let requestID, _) = try XCTUnwrap(listing) else {
            return XCTFail("Expected hidden Bot session discovery")
        }

        model.handle(.botSessions(
            requestID: "stale-request",
            botID: helper.id,
            sessions: [target]
        ))
        XCTAssertEqual(model.pendingBotSessionResume?.sessionID, target.sessionId)

        model.handle(.botSessions(
            requestID: requestID,
            botID: helper.id,
            sessions: [other]
        ))

        XCTAssertNil(model.pendingBotSessionResume)
        XCTAssertEqual(model.botSessions, [other])
        XCTAssertEqual(
            model.navigationPath,
            [.swarm("swarm-1"), .swarmChat("swarm-1")]
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
        model.sessions = [source]
        model.connectionState = .ready

        model.resumeBotSession(botID: helper.id, sessionID: source.sessionId)
        let opening = await recorder.firstRequest(after: 0) { request in
            guard case .openSession(_, source.sessionId, _) = request else { return false }
            return true
        }

        XCTAssertNotNil(opening)
        XCTAssertEqual(model.destination, .chats)
        XCTAssertEqual(model.navigationPath, [.chat(.session(source.sessionId))])
        let requests = await recorder.requests()
        XCTAssertFalse(requests.contains { request in
            if case .listBotSessions = request { return true }
            return false
        })
    }

    func testApplyingSwarmsRejectsMissingLeadersAndUnorderedMessages() throws {
        let model = try model { _ in }
        model.bots = [bot()]
        let leader = SwarmMemberRecord(botId: "bot-1", handle: "helper")
        let message = { (id: String, sequence: UInt64) in
            SwarmMessageRecord(
                id: id,
                sequence: sequence,
                authorBotId: leader.botId,
                authorHandle: leader.handle,
                sourceSessionId: "chat-1",
                text: id,
                createdAtMs: Int64(sequence),
                inReplyToMessageId: nil,
                replyDepth: 0
            )
        }
        let valid = SwarmRecord(
            id: "swarm-1",
            title: "Quiet Foxes",
            leaderBotId: leader.botId,
            members: [leader],
            messages: [message("one", 1), message("two", 2)],
            updatedAtMs: 2
        )
        model.applySwarms([valid])

        model.applySwarms([SwarmRecord(
            id: "swarm-1",
            title: valid.title,
            leaderBotId: leader.botId,
            members: [],
            messages: [],
            updatedAtMs: 3
        )])
        XCTAssertEqual(model.swarms, [valid])

        model.applySwarms([SwarmRecord(
            id: "swarm-1",
            title: valid.title,
            leaderBotId: leader.botId,
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
            botDefaults: VersionedAgentConfig(revision: 1, config: composition()),
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
        let presentationRevision = model.chatPresentationRevision
        model.openChat("chat-2")

        XCTAssertEqual(model.destination, .chats)
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-2"))])
        XCTAssertEqual(model.chatPresentationRevision, presentationRevision + 1)
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
        model.navigationPath = []
        model.openChat("chat-2")
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-2"))])
        XCTAssertEqual(model.chatPresentationRevision, presentationRevision + 2)
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
        model.chatBotFilterIDs = [alpha.id]

        model.openBotChats("missing")
        XCTAssertEqual(model.destination, .bots)
        XCTAssertEqual(model.navigationPath, [.bot(beta.id)])
        XCTAssertEqual(model.chatBotFilterIDs, [alpha.id])

        model.openBotChats(beta.id)
        XCTAssertEqual(model.chatBotFilterIDs, [beta.id])
        XCTAssertEqual(model.destination, .chats)
        XCTAssertTrue(model.navigationPath.isEmpty)
    }

    func testPoppingNavigationPathClearsPresentedChat() throws {
        let model = try model()
        model.selectedSessionID = "chat-1"
        model.navigationPath = [.chat(.session("chat-1"))]

        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-1"))])

        model.navigationPath = []

        XCTAssertNil(model.presentedChatSessionID)
    }

    func testSingleBotIsAutoSelectedAndPresentsChatAfterGatewayOpensIt() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        model.connectionState = .ready
        let helper = bot()
        model.bots = [helper]

        let requestCount = await recorder.requestCount()
        model.chooseWorkspace("/srv/mobius")
        XCTAssertEqual(model.navigationPath, [.chat(.new)])
        XCTAssertEqual(model.pendingNewChatBotID, helper.id)
        try await Task.sleep(for: .milliseconds(20))
        let stagedRequests = await recorder.requests()
        XCTAssertTrue(stagedRequests.dropFirst(requestCount).isEmpty)

        model.composer = "Inspect the project"
        XCTAssertTrue(model.sendMessage())
        let request = await recorder.firstRequest(after: requestCount) { request in
            if case .createSession = request { return true }
            return false
        }
        guard case .createSession(let requestID, let path, let botID) = try XCTUnwrap(request) else {
            return XCTFail("Expected a create-session request")
        }
        XCTAssertEqual(path, "/srv/mobius")
        XCTAssertEqual(botID, "bot-1")
        XCTAssertEqual(model.navigationPath, [.chat(.new)])

        model.handle(.sessionOpened(
            requestID: requestID,
            payload: sessionReady(latestSequence: 0, sessionID: "chat-created")
        ))
        model.handle(.sessionReplayComplete(
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
        XCTAssertEqual(model.selectedSessionID, "chat-created")
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-created"))])
        XCTAssertNil(model.pendingNewChatBotID)
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
        model.connectionState = .ready
        let helper = bot()
        model.bots = [helper]
        model.chooseWorkspace("/srv/mobius")
        model.composer = "Try again"

        XCTAssertTrue(model.sendMessage())
        let request = await recorder.firstRequest(after: 0) {
            if case .createSession = $0 { return true }
            return false
        }
        guard case .createSession(let requestID, _, _) = try XCTUnwrap(request) else {
            return XCTFail("Expected a create-session request")
        }

        model.handle(.rejected(GatewayRejection(
            requestId: requestID,
            code: "create_failed",
            message: "Chat could not be created",
            fatal: false
        )))

        XCTAssertEqual(model.connectionState, .ready)
        XCTAssertEqual(model.pendingNewChatBotID, helper.id)
        XCTAssertEqual(model.composer, "Try again")
        XCTAssertNil(model.selectedSessionID)
    }

    func testDeletingMultipleChatsUsesOneAtomicRequest() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let first = session(sessionID: "chat-1", state: .idle)
        let second = session(sessionID: "chat-2", state: .idle)
        model.connectionState = .ready
        model.sessions = [first, second]

        model.deleteSessions([first, second, first])

        let request = await recorder.firstRequest(after: 0) { request in
            guard case .deleteSessions(_, let ids) = request else { return false }
            return ids == ["chat-1", "chat-2"]
        }
        guard case .deleteSessions(let requestID, _) = try XCTUnwrap(request) else {
            return XCTFail("Expected one multi-chat deletion")
        }
        model.handle(.accepted(requestID: requestID))
        model.handle(.sessions(requestID: requestID, sessions: []))

        XCTAssertTrue(model.sessions.isEmpty)
        XCTAssertNil(model.sessionMutationRequestID)
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
            guard case .deleteSessions(_, let ids) = request else { return false }
            return ids == ["chat-1"]
        }
        guard case .deleteSessions(let requestID, _) = try XCTUnwrap(request) else {
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
            guard case .deleteSessions(_, let ids) = request else { return false }
            return ids == ["chat-1"]
        }
        guard case .deleteSessions(let requestID, _) = try XCTUnwrap(deleteRequest) else {
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
            if case .deleteSessions = request { throw URLError(.cannotConnectToHost) }
        }
        let selected = session(sessionID: "chat-1", state: .running, turnID: "turn-1")
        model.connectionState = .ready
        model.sessions = [selected]
        model.selectedSessionID = selected.sessionId
        model.destination = .chats
        model.navigationPath = [.chat(.session(selected.sessionId))]
        model.activeTurnID = "turn-1"
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

    func testTurnCompleteFlushesPendingReasoning() throws {
        let model = try model()

        for delta in ["think", "ing"] {
            model.reduce(
                event: AgentEventRecord(submissionId: nil, msg: .object([
                    "type": .string("assistant_content_delta"),
                    "sessionId": .string("chat-1"),
                    "turnId": .string("turn-1"),
                    "modelStepId": .string("reasoning-1"),
                    "phase": .string("reasoning"),
                    "delta": .string(delta)
                ])),
                blocks: [],
                preview: nil
            )
        }
        XCTAssertTrue(model.transcript.isEmpty)

        model.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object([
                "type": .string("turn_complete")
            ])),
            blocks: [],
            preview: nil
        )

        XCTAssertEqual(model.transcript.map(\.text), ["thinking"])
        XCTAssertFalse(try XCTUnwrap(model.transcript.first).pending)
    }

    func testTurnCompleteCollapsesCompactionAndSteeringIntoFinishedTurnWork() throws {
        let model = try model()
        let turnID = "turn-1"
        model.reduce(record: recorded(1, .object([
            "type": .string("turn_started"),
            "turnId": .string(turnID),
        ])))
        model.reduce(record: recorded(2, testMessageEvent(text: "Start")))
        model.reduce(record: recorded(3, testAssistantMessage(
            turnID: turnID,
            modelStepID: "step-1",
            phase: "commentary",
            text: "Checking"
        )))
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
        model.reduce(record: recorded(5, testMessageEvent(
            delivery: .steer,
            text: "Also check tests"
        )))
        model.reduce(record: recorded(6, testMessageEvent(
            author: .peer(
                messageID: "message-1",
                sessionID: "chat-reviewer",
                handle: "@reviewer"
            ),
            delivery: .steer,
            text: "The parser boundary is covered."
        )))
        model.reduce(record: recorded(7, testAssistantMessage(
            turnID: turnID,
            modelStepID: "step-2",
            text: "Done"
        )))

        XCTAssertEqual(
            model.transcriptProjection(breakBefore: nil).rows.map(\.kind),
            [.user, .narrative, .activityGroup, .user, .peer, .narrative]
        )

        model.reduce(record: recorded(8, .object([
            "type": .string("turn_complete"),
            "turnId": .string(turnID),
        ])))

        let projection = model.transcriptProjection(breakBefore: nil)
        XCTAssertEqual(projection.rows.map(\.kind), [.user, .workedGroup, .narrative])
        XCTAssertEqual(
            projection.rows[1].records.map(\.text),
            ["Checking", "", "Also check tests", "The parser boundary is covered."]
        )
        XCTAssertEqual(
            projection.rows[1].records.map(\.title),
            ["", "context compacted", "", ""]
        )
        XCTAssertEqual(
            projection.rows[1].records.compactMap { $0.messageMetadata?.delivery },
            [.steer, .steer]
        )
        XCTAssertEqual(projection.rows[1].elapsedMs, 600)
        XCTAssertEqual(model.transcript.map(\.turnID), Array(repeating: turnID, count: 6))
        XCTAssertEqual(
            model.transcript.map(\.startsTurn),
            [true, false, false, false, false, false]
        )
        XCTAssertEqual(TranscriptProjection.turnCount(in: model.transcript), 1)
        XCTAssertEqual(model.transcript.last?.turnElapsedMs, 600)
    }

    func testPeerMessageStartsAndCollapsesACompletedTurnLikeUserMessage() throws {
        let model = try model()
        let turnID = "peer-turn"
        model.reduce(record: recorded(1, .object([
            "type": .string("turn_started"),
            "turnId": .string(turnID),
        ])))
        model.reduce(record: recorded(2, testMessageEvent(
            author: .peer(
                messageID: "message-1",
                sessionID: "chat-reviewer",
                handle: "@reviewer"
            ),
            text: "Review the parser boundary."
        )))
        model.reduce(record: recorded(3, testAssistantMessage(
            turnID: turnID,
            modelStepID: "step-1",
            phase: "commentary",
            text: "Checking"
        )))
        model.reduce(record: recorded(4, testAssistantMessage(
            turnID: turnID,
            modelStepID: "step-2",
            text: "Done"
        )))
        model.reduce(record: recorded(5, .object([
            "type": .string("turn_complete"),
            "turnId": .string(turnID),
        ])))

        XCTAssertEqual(model.transcript.map(\.turnID), Array(repeating: turnID, count: 3))
        XCTAssertEqual(model.transcript.map(\.startsTurn), [true, false, false])
        XCTAssertEqual(model.transcript.first?.messageMetadata?.delivery, .turn)
        XCTAssertEqual(
            model.transcript.first?.messageMetadata?.author,
            .peer(
                messageID: "message-1",
                sessionID: "chat-reviewer",
                handle: "@reviewer"
            )
        )
        XCTAssertEqual(
            model.transcriptProjection(breakBefore: nil).rows.map(\.kind),
            [.peer, .workedGroup, .narrative]
        )
    }

    func testQueuedMessageStartsTheNextTranscriptTurn() throws {
        let model = try model()
        for (sequence, turnID, delivery, text) in [
            (UInt64(1), "turn-1", MessageDelivery.turn, "Start"),
            (UInt64(5), "turn-2", MessageDelivery.queue, "Follow up"),
        ] {
            model.reduce(record: recorded(sequence, .object([
                "type": .string("turn_started"),
                "turnId": .string(turnID),
            ])))
            model.reduce(record: recorded(
                sequence + 1,
                testMessageEvent(delivery: delivery, text: text)
            ))
            model.reduce(record: recorded(
                sequence + 2,
                testAssistantMessage(
                    turnID: turnID,
                    modelStepID: "step-\(turnID)",
                    text: "Done \(turnID)"
                )
            ))
            model.reduce(record: recorded(sequence + 3, .object([
                "type": .string("turn_complete"),
                "turnId": .string(turnID),
            ])))
        }

        XCTAssertEqual(
            model.transcript
                .filter { $0.kind == .user }
                .compactMap { $0.messageMetadata?.delivery },
            [.turn, .queue]
        )
        XCTAssertEqual(
            model.transcript.filter { $0.kind == .user }.map(\.startsTurn),
            [true, true]
        )
        XCTAssertEqual(TranscriptProjection.turnCount(in: model.transcript), 2)
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
