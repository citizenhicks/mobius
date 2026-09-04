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
            userID: UUID(),
            email: "private@privaterelay.appleid.com",
            subscribed: true,
            sharesDiagnostics: false
        )
        XCTAssertFalse(model.isLoadingCloudAccount)

        model.reportCloud(MobiusCloudPurchaseError.unavailable)
        XCTAssertEqual(model.cloudError, "The App Store purchase could not be completed.")
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
        let subscriptionStartedAt = try XCTUnwrap(
            ISO8601DateFormatter().date(from: "2026-08-24T00:00:00Z")
        )
        var requests: [URLRequest] = []
        let responses = [
            #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"userId":"\#(userID.uuidString)","email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":true,"subscriptionStartedAt":"2026-08-24T00:00:00Z","luna":{"creditMicrousd":2400000,"remainingMicrousd":1992000,"resetsAt":"2099-02-01T00:00:00Z"}}"#,
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
        try await client.submitSubscription(
            jws: "header.payload.signature",
            appTransactionJWS: "app.header.signature"
        )
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
                userID: userID,
                email: "private@privaterelay.appleid.com",
                subscribed: true,
                sharesDiagnostics: true,
                subscriptionStartedAt: subscriptionStartedAt,
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
        let subscriptionBody = try XCTUnwrap(requests[3].httpBody)
        let subscriptionJSON = try XCTUnwrap(
            JSONSerialization.jsonObject(with: subscriptionBody) as? [String: String]
        )
        XCTAssertEqual(subscriptionJSON, [
            "jws": "header.payload.signature",
            "appTransactionJws": "app.header.signature",
        ])

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
            XCTFail("Expected rejected account deletion")
        } catch let error as MobiusCloudError {
            guard case .server(409) = error else {
                return XCTFail("Expected server(409), got \(error)")
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

    func testAuthenticationExplainsPendingAccountDeletion() async throws {
        let store = MobiusCloudSessionStore(service: "app.mobius.cloud.tests.\(UUID())")
        defer { try? store.remove() }
        let client = MobiusCloudClient(store: store) { request in
            try self.response(for: request, status: 409, json: #"{}"#)
        }

        do {
            _ = try await client.authenticate(
                authorizationCode: "apple-code",
                nonce: String(repeating: "n", count: 43)
            )
            XCTFail("Expected account deletion to block sign-in")
        } catch let error as MobiusCloudError {
            guard case .accountDeletionPending = error else {
                return XCTFail("Expected accountDeletionPending, got \(error)")
            }
            XCTAssertEqual(
                error.localizedDescription,
                "Your previous Cloud account is still being deleted. Try again shortly."
            )
        }
    }

    func testAuthenticationDoesNotDescribeARejectedAppleCodeAsAnExpiredSession() async throws {
        let store = MobiusCloudSessionStore(service: "app.mobius.cloud.tests.\(UUID())")
        defer { try? store.remove() }
        let client = MobiusCloudClient(store: store) { request in
            try self.response(for: request, status: 401, json: #"{}"#)
        }

        do {
            _ = try await client.authenticate(
                authorizationCode: "rejected-code",
                nonce: String(repeating: "n", count: 43)
            )
            XCTFail("Expected rejected Apple authorization")
        } catch let error as MobiusCloudError {
            guard case .invalidAuthorization = error else {
                return XCTFail("Expected invalidAuthorization, got \(error)")
            }
            XCTAssertEqual(error.localizedDescription, "Apple sign-in could not be completed.")
        }
    }

    func testSubscriptionConflictRequiresExactServerCode() async throws {
        let service = "app.mobius.cloud.tests.\(UUID())"
        let store = MobiusCloudSessionStore(service: service)
        defer { try? store.remove() }
        var requestCount = 0
        var errorCode = "something_else"
        let client = MobiusCloudClient(store: store) { request in
            requestCount += 1
            if requestCount == 1 {
                return try self.response(
                    for: request,
                    json: #"{"token":"ttttttttttttttttttttttttttttttttttttttttttt","userId":"00000000-0000-0000-0000-000000000001","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            }
            return try self.response(
                for: request,
                status: 409,
                json: #"{"error":"\#(errorCode)"}"#
            )
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        do {
            try await client.submitSubscription(
                jws: "header.payload.signature",
                appTransactionJWS: "app.header.signature"
            )
            XCTFail("Expected a generic conflict response")
        } catch let error as MobiusCloudError {
            guard case .server(409) = error else {
                return XCTFail("Expected server(409), got \(error)")
            }
        }

        errorCode = "subscription_account_conflict"
        do {
            try await client.submitSubscription(
                jws: "header.payload.signature",
                appTransactionJWS: "app.header.signature"
            )
            XCTFail("Expected a subscription ownership conflict")
        } catch let error as MobiusCloudError {
            guard case .subscriptionAccountConflict = error else {
                return XCTFail("Expected subscriptionAccountConflict, got \(error)")
            }
        }
    }

    func testUnauthorizedAccountDeletionClearsCloudSession() async throws {
        let userID = try XCTUnwrap(UUID(
            uuidString: "00000000-0000-0000-0000-000000000001"
        ))
        let service = "app.mobius.cloud.tests.\(UUID())"
        let store = MobiusCloudSessionStore(service: service)
        defer { try? store.remove() }
        let client = MobiusCloudClient(store: store) { request in
            switch (request.url?.path, request.httpMethod) {
            case ("/api/mobile/auth/apple", "POST"):
                return try self.response(
                    for: request,
                    json: #"{"token":"ttttttttttttttttttttttttttttttttttttttttttt","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            case ("/api/mobile/account", "GET"):
                return try self.response(
                    for: request,
                    json: #"{"userId":"\#(userID.uuidString)","email":null,"subscribed":false,"sharesDiagnostics":false}"#
                )
            default:
                return try self.response(for: request, status: 401, json: #"{}"#)
            }
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
            cloudClient: client,
            cloudPurchases: MobiusCloudPurchases(
                displayPrice: { "$9.99" },
                unfinishedPurchases: { MobiusCloudPurchaseScan() },
                currentEntitlements: { synchronize in
                    XCTAssertTrue(synchronize)
                    return MobiusCloudPurchaseScan()
                },
                purchase: { _ in throw MobiusCloudPurchaseError.unavailable }
            )
        )
        model.cloudAccount = MobiusCloudAccount(
            userID: userID,
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
        var requests: [URLRequest] = []
        let client = MobiusCloudClient(store: store) { request in
            requests.append(request)
            if request.url?.path == "/api/mobile/auth/apple" {
                return try self.response(
                    for: request,
                    json: #"{"token":"ttttttttttttttttttttttttttttttttttttttttttt","userId":"\#(currentUserID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            }
            if request.httpMethod == "GET" {
                return try self.response(
                    for: request,
                    json: #"{"userId":"\#(currentUserID.uuidString)","email":"private@privaterelay.appleid.com","subscribed":false,"sharesDiagnostics":false}"#
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
            cloudClient: client,
            cloudPurchases: MobiusCloudPurchases(
                displayPrice: { "$9.99" },
                unfinishedPurchases: { MobiusCloudPurchaseScan() },
                currentEntitlements: { synchronize in
                    XCTAssertTrue(synchronize)
                    return MobiusCloudPurchaseScan()
                },
                purchase: { _ in throw MobiusCloudPurchaseError.unavailable }
            )
        )
        model.cloudAccount = MobiusCloudAccount(
            userID: currentUserID,
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
        XCTAssertEqual(
            requests.map { ($0.url?.path ?? "") + ":" + ($0.httpMethod ?? "") },
            [
                "/api/mobile/auth/apple:POST",
                "/api/mobile/account:DELETE",
            ]
        )
        XCTAssertEqual(model.accounts.map(\.id), [retainedGateway.id])
        XCTAssertEqual(gatewayStore.loadAccounts().map(\.id), [retainedGateway.id])
        XCTAssertNoThrow(try gatewayStore.token(for: retainedGateway))
        XCTAssertThrowsError(try gatewayStore.token(for: deletedGateway))
        try await gatewayStore.remove(retainedGateway)
    }

    func testAccountDeletionDoesNotScanStoreKitOrBlockActiveSubscription() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            switch (request.url?.path, request.httpMethod) {
            case ("/api/mobile/auth/apple", "POST"):
                return try self.response(
                    for: request,
                    json: #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            case ("/api/mobile/account", "DELETE"):
                return try self.response(for: request, json: #"{}"#)
            default:
                return try self.response(for: request, status: 500, json: #"{}"#)
            }
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )
        var readCurrentEntitlements = false
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: client,
            cloudPurchases: MobiusCloudPurchases(
                displayPrice: { "$9.99" },
                unfinishedPurchases: { MobiusCloudPurchaseScan() },
                currentEntitlements: { _ in
                    readCurrentEntitlements = true
                    return MobiusCloudPurchaseScan()
                },
                purchase: { _ in throw MobiusCloudPurchaseError.unavailable }
            )
        )
        model.cloudAccount = MobiusCloudAccount(
            userID: userID,
            email: nil,
            subscribed: true,
            sharesDiagnostics: false,
            subscriptionStartedAt: .now
        )

        let deleted = await model.deleteCloudAccount(
            authorizationCode: "delete-code",
            nonce: String(repeating: "d", count: 43)
        )

        XCTAssertTrue(deleted)
        XCTAssertFalse(readCurrentEntitlements)
        XCTAssertNil(model.cloudSession)
        XCTAssertNil(model.cloudAccount)
        XCTAssertNil(try client.loadSession())
        XCTAssertEqual(requests.map { ($0.url?.path ?? "") + ":" + ($0.httpMethod ?? "") }, [
            "/api/mobile/auth/apple:POST",
            "/api/mobile/account:DELETE",
        ])
    }

    func testCloudAccountRejectsInvalidEmail() async throws {
        let store = MobiusCloudSessionStore(service: "app.mobius.cloud.tests.\(UUID())")
        defer { try? store.remove() }
        var requestCount = 0
        let client = MobiusCloudClient(store: store) { request in
            requestCount += 1
            let json = requestCount == 1
                ? #"{"token":"ttttttttttttttttttttttttttttttttttttttttttt","userId":"00000000-0000-0000-0000-000000000001","expiresAt":"2099-01-01T00:00:00Z"}"#
                : #"{"userId":"00000000-0000-0000-0000-000000000001","email":"not-an-email","subscribed":true,"sharesDiagnostics":false,"subscriptionStartedAt":"2026-08-24T00:00:00Z"}"#
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
                : #"{"userId":"00000000-0000-0000-0000-000000000001","email":null,"subscribed":true,"sharesDiagnostics":false,"subscriptionStartedAt":"2026-08-24T00:00:00Z","luna":{"creditMicrousd":2400000,"remainingMicrousd":2400001,"resetsAt":"2099-02-01T00:00:00Z"}}"#
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

    func testCloudAccountRequiresSubscriptionStartToMatchStatus() async throws {
        let userID = "00000000-0000-0000-0000-000000000001"
        let accountResponses = [
            #"{"userId":"00000000-0000-0000-0000-000000000001","email":null,"subscribed":true,"sharesDiagnostics":false}"#,
            #"{"userId":"00000000-0000-0000-0000-000000000001","email":null,"subscribed":false,"sharesDiagnostics":false,"subscriptionStartedAt":"2026-08-24T00:00:00Z"}"#,
        ]

        for accountResponse in accountResponses {
            let store = MobiusCloudSessionStore(service: "app.mobius.cloud.tests.\(UUID())")
            defer { try? store.remove() }
            var requestCount = 0
            let client = MobiusCloudClient(store: store) { request in
                requestCount += 1
                let json = requestCount == 1
                    ? #"{"token":"ttttttttttttttttttttttttttttttttttttttttttt","userId":"\#(userID)","expiresAt":"2099-01-01T00:00:00Z"}"#
                    : accountResponse
                return try self.response(for: request, json: json)
            }
            _ = try await client.authenticate(
                authorizationCode: "apple-code",
                nonce: String(repeating: "n", count: 43)
            )

            do {
                _ = try await client.account()
                XCTFail("Expected inconsistent subscription dates to be rejected")
            } catch let error as MobiusCloudError {
                guard case .invalidAccountResponse = error else {
                    return XCTFail("Expected invalidAccountResponse, got \(error)")
                }
            }
        }
    }

    func testSubscriptionRejectsJWSLargerThanBackendLimit() async throws {
        let client = MobiusCloudClient(
            store: MobiusCloudSessionStore(service: "app.mobius.cloud.tests.\(UUID())")
        ) { _ in
            XCTFail("Oversized JWS must be rejected before transport")
            throw URLError(.badURL)
        }
        let oversizedJWS = "a.\(String(repeating: "b", count: 16 * 1024)).c"

        for (jws, appTransactionJWS) in [
            (oversizedJWS, "app.header.signature"),
            ("header.payload.signature", oversizedJWS),
        ] {
            do {
                try await client.submitSubscription(
                    jws: jws,
                    appTransactionJWS: appTransactionJWS
                )
                XCTFail("Expected oversized JWS to be rejected")
            } catch let error as MobiusCloudError {
                guard case .invalidPurchaseJWS = error else {
                    return XCTFail("Expected invalidPurchaseJWS, got \(error)")
                }
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
            cloudClient: client,
            cloudPurchases: emptyCloudPurchases()
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

    func testAccountRefreshRejectsServerUserIDChangeWithoutRetaggingGateway() async throws {
        let sessionUserID = try XCTUnwrap(UUID(
            uuidString: "00000000-0000-0000-0000-000000000001"
        ))
        let otherUserID = try XCTUnwrap(UUID(
            uuidString: "00000000-0000-0000-0000-000000000002"
        ))
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        let responses = [
            #"{"token":"\#(token)","userId":"\#(sessionUserID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"userId":"\#(otherUserID.uuidString)","email":null,"subscribed":false,"sharesDiagnostics":false}"#,
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
        let gateway = GatewayAccount(
            endpoint: try GatewayEndpoint("wss://gateway.example"),
            cloudUserID: sessionUserID
        )
        try gatewayStore.save(gateway, token: "gateway-token")
        let model = AppModel(
            store: gatewayStore,
            settingsDefaults: defaults,
            cloudClient: client,
            cloudPurchases: emptyCloudPurchases()
        )

        await model.refreshCloudAccount()

        XCTAssertNil(model.cloudSession)
        XCTAssertNil(try client.loadSession())
        XCTAssertNil(model.cloudAccount)
        XCTAssertEqual(model.accounts.first?.cloudUserID, sessionUserID)
        XCTAssertEqual(gatewayStore.loadAccounts().first?.cloudUserID, sessionUserID)
        XCTAssertEqual(model.cloudError, MobiusCloudError.accountIdentityMismatch.localizedDescription)
        XCTAssertEqual(requests.map { $0.url?.path }, [
            "/api/mobile/auth/apple",
            "/api/mobile/account",
        ])
        try await gatewayStore.remove(try XCTUnwrap(model.accounts.first))
    }

    func testTransactionUpdateSubmitsBeforeFinishing() async throws {
        let userID = try XCTUnwrap(UUID(
            uuidString: "00000000-0000-0000-0000-000000000001"
        ))
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        var accountRequests = 0
        var backendAccepted = false
        var finished = false
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            switch request.url?.path {
            case "/api/mobile/auth/apple":
                return try self.response(
                    for: request,
                    json: #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            case "/api/mobile/account":
                accountRequests += 1
                let subscribed = accountRequests > 1
                let startedAt = subscribed
                    ? #", "subscriptionStartedAt":"2026-08-24T00:00:00Z""#
                    : ""
                return try self.response(
                    for: request,
                    json: #"{"userId":"\#(userID.uuidString)","email":null,"subscribed":\#(subscribed),"sharesDiagnostics":false\#(startedAt)}"#
                )
            case "/api/mobile/subscription":
                XCTAssertFalse(finished)
                backendAccepted = true
                return try self.response(for: request, json: #"{}"#)
            default:
                return try self.response(for: request, status: 500, json: #"{}"#)
            }
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        let transactionFinished = expectation(description: "Transaction finished")
        let purchase = MobiusCloudPurchase(
            jws: "header.payload.signature",
            appTransactionJWS: "app.header.signature"
        ) {
            XCTAssertTrue(backendAccepted)
            finished = true
            transactionFinished.fulfill()
        }
        let stream = AsyncStream.makeStream(of: MobiusCloudPurchase.self)
        let purchases = MobiusCloudPurchases(
            displayPrice: { "$9.99" },
            unfinishedPurchases: { MobiusCloudPurchaseScan() },
            currentEntitlements: { _ in MobiusCloudPurchaseScan() },
            purchase: { _ in throw MobiusCloudPurchaseError.unavailable },
            updates: { stream.stream }
        )
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: client,
            cloudPurchases: purchases
        )

        stream.continuation.yield(purchase)
        await fulfillment(of: [transactionFinished], timeout: 1)

        let accountUpdated = await eventually { model.cloudAccount?.subscribed == true }
        XCTAssertTrue(accountUpdated)
        XCTAssertTrue(finished)
        XCTAssertEqual(model.cloudSession?.userID, userID)
        XCTAssertEqual(requests.map { $0.url?.path }, [
            "/api/mobile/auth/apple",
            "/api/mobile/account",
            "/api/mobile/subscription",
            "/api/mobile/account",
        ])
    }

    func testStaleTransactionUpdateCannotUseOrClearNewSession() async throws {
        let firstUserID = UUID()
        let secondUserID = UUID()
        let firstToken = String(repeating: "a", count: 43)
        let secondToken = String(repeating: "b", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        let subscriptionStarted = expectation(description: "Subscription request started")
        var blockedResponse: (Data, HTTPURLResponse)?
        var blockedContinuation: CheckedContinuation<(Data, HTTPURLResponse), Never>?
        var authenticationCount = 0
        var subscriptionBearer: String?
        let client = MobiusCloudClient(store: sessionStore) { request in
            switch request.url?.path {
            case "/api/mobile/auth/apple":
                authenticationCount += 1
                let userID = authenticationCount == 1 ? firstUserID : secondUserID
                let token = authenticationCount == 1 ? firstToken : secondToken
                return try self.response(
                    for: request,
                    json: #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            case "/api/mobile/account":
                return try self.response(
                    for: request,
                    json: #"{"userId":"\#(firstUserID.uuidString)","email":null,"subscribed":false,"sharesDiagnostics":false}"#
                )
            case "/api/mobile/subscription":
                subscriptionBearer = request.value(forHTTPHeaderField: "Authorization")
                blockedResponse = try self.response(
                    for: request,
                    status: 401,
                    json: #"{}"#
                )
                subscriptionStarted.fulfill()
                return await withCheckedContinuation { blockedContinuation = $0 }
            default:
                return try self.response(for: request, status: 500, json: #"{}"#)
            }
        }
        _ = try await client.authenticate(
            authorizationCode: "first-code",
            nonce: String(repeating: "n", count: 43)
        )

        var finished = false
        let stream = AsyncStream.makeStream(of: MobiusCloudPurchase.self)
        let purchases = MobiusCloudPurchases(
            displayPrice: { "$9.99" },
            unfinishedPurchases: { MobiusCloudPurchaseScan() },
            currentEntitlements: { _ in MobiusCloudPurchaseScan() },
            purchase: { _ in throw MobiusCloudPurchaseError.unavailable },
            updates: { stream.stream }
        )
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: client,
            cloudPurchases: purchases
        )

        stream.continuation.yield(MobiusCloudPurchase(
            jws: "header.payload.signature",
            appTransactionJWS: "app.header.signature"
        ) {
            finished = true
        })
        await fulfillment(of: [subscriptionStarted], timeout: 1)
        let secondSession = try await client.authenticate(
            authorizationCode: "second-code",
            nonce: String(repeating: "m", count: 43)
        )
        model.cloudSession = secondSession
        blockedContinuation?.resume(returning: try XCTUnwrap(blockedResponse))
        let staleTaskFinished = await eventually { model.cloudPurchaseTasks.isEmpty }
        XCTAssertTrue(staleTaskFinished)

        XCTAssertEqual(subscriptionBearer, "Bearer \(firstToken)")
        XCTAssertFalse(finished)
        XCTAssertEqual(model.cloudSession, secondSession)
        XCTAssertEqual(try client.loadSession(), secondSession)
        XCTAssertNil(model.cloudError)
    }

    func testAccountRefreshFinishesUnfinishedPurchaseAfterBackendAcceptsIt() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        var backendAccepted = false
        var finished = false
        let responses = [
            #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"userId":"\#(userID.uuidString)","email":null,"subscribed":true,"sharesDiagnostics":false,"subscriptionStartedAt":"2026-08-24T00:00:00Z"}"#,
            #"{}"#,
            #"{"userId":"\#(userID.uuidString)","email":null,"subscribed":true,"sharesDiagnostics":false,"subscriptionStartedAt":"2026-08-24T00:00:00Z"}"#,
        ]
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            if request.url?.path == "/api/mobile/subscription" {
                XCTAssertFalse(finished)
                backendAccepted = true
            }
            return try self.response(for: request, json: responses[requests.count - 1])
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        let purchase = MobiusCloudPurchase(
            jws: "header.payload.signature",
            appTransactionJWS: "app.header.signature"
        ) {
            XCTAssertTrue(backendAccepted)
            finished = true
        }
        let purchases = MobiusCloudPurchases(
            displayPrice: { "$9.99" },
            unfinishedPurchases: { MobiusCloudPurchaseScan(purchases: [purchase]) },
            currentEntitlements: { _ in
                XCTFail("A subscribed account must not scan current entitlements")
                return MobiusCloudPurchaseScan()
            },
            purchase: { _ in throw MobiusCloudPurchaseError.unavailable }
        )
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: client,
            cloudPurchases: purchases
        )

        await model.refreshCloudAccount()

        XCTAssertTrue(finished)
        XCTAssertEqual(model.cloudAccount?.subscribed, true)
        XCTAssertEqual(requests.map { $0.url?.path }, [
            "/api/mobile/auth/apple",
            "/api/mobile/account",
            "/api/mobile/subscription",
            "/api/mobile/account",
        ])
    }

    func testRejectedPurchaseDoesNotStarveLaterVerifiedEntitlement() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var accountRequests = 0
        var submittedJWS: [String] = []
        let client = MobiusCloudClient(store: sessionStore) { request in
            switch request.url?.path {
            case "/api/mobile/auth/apple":
                return try self.response(
                    for: request,
                    json: #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            case "/api/mobile/account":
                accountRequests += 1
                let subscribed = accountRequests > 1
                let startedAt = subscribed
                    ? #", "subscriptionStartedAt":"2026-08-24T00:00:00Z""#
                    : ""
                return try self.response(
                    for: request,
                    json: #"{"userId":"\#(userID.uuidString)","email":null,"subscribed":\#(subscribed),"sharesDiagnostics":false\#(startedAt)}"#
                )
            case "/api/mobile/subscription":
                let body = try XCTUnwrap(request.httpBody)
                let value = try XCTUnwrap(
                    (JSONSerialization.jsonObject(with: body) as? [String: String])?["jws"]
                )
                submittedJWS.append(value)
                return try self.response(
                    for: request,
                    status: value.hasPrefix("bad.") ? 400 : 200,
                    json: #"{}"#
                )
            default:
                return try self.response(for: request, status: 500, json: #"{}"#)
            }
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        var rejectedFinished = false
        var acceptedFinished = false
        let rejected = MobiusCloudPurchase(
            jws: "bad.payload.signature",
            appTransactionJWS: "app.header.signature"
        ) {
            rejectedFinished = true
        }
        let accepted = MobiusCloudPurchase(
            jws: "good.payload.signature",
            appTransactionJWS: "app.header.signature"
        ) {
            acceptedFinished = true
        }
        let purchases = MobiusCloudPurchases(
            displayPrice: { "$9.99" },
            unfinishedPurchases: {
                MobiusCloudPurchaseScan(
                    purchases: [rejected],
                    hasUnverifiedPurchase: true
                )
            },
            currentEntitlements: { _ in
                MobiusCloudPurchaseScan(purchases: [accepted])
            },
            purchase: { _ in throw MobiusCloudPurchaseError.unavailable }
        )
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: client,
            cloudPurchases: purchases
        )

        await model.refreshCloudAccount()

        XCTAssertEqual(submittedJWS, ["bad.payload.signature", "good.payload.signature"])
        XCTAssertFalse(rejectedFinished)
        XCTAssertTrue(acceptedFinished)
        XCTAssertEqual(model.cloudAccount?.subscribed, true)
        XCTAssertNil(model.cloudError)
    }

    func testSubscriptionConflictIsActionableAndLeavesTransactionUnfinished() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            switch request.url?.path {
            case "/api/mobile/auth/apple":
                return try self.response(
                    for: request,
                    json: #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            case "/api/mobile/account":
                return try self.response(
                    for: request,
                    json: #"{"userId":"\#(userID.uuidString)","email":null,"subscribed":false,"sharesDiagnostics":false}"#
                )
            case "/api/mobile/subscription":
                return try self.response(
                    for: request,
                    status: 409,
                    json: #"{"error":"subscription_account_conflict"}"#
                )
            default:
                return try self.response(for: request, status: 500, json: #"{}"#)
            }
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        var finished = false
        let purchase = MobiusCloudPurchase(
            jws: "header.payload.signature",
            appTransactionJWS: "app.header.signature"
        ) {
            finished = true
        }
        var purchaseAttempts = 0
        let purchases = MobiusCloudPurchases(
            displayPrice: { "$9.99" },
            unfinishedPurchases: { MobiusCloudPurchaseScan() },
            currentEntitlements: { synchronize in
                XCTAssertFalse(synchronize)
                return MobiusCloudPurchaseScan(purchases: [purchase])
            },
            purchase: { _ in
                purchaseAttempts += 1
                throw MobiusCloudPurchaseError.unavailable
            }
        )
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: client,
            cloudPurchases: purchases
        )

        await model.refreshCloudAccount()

        XCTAssertFalse(finished)
        XCTAssertNotNil(model.cloudSession)
        XCTAssertEqual(
            model.cloudError,
            MobiusCloudError.subscriptionAccountConflict.localizedDescription
        )
        XCTAssertEqual(model.cloudIssue, .subscriptionAccountConflict)
        let retried = await model.purchaseCloud()
        XCTAssertFalse(retried)
        XCTAssertEqual(purchaseAttempts, 0)
        XCTAssertEqual(requests.map { $0.url?.path }, [
            "/api/mobile/auth/apple",
            "/api/mobile/account",
            "/api/mobile/subscription",
            "/api/mobile/account",
        ])
    }

    func testConcurrentAccountRefreshesSubmitAndFinishPurchaseOnce() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var subscriptionRequests = 0
        let client = MobiusCloudClient(store: sessionStore) { request in
            let json: String
            switch request.url?.path {
            case "/api/mobile/auth/apple":
                json = #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
            case "/api/mobile/subscription":
                subscriptionRequests += 1
                json = #"{}"#
            case "/api/mobile/account":
                json = #"{"userId":"\#(userID.uuidString)","email":null,"subscribed":true,"sharesDiagnostics":false,"subscriptionStartedAt":"2026-08-24T00:00:00Z"}"#
            default:
                return try self.response(for: request, status: 500, json: #"{}"#)
            }
            return try self.response(for: request, json: json)
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        let finishStarted = expectation(description: "First refresh started finishing")
        let secondScan = expectation(description: "Second refresh found the purchase")
        var finishContinuation: CheckedContinuation<Void, Never>?
        var finishCount = 0
        var scanCount = 0
        let purchase = MobiusCloudPurchase(
            jws: "header.payload.signature",
            appTransactionJWS: "app.header.signature"
        ) {
            finishCount += 1
            finishStarted.fulfill()
            await withCheckedContinuation { finishContinuation = $0 }
        }
        let purchases = MobiusCloudPurchases(
            displayPrice: { "$9.99" },
            unfinishedPurchases: {
                scanCount += 1
                if scanCount == 2 { secondScan.fulfill() }
                return MobiusCloudPurchaseScan(purchases: [purchase])
            },
            currentEntitlements: { _ in
                XCTFail("A subscribed account must not scan current entitlements")
                return MobiusCloudPurchaseScan()
            },
            purchase: { _ in throw MobiusCloudPurchaseError.unavailable }
        )
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: client,
            cloudPurchases: purchases
        )

        let firstRefresh = Task { await model.refreshCloudAccount() }
        await fulfillment(of: [finishStarted], timeout: 1)
        let secondRefresh = Task { await model.refreshCloudAccount() }
        await fulfillment(of: [secondScan], timeout: 1)
        finishContinuation?.resume()
        await firstRefresh.value
        await secondRefresh.value

        XCTAssertEqual(subscriptionRequests, 1)
        XCTAssertEqual(finishCount, 1)
        XCTAssertEqual(model.cloudAccount?.subscribed, true)
    }

    func testPurchaseUsesAccountUserIDButDoesNotOverrideServerSubscription() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        var backendAccepted = false
        var finished = false
        var purchaseStarted = false
        let responses = [
            #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"userId":"\#(userID.uuidString)","email":null,"subscribed":false,"sharesDiagnostics":false}"#,
            #"{}"#,
            #"{"userId":"\#(userID.uuidString)","email":null,"subscribed":false,"sharesDiagnostics":false}"#,
        ]
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            if request.url?.path == "/api/mobile/subscription", request.httpMethod == "PUT" {
                XCTAssertTrue(purchaseStarted)
                XCTAssertFalse(finished)
                backendAccepted = true
            }
            return try self.response(for: request, json: responses[requests.count - 1])
        }
        var purchaseUserID: UUID?
        var queriedActivePurchases = false
        let purchase = MobiusCloudPurchase(
            jws: "header.payload.signature",
            appTransactionJWS: "app.header.signature"
        ) {
            XCTAssertTrue(backendAccepted)
            finished = true
        }
        let purchases = MobiusCloudPurchases(
            displayPrice: { "$9.99" },
            unfinishedPurchases: { MobiusCloudPurchaseScan() },
            currentEntitlements: { synchronize in
                XCTAssertFalse(synchronize)
                queriedActivePurchases = true
                return MobiusCloudPurchaseScan()
            },
            purchase: { requestedUserID in
                purchaseStarted = true
                purchaseUserID = requestedUserID
                return purchase
            }
        )
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: client,
            cloudPurchases: purchases
        )

        let connected = await model.signInAndPurchaseCloud(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        XCTAssertFalse(connected)
        XCTAssertTrue(queriedActivePurchases)
        XCTAssertEqual(purchaseUserID, userID)
        XCTAssertTrue(finished)
        XCTAssertEqual(model.cloudAccount?.subscribed, false)
        XCTAssertEqual(
            model.cloudError,
            MobiusCloudError.subscriptionRequired.localizedDescription
        )
        XCTAssertEqual(requests.map { $0.url?.path }, [
            "/api/mobile/auth/apple",
            "/api/mobile/account",
            "/api/mobile/subscription",
            "/api/mobile/account",
        ])
        XCTAssertEqual(requests.map(\.httpMethod), ["POST", "GET", "PUT", "GET"])
        XCTAssertEqual(
            try JSONSerialization.jsonObject(with: try XCTUnwrap(requests[2].httpBody))
                as? [String: String],
            [
                "jws": "header.payload.signature",
                "appTransactionJws": "app.header.signature",
            ]
        )
    }

    func testCancelledStoreKitPurchaseDoesNotSubmitTransaction() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            switch request.url?.path {
            case "/api/mobile/auth/apple":
                return try self.response(
                    for: request,
                    json: #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            case "/api/mobile/account":
                return try self.response(
                    for: request,
                    json: #"{"userId":"\#(userID.uuidString)","email":null,"subscribed":false,"sharesDiagnostics":false}"#
                )
            case "/api/mobile/subscription":
                XCTFail("A cancelled purchase has no transaction to submit")
                return try self.response(for: request, status: 500, json: #"{}"#)
            default:
                return try self.response(for: request, status: 500, json: #"{}"#)
            }
        }
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: client,
            cloudPurchases: MobiusCloudPurchases(
                displayPrice: { "$9.99" },
                unfinishedPurchases: { MobiusCloudPurchaseScan() },
                currentEntitlements: { _ in MobiusCloudPurchaseScan() },
                purchase: { _ in throw MobiusCloudPurchaseError.cancelled }
            )
        )

        let connected = await model.signInAndPurchaseCloud(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        XCTAssertFalse(connected)
        XCTAssertNil(model.cloudError)
        XCTAssertEqual(requests.map { $0.url?.path }, [
            "/api/mobile/auth/apple",
            "/api/mobile/account",
        ])
        XCTAssertEqual(requests.map(\.httpMethod), ["POST", "GET"])
    }

    func testUnresolvedStoreKitPurchaseDoesNotSubmitTransaction() async throws {
        for purchaseError in [MobiusCloudPurchaseError.pending, .unavailable] {
            let userID = UUID()
            let token = String(repeating: "t", count: 43)
            let service = "app.mobius.cloud.tests.\(UUID())"
            let sessionStore = MobiusCloudSessionStore(service: service)
            defer { try? sessionStore.remove() }
            var methods: [String] = []
            let client = MobiusCloudClient(store: sessionStore) { request in
                methods.append(request.httpMethod ?? "")
                if request.url?.path == "/api/mobile/auth/apple" {
                    return try self.response(
                        for: request,
                        json: #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
                    )
                }
                if request.url?.path == "/api/mobile/account" {
                    return try self.response(
                        for: request,
                        json: #"{"userId":"\#(userID.uuidString)","email":null,"subscribed":false,"sharesDiagnostics":false}"#
                    )
                }
                return try self.response(for: request, status: 500, json: "")
            }
            let suiteName = "app.mobius.cloud.tests.\(UUID())"
            let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
            defer { defaults.removePersistentDomain(forName: suiteName) }
            let model = AppModel(
                store: GatewayStore(defaults: defaults),
                settingsDefaults: defaults,
                cloudClient: client,
                cloudPurchases: MobiusCloudPurchases(
                    displayPrice: { "$9.99" },
                    unfinishedPurchases: { MobiusCloudPurchaseScan() },
                    currentEntitlements: { _ in MobiusCloudPurchaseScan() },
                    purchase: { _ in throw purchaseError }
                )
            )

            _ = await model.signInAndPurchaseCloud(
                authorizationCode: "apple-code",
                nonce: String(repeating: "n", count: 43)
            )

            XCTAssertEqual(methods, ["POST", "GET"])
        }
    }

    func testPendingStoreKitPurchaseCanDeleteBeforeTransactionExists() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            switch (request.url?.path, request.httpMethod) {
            case ("/api/mobile/auth/apple", "POST"):
                return try self.response(
                    for: request,
                    json: #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            case ("/api/mobile/account", "GET"):
                return try self.response(
                    for: request,
                    json: #"{"userId":"\#(userID.uuidString)","email":null,"subscribed":false,"sharesDiagnostics":false}"#
                )
            case ("/api/mobile/account", "DELETE"):
                return try self.response(for: request, status: 202, json: "")
            default:
                return try self.response(for: request, status: 500, json: #"{}"#)
            }
        }
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: client,
            cloudPurchases: MobiusCloudPurchases(
                displayPrice: { "$9.99" },
                unfinishedPurchases: { MobiusCloudPurchaseScan() },
                currentEntitlements: { _ in MobiusCloudPurchaseScan() },
                purchase: { _ in throw MobiusCloudPurchaseError.pending }
            )
        )

        _ = await model.signInAndPurchaseCloud(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )
        let deleted = await model.deleteCloudAccount(
            authorizationCode: "delete-code",
            nonce: String(repeating: "d", count: 43)
        )

        XCTAssertTrue(deleted)
        XCTAssertNil(model.cloudSession)
        XCTAssertNil(model.cloudError)
        XCTAssertEqual(
            requests.map { "\($0.httpMethod ?? "") \($0.url?.path ?? "")" },
            [
                "POST /api/mobile/auth/apple",
                "GET /api/mobile/account",
                "DELETE /api/mobile/account",
            ]
        )
    }

    func testSubscribedSignInFinishesUnfinishedTransactionBeforeConnecting() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        var backendAccepted = false
        var finished = false
        let responses = [
            #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"userId":"\#(userID.uuidString)","email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false,"subscriptionStartedAt":"2026-08-24T00:00:00Z"}"#,
            #"{}"#,
            #"{"userId":"\#(userID.uuidString)","email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false,"subscriptionStartedAt":"2026-08-24T00:00:00Z"}"#,
            #"{"status":"ready"}"#,
            #"{"endpoint":"wss://gateway.example","pairingCode":"0123456789abcdef","expiresAt":"2099-01-01T00:00:00Z"}"#,
        ]
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            if request.url?.path == "/api/mobile/subscription" {
                XCTAssertFalse(finished)
                backendAccepted = true
            }
            return try self.response(for: request, json: responses[requests.count - 1])
        }

        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let gatewayStore = GatewayStore(defaults: defaults)
        let gatewayRequests = GatewayRequestRecorder()
        let purchase = MobiusCloudPurchase(
            jws: "header.payload.signature",
            appTransactionJWS: "app.header.signature"
        ) {
            XCTAssertTrue(backendAccepted)
            finished = true
        }
        let purchases = MobiusCloudPurchases(
            displayPrice: { "$9.99" },
            unfinishedPurchases: { MobiusCloudPurchaseScan(purchases: [purchase]) },
            currentEntitlements: { _ in
                XCTFail("A subscribed account must not scan current entitlements")
                return MobiusCloudPurchaseScan()
            },
            purchase: { _ in
                XCTFail("An unfinished transaction must be recovered before buying again")
                throw MobiusCloudPurchaseError.unavailable
            }
        )
        let model = AppModel(
            store: gatewayStore,
            settingsDefaults: defaults,
            requestSender: { request in await gatewayRequests.record(request) },
            connectionOpener: { _ in AsyncThrowingStream { _ in } },
            cloudClient: client,
            cloudPurchases: purchases
        )
        XCTAssertFalse(model.selectedGatewayIsMobiusCloud)

        let connection = Task {
            await model.signInAndPurchaseCloud(
                authorizationCode: "apple-code",
                nonce: String(repeating: "n", count: 43)
            )
        }
        _ = await gatewayRequests.firstRequest(after: 0) {
            if case .pair = $0 { return true }
            return false
        }
        model.handle(.paired(clientID: "cloud-client", token: "gateway-token"))
        let connected = await connection.value
        XCTAssertTrue(connected)
        XCTAssertTrue(finished)
        XCTAssertEqual(requests.map { $0.url?.path }, [
            "/api/mobile/auth/apple",
            "/api/mobile/account",
            "/api/mobile/subscription",
            "/api/mobile/account",
            "/api/mobile/gateway",
            "/api/mobile/gateway",
        ])
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

    func testUntaggedGatewayIsNotTreatedAsCloud() async throws {
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
            return try self.response(
                for: request,
                json: #"{"token":"\#(token)","userId":"00000000-0000-0000-0000-000000000001","expiresAt":"2099-01-01T00:00:00Z"}"#
            )
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )
        let model = AppModel(
            store: gatewayStore,
            settingsDefaults: defaults,
            cloudClient: client
        )

        XCTAssertNil(model.mobiusCloudGateway)
        XCTAssertFalse(model.selectedGatewayIsMobiusCloud)
        XCTAssertEqual(model.selectedAccount, account)
        XCTAssertEqual(gatewayStore.loadAccounts(), [account])
        try await gatewayStore.remove(account)
    }

    func testCloudSignOutClearsLocalAuthenticationWhenPushRemovalFails() async throws {
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
            machineName: mobiusCloudGatewayDisplayName,
            cloudUserID: try XCTUnwrap(UUID(
                uuidString: "00000000-0000-0000-0000-000000000001"
            ))
        )
        try gatewayStore.save(gateway, token: "gateway-token")

        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requestedPushRemoval = false
        let client = MobiusCloudClient(store: sessionStore) { request in
            if request.url?.path == "/api/mobile/push-token" {
                requestedPushRemoval = true
                return try self.response(for: request, status: 500, json: "{}")
            }
            return try self.response(
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
            userID: try XCTUnwrap(UUID(
                uuidString: "00000000-0000-0000-0000-000000000001"
            )),
            email: "private@privaterelay.appleid.com",
            subscribed: true,
            sharesDiagnostics: false
        )
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.destination = .chats
        model.navigationPath = [.chat(.session("chat-1"))]

        await model.signOutOfCloud()

        XCTAssertTrue(requestedPushRemoval)
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

    func testCloudSignOutDisconnectsAndReportsCloudKeychainDeletionFailure() async throws {
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        var failNextGatewayDelete = false
        let gatewayStore = GatewayStore(
            defaults: defaults,
            keychainDelete: { query in
                if failNextGatewayDelete {
                    failNextGatewayDelete = false
                    return errSecInteractionNotAllowed
                }
                return SecItemDelete(query)
            }
        )
        let cloudUserID = try XCTUnwrap(UUID(
            uuidString: "00000000-0000-0000-0000-000000000001"
        ))
        let gateway = GatewayAccount(
            endpoint: try GatewayEndpoint("wss://gateway.example"),
            displayName: "möbius Cloud",
            machineName: mobiusCloudGatewayDisplayName,
            cloudUserID: cloudUserID
        )
        try gatewayStore.save(gateway, token: "gateway-token")
        addTeardownBlock { try? await gatewayStore.remove(gateway) }

        let service = "app.mobius.cloud.tests.\(UUID())"
        var failNextCloudDelete = false
        let sessionStore = MobiusCloudSessionStore(
            service: service,
            keychainDelete: { query in
                if failNextCloudDelete {
                    failNextCloudDelete = false
                    return errSecInteractionNotAllowed
                }
                return SecItemDelete(query)
            }
        )
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
            userID: cloudUserID,
            email: nil,
            subscribed: true,
            sharesDiagnostics: false
        )
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        failNextGatewayDelete = true
        failNextCloudDelete = true

        await model.signOutOfCloud()

        XCTAssertNil(model.cloudSession)
        XCTAssertTrue(model.accounts.isEmpty)
        XCTAssertTrue(gatewayStore.loadAccounts().isEmpty)
        XCTAssertNil(model.selectedAccountID)
        XCTAssertNil(gatewayStore.selectedAccountID())
        XCTAssertNil(model.selectedSessionID)
        XCTAssertEqual(model.connectionState, .disconnected)
        XCTAssertTrue(model.showsPairing)
        XCTAssertNotNil(try client.loadSession())
        XCTAssertEqual(try gatewayStore.token(for: gateway), "gateway-token")
        XCTAssertEqual(model.toast?.tone, .error)
        XCTAssertNotEqual(model.toast?.message, "Gateway removed.")
    }

    func testGatewayStoreClearAllDataContinuesAfterKeychainDeletionFailure() async throws {
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        let draftDirectory = root.appendingPathComponent("Drafts", isDirectory: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: root)
        }
        var failNextDelete = false
        let gatewayStore = GatewayStore(
            defaults: defaults,
            catalogDirectory: root.appendingPathComponent("Catalogs", isDirectory: true),
            transcriptDirectory: root.appendingPathComponent("Transcripts", isDirectory: true),
            thumbnailDirectory: root.appendingPathComponent("Thumbnails", isDirectory: true),
            draftDirectory: draftDirectory,
            keychainDelete: { query in
                if failNextDelete {
                    failNextDelete = false
                    return errSecInteractionNotAllowed
                }
                return SecItemDelete(query)
            }
        )
        let gateway = GatewayAccount(endpoint: try GatewayEndpoint("wss://gateway.example"))
        try gatewayStore.save(gateway, token: "gateway-token")
        addTeardownBlock { try? await gatewayStore.remove(gateway) }
        try FileManager.default.createDirectory(at: draftDirectory, withIntermediateDirectories: true)
        let privateDraft = draftDirectory.appendingPathComponent("private.txt")
        try Data("private draft".utf8).write(to: privateDraft)
        failNextDelete = true

        do {
            try await gatewayStore.clearAllData()
            XCTFail("Expected Keychain deletion to fail")
        } catch let error as GatewayStore.StoreError {
            guard case .keychain(let status) = error else {
                return XCTFail("Expected a Keychain error")
            }
            XCTAssertEqual(status, errSecInteractionNotAllowed)
        }

        XCTAssertTrue(gatewayStore.loadAccounts().isEmpty)
        XCTAssertNil(gatewayStore.selectedAccountID())
        XCTAssertFalse(FileManager.default.fileExists(atPath: privateDraft.path))
        XCTAssertEqual(try gatewayStore.token(for: gateway), "gateway-token")
    }

    func testClearDataAndGatewayInformationPerformsALocalCleanReset() async throws {
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: root)
        }
        defaults.set(ThemePreference.light.rawValue, forKey: "theme")
        let gatewayStore = GatewayStore(
            defaults: defaults,
            catalogDirectory: root.appendingPathComponent("Catalogs", isDirectory: true),
            transcriptDirectory: root.appendingPathComponent("Transcripts", isDirectory: true),
            thumbnailDirectory: root.appendingPathComponent("Thumbnails", isDirectory: true),
            draftDirectory: root.appendingPathComponent("Drafts", isDirectory: true)
        )
        let firstGateway = GatewayAccount(
            endpoint: try GatewayEndpoint("tcp://localhost:9191")
        )
        let secondGateway = GatewayAccount(
            endpoint: try GatewayEndpoint("tcp://localhost:9192")
        )
        try gatewayStore.save(firstGateway, token: "first-token")
        try gatewayStore.save(secondGateway, token: "second-token")
        addTeardownBlock {
            try? await gatewayStore.remove(firstGateway)
            try? await gatewayStore.remove(secondGateway)
        }
        await gatewayStore.saveChatCatalog(
            CachedChatCatalog(bots: [], sessions: [], swarms: [], lastSessionID: nil),
            accountID: secondGateway.id
        )
        await gatewayStore.saveTranscript(
            accountID: secondGateway.id,
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
        await gatewayStore.saveThumbnail(
            Data([1]),
            accountID: secondGateway.id,
            sessionID: "chat-1",
            fileID: "file-1"
        )
        await gatewayStore.saveComposerDraft(
            ComposerDraft(text: "Delete this draft"),
            accountID: secondGateway.id,
            sessionID: "chat-1"
        )

        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        let client = MobiusCloudClient(store: sessionStore) { request in
            if request.url?.path == "/api/mobile/push-token" {
                return try self.response(for: request, status: 500, json: "{}")
            }
            return try self.response(
                for: request,
                json:
                    #"{"token":"ttttttttttttttttttttttttttttttttttttttttttt","userId":"00000000-0000-0000-0000-000000000001","expiresAt":"2099-01-01T00:00:00Z"}"#
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
        model.connectionState = .ready
        model.selectedSessionID = "chat-1"
        model.navigationPath = [.chat(.session("chat-1"))]

        await model.clearDataAndGatewayInformation()

        let catalog = await gatewayStore.loadChatCatalog(accountID: secondGateway.id)
        let transcript = await gatewayStore.loadTranscript(
            accountID: secondGateway.id,
            sessionID: "chat-1"
        )
        let thumbnail = await gatewayStore.loadThumbnail(
            accountID: secondGateway.id,
            sessionID: "chat-1",
            fileID: "file-1"
        )
        let draft = await gatewayStore.loadComposerDraft(
            accountID: secondGateway.id,
            sessionID: "chat-1"
        )
        XCTAssertNil(model.cloudSession)
        XCTAssertNil(try client.loadSession())
        XCTAssertTrue(model.accounts.isEmpty)
        XCTAssertTrue(gatewayStore.loadAccounts().isEmpty)
        XCTAssertNil(model.selectedAccountID)
        XCTAssertNil(gatewayStore.selectedAccountID())
        XCTAssertNil(catalog)
        XCTAssertNil(transcript)
        XCTAssertNil(thumbnail)
        XCTAssertEqual(draft, .empty)
        XCTAssertEqual(defaults.string(forKey: "theme"), ThemePreference.light.rawValue)
        XCTAssertEqual(model.connectionState, .disconnected)
        XCTAssertTrue(model.navigationPath.isEmpty)
        XCTAssertTrue(model.showsPairing)
        XCTAssertThrowsError(try gatewayStore.token(for: firstGateway))
        XCTAssertThrowsError(try gatewayStore.token(for: secondGateway))
    }

    func testRestoreRecoversOutsidePurchaseBeforeFinishingAndConnects() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        var backendAccepted = false
        var finished = false
        var synchronized = false
        let responses = [
            #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"userId":"\#(userID.uuidString)","email":"private@privaterelay.appleid.com","subscribed":false,"sharesDiagnostics":false}"#,
            #"{}"#,
            #"{"userId":"\#(userID.uuidString)","email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false,"subscriptionStartedAt":"2023-11-14T22:13:20Z"}"#,
            #"{"status":"ready"}"#,
            #"{"endpoint":"wss://gateway.example","pairingCode":"0123456789abcdef","expiresAt":"2099-01-01T00:00:00Z"}"#,
        ]
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            if request.url?.path == "/api/mobile/subscription" {
                XCTAssertFalse(finished)
                backendAccepted = true
            }
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
        let purchase = MobiusCloudPurchase(
            jws: "header.payload.signature",
            appTransactionJWS: "app.header.signature"
        ) {
            XCTAssertTrue(backendAccepted)
            finished = true
        }
        let purchases = MobiusCloudPurchases(
            displayPrice: { "$9.99" },
            unfinishedPurchases: { MobiusCloudPurchaseScan() },
            currentEntitlements: { shouldSynchronize in
                synchronized = shouldSynchronize
                return MobiusCloudPurchaseScan(purchases: [purchase])
            },
            purchase: { _ in throw MobiusCloudPurchaseError.unavailable }
        )
        let model = AppModel(
            store: gatewayStore,
            settingsDefaults: defaults,
            requestSender: { request in await gatewayRequests.record(request) },
            connectionOpener: { _ in AsyncThrowingStream { _ in } },
            cloudClient: client,
            cloudPurchases: purchases
        )

        let restoration = Task {
            await model.restoreCloudPurchases()
        }
        _ = await gatewayRequests.firstRequest(after: 0) {
            if case .pair = $0 { return true }
            return false
        }
        model.handle(.paired(clientID: "cloud-client", token: "gateway-token"))
        let restored = await restoration.value

        XCTAssertTrue(restored)
        XCTAssertTrue(synchronized)
        XCTAssertTrue(finished)
        XCTAssertEqual(model.accounts.first?.cloudUserID, userID)
        XCTAssertEqual(requests.map { $0.url?.path }, [
            "/api/mobile/auth/apple",
            "/api/mobile/account",
            "/api/mobile/subscription",
            "/api/mobile/account",
            "/api/mobile/gateway",
            "/api/mobile/gateway",
        ])
        XCTAssertEqual(
            requests.map(\.httpMethod),
            ["POST", "GET", "PUT", "GET", "GET", "POST"]
        )
        let body = try XCTUnwrap(requests[2].httpBody)
        XCTAssertEqual(
            try JSONSerialization.jsonObject(with: body) as? [String: String],
            [
                "jws": "header.payload.signature",
                "appTransactionJws": "app.header.signature",
            ]
        )
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
            #"{"userId":"\#(userID.uuidString)","email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false,"subscriptionStartedAt":"2023-11-14T22:13:20Z"}"#,
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
            #"{"userId":"\#(userID.uuidString)","email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false,"subscriptionStartedAt":"2023-11-14T22:13:20Z"}"#,
            #"{"status":"ready"}"#,
            #"{"endpoint":"wss://fresh.example","pairingCode":"fresh-code-1","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"userId":"\#(userID.uuidString)","email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false,"subscriptionStartedAt":"2023-11-14T22:13:20Z"}"#,
            #"{"status":"ready"}"#,
            #"{"endpoint":"wss://fresh.example","pairingCode":"fresh-code-2","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"userId":"\#(userID.uuidString)","email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false,"subscriptionStartedAt":"2023-11-14T22:13:20Z"}"#,
            #"{"status":"ready"}"#,
            #"{"endpoint":"wss://fresh.example","pairingCode":"fresh-code-3","expiresAt":"2099-01-01T00:00:00Z"}"#,
            #"{"userId":"\#(userID.uuidString)","email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false,"subscriptionStartedAt":"2023-11-14T22:13:20Z"}"#,
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
            userID: userID,
            email: "private@privaterelay.appleid.com",
            subscribed: true,
            sharesDiagnostics: false,
            subscriptionStartedAt: subscriptionStartedAt
        )
        model.pairingEndpoint = "wss://stale.example"
        model.pairingCode = "stale-code"
        model.showsPairing = true

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
        XCTAssertFalse(model.showsPairing)
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
            (200, #"{"userId":"00000000-0000-0000-0000-000000000001","email":"private@privaterelay.appleid.com","subscribed":false,"sharesDiagnostics":false}"#),
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
            cloudClient: client,
            cloudPurchases: emptyCloudPurchases()
        )

        await model.refreshCloudAccount()
        XCTAssertNil(model.cloudAccount)
        XCTAssertNotNil(model.cloudError)

        await model.refreshCloudAccount()
        XCTAssertEqual(
            model.cloudAccount,
            MobiusCloudAccount(
                userID: try XCTUnwrap(UUID(
                    uuidString: "00000000-0000-0000-0000-000000000001"
                )),
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

    func testCancelledCloudAccountRefreshIsSilentAndCanRetry() async throws {
        let userID = UUID()
        let token = String(repeating: "t", count: 43)
        let service = "app.mobius.cloud.tests.\(UUID())"
        let sessionStore = MobiusCloudSessionStore(service: service)
        defer { try? sessionStore.remove() }
        var requests: [URLRequest] = []
        let client = MobiusCloudClient(store: sessionStore) { request in
            requests.append(request)
            switch requests.count {
            case 1:
                return try self.response(
                    for: request,
                    json: #"{"token":"\#(token)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
                )
            case 2:
                throw URLError(.cancelled)
            default:
                return try self.response(
                    for: request,
                    json: #"{"userId":"\#(userID.uuidString)","email":null,"subscribed":false,"sharesDiagnostics":false}"#
                )
            }
        }
        let session = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )
        let suiteName = "app.mobius.cloud.tests.\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let model = AppModel(
            store: GatewayStore(defaults: defaults),
            settingsDefaults: defaults,
            cloudClient: client,
            cloudPurchases: emptyCloudPurchases()
        )

        await model.refreshCloudAccount()

        XCTAssertEqual(model.cloudSession, session)
        XCTAssertNil(model.cloudAccount)
        XCTAssertNil(model.cloudError)
        XCTAssertNil(model.toast)

        await model.refreshCloudAccount()

        XCTAssertEqual(model.cloudAccount?.userID, userID)
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
            (200, #"{"userId":"00000000-0000-0000-0000-000000000001","email":"private@privaterelay.appleid.com","subscribed":true,"sharesDiagnostics":false,"subscriptionStartedAt":"2026-08-24T00:00:00Z","luna":{"creditMicrousd":2400000,"remainingMicrousd":1992000,"resetsAt":"2099-02-01T00:00:00Z"}}"#),
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
            cloudClient: client,
            cloudPurchases: emptyCloudPurchases()
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

    func testPushTokenContractUsesAuthenticatedInstallationUpsertAndRemoval() async throws {
        let userID = UUID()
        let installationID = UUID()
        let bearer = String(repeating: "t", count: 43)
        let store = MobiusCloudSessionStore(service: "app.mobius.cloud.tests.\(UUID())")
        defer { try? store.remove() }
        var requests: [URLRequest] = []
        let client = MobiusCloudClient(store: store) { request in
            requests.append(request)
            let json = requests.count == 1
                ? #"{"token":"\#(bearer)","userId":"\#(userID.uuidString)","expiresAt":"2099-01-01T00:00:00Z"}"#
                : "{}"
            return try self.response(
                for: request,
                status: requests.count == 1 ? 200 : 204,
                json: json
            )
        }
        _ = try await client.authenticate(
            authorizationCode: "apple-code",
            nonce: String(repeating: "n", count: 43)
        )

        try await client.registerPushToken(
            installationID: installationID,
            token: "0011aabbccddeeff00112233445566778899aabb",
            environment: .production
        )
        try await client.unregisterPushToken(installationID: installationID)

        XCTAssertEqual(requests.map { $0.url?.path }, [
            "/api/mobile/auth/apple",
            "/api/mobile/push-token",
            "/api/mobile/push-token",
        ])
        XCTAssertEqual(requests.map(\.httpMethod), ["POST", "PUT", "DELETE"])
        XCTAssertNil(requests[0].value(forHTTPHeaderField: "Authorization"))
        XCTAssertEqual(
            requests[1].value(forHTTPHeaderField: "Authorization"),
            "Bearer \(bearer)"
        )
        XCTAssertEqual(
            requests[2].value(forHTTPHeaderField: "Authorization"),
            "Bearer \(bearer)"
        )
        XCTAssertEqual(
            try JSONSerialization.jsonObject(with: try XCTUnwrap(requests[1].httpBody))
                as? [String: String],
            [
                "installationId": installationID.uuidString,
                "token": "0011aabbccddeeff00112233445566778899aabb",
                "environment": "production",
            ]
        )
        XCTAssertEqual(
            try JSONSerialization.jsonObject(with: try XCTUnwrap(requests[2].httpBody))
                as? [String: String],
            ["installationId": installationID.uuidString]
        )
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

    private func emptyCloudPurchases() -> MobiusCloudPurchases {
        MobiusCloudPurchases(
            displayPrice: { throw MobiusCloudPurchaseError.unavailable },
            unfinishedPurchases: { MobiusCloudPurchaseScan() },
            currentEntitlements: { _ in MobiusCloudPurchaseScan() },
            purchase: { _ in throw MobiusCloudPurchaseError.unavailable }
        )
    }

    private func eventually(
        timeout: Duration = .seconds(1),
        _ predicate: @MainActor () async -> Bool
    ) async -> Bool {
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: timeout)
        repeat {
            if await predicate() { return true }
            try? await Task.sleep(for: .milliseconds(5))
        } while clock.now < deadline
        return await predicate()
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
