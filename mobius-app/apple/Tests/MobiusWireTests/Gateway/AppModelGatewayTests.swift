import Foundation
@testable import Mobius
import XCTest

@MainActor
extension AppModelTests {
    func testWorkspaceRequestsWaitForAuthenticationWithoutInterruptingTheConnection() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { await recorder.record($0) })
        for state: ConnectionState in [.connecting, .authenticating] {
            model.gateway.connectionState = state
            model.loadDirectory("/Users/test")
            let finished = await eventually { !model.isLoadingDirectories }
            XCTAssertTrue(finished)
            XCTAssertNotNil(model.directoryError)
            XCTAssertEqual(model.gateway.connectionState, state)
        }
        let prematureRequests = await recorder.requestCount()
        XCTAssertEqual(prematureRequests, 0)

        model.gateway.connectionState = .loading
        model.loadDirectory("/Users/test")
        let request = await recorder.firstRequest(after: 0) {
            if case .listDirectories = $0 { return true }
            return false
        }
        XCTAssertNotNil(request)
    }

    func testPairingProgressBelongsOnlyToThePendingPairing() async throws {
        let suite = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let store = GatewayStore(defaults: defaults)
        let account = GatewayAccount(endpoint: try GatewayEndpoint("wss://existing.example"))
        try store.save(account, token: "existing-token")
        addTeardownBlock { try await store.remove(account) }
        let model = AppModel(
            store: store, settingsDefaults: defaults,
            requestSender: { _ in },
            connectionOpener: { _ in AsyncThrowingStream { _ in } }
        )
        model.connect(to: account)
        XCTAssertNil(model.gateway.pairingConnectionState)
        let authenticated = await eventually { model.gateway.connectionState == .authenticating }
        XCTAssertTrue(authenticated)
        XCTAssertNil(model.gateway.pairingConnectionState)

        model.applyPairingSetup(
            try GatewayPairingSetup(endpoint: "wss://new.example", code: "new-code"))
        XCTAssertNil(model.gateway.pairingConnectionState)
        model.pair()
        XCTAssertEqual(model.gateway.pairingConnectionState, .connecting)
        let pairing = await eventually { model.gateway.pairingConnectionState == .authenticating }
        XCTAssertTrue(pairing)
        model.gateway.cancelPendingPairing()
        XCTAssertNil(model.gateway.pairingConnectionState)
        await model.gateway.shutdown().value
    }

    func testProviderUsageRequiresSetupAndIgnoresOldProfileResponses() throws {
        let model = try model()
        model.gateway.connectionState = .ready
        model.refreshProfile()
        let oldID = try XCTUnwrap(model.profileRequestID)
        model.refreshProfile()
        let currentID = try XCTUnwrap(model.profileRequestID)
        let quota = ProviderUsage(
            provider: "openai_codex",
            limits: [
                UsageLimit(
                    id: "codex:primary", label: "Codex", remainingFraction: 0.73,
                    windowSeconds: 18_000, resetsAt: nil
                )
            ],
            error: nil
        )
        let profile = ProfileSnapshot(
            userName: nil, dailyUsage: [], providerUsage: [quota],
            runStats: RunStats(), recentRunGroups: []
        )
        model.gateway.handle(.profile(requestID: oldID, profile: profile))
        XCTAssertNil(model.profile)
        model.gateway.handle(
            .rejected(
                GatewayRejection(
                    requestId: oldID, code: "profile_superseded",
                    message: "A newer refresh replaced this request.", fatal: false
                )
            )
        )
        XCTAssertEqual(model.profileRequestID, currentID)
        XCTAssertNil(model.toast)
        model.gateway.handle(.profile(requestID: currentID, profile: profile))
        XCTAssertEqual(model.profile, profile)
        XCTAssertTrue(model.providerUsage.isEmpty)

        var selection = composition().provider
        model.providerInstances = [
            ProviderInstance(
                label: "API", tint: .appDefault, configured: true,
                selection: selection, modelIds: [], reasoningEfforts: []
            )
        ]
        XCTAssertTrue(model.providerUsage.isEmpty)
        selection.provider = "openai_codex"
        let subscription = ProviderInstance(
            label: "Subscription", tint: .appDefault, configured: false,
            selection: selection, modelIds: [], reasoningEfforts: []
        )
        model.providerInstances = [subscription]
        XCTAssertTrue(model.providerUsage.isEmpty)
        model.providerInstances[0].configured = true
        XCTAssertEqual(model.providerUsage, [quota])

        let weekly = UsageLimit(
            id: "codex:secondary", label: "Codex", remainingFraction: 0.42,
            windowSeconds: 604_800, resetsAt: nil
        )
        let spark = UsageLimit(
            id: "codex-spark:secondary", label: "Spark", remainingFraction: 0.9,
            windowSeconds: 604_800, resetsAt: nil
        )
        model.profile?.providerUsage = [
            ProviderUsage(
                provider: quota.provider, limits: [spark] + (quota.limits ?? []), error: nil)
        ]
        XCTAssertNil(model.codexWeeklyUsage)
        model.refreshProfile()
        XCTAssertTrue(model.isLoadingCodexWeeklyUsage)
        model.profile?.providerUsage = [
            ProviderUsage(provider: quota.provider, limits: [spark, weekly], error: nil)
        ]
        XCTAssertEqual(model.codexWeeklyUsage, weekly)
        XCTAssertFalse(model.isLoadingCodexWeeklyUsage)

        model.refreshProfile()
        let failedID = try XCTUnwrap(model.profileRequestID)
        let expired = ProviderUsage(
            provider: quota.provider, limits: nil,
            error: "ChatGPT session expired; sign in again"
        )
        model.gateway.handle(
            .profile(
                requestID: failedID,
                profile: ProfileSnapshot(
                    userName: nil, dailyUsage: [], providerUsage: [expired],
                    runStats: RunStats(), recentRunGroups: []
                )
            )
        )
        XCTAssertEqual(model.providerUsageError(for: quota.provider), expired.error)
        XCTAssertNil(model.providerUsageError(for: "other-provider"))
        model.refreshProfile()
        let recoveredID = try XCTUnwrap(model.profileRequestID)
        model.gateway.handle(.profile(requestID: recoveredID, profile: profile))
        XCTAssertNil(model.providerUsageError(for: quota.provider))

        model.handleGatewayDisconnected("Disconnected")
        XCTAssertNil(model.profileRequestID)
        XCTAssertFalse(model.isLoadingCodexWeeklyUsage)
        XCTAssertEqual(
            model.providerUsage, [ProviderUsage(provider: quota.provider, limits: nil, error: nil)])
        model.providerInstances = []
        XCTAssertTrue(model.providerUsage.isEmpty)
    }

    func testLateGatewayStreamCannotReplaceTheCurrentAccount() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let store = GatewayStore(defaults: defaults)
        let first = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        let second = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9192"))
        try store.save(first, token: "first-token")
        try store.save(second, token: "second-token")
        addTeardownBlock {
            try await store.remove(first)
            try await store.remove(second)
        }
        let gate = AsyncGate()
        let oldStream = AsyncThrowingStream<GatewayEnvelope, Error>.makeStream()
        let currentStream = AsyncThrowingStream<GatewayEnvelope, Error>.makeStream()
        var oldConnectionStarted = false
        var oldConnectionReturned = false
        let model = AppModel(
            store: store,
            settingsDefaults: defaults,
            requestSender: { _ in },
            connectionOpener: { endpoint in
                if endpoint == first.endpoint {
                    oldConnectionStarted = true
                    await gate.wait()
                    oldConnectionReturned = true
                    return oldStream.stream
                }
                return currentStream.stream
            }
        )
        model.selectAccount(first.id)
        let started = await eventually { oldConnectionStarted }
        XCTAssertTrue(started)
        model.selectAccount(second.id)
        let config = VersionedAgentConfig(revision: 1, config: composition())
        currentStream.continuation.yield(.ready(ready(botDefaults: config, sessions: [])))
        let connected = await eventually { model.gateway.connectionState.isReady }
        XCTAssertTrue(connected)

        oldStream.continuation.yield(.ready(ready(botDefaults: config, bots: [], sessions: [])))
        oldStream.continuation.finish(
            throwing: GatewayWireError.unsupportedVersion(gatewayProtocolVersion + 1)
        )
        await gate.open()
        let returned = await eventually { oldConnectionReturned }
        XCTAssertTrue(returned)
        await Task.yield()
        currentStream.continuation.yield(
            .ready(
                ready(
                    botDefaults: config,
                    bots: [bot(id: "current-bot")],
                    sessions: []
                )))
        let stillConnected = await eventually { model.bots.first?.id == "current-bot" }
        XCTAssertTrue(stillConnected)
        XCTAssertEqual(model.gateway.selectedAccountID, second.id)
        XCTAssertTrue(model.gateway.connectionState.isReady)
        XCTAssertFalse(model.showsAppUpdateAlert)
        await model.gateway.shutdown().value
    }

    func testSelectingExpiredCloudKeepsTheCurrentSelfHostedGateway() throws {
        let model = try model()
        let userID = UUID()
        let selfHosted = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        let cloud = GatewayAccount(
            endpoint: try GatewayEndpoint("wss://cloud.sprites.app"),
            cloudUserID: userID
        )
        model.gateway.accounts = [selfHosted, cloud]
        model.gateway.selectedAccountID = selfHosted.id
        model.gateway.connectionState = .ready
        model.cloud.cloudSession = MobiusCloudSession(userID: userID, expiresAt: .distantFuture)
        model.cloud.cloudIssue = .subscriptionExpired

        model.selectAccount(cloud.id)

        XCTAssertEqual(model.gateway.selectedAccountID, selfHosted.id)
        XCTAssertTrue(model.gateway.connectionState.isReady)
        XCTAssertEqual(model.toast?.tone, .warning)

        model.cloud.notificationsEnabled = true
        model.cloud.openRemoteNotification(
            .session(
                eventID: "cloud-completed",
                kind: .completed,
                sessionID: "cloud-chat",
                runCount: 1,
                approvalRequestID: nil
            ))

        XCTAssertEqual(model.gateway.selectedAccountID, selfHosted.id)
        XCTAssertTrue(model.gateway.connectionState.isReady)
        XCTAssertNotNil(model.cloud.pendingRemoteNotification)
    }

    func testMissingGatewayTokenOpensPairingRepair() async throws {
        let model = try model()
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.showsPairing = false

        model.selectAccount(account.id)

        let repairing = await eventually { model.showsPairing }
        XCTAssertTrue(repairing)
        XCTAssertEqual(model.gateway.pairingEndpoint, account.endpoint.rawValue)
        XCTAssertTrue(model.gateway.pairingCode.isEmpty)
        XCTAssertNotNil(model.gateway.pairingError)
        XCTAssertTrue(model.gateway.automaticReconnectBlocked)
        await model.gateway.shutdown().value
    }

    func testBackgroundPreservesPairingUntilItsCloudContinuationCompletes() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let store = GatewayStore(defaults: defaults)
        let model = AppModel(
            store: store,
            settingsDefaults: defaults,
            requestSender: { _ in },
            connectionOpener: { _ in AsyncThrowingStream { _ in } }
        )
        model.applyPairingSetup(
            try GatewayPairingSetup(
                endpoint: "tcp://localhost:9191", code: "pairing-code"
            ))
        model.pair()

        try await withCheckedThrowingContinuation {
            (continuation: CheckedContinuation<Void, Error>) in
            model.cloud.cloudPairingContinuation = continuation
            model.appDidEnterBackground()
            XCTAssertTrue(model.gateway.hasPendingPairing)
            model.gateway.handle(.paired(clientID: "cloud-client", token: "gateway-token"))
            if model.cloud.cloudPairingContinuation != nil {
                model.cloud.completeCloudPairing(.failure(CancellationError()))
            }
        }

        XCTAssertNil(model.cloud.cloudPairingContinuation)
        XCTAssertFalse(model.gateway.hasPendingPairing)
        await model.gateway.shutdown().value
        for account in model.gateway.accounts { try await store.remove(account) }
    }

    func testSwitchingGatewaysRestoresTheNewAccountsReadCursors() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let store = GatewayStore(defaults: defaults)
        let first = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        let second = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9192"))
        try store.save(first, token: "first-token")
        try store.save(second, token: "second-token")
        addTeardownBlock {
            try await store.remove(first)
            try await store.remove(second)
        }
        let firstCursors = ["chat-1": SessionReadCursor(sequence: 7, wasActive: false)]
        let secondCursors = ["chat-1": SessionReadCursor(sequence: 42, wasActive: true)]
        store.saveSessionReadCursors(firstCursors, accountID: first.id)
        store.saveSessionReadCursors(secondCursors, accountID: second.id)
        store.select(first)
        let model = AppModel(
            store: store,
            settingsDefaults: defaults,
            requestSender: { _ in },
            connectionOpener: { _ in AsyncThrowingStream { _ in } }
        )
        XCTAssertEqual(model.chat.sessionReadCursors, firstCursors)

        model.selectAccount(second.id)

        XCTAssertEqual(model.gateway.selectedAccountID, second.id)
        XCTAssertEqual(model.chat.sessionReadCursors, secondCursors)
        await model.gateway.shutdown().value
    }

    func testBackgroundShutdownCannotRetireAReplacementConnection() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let store = GatewayStore(defaults: defaults)
        let first = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        let second = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9192"))
        try store.save(first, token: "first-token")
        try store.save(second, token: "second-token")
        addTeardownBlock {
            try await store.remove(first)
            try await store.remove(second)
        }
        store.select(first)
        let payload = ready(botDefaults: VersionedAgentConfig(revision: 1, config: composition()))
        let model = AppModel(
            store: store,
            settingsDefaults: defaults,
            requestSender: { _ in },
            connectionOpener: { _ in
                AsyncThrowingStream { $0.yield(.ready(payload)) }
            }
        )
        model.gateway.connectionState = .ready
        model.appIsInBackground = false

        model.appDidEnterBackground()
        model.setSceneActive(true)
        model.selectAccount(second.id)

        let reconnected = await eventually { model.gateway.connectionState.isReady }
        XCTAssertTrue(reconnected)
        XCTAssertEqual(model.gateway.selectedAccountID, second.id)
        await model.gateway.shutdown().value
    }

    func testRemovingGatewayDrainsPendingTranscriptWritesBeforeDeletingCache() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: directory)
        }
        let store = GatewayStore(defaults: defaults, transcriptDirectory: directory)
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        await store.saveTranscript(
            accountID: account.id,
            sessionID: "chat-1",
            sequence: 1,
            transcript: [
                TranscriptEntry(
                    id: "existing",
                    text: "Existing",
                    kind: .assistant,
                    format: "plain_text",
                    pending: false
                )
            ],
            currentUsage: TokenUsage(),
            lastUsage: TokenUsage()
        )
        let gate = AsyncGate()
        let model = AppModel(client: GatewayClient(), store: store)
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.chat.selectedSessionID = "chat-1"
        model.gateway.connectionState = .ready
        model.chat.transcriptIOTask = Task {
            await gate.wait()
            await store.saveTranscript(
                accountID: account.id,
                sessionID: "chat-1",
                sequence: 2,
                transcript: [
                    TranscriptEntry(
                        id: "stale",
                        text: "Must not resurrect",
                        kind: .assistant,
                        format: "plain_text",
                        pending: false
                    )
                ],
                currentUsage: TokenUsage(),
                lastUsage: TokenUsage()
            )
        }

        let removal = Task { @MainActor in await model.removeGateway(account) }
        let quiesced = await eventually {
            model.gateway.accounts.isEmpty && model.gateway.selectedAccountID == nil
        }
        XCTAssertTrue(quiesced)
        await gate.open()
        let removed = await removal.value
        let cached = await store.loadTranscript(accountID: account.id, sessionID: "chat-1")
        XCTAssertTrue(removed)
        XCTAssertNil(cached)
    }

    func testRemovingInactiveGatewayDrainsItsPendingChatWrites() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: directory)
        }
        let store = GatewayStore(
            defaults: defaults,
            transcriptDirectory: directory
        )
        let first = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        let second = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9192"))
        try store.save(first, token: "first-token")
        try store.save(second, token: "second-token")
        store.select(first)
        let model = AppModel(client: GatewayClient(), store: store, settingsDefaults: defaults)
        model.gateway.accounts = [first, second]
        model.gateway.selectedAccountID = second.id
        model.chat.selectedSessionID = "chat-1"
        let gate = AsyncGate()
        model.chat.transcriptIOTask = Task {
            await gate.wait()
            await store.saveTranscript(
                accountID: first.id,
                sessionID: "chat-1",
                sequence: 1,
                transcript: [
                    TranscriptEntry(
                        id: "stale",
                        text: "Must not resurrect",
                        kind: .assistant,
                        format: "plain_text",
                        pending: false
                    )
                ],
                currentUsage: TokenUsage(),
                lastUsage: TokenUsage()
            )
        }

        let removal = Task { @MainActor in await model.removeGateway(first) }
        let removedFromAccountList = await eventually { model.gateway.accounts.count == 1 }
        XCTAssertTrue(removedFromAccountList)
        await gate.open()
        let removed = await removal.value
        XCTAssertTrue(removed)
        let cached = await store.loadTranscript(accountID: first.id, sessionID: "chat-1")
        XCTAssertNil(cached)
        XCTAssertEqual(model.gateway.selectedAccountID, second.id)
    }

    func testConnectionStatePresentation() {
        let expectations: [(ConnectionState, ToastTone, Bool)] = [
            (.disconnected, .error, false),
            (.connecting, .warning, true),
            (.authenticating, .warning, true),
            (.loading, .warning, true),
            (.ready, .success, false),
            (.failed("unavailable"), .error, false),
        ]

        for (state, tone, isLoading) in expectations {
            XCTAssertEqual(state.tone, tone)
            XCTAssertEqual(state.isLoading, isLoading)
        }
    }

    func testStartRestoresCachedCatalogAndLastChatBeforeGatewayReady() async throws {
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
                sessions: [session(state: .running, sequence: 7)],
                swarms: [],
                lastSessionID: "chat-1"
            ),
            accountID: account.id
        )
        await store.saveTranscript(
            accountID: account.id,
            sessionID: "chat-1",
            sequence: 7,
            transcript: [
                TranscriptEntry(
                    id: "cached-answer",
                    text: "Restored before the network",
                    kind: .assistant,
                    format: "plain_text",
                    pending: false,
                    turnID: "turn-1",
                    startsTurn: true,
                    turnTerminal: true
                )
            ],
            currentUsage: TokenUsage(),
            lastUsage: TokenUsage()
        )
        let model = AppModel(
            client: GatewayClient(),
            store: GatewayStore(
                defaults: defaults,
                catalogDirectory: root.appendingPathComponent("Catalogs", isDirectory: true),
                transcriptDirectory: root.appendingPathComponent("Transcripts", isDirectory: true),
                thumbnailDirectory: root.appendingPathComponent("Thumbnails", isDirectory: true),
                draftDirectory: root.appendingPathComponent("Drafts", isDirectory: true)
            ),
            settingsDefaults: defaults,
            requestSender: { _ in },
            connectionOpener: { _ in AsyncThrowingStream { _ in } }
        )

        await model.start()

        XCTAssertEqual(model.chat.sessions.map(\.sessionId), ["chat-1"])
        XCTAssertEqual(model.chat.sessions.first?.activity.state, .idle)
        XCTAssertEqual(model.chat.selectedSessionID, "chat-1")
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-1"))])
        XCTAssertEqual(model.chat.displayedTranscript.map(\.text), ["Restored before the network"])
        XCTAssertFalse(model.gateway.connectionState.isReady)
    }

    func testConcurrentStartsOpenOnlyOneConnection() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        let store = GatewayStore(defaults: defaults)
        try store.save(account, token: "test-token")
        addTeardownBlock { try await store.remove(account) }
        var connectionAttempts = 0
        let model = AppModel(
            client: GatewayClient(),
            store: store,
            settingsDefaults: defaults,
            requestSender: { _ in },
            connectionOpener: { _ in
                connectionAttempts += 1
                return AsyncThrowingStream { _ in }
            }
        )
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.appIsInBackground = false

        async let firstStart = model.start()
        async let secondStart = model.start()
        await firstStart
        await secondStart

        let opened = await eventually { connectionAttempts == 1 }
        XCTAssertTrue(opened)
    }

    func testCloudConnectionRetriesFailureAndSilentWarmupUntilReady() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let store = GatewayStore(defaults: defaults)
        let userID = UUID()
        let account = GatewayAccount(
            endpoint: try GatewayEndpoint("wss://mobius-test-org.sprites.app"),
            cloudUserID: userID
        )
        try store.save(account, token: "test-token")
        addTeardownBlock { try await store.remove(account) }
        let payload = ready(
            botDefaults: VersionedAgentConfig(revision: 1, config: composition())
        )
        var attempts = 0
        let model = AppModel(
            client: GatewayClient(),
            store: store,
            settingsDefaults: defaults,
            requestSender: { _ in },
            connectionOpener: { _ in
                attempts += 1
                return AsyncThrowingStream { continuation in
                    if attempts == 1 {
                        continuation.finish(throwing: GatewayWireError.disconnected)
                        return
                    }
                    guard attempts > 2 else { return }
                    continuation.yield(.authenticated)
                    continuation.yield(.ready(payload))
                }
            },
            reconnectDelay: { _ in .milliseconds(20) }
        )
        model.cloud.cloudSession = MobiusCloudSession(userID: userID, expiresAt: .distantFuture)
        model.appIsInBackground = false

        await model.start()

        let becameReady = await eventually { model.gateway.connectionState.isReady }
        XCTAssertTrue(becameReady)
        XCTAssertEqual(attempts, 3)
    }

    func testCloudAccountRemainsRecognizedWhenAnotherGatewayIsSelected() throws {
        let model = try model()
        let userID = UUID()
        let cloud = GatewayAccount(
            endpoint: try GatewayEndpoint("wss://account.sprites.app"),
            cloudUserID: userID
        )
        let selfHosted = GatewayAccount(endpoint: try GatewayEndpoint("wss://gateway.example"))
        model.gateway.accounts = [cloud, selfHosted]
        model.cloud.cloudSession = MobiusCloudSession(userID: userID, expiresAt: .distantFuture)
        model.gateway.selectedAccountID = selfHosted.id

        XCTAssertEqual(model.cloud.cloudGateway?.id, cloud.id)
        XCTAssertFalse(model.selectedGatewayIsMobiusCloud)
        XCTAssertTrue(model.cloud.hasCloudAccount)
    }

    func testAttachmentsCanBeImportedWhileATurnIsActive() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.gateway.connectionState = .ready
        model.chat.selectedSessionID = "chat-1"
        model.chat.contributions = [fileAttachmentContribution()]
        model.chat.activeTurnID = "turn-1"

        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let fileURL = directory.appendingPathComponent("during-turn.txt")
        try Data("queued while running".utf8).write(to: fileURL)

        XCTAssertTrue(model.canImportAttachments)
        let requestCount = await recorder.requestCount()
        await model.importAttachments([fileURL])

        let request = await recorder.firstRequest(after: requestCount) { request in
            if case .beginSessionFileUpload = request { return true }
            return false
        }
        guard
            case .beginSessionFileUpload(_, let sessionID, let name, let size, _) = try XCTUnwrap(
                request
            )
        else { return XCTFail("Expected an attachment upload during the active turn") }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(name, "during-turn.txt")
        XCTAssertEqual(size, 20)
    }

    func testSwitchingGatewaysClearsGatewayScopedStateBeforeTokenLookup() throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }

        let store = GatewayStore(defaults: defaults)
        let first = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        let second = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9192"))
        store.select(first)

        let model = AppModel(client: GatewayClient(), store: store)
        model.gateway.accounts = [first, second]
        model.gateway.connectionState = .ready
        model.chat.composer = "Gateway A draft"
        model.providerAPIKey = "gateway-a-secret"
        model.providerActionState = .credentialSaved("Gateway A")
        model.pairingCodeInfo = PairingCodeInfo(code: "1234", expiresAt: .distantFuture)
        model.gitCredentialAvailable = true
        model.gitCredentialUsername = "octo"
        let sshIdentity = SshIdentityRecord(
            label: "id_ed25519",
            algorithm: "ssh-ed25519",
            fingerprint: "SHA256:safe"
        )
        model.sshIdentities = [sshIdentity]
        model.generatedSshIdentity = GeneratedSshIdentity(
            identity: sshIdentity,
            publicKey: "ssh-ed25519 AAAA mobius"
        )

        model.selectAccount(second.id)

        XCTAssertEqual(model.gateway.selectedAccountID, second.id)
        XCTAssertEqual(model.gateway.connectionState, .connecting)
        XCTAssertEqual(model.chat.composer, "")
        XCTAssertEqual(model.providerAPIKey, "")
        XCTAssertEqual(model.providerActionState, .idle)
        XCTAssertNil(model.pairingCodeInfo)
        XCTAssertNil(model.gitCredentialAvailable)
        XCTAssertNil(model.gitCredentialUsername)
        XCTAssertNil(model.sshIdentities)
        XCTAssertNil(model.generatedSshIdentity)
    }

    func testConnectionEndCancelsExtensionAndCredentialRequests() throws {
        let model = try model()
        model.gateway.connectionState = .ready
        model.extensionAction = .installing
        model.extensionRequestID = "extension-request"
        model.gitCredentialRequestID = "git-request"
        model.isApprovingGitCredential = true
        model.isCheckingGitCredential = true
        model.sshIdentityRequestID = "ssh-request"
        model.isLoadingSshIdentities = true
        model.isGeneratingSshIdentity = true

        model.gateway.connectionEnded(
            generation: model.gateway.connectionGeneration,
            message: "Gateway disconnected."
        )

        XCTAssertNil(model.extensionAction)
        XCTAssertNil(model.extensionRequestID)
        XCTAssertNil(model.gitCredentialRequestID)
        XCTAssertFalse(model.isApprovingGitCredential)
        XCTAssertFalse(model.isCheckingGitCredential)
        XCTAssertNil(model.sshIdentityRequestID)
        XCTAssertFalse(model.isLoadingSshIdentities)
        XCTAssertFalse(model.isGeneratingSshIdentity)
    }

    func testNewerGatewayPromptsForAppUpdateAndStopsReconnects() throws {
        let model = try model()

        model.gateway.connectionEnded(
            generation: model.gateway.connectionGeneration,
            error: GatewayWireError.unsupportedVersion(gatewayProtocolVersion + 1)
        )

        XCTAssertTrue(model.showsAppUpdateAlert)
        XCTAssertTrue(model.gateway.automaticReconnectBlocked)
        XCTAssertFalse(model.gateway.reconnectsOnActivation)

        let translations: [(AppLanguage, String, String, String)] = [
            (
                .french, "Ouvrir l’App Store", "Mettez l’app à jour",
                "La page de mise à jour de l’App Store est indisponible."
            ),
            (
                .german, "App Store öffnen", "Aktualisieren Sie die App",
                "Die Update-Seite im App Store ist nicht verfügbar."
            ),
        ]
        for (language, button, messagePrefix, unavailable) in translations {
            model.language = language
            XCTAssertEqual(model.localizedString("Open App Store"), button)
            XCTAssertTrue(
                model.localizedString(
                    "Update the app to connect to this gateway. Install the latest version from the App Store, then reopen the app."
                ).hasPrefix(messagePrefix))
            XCTAssertEqual(
                model.localizedString("The App Store update page is unavailable."),
                unavailable
            )
        }
    }

    func testStaleProtocolErrorCannotBlockTheCurrentConnection() throws {
        let model = try model()

        model.gateway.connectionEnded(
            generation: UUID(),
            error: GatewayWireError.unsupportedVersion(gatewayProtocolVersion + 1)
        )

        XCTAssertFalse(model.showsAppUpdateAlert)
        XCTAssertFalse(model.gateway.automaticReconnectBlocked)
    }

    func testRenamingGatewayPersistsItsFriendlyName() throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let account = GatewayAccount(
            endpoint: try GatewayEndpoint("wss://gateway.example"),
            displayName: "Gateway"
        )
        defaults.set(try JSONEncoder().encode([account]), forKey: "paired-gateways")
        defaults.set(account.id.uuidString, forKey: "selected-gateway")
        let store = GatewayStore(defaults: defaults)
        let model = AppModel(client: GatewayClient(), store: store)

        model.renameGateway(account, to: "Home gateway")

        XCTAssertEqual(model.gateway.selectedAccount?.displayName, "Home gateway")
        XCTAssertEqual(store.loadAccounts().first?.displayName, "Home gateway")
    }

    func testGatewayCatalogPersistsMachineNameForConfiguredAccount() throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let account = GatewayAccount(endpoint: try GatewayEndpoint("wss://gateway.example"))
        defaults.set(try JSONEncoder().encode([account]), forKey: "paired-gateways")
        defaults.set(account.id.uuidString, forKey: "selected-gateway")
        let store = GatewayStore(defaults: defaults)
        let model = AppModel(client: GatewayClient(), store: store)

        model.applyGatewayCatalog(
            ready(
                botDefaults: VersionedAgentConfig(revision: 1, config: composition())
            ))

        XCTAssertEqual(model.gateway.selectedAccount?.machineName, "snowwhite.local")
        XCTAssertEqual(store.loadAccounts().first?.machineName, "snowwhite.local")
    }

    func testReactivationReplacesAStaleConnectionAndPreservesThePresentedChat() throws {
        let model = try model()
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.chat.selectedSessionID = "chat-1"
        model.destination = .chats
        model.navigationPath = [.chat(.session("chat-1"))]
        model.gateway.connectionState = .ready

        model.setSceneActive(true)
        XCTAssertEqual(model.gateway.connectionState, .ready)

        model.setSceneActive(false)
        model.setSceneActive(true)

        XCTAssertEqual(model.gateway.connectionState, .connecting)
        XCTAssertEqual(model.chat.selectedSessionID, "chat-1")
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-1"))])
    }

    func testReactivationFromChatCatalogFindsUnreadTerminalWork() throws {
        let model = try model()
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.chat.selectedSessionID = "chat-1"
        model.destination = .chats
        model.navigationPath = []
        model.gateway.connectionState = .ready
        model.applySessions([
            session(
                state: .running,
                turnID: "turn-1",
                sequence: 1
            )
        ])

        model.setSceneActive(true)
        model.setSceneActive(false)
        model.setSceneActive(true)

        XCTAssertEqual(model.gateway.connectionState, .connecting)
        XCTAssertNil(model.chat.selectedSessionID)
        XCTAssertTrue(model.navigationPath.isEmpty)

        model.applySessions([
            session(
                state: .idle,
                outcome: .failed,
                message: "the agent stopped",
                sequence: 1
            )
        ])
        XCTAssertTrue(model.chat.unreadSessionIDs.contains("chat-1"))

        model.applySessions([
            session(
                state: .idle,
                outcome: .failed,
                message: "the agent stopped",
                sequence: 1
            ),
            session(sessionID: "chat-2", state: .idle, outcome: .completed, sequence: 1),
        ])
        XCTAssertTrue(model.chat.unreadSessionIDs.contains("chat-2"))
    }

    func testAutomaticReconnectRestoresDraftWithoutReplayingSubmission() async throws {
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
        let model = AppModel(
            client: GatewayClient(),
            store: store,
            settingsDefaults: defaults,
            requestSender: { request in await recorder.record(request) },
            connectionOpener: { endpoint in try await harness.open(endpoint) },
            reconnectDelay: { _ in .zero }
        )
        await model.appDidBecomeActive()

        await model.start()
        try await Task.sleep(for: .milliseconds(100))
        let connectedAttempts = await harness.attemptCount()
        XCTAssertEqual(connectedAttempts, 2)
        await harness.yield(.authenticated)
        await harness.yield(
            .ready(
                ready(
                    botDefaults: VersionedAgentConfig(revision: 1, config: composition())
                )))
        let gatewayReady = await eventually { model.gateway.connectionState.isReady }
        XCTAssertTrue(gatewayReady)
        let openRequestCount = await recorder.requestCount()
        model.openChat("chat-1")
        let recordedOpen = await recorder.firstRequest(
            after: openRequestCount
        ) { request in
            guard case .openSession(_, "chat-1", _) = request else { return false }
            return true
        }
        let openRequest = try XCTUnwrap(recordedOpen)
        guard case .openSession(let openRequestID, _, _) = openRequest else {
            return XCTFail("Expected session open")
        }
        await harness.yield(
            .sessionOpened(
                requestID: openRequestID,
                payload: sessionReady(latestSequence: 0)
            ))
        await harness.yield(
            .sessionReplayComplete(
                requestID: openRequestID,
                sessionID: "chat-1"
            ))
        try await Task.sleep(for: .milliseconds(50))
        model.chat.composer = "Run this once"
        XCTAssertTrue(model.canSendComposer)
        model.sendMessage()
        try await Task.sleep(for: .milliseconds(30))

        await harness.fail()
        try await Task.sleep(for: .milliseconds(100))

        let submissions = await recorder.requests().filter { request in
            if case .submit = request { return true }
            return false
        }
        XCTAssertEqual(submissions.count, 1)
        XCTAssertEqual(model.chat.composer, "Run this once")
        let reconnectAttempts = await harness.attemptCount()
        XCTAssertEqual(reconnectAttempts, 3)
    }

    func testApprovalRemainsAvailableWhenSendFails() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }

        let model = AppModel(
            client: GatewayClient(),
            store: GatewayStore(defaults: defaults)
        )
        let approval = PendingApproval(
            id: "approval-1",
            reason: "Run the command?",
            calls: [ApprovalCall(id: "call-1", name: "shell", arguments: "{}")]
        )
        model.chat.selectedSessionID = "chat-1"
        model.chat.pendingApproval = approval

        model.resolveApproval(.approved)
        let failed = await eventually { model.toast?.tone == .error }

        XCTAssertTrue(failed)
        XCTAssertEqual(model.chat.pendingApproval, approval)
        XCTAssertEqual(model.toast?.tone, .error)
    }

    func testGatewaySendFailureEndsTheStaleConnection() async throws {
        let model = try model { _ in throw POSIXError(.ENOTCONN) }
        model.gateway.connectionState = .ready
        model.extensionInstallSource = "https://github.com/DietrichGebert/ponytail.git"

        model.installExtension()

        let disconnected = await eventually {
            model.gateway.connectionState == .failed("The gateway disconnected.")
        }
        XCTAssertTrue(disconnected)
        XCTAssertNil(model.extensionAction)
        XCTAssertNil(model.extensionRequestID)
        XCTAssertEqual(model.toast?.message, "The gateway disconnected.")
    }

    func testSshIdentitySetupReturnsOnlyTheNewPublicKeyForSharing() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.gateway.connectionState = .ready

        model.listSshIdentities()
        let recordedList = await recorder.firstRequest(after: 0) { request in
            if case .listSshIdentities = request { return true }
            return false
        }
        let list = try XCTUnwrap(recordedList)
        guard case .listSshIdentities(let listID) = list else {
            return XCTFail("Expected SSH identity list request")
        }
        model.gateway.handle(.sshIdentities(requestID: listID, identities: []))

        let requestCount = await recorder.requestCount()
        model.generateSshIdentity()
        let recordedGenerate = await recorder.firstRequest(after: requestCount) { request in
            if case .generateSshIdentity = request { return true }
            return false
        }
        let generate = try XCTUnwrap(recordedGenerate)
        guard case .generateSshIdentity(let generateID) = generate else {
            return XCTFail("Expected SSH identity generation request")
        }
        let identity = SshIdentityRecord(
            label: "id_ed25519",
            algorithm: "ssh-ed25519",
            fingerprint: "SHA256:safe"
        )
        model.gateway.handle(
            .sshIdentityGenerated(
                requestID: generateID,
                identity: identity,
                publicKey: "ssh-ed25519 AAAA mobius"
            ))

        XCTAssertEqual(model.sshIdentities, [identity])
        XCTAssertEqual(model.generatedSshIdentity?.publicKey, "ssh-ed25519 AAAA mobius")
        XCTAssertFalse(model.isGeneratingSshIdentity)
    }

}

@MainActor
extension AppModelTests {
    func testBackgroundRetiresSocketBeforeDelayedCloudResume() throws {
        let model = try model()
        let userID = UUID()
        let account = GatewayAccount(
            endpoint: try GatewayEndpoint("wss://test.sprites.app"), cloudUserID: userID
        )
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.cloud.cloudSession = MobiusCloudSession(userID: userID, expiresAt: .distantFuture)
        model.chat.selectedSessionID = "chat-1"
        model.navigationPath = [.chat(.session("chat-1"))]
        model.chat.composer = "Keep my draft"
        model.gateway.connectionState = .ready
        let suspendedGeneration = model.gateway.connectionGeneration

        model.appDidEnterBackground()
        XCTAssertNotEqual(model.gateway.connectionGeneration, suspendedGeneration)
        XCTAssertEqual(model.gateway.connectionState, .disconnected)
        model.setSceneActive(true)
        model.gateway.connectionEnded(
            generation: suspendedGeneration,
            error: NSError(domain: NSPOSIXErrorDomain, code: Int(ECONNABORTED))
        )
        XCTAssertNil(model.toast)
        XCTAssertEqual(model.gateway.connectionState, .disconnected)
        XCTAssertEqual(model.chat.composer, "Keep my draft")
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-1"))])
        XCTAssertTrue(model.gateway.reconnectsOnActivation)

        // A failure on the replacement transport remains visible.
        model.gateway.connectionEnded(
            generation: model.gateway.connectionGeneration, message: "Current transport failed")
        XCTAssertEqual(model.gateway.connectionState, .failed("Current transport failed"))
        XCTAssertNotNil(model.toast)
    }
}

@MainActor
extension AppModelTests {
    func testReadyKeepsSettingsOpenedWhileThePreviousChatWasReconnecting() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { await recorder.record($0) }
        model.chat.selectedSessionID = "chat-1"
        model.chat.sessionToRestoreID = "chat-1"
        model.gateway.connectionState = .connecting
        model.destination = .providers
        model.navigationPath = [.settings(.provider("my-provider"))]
        model.gateway.handle(
            .ready(
                ready(
                    botDefaults: VersionedAgentConfig(revision: 1, config: composition())
                )))
        XCTAssertEqual(model.destination, .providers)
        XCTAssertEqual(model.navigationPath, [.settings(.provider("my-provider"))])
        XCTAssertNil(model.chat.sessionToRestoreID)
        XCTAssertNil(model.chat.selectedSessionID)
        let requests = await recorder.requests()
        XCTAssertFalse(
            requests.contains {
                if case .openSession = $0 { return true }
                return false
            })
    }
}
