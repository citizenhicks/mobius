import Foundation
import XCTest

@MainActor
extension AppModelTests {
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

    func testCloudConnectionRetriesFailureAndSilentWarmupUntilReady() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let store = GatewayStore(defaults: defaults)
        let account = GatewayAccount(endpoint: try GatewayEndpoint(
            "wss://mobius-test-org.sprites.app"
        ))
        try store.save(account, token: "test-token")
        addTeardownBlock { try await store.remove(account) }
        let payload = ready(
            defaultConfig: VersionedAgentConfig(revision: 1, config: composition())
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
        model.cloudSession = MobiusCloudSession(userID: UUID(), expiresAt: .distantFuture)
        await model.appDidBecomeActive()

        model.start()

        let becameReady = await eventually { model.connectionState.isReady }
        XCTAssertTrue(becameReady)
        XCTAssertEqual(attempts, 3)
    }

    func testCloudAccountRemainsRecognizedWhenAnotherGatewayIsSelected() throws {
        let model = try model()
        let cloud = GatewayAccount(endpoint: try GatewayEndpoint("wss://account.sprites.app"))
        let selfHosted = GatewayAccount(endpoint: try GatewayEndpoint("wss://gateway.example"))
        model.accounts = [cloud, selfHosted]
        model.cloudSession = MobiusCloudSession(userID: UUID(), expiresAt: .distantFuture)
        model.selectedAccountID = selfHosted.id

        XCTAssertEqual(model.mobiusCloudGateway?.id, cloud.id)
        XCTAssertFalse(model.selectedGatewayIsMobiusCloud)
        XCTAssertTrue(model.hasCloudAccount)
    }

    func testAttachmentsCanBeImportedWhileATurnIsActive() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model(requestSender: { request in
            await recorder.record(request)
        })
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.contributions = [fileAttachmentContribution()]
        model.activeTurnID = "turn-1"

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
        guard case .beginSessionFileUpload(_, let sessionID, let name, let size, _) = try XCTUnwrap(
            request
        ) else { return XCTFail("Expected an attachment upload during the active turn") }
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
        model.accounts = [first, second]
        model.connectionState = .ready
        model.composer = "Gateway A draft"
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

        XCTAssertEqual(model.selectedAccountID, second.id)
        XCTAssertEqual(model.connectionState, .connecting)
        XCTAssertEqual(model.composer, "")
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
        model.connectionState = .ready
        model.extensionAction = .installing
        model.extensionRequestID = "extension-request"
        model.gitCredentialRequestID = "git-request"
        model.isApprovingGitCredential = true
        model.isCheckingGitCredential = true
        model.sshIdentityRequestID = "ssh-request"
        model.isLoadingSshIdentities = true
        model.isGeneratingSshIdentity = true

        model.connectionEnded(
            generation: model.connectionGeneration,
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

        XCTAssertEqual(model.selectedAccount?.displayName, "Home gateway")
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

        model.applyGatewayCatalog(ready(
            defaultConfig: VersionedAgentConfig(revision: 1, config: composition())
        ))

        XCTAssertEqual(model.selectedAccount?.machineName, "snowwhite.local")
        XCTAssertEqual(store.loadAccounts().first?.machineName, "snowwhite.local")
    }

    func testReactivationReplacesAStaleConnectionAndPreservesThePresentedChat() throws {
        let model = try model()
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.selectedSessionID = "chat-1"
        model.destination = .chats
        model.navigationPath = [.chat(.session("chat-1"))]
        model.connectionState = .ready

        model.setSceneActive(true)
        XCTAssertEqual(model.connectionState, .ready)

        model.setSceneActive(false)
        model.setSceneActive(true)

        XCTAssertEqual(model.connectionState, .connecting)
        XCTAssertEqual(model.selectedSessionID, "chat-1")
        XCTAssertEqual(model.navigationPath, [.chat(.session("chat-1"))])
    }

    func testReactivationFromChatCatalogDoesNotPreserveSelectedSession() throws {
        let model = try model()
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        model.accounts = [account]
        model.selectedAccountID = account.id
        model.selectedSessionID = "chat-1"
        model.destination = .chats
        model.navigationPath = []
        model.connectionState = .ready

        model.setSceneActive(true)
        model.setSceneActive(false)
        model.setSceneActive(true)

        XCTAssertEqual(model.connectionState, .connecting)
        XCTAssertNil(model.selectedSessionID)
        XCTAssertTrue(model.navigationPath.isEmpty)
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

        model.start()
        try await Task.sleep(for: .milliseconds(100))
        let connectedAttempts = await harness.attemptCount()
        XCTAssertEqual(connectedAttempts, 2)
        await harness.yield(.authenticated)
        await harness.yield(.ready(ready(
            defaultConfig: VersionedAgentConfig(revision: 1, config: composition())
        )))
        let gatewayReady = await eventually { model.connectionState.isReady }
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
        await harness.yield(.sessionOpened(
            requestID: openRequestID,
            payload: sessionReady(latestSequence: 0)
        ))
        await harness.yield(.sessionReplayComplete(
            requestID: openRequestID,
            sessionID: "chat-1"
        ))
        try await Task.sleep(for: .milliseconds(50))
        model.composer = "Run this once"
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
        XCTAssertEqual(model.composer, "Run this once")
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
        model.selectedSessionID = "chat-1"
        model.pendingApproval = approval

        model.resolveApproval(.approved)
        try await Task.sleep(for: .milliseconds(20))

        XCTAssertEqual(model.pendingApproval, approval)
        XCTAssertEqual(model.toast?.tone, .error)
    }

    func testGatewaySendFailureEndsTheStaleConnection() async throws {
        let model = try model { _ in throw POSIXError(.ENOTCONN) }
        model.connectionState = .ready
        model.extensionInstallSource = "https://github.com/DietrichGebert/ponytail.git"

        model.installExtension()

        let disconnected = await eventually {
            model.connectionState == .failed("The gateway disconnected.")
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
        model.connectionState = .ready

        model.listSshIdentities()
        let recordedList = await recorder.firstRequest(after: 0) { request in
            if case .listSshIdentities = request { return true }
            return false
        }
        let list = try XCTUnwrap(recordedList)
        guard case .listSshIdentities(let listID) = list else {
            return XCTFail("Expected SSH identity list request")
        }
        model.handle(.sshIdentities(requestID: listID, identities: []))

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
        model.handle(.sshIdentityGenerated(
            requestID: generateID,
            identity: identity,
            publicKey: "ssh-ed25519 AAAA mobius"
        ))

        XCTAssertEqual(model.sshIdentities, [identity])
        XCTAssertEqual(model.generatedSshIdentity?.publicKey, "ssh-ed25519 AAAA mobius")
        XCTAssertFalse(model.isGeneratingSshIdentity)
    }

}
