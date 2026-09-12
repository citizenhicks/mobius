import Foundation
import SwiftUI
import UIKit
@testable import Mobius
import XCTest

@MainActor
extension AppModelTests {
    func testSimplifiedChatPresentationPersistsWithoutDiscardingWidgetsOrNewChatChoices() throws {
        let app = try model()
        app.chat.selectedSessionID = "chat-1"
        app.chat.mountedWidgets = ["voice", "tasks", "context"].map { symbol in
            MountedWidget(
                capability: "test",
                widget: FrontendWidget(
                    id: symbol, slot: .composerFooter, text: symbol, tone: "neutral",
                    symbol: symbol,
                    iconOnly: false, progress: nil, content: nil, action: nil
                ))
        }
        app.setSimplifiedChatUI(true)
        XCTAssertEqual(app.composerWidgets(in: .composerFooter).map(\.widget.id), ["voice"])
        XCTAssertEqual(app.chat.mountedWidgets.count, 3)
        app.chat.selectedSessionID = "chat-2"
        XCTAssertEqual(app.composerWidgets(in: .composerFooter).count, 1)
        app.chat.selectedSessionID = nil
        app.chat.pendingNewChatWorkspace = "/srv/project"
        app.chat.pendingNewChatBotIDs = ["bot-1"]
        XCTAssertEqual(app.composerWidgets(in: .composerFooter).count, 3)
        XCTAssertEqual(app.chat.pendingNewChatWorkspace, "/srv/project")
        XCTAssertEqual(app.chat.pendingNewChatBotIDs, ["bot-1"])
        let restored = AppModel(store: app.store, settingsDefaults: app.settingsDefaults)
        XCTAssertTrue(restored.simplifiedChatUI)
        app.setSimplifiedChatUI(false)
        app.chat.selectedSessionID = "chat-1"
        XCTAssertEqual(app.composerWidgets(in: .composerFooter).count, 3)
    }

    func testEventReadsPersistPerGatewayAndNeverResolvePendingActions() throws {
        let app = try eventCentreModel()
        let accountID = try XCTUnwrap(app.gateway.selectedAccountID)
        XCTAssertEqual(app.eventCentreItems.filter { $0.id == "approval:request-1" }.count, 1)
        XCTAssertTrue(app.hasUnreadEvents)
        app.markEventsRead(app.eventCentreItems)
        XCTAssertFalse(app.hasUnreadEvents)
        XCTAssertEqual(app.backgroundApprovals.count, 1)
        XCTAssertTrue(app.extensions[0].needsHookTrust)
        XCTAssertTrue(app.eventCentreItems.contains { $0.requiresAction })
        app.restoreSessionReadState(for: UUID())
        XCTAssertTrue(app.hasUnreadEvents)
        app.restoreSessionReadState(for: accountID)
        XCTAssertFalse(app.hasUnreadEvents)

        let extensionEvent = try XCTUnwrap(
            app.eventCentreItems.first { $0.id.hasPrefix("extension:") })
        app.openEvent(extensionEvent)
        XCTAssertEqual(app.destination, .extensions)
        XCTAssertEqual(app.navigationPath, [.settings(.extensionPackage(app.extensions[0].id))])
        app.extensions = [extensionRecord(hooksTrusted: true)]
        XCTAssertFalse(app.eventCentreItems.contains { $0.id.hasPrefix("extension:") })
    }

    func testBackgroundActivityRefreshesRoutineResultsAndOpensTheExistingRunPreview() async throws {
        let recorder = GatewayRequestRecorder()
        let app = try model { await recorder.record($0) }
        app.gateway.connectionState = .ready
        app.gateway.handle(.backgroundApprovals([]))
        let request = await recorder.firstRequest(after: 0) {
            if case .listRoutineHistory = $0 { return true }
            return false
        }
        XCTAssertNotNil(request)
        let run = RoutineRun(
            id: "run-1", routineId: "routine-1", botId: "bot-1",
            startedAt: 1, finishedAt: 2, status: .failed,
            sessionId: "work-1", message: "Command failed")
        app.gateway.handle(.routineHistory(requestID: "history", runs: [run]))
        let event = try XCTUnwrap(app.eventCentreItems.first)
        XCTAssertEqual(event.target, .routineRun(run.id))
        app.openEvent(event)
        XCTAssertEqual(app.presentedRoutineRun?.id, run.id)
        app.closeRoutineRunPreview()
    }

    func testEventCentreRefreshIsSharedAndStopsInBackground() async throws {
        let recorder = GatewayRequestRecorder()
        let app = try model { await recorder.record($0) }
        app.gateway.connectionState = .ready
        app.appIsInBackground = false
        app.startEventCentreRefresh()
        app.startEventCentreRefresh()
        let refresh = try XCTUnwrap(app.eventCentreRefreshTask)
        let request = await recorder.firstRequest(after: 0) {
            if case .listRoutineHistory = $0 { return true }
            return false
        }
        XCTAssertNotNil(request)
        app.appDidEnterBackground()
        await refresh.value
        XCTAssertNil(app.eventCentreRefreshTask)
        let requests = await recorder.requests()
        XCTAssertEqual(requests.count, 2)

        app.routineRuns = [
            RoutineRun(
                id: "skipped", routineId: "routine-1", botId: "bot-1", startedAt: 1,
                finishedAt: 2, status: .skipped, sessionId: nil, message: "Already running"
            )
        ]
        app.openEvent(try XCTUnwrap(app.eventCentreItems.first))
        XCTAssertEqual(app.destination, .bots)
        XCTAssertEqual(app.navigationPath, [.bot("bot-1")])
        XCTAssertNil(app.presentedRoutineRun)
    }

    func testEventCentreRendersActionableRowsAndCapturesTheScreen() async throws {
        let app = try eventCentreModel()
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let previous = scene.keyWindow
        previous?.isHidden = true
        let window = UIWindow(windowScene: scene)
        window.frame = scene.effectiveGeometry.coordinateSpace.bounds
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.isHidden = false
            previous?.makeKeyAndVisible()
        }
        let host = UIHostingController(
            rootView:
                NavigationStack { EventCentreView() }
                .modifier(MobiusTheme())
                .environment(app)
                .environment(\.colorScheme, .dark)
        )
        window.rootViewController = host
        window.makeKeyAndVisible()
        let appeared = await eventually {
            host.view.subviews.contains { $0.bounds.height > 0 }
        }
        XCTAssertTrue(appeared)
        try await Task.sleep(for: .milliseconds(700))
        XCTAssertEqual(app.eventCentreItems.count, 5)
        let image = UIGraphicsImageRenderer(bounds: host.view.bounds).image { _ in
            host.view.drawHierarchy(in: host.view.bounds, afterScreenUpdates: true)
        }
        let attachment = XCTAttachment(image: image)
        attachment.name = "event-centre"
        attachment.lifetime = .keepAlways
        add(attachment)

        app.bots = [bot(), bot(id: "bot-2", handle: "reviewer", name: "Reviewer")]
        var group = session(sessionID: "group-1", state: .idle, title: "Review the release")
        group.memberBotIds = ["bot-1", "bot-2"]
        app.chat.sessions = [group]
        app.chat.selectedSessionID = group.sessionId
        app.chat.selectedMemberBotIDs = group.memberBotIds
        app.chat.pendingApproval = nil
        app.chat.activeTurnIDs = []
        app.workspace = WorkspaceInfo(id: "project", path: "/srv/project")
        app.chat.reduce(
            record: recorded(1, testMessageEvent(text: "@helper @reviewer Check this release.")))
        app.chat.reduce(
            record: recorded(
                2,
                testMessageEvent(
                    author: .peer(
                        messageID: "review", sessionID: "private-reviewer", handle: "reviewer",
                        symbol: nil),
                    text: "The release checks pass. @helper can prepare the notes.")))
        let groupHost = UIHostingController(
            rootView: NavigationStack { ChatView() }.modifier(MobiusTheme()).environment(app))
        window.rootViewController = groupHost
        func editors(in view: UIView) -> [UITextView] {
            (view as? UITextView).map { [$0] } ?? view.subviews.flatMap { editors(in: $0) }
        }
        let composerAppeared = await eventually {
            editors(in: groupHost.view).contains { $0.isEditable && $0.bounds.height > 0 }
        }
        XCTAssertTrue(composerAppeared)
        let editor = try XCTUnwrap(editors(in: groupHost.view).first { $0.isEditable })
        XCTAssertTrue(editor.becomeFirstResponder())
        editor.insertText("@helper Please prepare the release notes.")
        XCTAssertTrue(editor.text.contains("@helper"))
        editor.resignFirstResponder()
        try await Task.sleep(for: .milliseconds(500))
        let groupImage = UIGraphicsImageRenderer(bounds: groupHost.view.bounds).image { _ in
            groupHost.view.drawHierarchy(in: groupHost.view.bounds, afterScreenUpdates: true)
        }
        let groupAttachment = XCTAttachment(image: groupImage)
        groupAttachment.name = "group-chat"
        groupAttachment.lifetime = .keepAlways
        add(groupAttachment)
        let members = try XCTUnwrap(
            testAccessibilityElements(groupHost.view).first {
                $0.accessibilityLabel == "Group members"
            })
        XCTAssertTrue(members.accessibilityActivate())
        let popupAppeared = await eventually {
            testAccessibilityElements(window).contains {
                $0.accessibilityLabel == "Helper (@helper)\nReviewer (@reviewer)"
            }
        }
        XCTAssertTrue(popupAppeared)
        let popupImage = UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
            window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
        }
        let popupAttachment = XCTAttachment(image: popupImage)
        popupAttachment.name = "group-members-popup"
        popupAttachment.lifetime = .keepAlways
        add(popupAttachment)
    }

    private func eventCentreModel() throws -> AppModel {
        let app = try model { _ in }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        app.gateway.accounts = [account]
        app.gateway.selectedAccountID = account.id
        app.gateway.connectionState = .ready
        app.chat.sessions = [
            session(
                state: .awaitingApproval, approvalRequestID: "request-1",
                title: "Review the release")
        ]
        app.chat.selectedSessionID = "chat-1"
        app.chat.pendingApproval = PendingApproval(id: "request-1", reason: "Run checks", calls: [])
        app.backgroundApprovals = [
            BackgroundApproval(
                sessionId: "work-1", botId: "bot-1",
                turnId: "turn-2", requestId: "request-2")
        ]
        app.extensions = [extensionRecord(hooksTrusted: false)]
        app.routineRuns = [
            RoutineRun(
                id: "morning-run", routineId: "morning", botId: "bot-1",
                startedAt: 200, finishedAt: 210, status: .succeeded,
                sessionId: "work-2", message: "Morning summary is ready"),
            RoutineRun(
                id: "sync-run", routineId: "sync", botId: "bot-1",
                startedAt: 100, finishedAt: 110, status: .failed,
                sessionId: "work-3", message: "Repository sync needs a retry"),
        ]
        return app
    }
}
