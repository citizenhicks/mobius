import Foundation
import SwiftUI
import UIKit
@testable import Mobius
import XCTest

@MainActor
extension AppModelTests {
    func testPrivateBotConversationsPageWithoutChangingTheActiveChat() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { await recorder.record($0) }
        model.bots = [bot()]
        model.gateway.connectionState = .ready
        model.chat.sessions = [session(state: .idle)]
        model.chat.selectedSessionID = "chat-1"
        model.chat.composer = "Unsent public draft"
        model.chat.transcript = [
            TranscriptEntry(
                id: "public", text: "Public history", kind: .assistant,
                format: "plain_text", pending: false)
        ]
        model.openBotConversations("bot-1")
        let listing = await recorder.firstRequest(after: 0) {
            if case .listBotConversations = $0 { return true }
            return false
        }
        guard case .listBotConversations(let firstID, "bot-1", nil) = try XCTUnwrap(listing)
        else { return XCTFail("Expected first private conversation page") }
        let first = privateBotConversation(id: "private-2")
        let cursor = BotConversationCursor(updatedAt: 100, sequence: 4, sessionId: first.id)
        model.gateway.handle(
            .botConversations(
                requestID: "stale", botID: "bot-1",
                page: BotConversationPage(conversations: [first], nextCursor: nil)))
        XCTAssertTrue(model.botConversationState.conversations.isEmpty)
        model.gateway.handle(
            .botConversations(
                requestID: firstID, botID: "bot-1",
                page: BotConversationPage(conversations: [first], nextCursor: cursor)))
        model.loadMoreBotConversations()
        let next = await recorder.firstRequest(after: 1) {
            if case .listBotConversations = $0 { return true }
            return false
        }
        guard
            case .listBotConversations(let nextID, "bot-1", let requestedCursor) = try XCTUnwrap(
                next)
        else { return XCTFail("Expected durable continuation") }
        XCTAssertEqual(requestedCursor, cursor)
        let second = privateBotConversation(id: "private-1")
        model.gateway.handle(
            .botConversations(
                requestID: nextID, botID: "bot-1",
                page: BotConversationPage(conversations: [second], nextCursor: nil)))
        XCTAssertEqual(model.botConversationState.conversations.map(\.id), [first.id, second.id])
        XCTAssertNil(model.botConversationState.nextCursor)
        model.presentBotConversation(first)
        let history = await recorder.firstRequest(after: 2) {
            if case .getBotConversationHistory = $0 { return true }
            return false
        }
        guard case .getBotConversationHistory(_, "bot-1", first.id, nil) = try XCTUnwrap(history)
        else { return XCTFail("Expected read-only private history") }
        XCTAssertEqual(model.chat.selectedSessionID, "chat-1")
        XCTAssertEqual(model.chat.transcript.map(\.text), ["Public history"])
        XCTAssertEqual(model.chat.composer, "Unsent public draft")
        XCTAssertEqual(model.chat.sessions.map(\.sessionId), ["chat-1"])
        XCTAssertEqual(model.navigationPath, [.botConversations("bot-1")])
        XCTAssertEqual(model.bot(forSessionID: first.id)?.id, "bot-1")
        let requests = await recorder.requests()
        XCTAssertFalse(
            requests.contains {
                if case .openSession = $0 { return true }; return false
            })
    }

    func testPrivateBotHistoryUsesSharedToolRenderingAndRejectsStalePages() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { await recorder.record($0) }
        model.bots = [bot()]
        model.gateway.connectionState = .ready
        let conversation = privateBotConversation()
        model.botConversationState = BotConversationState(
            botID: "bot-1", conversations: [conversation])
        model.presentBotConversation(conversation)
        let firstID = try XCTUnwrap(model.botConversationState.historyRequest?.id)
        let file = SessionFileReference(
            id: "result-file", name: "result.txt", size: 2, mediaType: "text/plain")
        let rendered = renderEvent(
            id: "read-1", title: "Read file", text: "Tool output", files: [file])
        let block = try FrontendBlock(json: XCTUnwrap(rendered.msg["block"]))
        let result = recorded(
            4,
            .object([
                "type": .string("tool_call_end"), "turnId": .string("turn"),
                "callId": .string("read-1"), "name": .string("read_file"),
                "output": .array([
                    .object(["type": .string("input_text"), "text": .string("Tool output")])
                ]),
                "isError": .bool(false),
            ]),
            blocks: [RenderedBlock(capability: "tools", block: block)])
        model.gateway.handle(
            .botConversationHistory(
                requestID: firstID, botID: "other", conversationID: conversation.id,
                records: [result], nextBeforeSequence: 4))
        XCTAssertEqual(model.botConversationState.historyRequest?.id, firstID)
        model.gateway.handle(
            .botConversationHistory(
                requestID: firstID, botID: "bot-1", conversationID: conversation.id,
                records: [result], nextBeforeSequence: 4))
        XCTAssertEqual(model.botConversationState.entries.map(\.text), ["Tool output"])
        XCTAssertEqual(model.botConversationState.entries.first?.files, [file])
        let pageLoad = Task { await model.loadEarlierBotConversationHistoryAndWait() }
        let earlier = await recorder.firstRequest(after: 1) {
            if case .getBotConversationHistory(_, _, _, 4) = $0 { return true }
            return false
        }
        guard case .getBotConversationHistory(let earlierID, _, _, 4) = try XCTUnwrap(earlier)
        else { return XCTFail("Expected earlier private history") }
        let message = recorded(1, testMessageEvent(text: "Check the private result"))
        model.gateway.handle(
            .botConversationHistory(
                requestID: earlierID, botID: "bot-1", conversationID: conversation.id,
                records: [message], nextBeforeSequence: nil))
        await pageLoad.value
        let shared = try self.model()
        shared.chat.mergeHistory([message, result])
        XCTAssertEqual(
            model.botConversationState.entries.map(\.text), shared.chat.transcript.map(\.text))
        XCTAssertEqual(
            TranscriptProjection(entries: model.botConversationState.entries).rows.map(\.kind),
            TranscriptProjection(entries: shared.chat.transcript).rows.map(\.kind))
        model.closeBotConversation()
        model.gateway.handle(
            .botConversationHistory(
                requestID: earlierID, botID: "bot-1", conversationID: conversation.id,
                records: [result], nextBeforeSequence: nil))
        XCTAssertTrue(model.botConversationState.entries.isEmpty)
        XCTAssertNil(model.chat.previewFileSource)
    }

    func testPrivateBotConversationErrorsAndDeletionClearOnlyPrivateState() throws {
        let model = try model { _ in }
        let helper = bot()
        model.bots = [helper]
        model.gateway.connectionState = .ready
        model.chat.selectedSessionID = "chat-1"
        model.chat.composer = "Keep the draft"
        model.openBotConversations(helper.id)
        let id = try XCTUnwrap(model.botConversationState.request?.id)
        model.gateway.handle(
            .botConversations(
                requestID: id, botID: helper.id,
                page: BotConversationPage(
                    conversations: [privateBotConversation(botID: "other")], nextCursor: nil)))
        XCTAssertNotNil(model.botConversationState.error)
        XCTAssertTrue(model.botConversationState.conversations.isEmpty)
        model.refreshBotConversations(helper.id)
        model.gateway.handle(
            .rejected(
                GatewayRejection(
                    requestId: try XCTUnwrap(model.botConversationState.request?.id),
                    code: "conflict", message: "Try again", fatal: false)))
        XCTAssertEqual(model.botConversationState.error, "Try again")
        XCTAssertNil(model.botConversationState.request)
        let conversation = privateBotConversation()
        model.botConversationState.conversations = [conversation]
        model.presentBotConversation(conversation)
        model.applyBots([])
        XCTAssertNil(model.botConversationState.botID)
        XCTAssertNil(model.botConversationState.presented)
        XCTAssertNil(model.botConversationState.historyRequest)
        XCTAssertEqual(model.chat.selectedSessionID, "chat-1")
        XCTAssertEqual(model.chat.composer, "Keep the draft")
    }

    func privateBotConversation(id: String = "private-1", botID: String = "bot-1")
        -> BotConversation
    {
        BotConversation(
            conversationId: id, botId: botID, chatId: "chat-1",
            sessionContext: SessionContext(originLabel: "group"), sequence: 4,
            firstUserMessage: "Review private history", executionStats: ExecutionStats(),
            activity: SessionActivity(
                state: .idle, turnId: nil, approvalRequestId: nil,
                startedAt: nil, lastOutcome: nil, message: nil),
            createdAt: 100, updatedAt: 200)
    }

    func testPrivateConversationListUsesTheSharedLoadingPlaceholder() async throws {
        let model = try model { _ in }
        model.bots = [bot()]
        model.gateway.connectionState = .ready
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 402, height: 874)
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.makeKeyAndVisible()
        }
        let host = UIHostingController(
            rootView: NavigationStack { BotConversationsView(botID: "bot-1") }
                .modifier(MobiusTheme()).environment(model))
        window.rootViewController = host
        window.makeKeyAndVisible()
        let appeared = await eventually {
            testAccessibilityElements(host.view).contains {
                $0.accessibilityLabel == "Loading conversations"
            }
        }
        XCTAssertTrue(appeared)
        XCTAssertNotNil(model.botConversationState.request)
        let image = UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
            window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
        }
        let attachment = XCTAttachment(image: image)
        attachment.name = "private-conversations-loading-placeholder"
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    func testPrivateConversationSheetUsesTheSharedReadOnlyTranscriptView() async throws {
        let model = try model { _ in }
        model.bots = [bot(name: "Private Helper")]
        model.gateway.connectionState = .ready
        model.chat.selectedSessionID = "public-chat"
        model.chat.composer = "Public draft"
        let conversation = privateBotConversation()
        let markdown =
            "# Private answer\n\nA **Markdown** result.\n\n- First finding\n- Second finding"
        model.botConversationState = BotConversationState(
            botID: "bot-1", conversations: [conversation], presented: conversation,
            historyRequest: ("private-history", nil))
        model.gateway.handle(
            .botConversationHistory(
                requestID: "private-history", botID: "bot-1", conversationID: conversation.id,
                records: [
                    recorded(
                        1, .object(["type": .string("turn_started"), "turnId": .string("turn-1")])),
                    recorded(2, testMessageEvent(text: "Review the findings")),
                    recorded(
                        3,
                        testAssistantMessage(
                            turnID: "turn-1", modelStepID: "work", phase: "commentary",
                            text: "Checking the findings")),
                    recorded(
                        4,
                        testAssistantMessage(
                            turnID: "turn-1", modelStepID: "answer", text: markdown)),
                    recorded(
                        5, .object(["type": .string("turn_complete"), "turnId": .string("turn-1")])),
                ], nextBeforeSequence: nil))
        XCTAssertEqual(model.botConversationState.entries.last?.text, markdown)
        XCTAssertEqual(
            TranscriptProjection(entries: model.botConversationState.entries).rows.map(\.kind),
            [.user, .workedGroup, .narrative])
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 402, height: 874)
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.makeKeyAndVisible()
        }
        let host = UIHostingController(
            rootView: BotConversationTranscriptSheet().modifier(MobiusTheme()).environment(model))
        window.rootViewController = host
        window.makeKeyAndVisible()
        let appeared = await eventually {
            testAccessibilityElements(host.view).compactMap(\.accessibilityLabel)
                .joined(separator: " ")
                .contains("Second finding")
        }
        XCTAssertTrue(appeared)
        let labels = testAccessibilityElements(host.view).compactMap(\.accessibilityLabel)
        XCTAssertTrue(labels.contains("Copy"))
        XCTAssertTrue(labels.contains("Private Helper"))
        XCTAssertTrue(labels.contains { $0.contains("Review the findings") })
        XCTAssertTrue(labels.contains { $0.contains("Worked for") })
        XCTAssertFalse(labels.contains { $0.contains("Checking the findings") })
        XCTAssertFalse(labels.contains("Reply"))
        XCTAssertFalse(labels.contains("Send"))
        XCTAssertEqual(model.chat.selectedSessionID, "public-chat")
        XCTAssertEqual(model.chat.composer, "Public draft")
        let image = UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
            window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
        }
        let attachment = XCTAttachment(image: image)
        attachment.name = "private-conversation-shared-transcript"
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    func testPrivateConversationAttachmentOpensAboveTheTranscriptSheet() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { await recorder.record($0) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready
        model.bots = [bot()]
        model.destination = .bots
        let conversation = privateBotConversation()
        let contents = "Private attachment contents"
        let file = SessionFileReference(
            id: "private-file", name: "private.txt", size: Int64(contents.utf8.count),
            mediaType: "text/plain")
        model.botConversationState = BotConversationState(
            botID: "bot-1", conversations: [conversation],
            entries: [
                TranscriptEntry(
                    id: "answer", text: "", kind: .assistant,
                    format: "plain_text", pending: false, files: [file])
            ])
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 402, height: 874)
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.makeKeyAndVisible()
        }
        let host = UIHostingController(
            rootView: AppShell().modifier(MobiusTheme()).environment(model))
        window.rootViewController = host
        window.makeKeyAndVisible()
        model.chat.previewFileSource = .botConversation(
            botID: "bot-1", conversationID: conversation.id)
        model.botConversationState.presented = conversation
        let appeared = await eventually(timeout: .seconds(3)) {
            guard let presented = host.presentedViewController,
                !presented.isBeingPresented, presented.transitionCoordinator == nil
            else { return false }
            return testAccessibilityElements(window).contains {
                $0.accessibilityLabel == "Open file private.txt"
            }
        }
        XCTAssertTrue(appeared)
        let fileButton = try XCTUnwrap(
            testAccessibilityElements(window).first {
                $0.accessibilityLabel == "Open file private.txt"
            })
        XCTAssertTrue(fileButton.accessibilityActivate())
        let read = await recorder.firstRequest(after: 0) {
            if case .readBotConversationFile = $0 { return true }
            return false
        }
        guard
            case .readBotConversationFile(let requestID, "bot-1", conversation.id, file.id, 0, _) =
                try XCTUnwrap(read)
        else { return XCTFail("Expected private attachment read") }
        model.gateway.handle(
            .sessionFileChunk(
                requestID: requestID, sessionID: conversation.id, fileID: file.id,
                offset: 0, data: Data(contents.utf8), nextOffset: nil))
        let filePresented = await eventually(timeout: .seconds(3)) {
            testAccessibilityElements(window).contains { $0.accessibilityLabel == "Done" }
        }
        XCTAssertTrue(filePresented)
        XCTAssertEqual(model.botConversationState.presented?.id, conversation.id)
        let image = UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
            window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
        }
        let attachment = XCTAttachment(image: image)
        attachment.name = "private-attachment-above-transcript"
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    func testPrimaryBotFollowsSelectionWithoutChangingSelectionOrder() throws {
        let model = try model { _ in }
        model.gateway.connectionState = .ready
        let helper = bot()
        let reviewer = bot(id: "bot-2", handle: "reviewer", name: "Reviewer")
        model.bots = [helper, reviewer]
        model.chooseWorkspace("/srv/project")
        model.selectBotForNewChat(reviewer)
        model.selectBotForNewChat(helper)
        XCTAssertEqual(model.chat.pendingNewChatPrimaryBotID, reviewer.id)
        model.selectPrimaryBotForNewChat(helper)
        XCTAssertEqual(model.chat.pendingNewChatPrimaryBotID, helper.id)
        XCTAssertEqual(model.chat.pendingNewChatBotIDs, [reviewer.id, helper.id])
        model.selectBotForNewChat(helper, selected: false)
        XCTAssertEqual(model.chat.pendingNewChatPrimaryBotID, reviewer.id)
        model.selectPrimaryBotForNewChat(helper)
        XCTAssertEqual(model.chat.pendingNewChatPrimaryBotID, reviewer.id)
        model.selectBotForNewChat(reviewer, selected: false)
        XCTAssertNil(model.chat.pendingNewChatPrimaryBotID)
    }

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
        XCTAssertEqual(app.chat.sessions.first?.primaryBotId, "bot-1")
        XCTAssertFalse(app.canReassignSession(original))
        app.gateway.handle(.accepted(requestID: id))
        XCTAssertEqual(app.chat.sessions.first?.primaryBotId, "bot-1")
        app.gateway.handle(
            .sessions(requestID: id, sessions: [session(state: .idle, botID: "bot-2"), other]))
        XCTAssertNil(app.chat.sessionMutationRequestID)
        XCTAssertEqual(app.chat.sessions.first?.primaryBotId, "bot-2")
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
        let opened = await recorder.firstRequest(after: 0) { request in
            if case .openSession(_, "chat-2", _) = request { true } else { false }
        }
        XCTAssertNotNil(opened)

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
        model.selectBotForNewChat(helper, selected: false)
        model.selectBotForNewChat(helper)
        model.selectBotForNewChat(reviewer)
        XCTAssertEqual(model.chat.pendingNewChatBotIDs, [reviewer.id, helper.id])
        XCTAssertEqual(model.selectedChatBots.map(\.name), ["Reviewer", "Helper"])
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
        XCTAssertEqual(model.chat.pendingNewChatBotIDs, [reviewer.id, helper.id])
        XCTAssertTrue(model.sendMessage())
        let request = await recorder.firstRequest(after: 0) { request in
            guard
                case .createSession(_, "/srv/final-project", let botIDs, let primaryBotID) = request,
                botIDs == ["bot-2", "bot-1"]
                    && primaryBotID == "bot-2"
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

    func testBackgroundApprovalSnapshotValidatesOwnershipAndNotifiesOncePerRequest() throws {
        let model = try model()
        model.gateway.connectionState = .ready
        let first = backgroundApproval()

        model.gateway.handle(.backgroundApprovals([first]))
        let firstToastID = try XCTUnwrap(model.toast?.id)
        XCTAssertEqual(model.backgroundApprovals, [first])
        XCTAssertEqual(model.toast?.message, "Helper needs approval.")
        XCTAssertEqual(model.toast?.target, .approval(first.id))
        XCTAssertEqual(model.bot(for: model.toast?.target)?.id, first.botId)

        model.gateway.handle(.backgroundApprovals([first]))
        XCTAssertEqual(model.toast?.id, firstToastID)

        let second = backgroundApproval(id: "approval-2")
        model.gateway.handle(.backgroundApprovals([second]))
        XCTAssertNotEqual(model.toast?.id, firstToastID)

        XCTAssertFalse(
            model.applyBackgroundApprovals(
                [
                    backgroundApproval(id: "approval-3", botID: "missing-bot")
                ], notifyingNew: true))
        XCTAssertEqual(model.backgroundApprovals, [second])
    }

    func testStaleBackgroundApprovalToastCannotOpenAChat() throws {
        let model = try model()
        model.gateway.connectionState = .ready
        model.backgroundApprovals = [backgroundApproval()]
        model.applyBackgroundApprovals([], notifyingNew: false)

        model.openNotificationTarget(.approval("approval-1"))

        XCTAssertNil(model.chat.selectedSessionID)
        XCTAssertTrue(model.navigationPath.isEmpty)
    }

    func testCanonicalApprovalsRefreshSelectedChatAndUseRequestingBotIdentity() throws {
        let app = try model()
        let reviewer = bot(id: "bot-2", handle: "reviewer", name: "Reviewer")
        app.bots.append(reviewer)
        app.gateway.connectionState = .ready
        var group = session(state: .running, title: "Release review")
        group.memberBotIds = ["bot-1", reviewer.id]
        app.chat.sessions = [group]
        app.chat.selectedSessionID = group.sessionId
        let first = backgroundApproval(chatID: group.sessionId)
        let second = backgroundApproval(
            id: "approval-2", botID: reviewer.id, chatID: group.sessionId)
        XCTAssertTrue(app.applyBackgroundApprovals([first], notifyingNew: false))
        app.openApproval(first.id)
        XCTAssertEqual(app.presentedApproval?.id, first.id)

        XCTAssertTrue(app.applyBackgroundApprovals([second], notifyingNew: true))
        XCTAssertEqual(app.chat.pendingApprovals.map(\.id), [second.id])
        XCTAssertNil(app.presentedApproval)
        XCTAssertEqual(app.toast?.message, "Reviewer needs approval.")
        XCTAssertEqual(app.bot(for: app.toast?.target)?.id, reviewer.id)

        XCTAssertTrue(app.applyBackgroundApprovals([], notifyingNew: true))
        XCTAssertTrue(app.chat.pendingApprovals.isEmpty)
        app.presentSessionNotification(
            .completed, sessionID: group.sessionId, runCount: 1, detail: "Checks passed.")
        XCTAssertEqual(app.toast?.message, "Release review: Checks passed.")
        XCTAssertNil(app.bot(for: app.toast?.target))
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
        guard case .createSession(let requestID, let path, let botID, _) = try XCTUnwrap(request)
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
            guard case .submit("chat-created", let submission, _) = request,
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
            if case .submit("chat-created", _, _) = $0 { return true }
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
        guard case .createSession(let requestID, _, _, _) = try XCTUnwrap(request) else {
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
                        messageID: "reply", sessionID: "group-1", handle: "reviewer",
                        symbol: nil),
                    delivery: .turn, text: "Review complete"
                )))
        XCTAssertEqual(app.chat.transcript.last?.text, "Review complete")
        XCTAssertEqual(
            app.chat.transcript.last?.messageMetadata?.author,
            .peer(
                messageID: "reply", sessionID: "group-1", handle: "reviewer", symbol: nil))
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
        app.approvalReviewRequest = ("decision-a", "a")
        app.chat.reduce(
            record: recorded(
                103,
                .object([
                    "type": .string("turn_complete"), "turnId": .string("turn-a"),
                ])))
        XCTAssertEqual(app.chat.activeTurnIDs, ["turn-b"])
        XCTAssertEqual(app.chat.pendingApproval?.id, "b")
        XCTAssertEqual(app.approvalReviewRequest?.id, "decision-a")
        app.gateway.handle(.accepted(requestID: "decision-a"))
        XCTAssertEqual(app.chat.pendingApproval?.id, "b")
    }

}
