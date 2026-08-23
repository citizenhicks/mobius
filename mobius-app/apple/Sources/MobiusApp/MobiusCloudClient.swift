import CryptoKit
import Foundation
import Security

let mobiusCloudMonthlyProductID = "app.mobius.client.cloud.monthly.v2"

struct MobiusCloudSession: Equatable, Sendable {
    let userID: UUID
    let expiresAt: Date
}

struct MobiusCloudUsageLimit: Decodable, Equatable, Sendable {
    let creditMicrousd: Int
    let remainingMicrousd: Int
    let resetsAt: Date

    var remainingFraction: Double {
        Double(remainingMicrousd) / Double(creditMicrousd)
    }
}

struct MobiusCloudAccount: Equatable, Sendable {
    let email: String?
    let subscribed: Bool
    let sharesDiagnostics: Bool
    let subscriptionStartedAt: Date?
    let luna: MobiusCloudUsageLimit?

    init(
        email: String?,
        subscribed: Bool,
        sharesDiagnostics: Bool,
        subscriptionStartedAt: Date? = nil,
        luna: MobiusCloudUsageLimit? = nil
    ) {
        self.email = email
        self.subscribed = subscribed
        self.sharesDiagnostics = sharesDiagnostics
        self.subscriptionStartedAt = subscriptionStartedAt
        self.luna = luna
    }
}

struct MobiusCloudPairingGrant: Equatable, Sendable {
    let setup: GatewayPairingSetup
    let expiresAt: Date
}

struct MobiusCloudExtensionSource: Decodable, Equatable, Sendable {
    let url: String
    let reference: String?
    let subdirectory: String?
}

struct MobiusCloudExtensionCatalogItem: Decodable, Equatable, Identifiable, Sendable {
    let id: String
    let name: String
    let description: String
    let source: MobiusCloudExtensionSource
}

enum MobiusCloudGatewayStatus: String, Decodable, Sendable {
    case waiting
    case ready
    case error
}

struct MobiusCloudAppleNonce: Equatable, Sendable {
    let rawValue: String
    let requestValue: String

    static func make() throws -> Self {
        var bytes = [UInt8](repeating: 0, count: 32)
        guard SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess else {
            throw MobiusCloudError.secureRandomUnavailable
        }
        let rawValue = Data(bytes).base64EncodedString()
            .replacing("+", with: "-")
            .replacing("/", with: "_")
            .replacing("=", with: "")
        let requestValue = SHA256.hash(data: Data(rawValue.utf8)).map { byte in
            let hex = String(byte, radix: 16)
            return byte < 16 ? "0\(hex)" : hex
        }.joined()
        return Self(rawValue: rawValue, requestValue: requestValue)
    }
}

enum MobiusCloudError: LocalizedError {
    case activeSubscription
    case authenticationRequired
    case invalidAccountResponse
    case invalidAuthenticationResponse
    case invalidAuthorization
    case invalidExtensionCatalog
    case invalidGatewayResponse
    case invalidSignedTransaction
    case keychain(OSStatus)
    case oversizedResponse
    case provisioningFailed
    case provisioningTimedOut
    case secureRandomUnavailable
    case server(Int)
    case sessionExpired
    case subscriptionRequired
    case unverifiedTransaction

    var errorDescription: String? {
        switch self {
        case .activeSubscription: "Cancel your Cloud subscription before deleting your account."
        case .authenticationRequired: "Sign in with Apple to continue."
        case .invalidAccountResponse: "möbius Cloud returned invalid account information."
        case .invalidAuthenticationResponse: "möbius Cloud returned an invalid sign-in response."
        case .invalidAuthorization: "Apple sign-in could not be completed."
        case .invalidExtensionCatalog: "möbius Cloud returned an invalid extension catalog."
        case .invalidGatewayResponse: "möbius Cloud returned an invalid gateway response."
        case .invalidSignedTransaction: "The App Store transaction is invalid."
        case .keychain: "The Cloud sign-in could not be saved securely."
        case .oversizedResponse: "möbius Cloud returned too much data."
        case .provisioningFailed: "möbius Cloud could not provision your gateway."
        case .provisioningTimedOut: "Gateway setup is taking longer than expected. Try again shortly."
        case .secureRandomUnavailable: "A secure Apple sign-in could not be started."
        case .server(let status):
            status == 401
                ? "Your Cloud sign-in expired. Sign in with Apple again."
                : "möbius Cloud is temporarily unavailable."
        case .sessionExpired: "Your Cloud sign-in expired. Sign in with Apple again."
        case .subscriptionRequired: "An active Cloud subscription is required."
        case .unverifiedTransaction: "The App Store could not verify this transaction."
        }
    }
}

private struct MobiusCloudCredential: Codable {
    let token: String
    let userID: UUID
    let expiresAt: Date

    var session: MobiusCloudSession {
        MobiusCloudSession(userID: userID, expiresAt: expiresAt)
    }

    var hasValidToken: Bool {
        token.utf8.count == 43 && token.utf8.allSatisfy {
            ($0 >= 0x30 && $0 <= 0x39)
                || ($0 >= 0x41 && $0 <= 0x5a)
                || ($0 >= 0x61 && $0 <= 0x7a)
                || $0 == 0x2d
                || $0 == 0x5f
        }
    }
}

@MainActor
final class MobiusCloudSessionStore {
    private let service: String
    private let account = "mobile-session"
    private let encoder = JSONEncoder()
    private let decoder = JSONDecoder()

    init(service: String = "app.mobius.cloud") {
        self.service = service
    }

    fileprivate func load() throws -> MobiusCloudCredential? {
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: service,
            kSecAttrAccount: account,
            kSecReturnData: true,
            kSecMatchLimit: kSecMatchLimitOne,
        ]
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        guard status != errSecItemNotFound else { return nil }
        guard status == errSecSuccess,
              let data = result as? Data,
              data.count <= 8 * 1024,
              let credential = try? decoder.decode(MobiusCloudCredential.self, from: data),
              credential.hasValidToken
        else {
            if status == errSecSuccess { try? remove() }
            throw status == errSecSuccess
                ? MobiusCloudError.invalidAuthenticationResponse
                : MobiusCloudError.keychain(status)
        }
        return credential
    }

    fileprivate func save(_ credential: MobiusCloudCredential) throws {
        let data = try encoder.encode(credential)
        guard data.count <= 8 * 1024 else {
            throw MobiusCloudError.invalidAuthenticationResponse
        }
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: service,
            kSecAttrAccount: account,
        ]
        let attributes: [CFString: Any] = [
            kSecValueData: data,
            kSecAttrAccessible: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
        ]
        let updateStatus = SecItemUpdate(query as CFDictionary, attributes as CFDictionary)
        guard updateStatus == errSecItemNotFound else {
            guard updateStatus == errSecSuccess else {
                throw MobiusCloudError.keychain(updateStatus)
            }
            return
        }
        let item = query.merging(attributes) { _, new in new }
        let addStatus = SecItemAdd(item as CFDictionary, nil)
        guard addStatus == errSecSuccess else { throw MobiusCloudError.keychain(addStatus) }
    }

    func remove() throws {
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: service,
            kSecAttrAccount: account,
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw MobiusCloudError.keychain(status)
        }
    }

    fileprivate func remove(ifTokenMatches token: String) throws {
        guard try load()?.token == token else { return }
        try remove()
    }
}

@MainActor
final class MobiusCloudClient {
    typealias Transport = @MainActor (URLRequest) async throws -> (Data, HTTPURLResponse)

    private struct AppleAuthenticationRequest: Encodable {
        let authorizationCode: String
        let nonce: String
    }

    private struct AppleAuthenticationResponse: Decodable {
        let token: String
        let userId: String
        let expiresAt: Date
    }

    private struct SubscriptionRequest: Encodable {
        let signedTransaction: String
    }

    private struct AccountResponse: Decodable {
        let email: String?
        let subscribed: Bool
        let sharesDiagnostics: Bool
        let luna: MobiusCloudUsageLimit?
    }

    private struct AccountUpdateRequest: Encodable {
        let sharesDiagnostics: Bool
    }

    private struct GatewayStatusResponse: Decodable {
        let status: MobiusCloudGatewayStatus
    }

    private struct PairingResponse: Decodable {
        let endpoint: String
        let pairingCode: String
        let expiresAt: Date
    }

    private struct ExtensionCatalogResponse: Decodable {
        let extensions: [MobiusCloudExtensionCatalogItem]
    }

    private static let authenticationURL = cloudURL("api/mobile/auth/apple")
    private static let accountURL = cloudURL("api/mobile/account")
    private static let subscriptionURL = cloudURL("api/mobile/subscription")
    private static let gatewayURL = cloudURL("api/mobile/gateway")
    private static let extensionCatalogURL = cloudURL("api/mobile/extensions/catalog")
    private static let maximumResponseBytes = 64 * 1024
    private static let maximumSignedTransactionBytes = 64 * 1024

    private let store: MobiusCloudSessionStore
    private let transport: Transport
    private let encoder = JSONEncoder()
    private let decoder: JSONDecoder

    init(
        store: MobiusCloudSessionStore = MobiusCloudSessionStore(),
        transport: Transport? = nil
    ) {
        self.store = store
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        self.decoder = decoder
        if let transport {
            self.transport = transport
        } else {
            let configuration = URLSessionConfiguration.ephemeral
            configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
            configuration.urlCache = nil
            configuration.httpCookieStorage = nil
            let session = URLSession(configuration: configuration)
            self.transport = { request in
                let (data, response) = try await session.data(for: request)
                guard let response = response as? HTTPURLResponse else {
                    throw MobiusCloudError.server(0)
                }
                return (data, response)
            }
        }
    }

    func loadSession() throws -> MobiusCloudSession? {
        guard let credential = try store.load() else { return nil }
        guard credential.expiresAt > .now else {
            try store.remove()
            return nil
        }
        return credential.session
    }

    /// Forgets the stored bearer. AppModel owns the paired-gateway cleanup.
    func signOut() throws {
        try store.remove()
    }

    func authenticate(authorizationCode: String, nonce: String) async throws -> MobiusCloudSession {
        let data = try await send(
            url: Self.authenticationURL,
            method: "POST",
            body: try appleAuthenticationBody(authorizationCode: authorizationCode, nonce: nonce)
        )
        let response: AppleAuthenticationResponse
        do {
            response = try decoder.decode(AppleAuthenticationResponse.self, from: data)
        } catch {
            throw MobiusCloudError.invalidAuthenticationResponse
        }
        guard let userID = UUID(uuidString: response.userId),
              response.expiresAt > .now
        else { throw MobiusCloudError.invalidAuthenticationResponse }

        let credential = MobiusCloudCredential(
            token: response.token,
            userID: userID,
            expiresAt: response.expiresAt
        )
        guard credential.hasValidToken else {
            throw MobiusCloudError.invalidAuthenticationResponse
        }
        try store.save(credential)
        return credential.session
    }

    func deleteAccount(authorizationCode: String, nonce: String) async throws {
        do {
            _ = try await send(
                url: Self.accountURL,
                method: "DELETE",
                body: try appleAuthenticationBody(
                    authorizationCode: authorizationCode,
                    nonce: nonce
                ),
                authenticated: true,
                forgetAuthenticationOnSuccess: true
            )
        } catch MobiusCloudError.server(409) {
            throw MobiusCloudError.activeSubscription
        } catch MobiusCloudError.server(403) {
            throw MobiusCloudError.invalidAuthorization
        }
    }

    func account() async throws -> MobiusCloudAccount {
        let data = try await send(
            url: Self.accountURL,
            method: "GET",
            authenticated: true
        )
        let response: AccountResponse
        do {
            response = try decoder.decode(AccountResponse.self, from: data)
        } catch {
            throw MobiusCloudError.invalidAccountResponse
        }
        guard response.email.map(Self.isValidEmail) ?? true,
              response.luna.map({
                  response.subscribed &&
                      $0.creditMicrousd > 0 &&
                      (0 ... $0.creditMicrousd).contains($0.remainingMicrousd)
              }) ?? true
        else {
            throw MobiusCloudError.invalidAccountResponse
        }
        return MobiusCloudAccount(
            email: response.email,
            subscribed: response.subscribed,
            sharesDiagnostics: response.sharesDiagnostics,
            luna: response.luna
        )
    }

    func updateSharesDiagnostics(_ sharesDiagnostics: Bool) async throws {
        _ = try await send(
            url: Self.accountURL,
            method: "PUT",
            body: try encoder.encode(AccountUpdateRequest(
                sharesDiagnostics: sharesDiagnostics
            )),
            authenticated: true
        )
    }

    func submitSubscription(signedTransaction: String) async throws {
        let parts = signedTransaction.split(separator: ".", omittingEmptySubsequences: false)
        guard signedTransaction.utf8.count <= Self.maximumSignedTransactionBytes,
              parts.count == 3,
              parts.allSatisfy({ part in
                  !part.isEmpty && part.utf8.allSatisfy {
                      ($0 >= 0x30 && $0 <= 0x39)
                          || ($0 >= 0x41 && $0 <= 0x5a)
                          || ($0 >= 0x61 && $0 <= 0x7a)
                          || $0 == 0x2d
                          || $0 == 0x5f
                  }
              })
        else { throw MobiusCloudError.invalidSignedTransaction }
        _ = try await send(
            url: Self.subscriptionURL,
            method: "PUT",
            body: try encoder.encode(SubscriptionRequest(signedTransaction: signedTransaction)),
            authenticated: true
        )
    }

    func gatewayStatus() async throws -> MobiusCloudGatewayStatus {
        let data = try await send(
            url: Self.gatewayURL,
            method: "GET",
            authenticated: true
        )
        do {
            return try decoder.decode(GatewayStatusResponse.self, from: data).status
        } catch {
            throw MobiusCloudError.invalidGatewayResponse
        }
    }

    func createPairingGrant() async throws -> MobiusCloudPairingGrant {
        let data = try await send(
            url: Self.gatewayURL,
            method: "POST",
            authenticated: true
        )
        let response: PairingResponse
        do {
            response = try decoder.decode(PairingResponse.self, from: data)
        } catch {
            throw MobiusCloudError.invalidGatewayResponse
        }
        guard response.expiresAt > .now else { throw MobiusCloudError.invalidGatewayResponse }
        do {
            return MobiusCloudPairingGrant(
                setup: try GatewayPairingSetup(
                    endpoint: response.endpoint,
                    code: response.pairingCode
                ),
                expiresAt: response.expiresAt
            )
        } catch {
            throw MobiusCloudError.invalidGatewayResponse
        }
    }

    func extensionCatalog() async throws -> [MobiusCloudExtensionCatalogItem] {
        let data = try await send(
            url: Self.extensionCatalogURL,
            method: "GET",
            authenticated: true
        )
        let items: [MobiusCloudExtensionCatalogItem]
        do {
            items = try decoder.decode(ExtensionCatalogResponse.self, from: data).extensions
        } catch {
            throw MobiusCloudError.invalidExtensionCatalog
        }
        guard items.count <= 100 else { throw MobiusCloudError.invalidExtensionCatalog }

        var ids = Set<String>()
        guard items.allSatisfy({ item in
            ids.insert(item.id).inserted
                && item.id.range(
                    of: #"^[a-z0-9][a-z0-9._-]{0,127}$"#,
                    options: .regularExpression
                ) != nil
                && !item.name.isEmpty
                && item.name.utf8.count <= 100
                && item.description.utf8.count <= 1_000
                && Self.isValidExtensionSource(item.source)
        }) else {
            throw MobiusCloudError.invalidExtensionCatalog
        }
        return items
    }

    private func send(
        url: URL,
        method: String,
        body: Data? = nil,
        authenticated: Bool = false,
        forgetAuthenticationOnSuccess: Bool = false
    ) async throws -> Data {
        var request = URLRequest(url: url)
        var bearer: String?
        request.httpMethod = method
        request.timeoutInterval = 30
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if let body {
            request.httpBody = body
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        if authenticated {
            guard let credential = try store.load() else {
                throw MobiusCloudError.authenticationRequired
            }
            guard credential.expiresAt > .now else {
                try store.remove()
                throw MobiusCloudError.sessionExpired
            }
            bearer = credential.token
            request.setValue("Bearer \(credential.token)", forHTTPHeaderField: "Authorization")
        }

        let (data, response) = try await transport(request)
        guard data.count <= Self.maximumResponseBytes else { throw MobiusCloudError.oversizedResponse }
        guard (200..<300).contains(response.statusCode) else {
            if response.statusCode == 401, let bearer {
                try? store.remove(ifTokenMatches: bearer)
            }
            throw MobiusCloudError.server(response.statusCode)
        }
        if forgetAuthenticationOnSuccess, let bearer {
            // The server revoked every session; a newer local sign-in must still survive a
            // stale response from this request.
            try? store.remove(ifTokenMatches: bearer)
        }
        return data
    }

    private func appleAuthenticationBody(
        authorizationCode: String,
        nonce: String
    ) throws -> Data {
        guard !authorizationCode.isEmpty,
              authorizationCode.utf8.count <= 2_048,
              !authorizationCode.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains),
              nonce.range(of: #"^[A-Za-z0-9_-]{43}$"#, options: .regularExpression) != nil
        else { throw MobiusCloudError.invalidAuthorization }
        return try encoder.encode(AppleAuthenticationRequest(
            authorizationCode: authorizationCode,
            nonce: nonce
        ))
    }

    private static func cloudURL(_ path: String) -> URL {
        guard let url = URL(string: "https://mobius.thinkingsand.dev/\(path)") else {
            fatalError("Invalid möbius Cloud URL")
        }
        return url
    }

    private static func isValidEmail(_ email: String) -> Bool {
        email.utf8.count <= 254
            && email.range(
                of: #"^[^\s@]+@[^\s@]+\.[^\s@]+$"#,
                options: .regularExpression
            ) != nil
    }

    private static func isValidExtensionSource(_ source: MobiusCloudExtensionSource) -> Bool {
        guard source.url.utf8.count <= 2_048,
              let url = URL(string: source.url),
              url.scheme?.lowercased() == "https",
              url.host != nil,
              url.user == nil,
              url.password == nil,
              source.reference.map({ !$0.isEmpty && $0.utf8.count <= 256 }) ?? true,
              source.subdirectory.map({ !$0.isEmpty && $0.utf8.count <= 1_024 }) ?? true
        else { return false }
        return true
    }
}
