import Foundation
import SwiftUI
import UIKit
import XCTest

@MainActor
private final class PushTokenRaceHarness {
    private var pendingRegistration: CheckedContinuation<Void, Never>?
    private(set) var hasInstallation = false
    private(set) var methods: [String] = []

    var registrationIsWaiting: Bool { pendingRegistration != nil }

    func send(_ request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        let method = request.httpMethod ?? ""
        methods.append(method)
        switch method {
        case "POST":
            return try response(
                for: request,
                status: 200,
                json: #"{"token":"ttttttttttttttttttttttttttttttttttttttttttt","userId":"00000000-0000-0000-0000-000000000001","expiresAt":"2099-01-01T00:00:00Z"}"#
            )
        case "PUT":
            await withCheckedContinuation { pendingRegistration = $0 }
            hasInstallation = true
            return try response(for: request, status: 204)
        case "DELETE":
            hasInstallation = false
            return try response(for: request, status: 204)
        default:
            throw URLError(.badServerResponse)
        }
    }

    func completeRegistration() {
        let continuation = pendingRegistration
        pendingRegistration = nil
        continuation?.resume()
    }

    private func response(
        for request: URLRequest,
        status: Int,
        json: String = ""
    ) throws -> (Data, HTTPURLResponse) {
        let url = try XCTUnwrap(request.url)
        let response = try XCTUnwrap(HTTPURLResponse(
            url: url,
            statusCode: status,
            httpVersion: "HTTP/1.1",
            headerFields: ["Content-Type": "application/json"]
        ))
        return (Data(json.utf8), response)
    }
}

@MainActor
extension AppModelTests {
    func testAppLockAuthenticatesBeforePersistingAndKeepsWorkspaceDraftInMemory() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        var method = AppLockAuthenticationMethod.unavailable
        var results = [false, true, true, true]
        let authenticator = AppLockAuthenticator(
            method: { method },
            authenticate: { _ in results.removeFirst() }
        )
        let app = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            appLockAuthenticator: authenticator
        )
        await app.appDidBecomeActive()

        await app.setAppLockEnabled(true)
        XCTAssertFalse(app.appLockEnabled)
        XCTAssertFalse(defaults.bool(forKey: "app-lock-enabled"))

        method = .faceID
        await app.setAppLockEnabled(true)
        XCTAssertFalse(app.appLockEnabled)
        XCTAssertEqual(app.appLockAuthenticationMethod.settingTitle, "Require Face ID")

        await app.setAppLockEnabled(true)
        XCTAssertTrue(app.appLockEnabled)
        XCTAssertFalse(app.isAppLocked)
        XCTAssertTrue(defaults.bool(forKey: "app-lock-enabled"))

        let relaunched = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            appLockAuthenticator: authenticator
        )
        XCTAssertTrue(relaunched.isAppLocked)
        await relaunched.appDidBecomeActive()
        XCTAssertFalse(relaunched.isAppLocked)
        let draftID = UUID()
        relaunched.textFilePreview = TextFilePreview(
            id: draftID,
            name: "New File",
            contents: "",
            workspaceSessionID: "chat-1",
            workspacePath: ""
        )
        relaunched.updateWorkspaceFileDraft(id: draftID, path: ".env")
        relaunched.updateWorkspaceFileDraft(id: draftID, contents: "TOKEN=unsaved\n")
        relaunched.appDidEnterBackground()
        XCTAssertTrue(relaunched.isAppLocked)
        XCTAssertEqual(relaunched.textFilePreview?.workspacePath, ".env")
        XCTAssertEqual(relaunched.textFilePreview?.contents, "TOKEN=unsaved\n")
        await relaunched.appDidBecomeActive()
        XCTAssertFalse(relaunched.isAppLocked)
        XCTAssertEqual(relaunched.textFilePreview?.contents, "TOKEN=unsaved\n")

        let freshLaunch = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            appLockAuthenticator: authenticator
        )
        XCTAssertNil(freshLaunch.textFilePreview)
        XCTAssertTrue(results.isEmpty)
    }

    func testClearCachedDataKeepsGatewayDraftAndSettings() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: root)
        }
        defaults.set(ThemePreference.light.rawValue, forKey: "theme")
        defaults.set(AppLanguage.french.rawValue, forKey: "language")
        let store = GatewayStore(
            defaults: defaults,
            catalogDirectory: root.appendingPathComponent("Catalogs", isDirectory: true),
            transcriptDirectory: root.appendingPathComponent("Transcripts", isDirectory: true),
            thumbnailDirectory: root.appendingPathComponent("Thumbnails", isDirectory: true),
            draftDirectory: root.appendingPathComponent("Drafts", isDirectory: true)
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        try store.save(account, token: "test-token")
        addTeardownBlock { try await store.remove(account) }
        await store.saveChatCatalog(
            CachedChatCatalog(
                bots: [bot()],
                sessions: [session(state: .idle)],
                swarms: [],
                lastSessionID: "chat-1"
            ),
            accountID: account.id
        )
        await store.saveTranscript(
            accountID: account.id,
            sessionID: "chat-1",
            sequence: 1,
            transcript: [
                TranscriptEntry(
                    id: "cached-answer",
                    text: "Cached",
                    kind: .assistant,
                    format: "plain_text",
                    pending: false
                )
            ],
            currentUsage: TokenUsage(),
            lastUsage: TokenUsage()
        )
        let png = try tinyPNGData()
        await store.saveThumbnail(
            png,
            accountID: account.id,
            sessionID: "chat-1",
            fileID: "file-1"
        )
        await store.saveComposerDraft(
            ComposerDraft(text: "Keep this draft"),
            accountID: account.id,
            sessionID: "chat-1"
        )
        let model = AppModel(
            client: GatewayClient(),
            store: store,
            settingsDefaults: defaults
        )
        let image = await AppModel.downsampledFileThumbnail(from: png)
        model.cacheFileThumbnail(
            try XCTUnwrap(image),
            for: .session(sessionID: "chat-1", fileID: "file-1")
        )

        await model.clearCachedData()

        let catalog = await store.loadChatCatalog(accountID: account.id)
        let transcript = await store.loadTranscript(accountID: account.id, sessionID: "chat-1")
        let thumbnail = await store.loadThumbnail(
            accountID: account.id,
            sessionID: "chat-1",
            fileID: "file-1"
        )
        let draft = await store.loadComposerDraft(
            accountID: account.id,
            sessionID: "chat-1"
        )
        XCTAssertNil(catalog)
        XCTAssertNil(transcript)
        XCTAssertNil(thumbnail)
        XCTAssertEqual(draft, ComposerDraft(text: "Keep this draft"))
        XCTAssertEqual(try store.token(for: account), "test-token")
        XCTAssertEqual(store.loadAccounts().map(\.id), [account.id])
        XCTAssertEqual(defaults.string(forKey: "theme"), ThemePreference.light.rawValue)
        XCTAssertEqual(defaults.string(forKey: "language"), AppLanguage.french.rawValue)
        XCTAssertTrue(model.fileThumbnails.isEmpty)
    }

    func testClearCachedDataReportsDeletionFailure() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        let blocker = root.appendingPathComponent("blocker")
        let transcriptDirectory = root.appendingPathComponent("Transcripts", isDirectory: true)
        let marker = transcriptDirectory.appendingPathComponent("cached.json")
        try FileManager.default.createDirectory(
            at: transcriptDirectory,
            withIntermediateDirectories: true
        )
        try Data([1]).write(to: blocker)
        try Data([1]).write(to: marker)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: root)
        }
        let model = AppModel(
            client: GatewayClient(),
            store: GatewayStore(
                defaults: defaults,
                catalogDirectory: blocker.appendingPathComponent("Catalogs", isDirectory: true),
                transcriptDirectory: transcriptDirectory,
                thumbnailDirectory: root.appendingPathComponent("Thumbnails", isDirectory: true),
                draftDirectory: root.appendingPathComponent("Drafts", isDirectory: true)
            ),
            settingsDefaults: defaults
        )

        await model.clearCachedData()

        XCTAssertEqual(model.toast?.tone, .error)
        XCTAssertTrue(FileManager.default.fileExists(atPath: marker.path))
    }

    func testAppearanceUsesTheInjectedDefaults() throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        defaults.set(ThemePreference.lightsOut.rawValue, forKey: "theme")
        defaults.set(AppLanguage.french.rawValue, forKey: "language")
        defaults.set(AccentTint.purple.rawValue, forKey: "accent-tint")
        let model = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults
        )

        XCTAssertEqual(model.theme, .lightsOut)
        XCTAssertEqual(model.language, .french)
        XCTAssertEqual(model.accentTint, .purple)
        model.setTheme(.light)
        model.setLanguage(.english)
        model.setAccentTint(.orange)
        XCTAssertEqual(defaults.string(forKey: "theme"), ThemePreference.light.rawValue)
        XCTAssertEqual(defaults.string(forKey: "language"), AppLanguage.english.rawValue)
        XCTAssertEqual(defaults.string(forKey: "accent-tint"), AccentTint.orange.rawValue)
    }

    func testNotificationPreferenceUsesSystemAuthorizationAndPersistsInstallation() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        var authorization = RemoteNotificationAuthorization.denied
        var authorizationRequests = 0
        var registrations = 0
        var unregistrations = 0
        var removals = 0
        var settingsOpens = 0
        let system = RemoteNotificationSystem(
            authorization: { authorization },
            requestAuthorization: {
                authorizationRequests += 1
                return false
            },
            register: { registrations += 1 },
            unregister: { unregistrations += 1 },
            removeAll: { removals += 1 },
            openSettings: { settingsOpens += 1 }
        )
        let model = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            remoteNotifications: system
        )
        model.cloudSession = MobiusCloudSession(userID: UUID(), expiresAt: .distantFuture)

        await model.setNotificationsEnabled(true)

        XCTAssertTrue(model.notificationsEnabled)
        XCTAssertTrue(defaults.bool(forKey: notificationsEnabledKey))
        XCTAssertEqual(settingsOpens, 1)
        XCTAssertEqual(authorizationRequests, 0)
        XCTAssertEqual(registrations, 0)

        model.cloudSession = nil
        let staleNotification = RemoteNotification.session(
            eventID: "stale-event",
            kind: .completed,
            sessionID: "chat-1",
            runCount: 1,
            approvalRequestID: nil
        )
        model.receivedForegroundRemoteNotification(staleNotification)
        model.openRemoteNotification(staleNotification)
        XCTAssertTrue(
            model.pendingRemoteNotification == nil
                && model.remoteNotificationEventIDs.isEmpty
        )

        model.pendingRemoteNotification = RemoteNotification.session(
            eventID: "event-1",
            kind: .completed,
            sessionID: "chat-1",
            runCount: 1,
            approvalRequestID: nil
        )
        model.remoteNotificationEventIDs = ["event-1"]
        await model.setNotificationsEnabled(false)
        XCTAssertFalse(model.notificationsEnabled)
        XCTAssertFalse(defaults.bool(forKey: notificationsEnabledKey))
        XCTAssertEqual(unregistrations, 1)
        XCTAssertEqual(removals, 1)
        XCTAssertNil(model.pendingRemoteNotification)
        XCTAssertTrue(model.remoteNotificationEventIDs.isEmpty)

        model.cloudSession = MobiusCloudSession(userID: UUID(), expiresAt: .distantFuture)
        authorization = .notDetermined
        await model.setNotificationsEnabled(true)
        XCTAssertEqual(authorizationRequests, 1)
        XCTAssertEqual(settingsOpens, 2)
        XCTAssertEqual(model.notificationError, "Notifications are off in Settings.")

        authorization = .authorized
        await model.cloudAuthenticationDidChange()
        XCTAssertEqual(registrations, 1)
        XCTAssertNil(model.notificationError)

        model.remoteNotificationDeviceToken = "0011"
        do {
            try await model.unregisterRemoteNotificationsForCloudSignOut()
            XCTFail("Expected push-token removal without a stored credential to fail")
        } catch {}
        XCTAssertEqual(unregistrations, 1)
        XCTAssertEqual(removals, 1)
        XCTAssertEqual(model.remoteNotificationDeviceToken, "0011")
        XCTAssertTrue(model.pushTokenRemovalPending)

        let relaunched = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            remoteNotifications: system
        )
        XCTAssertTrue(relaunched.notificationsEnabled)
        XCTAssertEqual(relaunched.pushInstallationID, model.pushInstallationID)
    }

    func testNotificationOptOutRemovesRegistrationThatFinishesAfterInitialDelete() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let cloudStore = MobiusCloudSessionStore(service: suiteName)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? cloudStore.remove()
        }
        defaults.set(true, forKey: notificationsEnabledKey)
        let harness = PushTokenRaceHarness()
        let cloudClient = MobiusCloudClient(store: cloudStore, transport: harness.send)
        let cloudSession = try await cloudClient.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )
        let model = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: cloudClient
        )
        model.cloudSession = cloudSession

        model.receivedRemoteNotificationDeviceToken(Data([0x00, 0x11]))
        let registrationStarted = await eventually { harness.registrationIsWaiting }
        XCTAssertTrue(registrationStarted)

        await model.setNotificationsEnabled(false)
        XCTAssertEqual(harness.methods, ["POST", "PUT", "DELETE"])
        XCTAssertFalse(harness.hasInstallation)

        harness.completeRegistration()
        let staleRegistrationRemoved = await eventually {
            harness.methods == ["POST", "PUT", "DELETE", "DELETE"]
                && !harness.hasInstallation
        }
        XCTAssertTrue(staleRegistrationRemoved)
        XCTAssertFalse(model.notificationsEnabled)
        XCTAssertFalse(model.pushTokenRemovalPending)
        XCTAssertNil(model.notificationError)
    }

    func testCloudSignOutWaitsForRegistrationBeforeRemovingInstallation() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let cloudStore = MobiusCloudSessionStore(service: suiteName)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? cloudStore.remove()
        }
        defaults.set(true, forKey: notificationsEnabledKey)
        let harness = PushTokenRaceHarness()
        let cloudClient = MobiusCloudClient(store: cloudStore, transport: harness.send)
        let cloudSession = try await cloudClient.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )
        let model = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: cloudClient
        )
        model.cloudSession = cloudSession
        model.receivedRemoteNotificationDeviceToken(Data([0x00, 0x11]))
        let registrationStarted = await eventually { harness.registrationIsWaiting }
        XCTAssertTrue(registrationStarted)

        let signOut = Task { await model.signOutOfCloud() }
        let removalPending = await eventually { model.pushTokenRemovalPending }
        XCTAssertTrue(removalPending)
        XCTAssertEqual(harness.methods, ["POST", "PUT"])

        harness.completeRegistration()
        await signOut.value
        XCTAssertEqual(harness.methods, ["POST", "PUT", "DELETE"])
        XCTAssertFalse(harness.hasInstallation)
        XCTAssertNil(model.cloudSession)
        XCTAssertFalse(model.pushTokenRemovalPending)
    }

    func testCanonicalGatewayCompletionRefinesAnEarlierRichRemotePreview() throws {
        let model = try model()
        model.cloudSession = MobiusCloudSession(userID: UUID(), expiresAt: .distantFuture)
        model.notificationsEnabled = true
        model.bots = []
        model.sessions = [session(
            state: .running,
            turnID: "turn-1",
            executionStats: ExecutionStats(runCount: 0),
            title: "Deploy"
        )]
        model.receivedForegroundRemoteNotification(RemoteNotification.session(
            eventID: "event-1",
            kind: .completed,
            sessionID: "chat-1",
            runCount: 1,
            approvalRequestID: nil
        ), agentName: "Luna", detail: "You are right.\nThis was a mistake.")
        let remoteToastID = try XCTUnwrap(model.toast?.id)
        XCTAssertEqual(model.toast?.message, "Luna: You are right. This was a mistake.")

        model.bots = [bot()]
        model.applySessions([session(
            state: .idle,
            outcome: .completed,
            message: "The corrected canonical answer.",
            executionStats: ExecutionStats(runCount: 1),
            sequence: 2,
            title: "Deploy"
        )])

        XCTAssertNotEqual(model.toast?.id, remoteToastID)
        XCTAssertEqual(model.toast?.message, "Helper: The corrected canonical answer.")
    }

    func testSecondRichRemoteCompletionForTheSameRunIsDeduplicated() throws {
        let model = try model()
        model.cloudSession = MobiusCloudSession(userID: UUID(), expiresAt: .distantFuture)
        model.notificationsEnabled = true
        model.bots = []
        model.sessions = [session(
            state: .running,
            turnID: "turn-1",
            executionStats: ExecutionStats(runCount: 0)
        )]
        model.receivedForegroundRemoteNotification(RemoteNotification.session(
            eventID: "event-1",
            kind: .completed,
            sessionID: "chat-1",
            runCount: 1,
            approvalRequestID: nil
        ), agentName: "Luna", detail: "First answer.")
        let firstToastID = try XCTUnwrap(model.toast?.id)

        model.receivedForegroundRemoteNotification(RemoteNotification.session(
            eventID: "event-2",
            kind: .completed,
            sessionID: "chat-1",
            runCount: 1,
            approvalRequestID: nil
        ), agentName: "Luna", detail: "Second answer.")

        XCTAssertEqual(model.toast?.id, firstToastID)
        XCTAssertEqual(model.toast?.message, "Luna: First answer.")
    }

    func testSessionToastAccessibilityIncludesBotWithoutDuplicatingItsName() throws {
        let model = try model()
        model.bots = [bot()]
        model.sessions = [session(state: .idle)]

        XCTAssertEqual(
            model.accessibilityMessage(for: AppToast(
                message: "Deploy needs approval.",
                tone: .warning,
                target: .session("chat-1")
            )),
            "Helper: Deploy needs approval."
        )
        XCTAssertEqual(
            model.accessibilityMessage(for: AppToast(
                message: "Helper: Deployment succeeded.",
                tone: .success,
                target: .session("chat-1")
            )),
            "Helper: Deployment succeeded."
        )
    }

    func testGatewayPreviewRefinesGenericRemoteCompletionWithoutAnEarlierBotCatalog() throws {
        let model = try model()
        model.cloudSession = MobiusCloudSession(userID: UUID(), expiresAt: .distantFuture)
        model.notificationsEnabled = true
        model.bots = []
        model.sessions = [session(
            state: .running,
            turnID: "turn-1",
            executionStats: ExecutionStats(runCount: 0)
        )]
        model.receivedForegroundRemoteNotification(RemoteNotification.session(
            eventID: "event-1",
            kind: .completed,
            sessionID: "chat-1",
            runCount: 1,
            approvalRequestID: nil
        ))
        let remoteToastID = try XCTUnwrap(model.toast?.id)
        XCTAssertEqual(model.toast?.message, "Bot: Finished.")

        model.bots = [bot()]
        model.applySessions([session(
            state: .idle,
            outcome: .completed,
            message: "  Deployment succeeded.\nAll checks passed. ",
            executionStats: ExecutionStats(runCount: 1),
            sequence: 2
        )])

        XCTAssertNotEqual(model.toast?.id, remoteToastID)
        XCTAssertEqual(model.toast?.message, "Helper: Deployment succeeded. All checks passed.")
    }

    func testForegroundRemoteDoesNotReplaceAnExistingGatewayCompletionPreview() throws {
        let model = try model()
        model.cloudSession = MobiusCloudSession(userID: UUID(), expiresAt: .distantFuture)
        model.notificationsEnabled = true
        model.sessions = [session(
            state: .running,
            turnID: "turn-1",
            executionStats: ExecutionStats(runCount: 0)
        )]
        model.applySessions([session(
            state: .idle,
            outcome: .completed,
            message: "Deployment succeeded.",
            executionStats: ExecutionStats(runCount: 1),
            sequence: 2
        )])
        let gatewayToastID = try XCTUnwrap(model.toast?.id)

        model.receivedForegroundRemoteNotification(RemoteNotification.session(
            eventID: "event-1",
            kind: .completed,
            sessionID: "chat-1",
            runCount: 1,
            approvalRequestID: nil
        ))

        XCTAssertEqual(model.toast?.id, gatewayToastID)
        XCTAssertEqual(model.toast?.message, "Helper: Deployment succeeded.")
    }

    func testGatewayThenForegroundApprovalShowsOneSharedToast() throws {
        let model = try model()
        model.cloudSession = MobiusCloudSession(userID: UUID(), expiresAt: .distantFuture)
        model.notificationsEnabled = true
        model.sessions = [session(state: .running, turnID: "turn-1", title: "Deploy")]
        model.applySessions([session(
            state: .awaitingApproval,
            turnID: "turn-1",
            approvalRequestID: "approval-1",
            sequence: 2,
            title: "Deploy"
        )])
        let gatewayToastID = try XCTUnwrap(model.toast?.id)
        XCTAssertEqual(model.toast?.message, "Deploy needs approval.")

        model.receivedForegroundRemoteNotification(RemoteNotification.session(
            eventID: "event-approval",
            kind: .awaitingApproval,
            sessionID: "chat-1",
            runCount: nil,
            approvalRequestID: "approval-1"
        ))

        XCTAssertEqual(model.toast?.id, gatewayToastID)
        XCTAssertEqual(model.toast?.message, "Deploy needs approval.")
    }

    func testForegroundHiddenApprovalUsesRemoteBotNameBeforeCatalogArrives() throws {
        let model = try model()
        model.cloudSession = MobiusCloudSession(userID: UUID(), expiresAt: .distantFuture)
        model.notificationsEnabled = true

        model.receivedForegroundRemoteNotification(RemoteNotification.session(
            eventID: "event-approval",
            kind: .awaitingApproval,
            sessionID: "work-1",
            runCount: nil,
            approvalRequestID: "approval-1"
        ), agentName: "Helper")

        XCTAssertEqual(model.toast?.message, "Helper needs approval.")
    }

    func testNotificationTapWaitsForCloudCatalogThenOpensChat() throws {
        let model = try model()
        let userID = UUID()
        let cloudGateway = GatewayAccount(
            endpoint: try GatewayEndpoint("tcp://localhost:9191"),
            cloudUserID: userID
        )
        model.cloudSession = MobiusCloudSession(userID: userID, expiresAt: .distantFuture)
        model.notificationsEnabled = true
        model.accounts = [cloudGateway]
        model.selectedAccountID = cloudGateway.id
        model.connectionState = .loading
        let notification = RemoteNotification.session(
            eventID: "event-tap",
            kind: .completed,
            sessionID: "chat-1",
            runCount: 1,
            approvalRequestID: nil
        )

        model.openRemoteNotification(notification)
        XCTAssertEqual(model.pendingRemoteNotification, notification)

        model.sessions = [session(state: .idle)]
        model.connectionState = .ready
        XCTAssertTrue(model.openPendingRemoteNotification())
        XCTAssertNil(model.pendingRemoteNotification)
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-1"))])
    }

    func testSwarmAttentionNotificationTapWaitsForCatalogThenOpensSwarmChat() throws {
        let model = try model()
        let userID = UUID()
        let cloudGateway = GatewayAccount(
            endpoint: try GatewayEndpoint("tcp://localhost:9191"),
            cloudUserID: userID
        )
        let helper = bot()
        let swarm = SwarmRecord(
            id: "swarm-1",
            title: "Quiet Foxes",
            leaderBotId: helper.id,
            members: [SwarmMemberRecord(botId: helper.id, handle: helper.handle)],
            messages: [],
            updatedAtMs: 1
        )
        let notification = RemoteNotification.swarmAttention(
            eventID: "event-tap",
            swarmID: swarm.id,
            messageID: "message-1"
        )
        model.cloudSession = MobiusCloudSession(userID: userID, expiresAt: .distantFuture)
        model.notificationsEnabled = true
        model.accounts = [cloudGateway]
        model.selectedAccountID = cloudGateway.id
        model.connectionState = .loading

        model.openRemoteNotification(notification)
        XCTAssertEqual(model.pendingRemoteNotification, notification)

        model.bots = [helper]
        model.swarms = [swarm]
        model.connectionState = .ready
        XCTAssertTrue(model.openPendingRemoteNotification())
        XCTAssertNil(model.pendingRemoteNotification)
        XCTAssertEqual(
            model.navigationPath,
            [.swarm(swarm.id), .swarmChat(swarm.id)]
        )
    }

    func testForegroundAndLiveSwarmAttentionDeduplicateByMessageID() throws {
        let model = try model()
        let helper = bot()
        let swarm = SwarmRecord(
            id: "swarm-1",
            title: "Quiet Foxes",
            leaderBotId: helper.id,
            members: [SwarmMemberRecord(botId: helper.id, handle: helper.handle)],
            messages: [],
            updatedAtMs: 1
        )
        let attention = SwarmAttention(
            swarmId: swarm.id,
            swarmTitle: swarm.title,
            messageId: "message-1",
            botId: helper.id,
            text: "Choose a migration path."
        )
        model.cloudSession = MobiusCloudSession(userID: UUID(), expiresAt: .distantFuture)
        model.notificationsEnabled = true
        model.bots = [helper]
        model.swarms = [swarm]

        model.receivedForegroundRemoteNotification(
            .swarmAttention(
                eventID: "event-1",
                swarmID: swarm.id,
                messageID: attention.messageId
            ),
            agentName: helper.name,
            detail: attention.text
        )
        let remoteToastID = try XCTUnwrap(model.toast?.id)

        model.handle(.swarmAttentions([attention]))

        XCTAssertEqual(model.toast?.id, remoteToastID)
        XCTAssertEqual(model.toast?.message, "Helper: Choose a migration path.")
        XCTAssertEqual(model.bot(for: model.toast?.target)?.id, helper.id)
        XCTAssertTrue(model.hasSwarmAttention(forSwarmID: swarm.id))
    }

    func testBackgroundApprovalNotificationTapResumesValidatedHiddenSession() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { request in await recorder.record(request) }
        let userID = UUID()
        let cloudGateway = GatewayAccount(
            endpoint: try GatewayEndpoint("tcp://localhost:9191"),
            cloudUserID: userID
        )
        let approval = BackgroundApproval(
            sessionId: "work-1",
            botId: "bot-1",
            turnId: "turn-1",
            requestId: "approval-1"
        )
        model.cloudSession = MobiusCloudSession(userID: userID, expiresAt: .distantFuture)
        model.notificationsEnabled = true
        model.accounts = [cloudGateway]
        model.selectedAccountID = cloudGateway.id
        model.connectionState = .ready
        XCTAssertTrue(model.applyBackgroundApprovals([approval], notifyingNew: false))

        model.openRemoteNotification(RemoteNotification.session(
            eventID: "event-approval",
            kind: .awaitingApproval,
            sessionID: approval.sessionId,
            runCount: nil,
            approvalRequestID: approval.requestId
        ))

        let listing = await recorder.firstRequest(after: 0) { request in
            if case .listBotSessions = request { return true }
            return false
        }
        guard case .listBotSessions(let requestID, let botID) = try XCTUnwrap(listing) else {
            return XCTFail("Expected hidden Bot session discovery")
        }
        XCTAssertEqual(botID, approval.botId)
        XCTAssertNil(model.pendingRemoteNotification)

        let hidden = session(
            sessionID: approval.sessionId,
            state: .awaitingApproval,
            turnID: approval.turnId,
            approvalRequestID: approval.requestId,
            originLabel: "routine",
            botID: approval.botId
        )
        model.handle(.botSessions(
            requestID: requestID,
            botID: approval.botId,
            sessions: [hidden]
        ))
        let opening = await recorder.firstRequest(after: 1) { request in
            guard case .openSession(_, approval.sessionId, _) = request else { return false }
            return true
        }
        XCTAssertNotNil(opening)
        XCTAssertEqual(model.navigationPath, [.chat(.session(approval.sessionId))])
    }

    func testRemoteNotificationPayloadRequiresAStableCursorForItsKind() throws {
        XCTAssertEqual(
            RemoteNotification(userInfo: [
                "eventId": "event-1",
                "kind": "session.completed",
                "sessionId": "chat-1",
                "runCount": 7,
            ]),
            RemoteNotification.session(
                eventID: "event-1",
                kind: .completed,
                sessionID: "chat-1",
                runCount: 7,
                approvalRequestID: nil
            )
        )
        XCTAssertNil(RemoteNotification(userInfo: [
            "eventId": "event-2",
            "kind": "session.completed",
            "sessionId": "chat-1",
        ]))
        XCTAssertNil(RemoteNotification(userInfo: [
            "eventId": "event-3",
            "kind": "session.awaiting_approval",
            "sessionId": "chat-1",
        ]))
        XCTAssertEqual(
            RemoteNotification(userInfo: [
                "eventId": "event-4",
                "kind": "session.awaiting_approval",
                "sessionId": "work-1",
                "approvalRequestId": "approval-1",
            ]),
            RemoteNotification.session(
                eventID: "event-4",
                kind: .awaitingApproval,
                sessionID: "work-1",
                runCount: nil,
                approvalRequestID: "approval-1"
            )
        )
        XCTAssertEqual(
            RemoteNotification(userInfo: [
                "eventId": "event-5",
                "kind": "swarm.attention",
                "swarmId": "swarm-1",
                "messageId": "message-1",
            ]),
            .swarmAttention(
                eventID: "event-5",
                swarmID: "swarm-1",
                messageID: "message-1"
            )
        )
        XCTAssertNil(RemoteNotification(userInfo: [
            "eventId": "event-6",
            "kind": "swarm.attention",
            "swarmId": "swarm-1",
        ]))
    }

    func testLanguageLocalesPreserveTheSystemChoice() {
        XCTAssertEqual(AppLanguage.system.locale, .autoupdatingCurrent)
        XCTAssertEqual(AppLanguage.english.locale.identifier, "en")
        XCTAssertEqual(AppLanguage.french.locale.identifier, "fr")
        XCTAssertEqual(AppLanguage.german.locale.identifier, "de")
        XCTAssertEqual(AppLanguage.allCases, [.system, .english, .french, .german])
    }

    func testLanguageDefaultsToSystem() throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }

        var model = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults
        )
        XCTAssertEqual(model.language, .system)

        defaults.set("unsupported", forKey: "language")
        model = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults
        )
        XCTAssertEqual(model.language, .system)
    }

    func testLightsOutUsesBlackCanvasWithDarkPalette() {
        let dark = MobiusPalette(.dark)
        let lightsOut = MobiusPalette(.light, lightsOut: true)

        XCTAssertEqual(lightsOut.canvas, .black)
        XCTAssertEqual(lightsOut.recessed, .black)
        XCTAssertEqual(
            [
                lightsOut.panel, lightsOut.raised, lightsOut.line,
                lightsOut.accent, lightsOut.accentFill, lightsOut.accentSoft,
                lightsOut.signal, lightsOut.warning, lightsOut.danger,
                lightsOut.muted, lightsOut.onAccent, lightsOut.onDanger,
                lightsOut.onMedia, lightsOut.shadow, lightsOut.sidebarScrim,
            ],
            [
                dark.panel, dark.raised, dark.line,
                dark.accent, dark.accentFill, dark.accentSoft,
                dark.signal, dark.warning, dark.danger,
                dark.muted, dark.onAccent, dark.onDanger,
                dark.onMedia, dark.shadow, dark.sidebarScrim,
            ]
        )
    }

    func testAccentTintColorsThePalette() {
        let tint = AccentTint.purple
        let dark = MobiusPalette(.dark, accentTint: tint)
        let light = MobiusPalette(.light, accentTint: tint)
        let defaultDark = MobiusPalette(.dark)
        let defaultLight = MobiusPalette(.light)

        XCTAssertEqual(
            dark.accent,
            tint.color.mix(with: .white, by: 0.55, in: .device)
        )
        XCTAssertEqual(
            dark.accentFill,
            tint.color.mix(with: .black, by: 0.5, in: .device)
        )
        XCTAssertEqual(
            dark.accentSoft,
            dark.panel.mix(with: tint.color, by: 0.10, in: .device)
        )
        XCTAssertEqual(
            light.accent,
            tint.color.mix(with: .black, by: 0.55, in: .device)
        )
        XCTAssertEqual(
            light.accentFill,
            tint.color.mix(with: .black, by: 0.6, in: .device)
        )
        XCTAssertEqual(
            light.accentSoft,
            light.panel.mix(with: tint.color, by: 0.18, in: .device)
        )
        XCTAssertTrue(zip(
            [dark.canvas, dark.recessed, dark.panel, dark.raised, dark.line, dark.sidebarScrim],
            [
                defaultDark.canvas, defaultDark.recessed, defaultDark.panel,
                defaultDark.raised, defaultDark.line, defaultDark.sidebarScrim,
            ]
        ).allSatisfy { $0.0 != $0.1 })
        XCTAssertTrue(zip(
            [light.canvas, light.recessed, light.panel, light.raised, light.line, light.sidebarScrim],
            [
                defaultLight.canvas, defaultLight.recessed, defaultLight.panel,
                defaultLight.raised, defaultLight.line, defaultLight.sidebarScrim,
            ]
        ).allSatisfy { $0.0 != $0.1 })
    }

    func testAccentTintsMeetTextContrastInEveryAppearance() {
        let appearances: [(ColorScheme, Bool)] = [(.light, false), (.dark, false), (.dark, true)]
        for (scheme, lightsOut) in appearances {
            for tint in AccentTint.allCases {
                let palette = MobiusPalette(scheme, lightsOut: lightsOut, accentTint: tint)
                for surface in [
                    palette.canvas, palette.recessed, palette.panel,
                    palette.raised, palette.accentSoft,
                ] {
                    XCTAssertGreaterThanOrEqual(
                        palette.accent.contrastRatio(with: surface),
                        4.5,
                        "\(tint.label) in \(scheme) mode"
                    )
                    XCTAssertGreaterThanOrEqual(
                        palette.muted.contrastRatio(with: surface),
                        3,
                        "\(tint.label) muted text in \(scheme) mode"
                    )
                }
                XCTAssertGreaterThanOrEqual(
                    palette.onAccent.contrastRatio(with: palette.accentFill),
                    4.5,
                    "\(tint.label) fill in \(scheme) mode"
                )
            }
        }
    }

    func testGitBranchSwitchUsesAnAdvertisedBranch() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.gitStatus = GitStatus(currentBranch: "main", branches: ["feature", "main"])

        model.switchGitBranch(to: "unknown")
        let requestCount = await recorder.requestCount()
        model.switchGitBranch(to: "feature")
        let request = await recorder.firstRequest(after: requestCount) { request in
            if case .switchGitBranch = request { return true }
            return false
        }

        let requests = await recorder.requests()
        XCTAssertEqual(requests.count, 1)
        guard case .switchGitBranch(_, let sessionID, let branch) = try XCTUnwrap(request) else {
            return XCTFail("Expected a branch switch request")
        }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(branch, "feature")
    }

}

@MainActor
private extension Color {
    func contrastRatio(with other: Color) -> CGFloat {
        let brighter = max(relativeLuminance, other.relativeLuminance)
        let darker = min(relativeLuminance, other.relativeLuminance)
        return (brighter + 0.05) / (darker + 0.05)
    }

    var relativeLuminance: CGFloat {
        var red: CGFloat = 0
        var green: CGFloat = 0
        var blue: CGFloat = 0
        var alpha: CGFloat = 0
        guard UIColor(self).getRed(&red, green: &green, blue: &blue, alpha: &alpha) else {
            return 0
        }
        let channels = [red, green, blue].map { component in
            component <= 0.04045
                ? component / 12.92
                : pow((component + 0.055) / 1.055, 2.4)
        }
        return 0.2126 * channels[0] + 0.7152 * channels[1] + 0.0722 * channels[2]
    }
}
