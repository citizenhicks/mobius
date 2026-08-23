import CryptoKit
import Foundation
import Security
import XCTest

@MainActor
final class MobiusCloudTests: XCTestCase {
    func testAppleNonceUsesThirtyTwoRandomBytesAndSHA256() throws {
        let nonce = try MobiusCloudAppleNonce.make()
        var encoded = nonce.rawValue
            .replacing("-", with: "+")
            .replacing("_", with: "/")
        encoded += String(repeating: "=", count: (4 - encoded.count % 4) % 4)
        let rawBytes = try XCTUnwrap(Data(base64Encoded: encoded))
        let expectedHash = SHA256.hash(data: Data(nonce.rawValue.utf8)).map { byte in
            let hex = String(byte, radix: 16)
            return byte < 16 ? "0\(hex)" : hex
        }.joined()

        XCTAssertEqual(rawBytes.count, 32)
        XCTAssertEqual(nonce.rawValue.count, 43)
        XCTAssertEqual(nonce.requestValue, expectedHash)
    }

    func testCloudAccountLoadingStateStopsForDataOrError() throws {
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let cloudStore = MobiusCloudSessionStore(service: suiteName)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? cloudStore.remove()
        }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: MobiusCloudClient(store: cloudStore) { _ in
                throw URLError(.notConnectedToInternet)
            }
        )
        model.cloudSession = MobiusCloudSession(userID: UUID(), expiresAt: .distantFuture)

        XCTAssertTrue(model.isLoadingCloudAccount)
        model.cloudError = "Unavailable"
        XCTAssertFalse(model.isLoadingCloudAccount)
        model.cloudError = nil
        model.cloudAccount = MobiusCloudAccount(
            email: "private@privaterelay.appleid.com",
            subscribed: true,
            sharesDiagnostics: false
        )
        XCTAssertFalse(model.isLoadingCloudAccount)
    }

    func testClientUsesNativeCloudContractAndBearerFromDeviceOnlyKeychain() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let store = MobiusCloudSessionStore(service: service)
        defer { try? store.remove() }
        let reset = try XCTUnwrap(
            ISO8601DateFormatter().date(from: "2099-02-01T00:00:00Z")
        )
        var requests: [URLRequest] = []
        let responses = [
            #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":true,"luna":{"creditMicrousd":2400000,"remainingMicrousd":1992000,"resetsAt":"2099-02-01T00:00:00Z"}}"#,
            #"{}"#,
            #"{"accepted":true}"#,
            #"{"status":"ready"}"#,
            #"{"endpoint":"wss://gateway.example","pairingCode":"0123456789abcdef","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{}"#,
        ]
        let client = MobiusCloudClient(store: store) { request in
            requests.append(request)
            let index = requests.count - 1
            return try self.response(
                for: request,
                status: index == responses.count - 1 ? 202 : 200,
                json: responses[index]
            )
        }

        let session = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )
        let attributes = try keychainAttributes(service: service)
        XCTAssertEqual(
            attributes[kSecAttrAccessible as String] as? String,
            kSecAttrAccessibleWhenUnlockedThisDeviceOnly as String
        )
        let account = try await client.account()
        try await client.updateSharesDiagnostics(false)
        try await client.submitSubscription(signedTransaction: "header.payload.signature")
        let status = try await client.gatewayStatus()
        let grant = try await client.createPairingGrant()
        try await client.deleteAccount(
            authorizationCode: "delete-code",
            nonce: String(repeating: "d", count: 43)
        )

        XCTAssertEqual(session.userID, userID)
        XCTAssertEqual(
            account,
            MobiusCloudAccount(
                email: "private@privaterelay.appleid.com",
                subscribed: true,
                sharesDiagnostics: true,
                luna: MobiusCloudUsageLimit(
                    creditMicrousd: 2_400_000,
                    remainingMicrousd: 1_992_000,
                    resetsAt: reset
                )
            )
        )
        XCTAssertEqual(account.luna?.remainingFraction ?? 0, 0.83, accuracy: 0.000_001)
        XCTAssertEqual(status, .ready)
        XCTAssertEqual(grant.setup.endpoint.rawValue, "wss://gateway.example")
        XCTAssertEqual(grant.setup.code, "0123456789abcdef")
        XCTAssertEqual(requests.map { $0.url?.path }, [
            "/api/mobile/auth/apple",
            "/api/mobile/account",
            "/api/mobile/account",
            "/api/mobile/subscription",
            "/api/mobile/gateway",
            "/api/mobile/gateway",
            "/api/mobile/account",
        ])
        XCTAssertEqual(
            requests.map(\.httpMethod),
            ["POST", "GET", "PUT", "PUT", "GET", "POST", "DELETE"]
        )
        XCTAssertNil(requests[0].value(forHTTPHeaderField: "Authorization"))
        for request in requests.dropFirst() {
            XCTAssertEqual(request.value(forHTTPHeaderField: "Authorization"), "Bearer \(token)")
        }
        let authBody = try XCTUnwrap(requests[0].httpBody)
        let authJSON = try XCTUnwrap(
            JSONSerialization.jsonObject(with: authBody) as? [String: String]
        )
        XCTAssertEqual(authJSON, [
            "authorizationCode": "apple-code",
            "nonce": String(repeating: "n", count: 43),
        ])
        let deletionBody = try XCTUnwrap(requests[6].httpBody)
        let deletionJSON = try XCTUnwrap(
            JSONSerialization.jsonObject(with: deletionBody) as? [String: String]
        )
        XCTAssertEqual(deletionJSON, [
            "authorizationCode": "delete-code",
            "nonce": String(repeating: "d", count: 43),
        ])
        XCTAssertNil(try client.loadSession())
        let accountUpdateBody = try XCTUnwrap(requests[2].httpBody)
        let accountUpdateJSON = try XCTUnwrap(
            JSONSerialization.jsonObject(with: accountUpdateBody) as? [String: Bool]
        )
        XCTAssertEqual(accountUpdateJSON, ["sharesDiagnostics": false])

    }

    func testRejectedAccountDeletionDoesNotClearSession() async throws {
        let service = "app.mobius.cloud.tests.\(UUID())"
        let store = MobiusCloudSessionStore(service: service)
        defer { try? store.remove() }
        var requestCount = 0
        var deletionStatus = 409
        let client = MobiusCloudClient(store: store) { request in
            requestCount += 1
            if requestCount == 1 {
                return try self.response(
                    for: request,
                    json: #"{"token":"ttttttttttttttttttttttttttttttttttttttttttt","userId":"00000000-0000-0000-0000-000000000001","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            }
            return try self.response(for: request, status: deletionStatus, json: #"{}"#)
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        do {
            try await client.deleteAccount(
                authorizationCode: "delete-code",
                nonce: String(repeating: "d", count: 43)
            )
            XCTFail("Expected an active subscription to block deletion")
        } catch let error as MobiusCloudError {
            guard case .activeSubscription = error else {
                return XCTFail("Expected activeSubscription, got \(error)")
            }
        }
        XCTAssertNotNil(try client.loadSession())

        deletionStatus = 403
        do {
            try await client.deleteAccount(
                authorizationCode: "expired-code",
                nonce: String(repeating: "e", count: 43)
            )
            XCTFail("Expected rejected Apple authorization")
        } catch let error as MobiusCloudError {
            guard case .invalidAuthorization = error else {
                return XCTFail("Expected invalidAuthorization, got \(error)")
            }
        }
        XCTAssertNotNil(try client.loadSession())
    }

    func testUnauthorizedAccountDeletionClearsCloudSession() async throws {
        let service = "app.mobius.cloud.tests.\(UUID())"
        let store = MobiusCloudSessionStore(service: service)
        defer { try? store.remove() }
        var requestCount = 0
        let client = MobiusCloudClient(store: store) { request in
            requestCount += 1
            if requestCount == 1 {
                return try self.response(
                    for: request,
                    json: #"{"token":"ttttttttttttttttttttttttttttttttttttttttttt","userId":"00000000-0000-0000-0000-000000000001","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            }
            return try self.response(for: request, status: 401, json: #"{}"#)
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: client
        )
        model.cloudAccount = MobiusCloudAccount(
            email: nil,
            subscribed: false,
            sharesDiagnostics: false
        )

        let deleted = await model.deleteCloudAccount(
            authorizationCode: "delete-code",
            nonce: String(repeating: "d", count: 43)
        )
        XCTAssertFalse(deleted)
        XCTAssertNil(model.cloudSession)
        XCTAssertNil(model.cloudAccount)
        XCTAssertNil(try client.loadSession())
    }

    func testAcceptedAccountDeletionClearsLocalCloudState() async throws {
        let currentUserID = try XCTUnwrap(UUID(
            uuidString: "00000000-0000-0000-0000-000000000001"
        ))
        let service = "app.mobius.cloud.tests.\(UUID())"
        let store = MobiusCloudSessionStore(service: service)
        defer { try? store.remove() }
        var requestCount = 0
        let client = MobiusCloudClient(store: store) { request in
            requestCount += 1
            if requestCount == 1 {
                return try self.response(
                    for: request,
                    json: #"{"token":"ttttttttttttttttttttttttttttttttttttttttttt","userId":"\#(currentUserID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            }
            return try self.response(for: request, status: 202, json: "")
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: root)
        }
        let gatewayStore = GatewayStore(
            defaults: defaults,
            transcriptDirectory: root.appendingPathComponent("Transcripts"),
            draftDirectory: root.appendingPathComponent("Drafts")
        )
        let retainedGateway = GatewayAccount(
            endpoint: try GatewayEndpoint("wss://retained.sprites.app"),
            cloudUserID: UUID()
        )
        let deletedGateway = GatewayAccount(
            endpoint: try GatewayEndpoint("wss://deleted.sprites.app"),
            cloudUserID: currentUserID
        )
        try gatewayStore.save(retainedGateway, token: "retained-token")
        try gatewayStore.save(deletedGateway, token: "deleted-token")
        let model = AppModel(
            store: gatewayStore,
            settingsDefaults: defaults,
            requestSender: { _ in },
            connectionOpener: { _ in AsyncThrowingStream { _ in } },
            cloudClient: client
        )
        model.cloudAccount = MobiusCloudAccount(
            email: "private@privaterelay.appleid.com",
            subscribed: false,
            sharesDiagnostics: false
        )
        XCTAssertEqual(model.mobiusCloudGateway?.id, deletedGateway.id)

        let deleted = await model.deleteCloudAccount(
            authorizationCode: "delete-code",
            nonce: String(repeating: "d", count: 43)
        )
        XCTAssertTrue(deleted)
        XCTAssertNil(model.cloudSession)
        XCTAssertNil(model.cloudAccount)
        XCTAssertNil(try client.loadSession())
        XCTAssertEqual(model.cloudAction, .idle)
        XCTAssertEqual(model.accounts.map(\.id), [retainedGateway.id])
        XCTAssertEqual(gatewayStore.loadAccounts().map(\.id), [retainedGateway.id])
        XCTAssertNoThrow(try gatewayStore.token(for: retainedGateway))
        XCTAssertThrowsError(try gatewayStore.token(for: deletedGateway))
        try await gatewayStore.remove(retainedGateway)
    }

    func testCloudAccountRejectsInvalidEmail() async throws {
        let store = MobiusCloudSessionStore(service: "app.mobius.cloud.tests.\(UUID())")
        defer { try? store.remove() }
        var requestCount = 0
        let client = MobiusCloudClient(store: store) { request in
            requestCount += 1
            let json = requestCount == 1
                ? #"{"token":"ttttttttttttttttttttttttttttttttttttttttttt","userId":"00000000-0000-0000-0000-000000000001","expiresAt":"2099-01-01T00:00:00Z"}"#
                : #"{"email":"not-an-email","subscribed":true,"sharesDiagnostics":false}"#
            return try self.response(for: request, json: json)
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        do {
            _ = try await client.account()
            XCTFail("Expected invalid account email to be rejected")
        } catch let error as MobiusCloudError {
            guard case .invalidAccountResponse = error else {
                return XCTFail("Expected invalidAccountResponse, got \(error)")
            }
        }
    }

    func testCloudAccountRejectsInvalidUsageLimit() async throws {
        let store = MobiusCloudSessionStore(service: "app.mobius.cloud.tests.\(UUID())")
        defer { try? store.remove() }
        var requestCount = 0
        let client = MobiusCloudClient(store: store) { request in
            requestCount += 1
            let json = requestCount == 1
                ? #"{"token":"ttttttttttttttttttttttttttttttttttttttttttt","userId":"00000000-0000-0000-0000-000000000001","expiresAt":"2099-01-01T00:00:00Z"}"#
                : #"{"email":null,"subscribed":true,"sharesDiagnostics":false,"luna":{"creditMicrousd":2400000,"remainingMicrousd":2400001,"resetsAt":"2099-02-01T00:00:00Z"}}"#
            return try self.response(for: request, json: json)
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        do {
            _ = try await client.account()
            XCTFail("Expected invalid Cloud usage limit to be rejected")
        } catch let error as MobiusCloudError {
            guard case .invalidAccountResponse = error else {
                return XCTFail("Expected invalidAccountResponse, got \(error)")
            }
        }
    }

    func testStaleUnauthorizedResponseDoesNotClearNewCloudSignIn() async throws {
        let firstUserID = UUID()
        let secondUserID = UUID()
        let firstToken = String(repeating: "a", count: 43)
        let secondToken = String(repeating: "b", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        let staleRequestStarted = expectation(description: "Stale request started")
        var staleContinuation: CheckedContinuation<(Data, HTTPURLResponse), Never>?
        var staleResponse: (Data, HTTPURLResponse)?
        var requestCount = 0
        let client = MobiusCloudClient(store: sessionStore) { request in
            requestCount += 1
            switch requestCount {
            case 1:
                return try self.response(
                    for: request,
                    json: #"{"token":"\#(firstToken)","userId":"\#(firstUserID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            case 2:
                staleResponse = try self.response(for: request, status: 401, json: "{}")
                return await withCheckedContinuation { continuation in
                    staleContinuation = continuation
                    staleRequestStarted.fulfill()
                }
            case 3:
                return try self.response(
                    for: request,
                    json: #"{"token":"\#(secondToken)","userId":"\#(secondUserID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            default:
                return try self.response(for: request, status: 500, json: "{}")
            }
        }
        _ = try await client.authenticate(
            authorizationCode: "first-code",
            nonce: String(repeating: "n", count: 43)
        )

        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: client
        )
        let staleRefresh = Task { await model.refreshCloudAccount() }
        await fulfillment(of: [staleRequestStarted], timeout: 1)

        let secondSession = try await client.authenticate(
            authorizationCode: "second-code",
            nonce: String(repeating: "n", count: 43)
        )
        model.cloudSession = secondSession
        staleContinuation?.resume(returning: try XCTUnwrap(staleResponse))
        await staleRefresh.value

        XCTAssertEqual(model.cloudSession?.userID, secondUserID)
        XCTAssertEqual(try client.loadSession()?.userID, secondUserID)
        XCTAssertNil(model.cloudError)
    }

    func testSubscribedSignInConnectsWithoutProductOrSubmittingAnotherTransaction() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        let responses = [
            #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false}"#,
            #"{"status":"ready"}"#,
            #"{"endpoint":"wss://gateway.example","pairingCode":"0123456789abcdef","expiresAt":"2099-01-01T00:00:00Z"}"#,
        ]
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            return try self.response(for: request, json: responses[requests.count - 1])
        }

        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let gatewayStore = GatewayStore(defaults: defaults)
        let gatewayRequests = GatewayRequestRecorder()
        let model = AppModel(
            store: gatewayStore,
            settingsDefaults: defaults,
            requestSender: { request in await gatewayRequests.record(request) },
            connectionOpener: { _ in AsyncThrowingStream { _ in } },
            cloudClient: client
        )
        XCTAssertFalse(model.selectedGatewayIsMobiusCloud)

        let connection = Task {
            await model.signInAndPurchaseCloud(
                authorizationCode: "apple-code",
                nonce: String(repeating: "n", count: 43),
                product: nil
            )
        }
        _ = await gatewayRequests.firstRequest(after: 0) {
            if case .pair = $0 { return true }
            return false
        }
        model.handle(.paired(clientID: "cloud-client", token: "gateway-token"))
        let connected = await connection.value
        XCTAssertTrue(connected)
        XCTAssertEqual(requests.map { $0.url?.path }, [
            "/api/mobile/auth/apple",
            "/api/mobile/account",
            "/api/mobile/gateway",
            "/api/mobile/gateway",
        ])
        XCTAssertFalse(requests.contains { $0.url?.path == "/api/mobile/subscription" })
        XCTAssertEqual(model.cloudAccount?.subscribed, true)
        XCTAssertTrue(model.selectedGatewayIsMobiusCloud)
        XCTAssertEqual(model.selectedAccount?.displayName, "möbius Cloud")
        XCTAssertEqual(gatewayStore.loadAccounts().first?.displayName, "möbius Cloud")
        let relaunched = AppModel(
            store: gatewayStore,
            settingsDefaults: defaults,
            requestSender: { _ in },
            connectionOpener: { _ in AsyncThrowingStream { _ in } },
            cloudClient: client
        )
        XCTAssertTrue(relaunched.selectedGatewayIsMobiusCloud)
        try await gatewayStore.remove(try XCTUnwrap(model.accounts.first))
    }

    func testExistingSpriteGatewayIsRecognizedAndNamedAfterCloudSignIn() async throws {
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let gatewayStore = GatewayStore(defaults: defaults)
        let account = GatewayAccount(
            endpoint: try GatewayEndpoint("wss://existing-beta.sprites.app"),
            displayName: "existing-beta.sprites.app",
            machineName: "opaque-sprite-machine"
        )
        try gatewayStore.save(account, token: "existing-gateway-token")

        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        let token = String(repeating: "t", count: 43)
        let client = MobiusCloudClient(store: sessionStore) { request in
            try self.response(
                for: request,
                json: #"{"token":"\#(token)","userId":"00000000-0000-0000-0000-000000000001","expiresAt":"2099-01-01T00:00:00Z"}"#
            )
        }
        let signedOut = AppModel(
            store: gatewayStore,
            settingsDefaults: defaults,
            cloudClient: client
        )
        XCTAssertFalse(signedOut.selectedGatewayIsMobiusCloud)
        XCTAssertEqual(signedOut.selectedAccount?.displayName, "existing-beta.sprites.app")
        XCTAssertEqual(signedOut.selectedAccount?.machineName, "opaque-sprite-machine")

        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )
        let signedIn = AppModel(
            store: gatewayStore,
            settingsDefaults: defaults,
            cloudClient: client
        )

        XCTAssertTrue(signedIn.selectedGatewayIsMobiusCloud)
        XCTAssertEqual(signedIn.selectedAccount?.displayName, "möbius Cloud")
        XCTAssertEqual(signedIn.selectedAccount?.machineName, "möbius Cloud")
        XCTAssertEqual(signedIn.selectedAccount?.endpoint, account.endpoint)
        XCTAssertEqual(gatewayStore.loadAccounts().first?.displayName, "möbius Cloud")
        XCTAssertEqual(gatewayStore.loadAccounts().first?.machineName, "möbius Cloud")

        try gatewayStore.recordMachineName(
            "opaque-sprite-machine",
            for: try XCTUnwrap(signedIn.selectedAccount)
        )
        let repaired = AppModel(
            store: gatewayStore,
            settingsDefaults: defaults,
            cloudClient: client
        )
        XCTAssertEqual(repaired.selectedAccount?.displayName, "möbius Cloud")
        XCTAssertEqual(repaired.selectedAccount?.machineName, "möbius Cloud")
        XCTAssertEqual(gatewayStore.loadAccounts().first?.machineName, "möbius Cloud")

        repaired.applyGatewayCatalog(ReadyPayload(
            machineName: "opaque-sprite-machine",
            sessions: [],
            providers: [],
            providerInstances: [],
            defaultConfig: nil,
            models: [],
            modelProviders: [:],
            middlewareFeatures: [],
            extensions: [],
            contributions: [],
            maxActiveSessions: 4
        ))

        XCTAssertEqual(repaired.gatewayMachineName, "möbius Cloud")
        XCTAssertEqual(repaired.selectedAccount?.machineName, "möbius Cloud")
        XCTAssertEqual(gatewayStore.loadAccounts().first?.machineName, "möbius Cloud")
        XCTAssertEqual(
            try gatewayStore.token(for: try XCTUnwrap(repaired.selectedAccount)),
            "existing-gateway-token"
        )
        try await gatewayStore.remove(try XCTUnwrap(repaired.selectedAccount))
    }

    func testCloudSignOutForgetsCloudGatewayAndClearsPresentedSession() async throws {
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: root)
        }
        let gatewayStore = GatewayStore(
            defaults: defaults,
            transcriptDirectory: root.appendingPathComponent("Transcripts"),
            draftDirectory: root.appendingPathComponent("Drafts")
        )
        let gateway = GatewayAccount(
            endpoint: try GatewayEndpoint("wss://gateway.example"),
            displayName: "Renamed Cloud gateway",
            machineName: mobiusCloudGatewayDisplayName
        )
        try gatewayStore.save(gateway, token: "gateway-token")

        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        let client = MobiusCloudClient(store: sessionStore) { request in
            try self.response(
                for: request,
                json: #"{"token":"ttttttttttttttttttttttttttttttttttttttttttt","userId":"00000000-0000-0000-0000-000000000001","expiresAt":"2099-01-01T00:00:00Z"}"#
            )
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )
        let model = AppModel(
            store: gatewayStore,
            settingsDefaults: defaults,
            requestSender: { _ in },
            connectionOpener: { _ in AsyncThrowingStream { _ in } },
            cloudClient: client
        )
        model.cloudAccount = MobiusCloudAccount(
            email: "private@privaterelay.appleid.com",
            subscribed: true,
            sharesDiagnostics: false
        )
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.destination = .chats
        model.navigationPath = [.chat(.session("chat-1"))]

        await model.signOutOfCloud()

        XCTAssertNil(model.cloudSession)
        XCTAssertNil(model.cloudAccount)
        XCTAssertNil(try client.loadSession())
        XCTAssertTrue(model.accounts.isEmpty)
        XCTAssertTrue(gatewayStore.loadAccounts().isEmpty)
        XCTAssertNil(model.selectedAccountID)
        XCTAssertNil(gatewayStore.selectedAccountID())
        XCTAssertNil(model.selectedSessionID)
        XCTAssertTrue(model.navigationPath.isEmpty)
        XCTAssertEqual(model.connectionState, .disconnected)
        XCTAssertTrue(model.showsPairing)
        XCTAssertThrowsError(try gatewayStore.token(for: gateway)) { error in
            guard case GatewayStore.StoreError.missingToken = error else {
                return XCTFail("Expected the gateway token to be removed")
            }
        }
    }

    func testRestoreConnectsSubscribedAccountWithoutSubmittingTransaction() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        let responses = [
            #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false}"#,
            #"{"status":"ready"}"#,
            #"{"endpoint":"wss://gateway.example","pairingCode":"0123456789abcdef","expiresAt":"2099-01-01T00:00:00Z"}"#,
        ]
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            return try self.response(for: request, json: responses[requests.count - 1])
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let gatewayStore = GatewayStore(defaults: defaults)
        let gatewayRequests = GatewayRequestRecorder()
        let model = AppModel(
            store: gatewayStore,
            settingsDefaults: defaults,
            requestSender: { request in await gatewayRequests.record(request) },
            connectionOpener: { _ in AsyncThrowingStream { _ in } },
            cloudClient: client
        )
        var synchronized = false

        let restoration = Task {
            await model.restoreCloudPurchases {
                synchronized = true
            }
        }
        _ = await gatewayRequests.firstRequest(after: 0) {
            if case .pair = $0 { return true }
            return false
        }
        model.handle(.paired(clientID: "cloud-client", token: "gateway-token"))
        let restored = await restoration.value

        XCTAssertTrue(restored)
        XCTAssertTrue(synchronized)
        XCTAssertEqual(model.accounts.first?.cloudUserID, userID)
        XCTAssertEqual(requests.map { $0.url?.path }, [
            "/api/mobile/auth/apple",
            "/api/mobile/account",
            "/api/mobile/gateway",
            "/api/mobile/gateway",
        ])
        XCTAssertEqual(requests.map(\.httpMethod), ["POST", "GET", "GET", "POST"])
        XCTAssertFalse(requests.contains { $0.url?.path == "/api/mobile/subscription" })
        try await gatewayStore.remove(try XCTUnwrap(model.accounts.first))
    }

    func testExpiredGatewayStopsCloudProvisioning() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        let responses = [
            #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false}"#,
            #"{"status":"expired"}"#,
        ]
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            return try self.response(for: request, json: responses[requests.count - 1])
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: client
        )

        let connected = await model.connectCloudGateway()

        XCTAssertFalse(connected)
        XCTAssertEqual(model.cloudAction, .idle)
        XCTAssertEqual(model.cloudError, MobiusCloudError.subscriptionRequired.localizedDescription)
        XCTAssertEqual(requests.map { $0.url?.path }, [
            "/api/mobile/auth/apple",
            "/api/mobile/account",
            "/api/mobile/gateway",
        ])
    }

    func testCloudPairingRetriesWithFreshGrantAfterFailureResetOrCancellation() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        let responses = [
            #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false}"#,
            #"{"status":"ready"}"#,
            #"{"endpoint":"wss://fresh.example","pairingCode":"fresh-code-1","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false}"#,
            #"{"status":"ready"}"#,
            #"{"endpoint":"wss://fresh.example","pairingCode":"fresh-code-2","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false}"#,
            #"{"status":"ready"}"#,
            #"{"endpoint":"wss://fresh.example","pairingCode":"fresh-code-3","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false}"#,
            #"{"status":"ready"}"#,
            #"{"endpoint":"wss://fresh.example","pairingCode":"fresh-code-4","expiresAt":"2099-01-01T00:00:00Z"}"#,
        ]
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            return try self.response(for: request, json: responses[requests.count - 1])
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let gatewayStore = GatewayStore(defaults: defaults)
        let gatewayRequests = GatewayRequestRecorder()
        var openedEndpoints: [GatewayEndpoint] = []
        let model = AppModel(
            store: gatewayStore,
            settingsDefaults: defaults,
            requestSender: { request in await gatewayRequests.record(request) },
            connectionOpener: { endpoint in
                openedEndpoints.append(endpoint)
                return AsyncThrowingStream { _ in }
            },
            cloudClient: client
        )
        let subscriptionStartedAt = Date(timeIntervalSince1970: 1_700_000_000)
        model.cloudAccount = MobiusCloudAccount(
            email: "private@privaterelay.appleid.com",
            subscribed: true,
            sharesDiagnostics: false,
            subscriptionStartedAt: subscriptionStartedAt
        )
        model.pairingEndpoint = "wss://stale.example"
        model.pairingCode = "stale-code"

        let failedConnection = Task { await model.connectCloudGateway() }
        let firstPairRequest = await gatewayRequests.firstRequest(after: 0) {
            if case .pair = $0 { return true }
            return false
        }
        let firstPair = try XCTUnwrap(firstPairRequest)
        guard case .pair(let firstCode, _, _) = firstPair else {
            return XCTFail("Expected the first fresh pairing request")
        }
        XCTAssertEqual(firstCode, "fresh-code-1")
        XCTAssertEqual(openedEndpoints.map(\.rawValue), ["wss://fresh.example"])
        XCTAssertEqual(model.cloudAction, .connecting)

        model.handle(.error(GatewayFailure(
            code: "unauthorized",
            message: "pairing failed",
            fatal: true
        )))
        let failed = await failedConnection.value
        XCTAssertFalse(failed)
        XCTAssertEqual(model.pairingEndpoint, "wss://")
        XCTAssertEqual(model.pairingCode, "")
        XCTAssertNil(model.pendingPairingAccount)

        let firstRequestCount = await gatewayRequests.requestCount()
        let cancelledConnection = Task { await model.connectCloudGateway() }
        let secondPairRequest = await gatewayRequests.firstRequest(after: firstRequestCount) {
            if case .pair = $0 { return true }
            return false
        }
        let secondPair = try XCTUnwrap(secondPairRequest)
        guard case .pair(let secondCode, _, _) = secondPair else {
            return XCTFail("Expected the replacement pairing request")
        }
        XCTAssertEqual(secondCode, "fresh-code-2")
        XCTAssertEqual(model.cloudAction, .connecting)

        model.resetGatewayState(preservingDrafts: false)
        let cancelled = await cancelledConnection.value
        XCTAssertFalse(cancelled)
        XCTAssertEqual(model.cloudAction, .idle)
        XCTAssertEqual(model.pairingEndpoint, "wss://")
        XCTAssertEqual(model.pairingCode, "")
        XCTAssertNil(model.pendingPairingAccount)

        let secondRequestCount = await gatewayRequests.requestCount()
        let taskCancelledConnection = Task { await model.connectCloudGateway() }
        let thirdPairRequest = await gatewayRequests.firstRequest(after: secondRequestCount) {
            if case .pair = $0 { return true }
            return false
        }
        let thirdPair = try XCTUnwrap(thirdPairRequest)
        guard case .pair(let thirdCode, _, _) = thirdPair else {
            return XCTFail("Expected the post-reset pairing request")
        }
        XCTAssertEqual(thirdCode, "fresh-code-3")
        XCTAssertEqual(model.cloudAction, .connecting)

        taskCancelledConnection.cancel()
        let taskCancelled = await taskCancelledConnection.value
        XCTAssertFalse(taskCancelled)
        XCTAssertEqual(model.cloudAction, .idle)
        XCTAssertEqual(model.pairingEndpoint, "wss://")
        XCTAssertEqual(model.pairingCode, "")
        XCTAssertNil(model.pendingPairingAccount)

        let thirdRequestCount = await gatewayRequests.requestCount()
        let successfulConnection = Task { await model.connectCloudGateway() }
        let fourthPairRequest = await gatewayRequests.firstRequest(after: thirdRequestCount) {
            if case .pair = $0 { return true }
            return false
        }
        let fourthPair = try XCTUnwrap(fourthPairRequest)
        guard case .pair(let fourthCode, _, _) = fourthPair else {
            return XCTFail("Expected the post-cancellation pairing request")
        }
        XCTAssertEqual(fourthCode, "fresh-code-4")
        XCTAssertEqual(model.cloudAction, .connecting)

        model.handle(.paired(clientID: "cloud-client", token: "gateway-token"))
        let succeeded = await successfulConnection.value
        XCTAssertTrue(succeeded)
        XCTAssertEqual(model.cloudAction, .idle)
        XCTAssertEqual(model.cloudAccount?.subscriptionStartedAt, subscriptionStartedAt)
        XCTAssertEqual(
            requests.dropFirst().map { "\($0.httpMethod ?? "") \($0.url?.path ?? "")" },
            [
                "GET /api/mobile/account",
                "GET /api/mobile/gateway",
                "POST /api/mobile/gateway",
                "GET /api/mobile/account",
                "GET /api/mobile/gateway",
                "POST /api/mobile/gateway",
                "GET /api/mobile/account",
                "GET /api/mobile/gateway",
                "POST /api/mobile/gateway",
                "GET /api/mobile/account",
                "GET /api/mobile/gateway",
                "POST /api/mobile/gateway",
            ]
        )
        try await gatewayStore.remove(try XCTUnwrap(model.accounts.first))
    }

    func testCloudAccountRefreshCanRetryAfterTransientFailure() async throws {
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        let responses = [
            (200, #"{"token":"\#(token)","userId":"00000000-0000-0000-0000-000000000001","expiresAt":"2099-01-01T00:00:00Z"}"#),
            (503, #"{}"#),
            (200, #"{"email":"private@privaterelay.appleid.com","subscribed":false,"sharesDiagnostics":false}"#),
        ]
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            let response = responses[requests.count - 1]
            return try self.response(for: request, status: response.0, json: response.1)
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            requestSender: { _ in },
            connectionOpener: { _ in AsyncThrowingStream { _ in } },
            cloudClient: client
        )

        await model.refreshCloudAccount()
        XCTAssertNil(model.cloudAccount)
        XCTAssertNotNil(model.cloudError)

        await model.refreshCloudAccount()
        XCTAssertEqual(
            model.cloudAccount,
            MobiusCloudAccount(
                email: "private@privaterelay.appleid.com",
                subscribed: false,
                sharesDiagnostics: false
            )
        )
        XCTAssertNil(model.cloudError)
        XCTAssertEqual(requests.map { $0.url?.path }, [
            "/api/mobile/auth/apple",
            "/api/mobile/account",
            "/api/mobile/account",
        ])
    }

    func testCloudDiagnosticsChangesOnlyAfterServerAcceptsUpdate() async throws {
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        let responses = [
            (200, #"{"token":"\#(token)","userId":"00000000-0000-0000-0000-000000000001","expiresAt":"2099-01-01T00:00:00Z"}"#),
            (200, #"{"email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false,"luna":{"creditMicrousd":2400000,"remainingMicrousd":1992000,"resetsAt":"2099-02-01T00:00:00Z"}}"#),
            (503, #"{}"#),
            (204, #"{}"#),
        ]
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            let response = responses[requests.count - 1]
            return try self.response(for: request, status: response.0, json: response.1)
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            requestSender: { _ in },
            connectionOpener: { _ in AsyncThrowingStream { _ in } },
            cloudClient: client
        )

        await model.refreshCloudAccount()
        XCTAssertEqual(model.cloudAccount?.sharesDiagnostics, false)
        let usageLimit = try XCTUnwrap(model.cloudAccount?.luna)

        await model.setCloudSharesDiagnostics(true)
        XCTAssertEqual(model.cloudAccount?.sharesDiagnostics, false)
        XCTAssertNotNil(model.cloudError)

        await model.setCloudSharesDiagnostics(true)
        XCTAssertEqual(model.cloudAccount?.sharesDiagnostics, true)
        XCTAssertEqual(model.cloudAccount?.luna, usageLimit)
        XCTAssertNil(model.cloudError)
        XCTAssertFalse(model.isUpdatingCloudDiagnostics)
        XCTAssertEqual(
            requests.dropFirst().map { "\($0.httpMethod ?? "") \($0.url?.path ?? "")" },
            [
                "GET /api/mobile/account",
                "PUT /api/mobile/account",
                "PUT /api/mobile/account",
            ]
        )
        for request in requests.suffix(2) {
            XCTAssertEqual(request.value(forHTTPHeaderField: "Authorization"), "Bearer \(token)")
            let body = try XCTUnwrap(request.httpBody)
            let json = try XCTUnwrap(
                JSONSerialization.jsonObject(with: body) as? [String: Bool]
            )
            XCTAssertEqual(json, ["sharesDiagnostics": true])
        }
    }

    func testExtensionCatalogUsesAuthenticatedGenericCloudContract() async throws {
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let store = MobiusCloudSessionStore(service: service)
        defer { try? store.remove() }
        var requests: [URLRequest] = []
        let responses = [
            #"{"token":"\#(token)","userId":"00000000-0000-0000-0000-000000000001","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"extensions":[{"id":"ponytail","name":"Ponytail","description":"Prefer the smallest correct implementation.","source":{"url":"https://github.com/DietrichGebert/ponytail.git","reference":"v4.9.0"}}]}"#,
        ]
        let client = MobiusCloudClient(store: store) { request in
            requests.append(request)
            return try self.response(for: request, json: responses[requests.count - 1])
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        let catalog = try await client.extensionCatalog()

        XCTAssertEqual(catalog, [MobiusCloudExtensionCatalogItem(
            id: "ponytail",
            name: "Ponytail",
            description: "Prefer the smallest correct implementation.",
            source: MobiusCloudExtensionSource(
                url: "https://github.com/DietrichGebert/ponytail.git",
                reference: "v4.9.0",
                subdirectory: nil
            )
        )])
        XCTAssertEqual(requests.last?.url?.path, "/api/mobile/extensions/catalog")
        XCTAssertEqual(requests.last?.httpMethod, "GET")
        XCTAssertEqual(
            requests.last?.value(forHTTPHeaderField: "Authorization"),
            "Bearer \(token)"
        )
    }

    func testCloudPairingGrantRejectsUnsafeGatewayFields() async throws {
        let store = MobiusCloudSessionStore(service: "app.mobius.cloud.tests.\(UUID())")
        defer { try? store.remove() }
        var requestCount = 0
        let client = MobiusCloudClient(store: store) { request in
            requestCount += 1
            let json = requestCount == 1
                ? #"{"token":"ttttttttttttttttttttttttttttttttttttttttttt","userId":"00000000-0000-0000-0000-000000000001","expiresAt":"2099-01-01T00:00:00Z"}"#
                : #"{"endpoint":"tcp://gateway.example:8741","pairingCode":"code","expiresAt":"2099-01-01T00:00:00Z"}"#
            return try self.response(for: request, json: json)
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        do {
            _ = try await client.createPairingGrant()
            XCTFail("Expected unsafe pairing fields to be rejected")
        } catch {
            XCTAssertTrue(error is MobiusCloudError)
        }
    }

    private func response(
        for request: URLRequest,
        status: Int = 200,
        json: String
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

    private func keychainAttributes(service: String) throws -> [String: Any] {
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: service,
            kSecAttrAccount: "mobile-session",
            kSecReturnAttributes: true,
            kSecMatchLimit: kSecMatchLimitOne,
        ]
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        XCTAssertEqual(status, errSecSuccess)
        return try XCTUnwrap(result as? [String: Any])
    }
}
