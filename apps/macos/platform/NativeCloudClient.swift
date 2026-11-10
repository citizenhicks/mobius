import CryptoKit
import Foundation
import Security

/// A failure the Account page explains; `kind` selects its guidance.
struct CloudFailure: LocalizedError, Equatable {
    enum Kind: String, Encodable {
        case session
        case other
    }

    let kind: Kind
    let message: String

    init(_ kind: Kind, _ message: String) {
        self.kind = kind
        self.message = message
    }

    init(_ message: String) { self.init(.other, message) }

    var errorDescription: String? { message }

    static let unavailable = CloudFailure("möbius Cloud is temporarily unavailable.")
    static let expired = CloudFailure(
        .session, "Your Cloud sign-in expired. Sign in with Apple again.")
    static let reauthenticationRequired = CloudFailure(
        "Sign in with Apple again to connect your gateway.")
    static let invalidAccount = CloudFailure("möbius Cloud returned invalid account information.")
    static let invalidGateway = CloudFailure("möbius Cloud returned an invalid gateway response.")
    static let invalidSignIn = CloudFailure("möbius Cloud returned an invalid sign-in response.")
    static let keychain = CloudFailure("The Cloud sign-in could not be saved securely.")
}

/// One browser sign-in, bound to this process by state and an S256 PKCE verifier.
struct CloudSignIn: Equatable, Sendable {
    static let callbackScheme = "app.mobius.desktop"
    let state: String
    let codeVerifier: String

    var codeChallenge: String {
        Self.base64URL(Data(SHA256.hash(data: Data(codeVerifier.utf8))))
    }

    static func make() throws -> Self {
        try Self(state: randomValue(), codeVerifier: randomValue())
    }

    private static func randomValue() throws -> String {
        var bytes = [UInt8](repeating: 0, count: 32)
        guard SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess else {
            throw CloudFailure("A secure Apple sign-in could not be started.")
        }
        return base64URL(Data(bytes))
    }

    private static func base64URL(_ data: Data) -> String {
        data.base64EncodedString()
            .replacing("+", with: "-").replacing("/", with: "_").replacing("=", with: "")
    }

    func code(from url: URL) throws -> String {
        guard let callback = URLComponents(url: url, resolvingAgainstBaseURL: false),
            callback.scheme == Self.callbackScheme, callback.percentEncodedHost == "auth",
            callback.percentEncodedPath == "/callback", callback.user == nil,
            callback.password == nil, callback.port == nil, callback.fragment == nil,
            let items = callback.queryItems, items.count == 2,
            items.first(where: { $0.name == "state" })?.value == state
        else { throw CloudFailure.invalidSignIn }
        if let code = items.first(where: { $0.name == "code" })?.value, isCloudToken(code) {
            return code
        }
        switch items.first(where: { $0.name == "error" })?.value {
        case "cancelled": throw CancellationError()
        case "pending":
            throw CloudFailure(
                "Your previous Cloud account is still being deleted. Try again shortly.")
        case "unavailable": throw CloudFailure.unavailable
        default: throw CloudFailure.invalidSignIn
        }
    }
}

private func isCloudToken(_ value: String) -> Bool {
    value.utf8.count == 43
        && value.unicodeScalars.allSatisfy {
            CharacterSet.alphanumerics.contains($0) && $0.isASCII || $0 == "-" || $0 == "_"
        }
}

/// The mobile bearer and what this Mac remembers about the account. Persisted only in Keychain.
struct CloudCredential: Codable, Equatable {
    let token: String
    let userID: UUID
    let expiresAt: Date
    var name: String?
    var gatewayEndpoint: String?

    var isValid: Bool {
        isCloudToken(token)
    }

    var expectedGatewayEndpoint: String {
        let digest = SHA256.hash(data: Data(userID.uuidString.lowercased().utf8))
            .map { String(format: "%02x", $0) }.joined()
        return "wss://mobius-\(digest.prefix(24)).thinkingsand.dev"
    }

    func gatewayToken(for endpoint: String, now: Date = .now) throws -> String {
        guard isValid, expiresAt > now, endpoint == expectedGatewayEndpoint else {
            throw CloudFailure.expired
        }
        return token
    }
}

func isManagedCloudGateway(_ endpoint: String) -> Bool {
    endpoint.range(
        of: #"\Awss://mobius-[0-9a-f]{24}\.thinkingsand\.dev\z"#, options: .regularExpression
    ) != nil
}

/// Called on Rust's socket worker; reads Keychain afresh for pairing, reconnects and transfers.
/// 0 means self-hosted, 1 means a credential was copied, -1 means Cloud sign-in is required.
@_cdecl("mobius_native_cloud_gateway_token")
public func nativeCloudGatewayToken(
    _ endpoint: UnsafePointer<CChar>, _ output: UnsafeMutablePointer<UInt8>, _ capacity: Int
) -> Int32 {
    let endpoint = String(cString: endpoint)
    guard isManagedCloudGateway(endpoint) else { return 0 }
    do {
        guard let credential = try CloudKeychain().load() else { return -1 }
        let bytes = Array(try credential.gatewayToken(for: endpoint).utf8)
        guard bytes.count == capacity else { return -1 }
        output.update(from: bytes, count: bytes.count)
        return 1
    } catch { return -1 }
}

struct CloudKeychain {
    var service = "app.mobius.desktop.cloud"
    private let account = "mobile-session"

    private var query: [CFString: Any] {
        [kSecClass: kSecClassGenericPassword, kSecAttrService: service, kSecAttrAccount: account]
    }

    func load() throws -> CloudCredential? {
        var result: CFTypeRef?
        let search = query.merging([kSecReturnData: true, kSecMatchLimit: kSecMatchLimitOne]) {
            $1
        }
        let status = SecItemCopyMatching(search as CFDictionary, &result)
        guard status != errSecItemNotFound else { return nil }
        guard status == errSecSuccess else { throw CloudFailure.keychain }
        guard let data = result as? Data, data.count <= 8 * 1024,
            let credential = try? JSONDecoder().decode(CloudCredential.self, from: data),
            credential.isValid
        else {
            try remove()
            throw CloudFailure.invalidSignIn
        }
        return credential
    }

    func save(_ credential: CloudCredential) throws {
        let data = try JSONEncoder().encode(credential)
        let update = SecItemUpdate(query as CFDictionary, [kSecValueData: data] as CFDictionary)
        if update == errSecSuccess { return }
        guard update == errSecItemNotFound else { throw CloudFailure.keychain }
        let item = query.merging([kSecValueData: data]) { $1 }
        guard SecItemAdd(item as CFDictionary, nil) == errSecSuccess else {
            throw CloudFailure.keychain
        }
    }

    func remove() throws {
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw CloudFailure.keychain
        }
    }
}

struct CloudAccount: Equatable {
    let userID: UUID
    let email: String?
    let subscribed: Bool
    let plan: String?
    let subscription: String?
    /// When the AI allowance and Live minutes start again; nil when the plan does not renew.
    var allowanceRenewsOn: String? = nil
    /// When access ends for a plan that does not renew.
    var accessEndsOn: String? = nil
    /// Some of this billing period's AI allowance is still unspent.
    var allowanceLeft: Bool? = nil
    /// Cloud's age decision, apart from the subscription; nil until one is recorded.
    var ageEligible: Bool?
}

/// The SwiftUI app's mobile endpoints; every authenticated call takes the bearer explicitly.
@MainActor
final class CloudClient {
    typealias Transport = @MainActor (URLRequest) async throws -> (Data, HTTPURLResponse)

    private struct Session: Decodable {
        let token: String
        let userId: String
        let expiresAt: Date
    }

    private struct Account: Decodable {
        struct Subscription: Decodable {
            let tier: String
            let status: String
            let billingPeriodEndsAt: Date?
            let accessEndsAt: Date?
            let autoRenews: Bool?
        }
        struct Allowance: Decodable {
            let remainingMicrousd: Int
            let resetsAt: Date?
        }
        struct AgeEligibility: Decodable { let eligible: Bool }
        let userId: String
        let email: String?
        let subscribed: Bool
        let subscription: Subscription?
        let luna: Allowance?
        let ageEligibility: AgeEligibility?
    }

    private struct Gateway: Decodable { let status: String }

    private struct Grant: Decodable {
        let endpoint: String
        let pairingCode: String
        let expiresAt: Date
    }

    static let maximumResponseBytes = 64 * 1024
    private let base: URL
    private let transport: Transport
    private let decoder: JSONDecoder

    init(base: URL = URL(string: "https://mobius.thinkingsand.dev/")!, transport: Transport? = nil)
    {
        self.base = base
        self.transport = transport ?? Self.liveTransport()
        decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .custom { decoder in
            let text = try decoder.singleValueContainer().decode(String.self)
            let fractional = Date.ISO8601FormatStyle(includingFractionalSeconds: true)
            guard
                let date = (try? fractional.parse(text))
                    ?? (try? Date.ISO8601FormatStyle().parse(text))
            else {
                throw DecodingError.dataCorrupted(
                    .init(codingPath: decoder.codingPath, debugDescription: "Invalid date"))
            }
            return date
        }
    }

    private static func liveTransport() -> Transport {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.urlCache = nil
        configuration.httpCookieStorage = nil
        return liveTransport(session: URLSession(configuration: configuration))
    }

    static func liveTransport(session: URLSession) -> Transport {
        { request in
            let (bytes, response) = try await session.bytes(for: request)
            guard let response = response as? HTTPURLResponse else {
                throw CloudFailure.unavailable
            }
            var data = Data()
            data.reserveCapacity(Self.maximumResponseBytes)
            for try await byte in bytes {
                data.append(byte)
                guard data.count <= Self.maximumResponseBytes else {
                    throw CloudFailure("möbius Cloud returned too much data.")
                }
            }
            return (data, response)
        }
    }

    func signInURL(_ authorization: CloudSignIn) -> URL {
        base.appending(path: "api/desktop/auth/apple").appending(queryItems: [
            URLQueryItem(name: "state", value: authorization.state),
            URLQueryItem(name: "code_challenge", value: authorization.codeChallenge),
        ])
    }

    func authenticate(code: String, codeVerifier: String) async throws -> CloudCredential {
        guard isCloudToken(code), isCloudToken(codeVerifier) else {
            throw CloudFailure.invalidSignIn
        }
        let body = try JSONEncoder().encode([
            "code": code, "codeVerifier": codeVerifier,
        ])
        let data: Data
        do {
            data = try await send("api/desktop/auth/exchange", method: "POST", body: body)
        } catch let error as CloudHTTPError {
            switch error.status {
            case 401:
                throw CloudFailure(
                    "This Cloud sign-in link expired or was already used. Sign in with Apple again."
                )
            default: throw CloudFailure.unavailable
            }
        }
        guard let session = try? decoder.decode(Session.self, from: data),
            let userID = UUID(uuidString: session.userId), session.expiresAt > .now
        else { throw CloudFailure.invalidSignIn }
        let credential = CloudCredential(
            token: session.token, userID: userID, expiresAt: session.expiresAt)
        guard credential.isValid else { throw CloudFailure.invalidSignIn }
        return credential
    }

    func account(token: String) async throws -> CloudAccount {
        let data = try await authorized("api/mobile/account", method: "GET", token: token)
        guard let account = try? decoder.decode(Account.self, from: data),
            let userID = UUID(uuidString: account.userId)
        else { throw CloudFailure.invalidAccount }
        let plan = account.subscription.map { $0.tier == "cloud_plus" ? "Cloud Plus" : "Cloud" }
        let status: String? =
            switch account.subscription?.status {
            case "active": "Active"
            case "grace_period": "Payment issue — access continues"
            case "billing_retry": "Payment issue — access paused"
            case "expired": "Expired"
            case "revoked": "Revoked"
            default: nil
            }
        let renews = account.subscription?.autoRenews != false
        let day = { (date: Date) in date.formatted(.dateTime.month(.abbreviated).day()) }
        return CloudAccount(
            userID: userID, email: account.email, subscribed: account.subscribed, plan: plan,
            subscription: status,
            allowanceRenewsOn: renews
                ? (account.luna?.resetsAt ?? account.subscription?.billingPeriodEndsAt).map(day)
                : nil,
            accessEndsOn: renews ? nil : account.subscription?.accessEndsAt.map(day),
            allowanceLeft: account.luna.map { $0.remainingMicrousd > 0 },
            ageEligible: account.ageEligibility?.eligible)
    }

    /// Apple's declared age range; both bounds are sent and null leaves that side open.
    func submitAgeRange(token: String, lowerBound: Int?, upperBound: Int?) async throws {
        let body = try JSONEncoder().encode(["lowerBound": lowerBound, "upperBound": upperBound])
        _ = try await authorized("api/mobile/age-range", method: "PUT", body: body, token: token)
    }

    func gatewayStatus(token: String, timeout: TimeInterval = 30) async throws -> String {
        let data = try await authorized(
            "api/mobile/gateway", method: "GET", token: token, timeout: timeout)
        guard let gateway = try? decoder.decode(Gateway.self, from: data),
            ["waiting", "ready", "expired", "error"].contains(gateway.status)
        else { throw CloudFailure.invalidGateway }
        return gateway.status
    }

    func pairingGrant(token: String, timeout: TimeInterval = 120) async throws -> (
        endpoint: String, code: String
    ) {
        let data = try await authorized(
            "api/mobile/gateway", method: "POST", token: token, timeout: timeout)
        guard let grant = try? decoder.decode(Grant.self, from: data), grant.expiresAt > .now,
            isManagedCloudGateway(grant.endpoint),
            !grant.pairingCode.isEmpty, grant.pairingCode.utf8.count <= 256
        else { throw CloudFailure.invalidGateway }
        return (grant.endpoint, grant.pairingCode)
    }

    func signOut(token: String) async throws {
        _ = try await authorized("api/mobile/sign-out", method: "POST", token: token)
    }

    private func authorized(
        _ path: String, method: String, body: Data? = nil, token: String,
        timeout: TimeInterval = 30
    )
        async throws -> Data
    {
        do {
            return try await send(
                path, method: method, body: body, token: token, timeout: timeout)
        } catch let error as CloudHTTPError {
            if error.status == 403, error.code == "reauthentication_required" {
                throw CloudFailure.reauthenticationRequired
            }
            throw error.status == 401 ? CloudFailure.expired : CloudFailure.unavailable
        }
    }

    private func send(
        _ path: String, method: String, body: Data? = nil, token: String? = nil,
        timeout: TimeInterval = 30
    )
        async throws -> Data
    {
        var request = URLRequest(url: base.appending(path: path))
        request.httpMethod = method
        request.timeoutInterval = timeout
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if let body {
            request.httpBody = body
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        if let token { request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization") }
        let prepared = request
        let (data, response) = try await withThrowingTaskGroup(of: (Data, HTTPURLResponse).self) {
            group in
            defer { group.cancelAll() }
            group.addTask { try await self.transport(prepared) }
            group.addTask {
                try await Task.sleep(for: .seconds(timeout))
                throw CloudFailure.unavailable
            }
            guard let response = try await group.next() else { throw CloudFailure.unavailable }
            return response
        }
        guard data.count <= Self.maximumResponseBytes else {
            throw CloudFailure("möbius Cloud returned too much data.")
        }
        guard (200..<300).contains(response.statusCode) else {
            let code = try? JSONDecoder().decode(CloudErrorResponse.self, from: data).code
            throw CloudHTTPError(status: response.statusCode, code: code)
        }
        return data
    }
}

private struct CloudErrorResponse: Decodable { let code: String? }
struct CloudHTTPError: Error {
    let status: Int
    var code: String? = nil
}
