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
        .session, "Your Cloud sign-in expired. Sign in again.")
    static let reauthenticationRequired = CloudFailure(
        "Sign in again to connect your gateway.")
    static let invalidAccount = CloudFailure("möbius Cloud returned invalid account information.")
    static let invalidGateway = CloudFailure("möbius Cloud returned an invalid gateway response.")
    static let invalidSignIn = CloudFailure("möbius Cloud returned an invalid sign-in response.")
    static let keychain = CloudFailure("The Cloud sign-in could not be saved securely.")
}

/// Where Cloud lives and how users sign in to it. The build's defaults are the
/// `MobiusCloud` dictionary in Info.plist; `~/.mobius/cloud.json` overrides any of its keys
/// and is written from those defaults only when missing. A build with neither has no Cloud.
struct CloudConfig: Codable, Equatable, Sendable {
    /// The HTTPS service serving the `api/desktop/*` and `api/mobile/*` routes.
    var api: URL
    /// Marketing and legal pages (`{site}/legal/en/{slug}`).
    var site: String
    /// Hosted gateways; `{id}` is the first 24 hex digits of SHA-256(lowercased user ID).
    var gateway: String
    /// Sign-in buttons, in order; each opens `{api}/api/desktop/auth/{provider}`.
    var providers: [String]
    /// The browser returns to `{callbackScheme}://auth/callback`.
    var callbackScheme: String
    /// Asks Apple for the user's age range against this gate; null or absent skips it.
    var ageGate: Int?

    static let file = FileManager.default.homeDirectoryForCurrentUser
        .appending(path: ".mobius/cloud.json")
    static let current = load(
        bundled: Bundle.main.object(forInfoDictionaryKey: "MobiusCloud") as? [String: Any])

    /// The bearer goes to `api` and gateways matching `gateway`, so both must be TLS
    /// (plain HTTP only on loopback, for a local server) and every value must be URL-safe.
    var isValid: Bool {
        func secure(_ url: URL?, _ tls: String, _ plain: String) -> Bool {
            guard let url, let host = url.host(), !host.isEmpty, url.user == nil,
                url.password == nil, url.query == nil, url.fragment == nil
            else {
                return false
            }
            return url.scheme == tls
                || url.scheme == plain && ["localhost", "127.0.0.1", "::1", "[::1]"].contains(host)
        }
        let gatewayURL = URL(string: gateway.replacing("{id}", with: "id"))
        let token = #"\A[a-z0-9-]{1,32}\z"#
        return secure(api, "https", "http")
            && secure(URL(string: site), "https", "http")
            && gateway.components(separatedBy: "{id}").count == 2
            // Endpoint supports TLS WebSockets with only a host and optional nondefault port.
            && secure(gatewayURL, "wss", "wss")
            && gatewayURL?.path == ""
            && gatewayURL?.port != 443
            && gatewayURL?.port.map { (1...65535).contains($0) } ?? true
            && (1...4).contains(providers.count)
            && Set(providers).count == providers.count
            && providers.allSatisfy { $0.range(of: token, options: .regularExpression) != nil }
            && callbackScheme.range(
                of: #"\A[a-z][a-z0-9+.-]*\z"#, options: .regularExpression) != nil
            && ageGate.map { (1...99).contains($0) } ?? true
    }

    /// The bundled defaults with the user's file over them. The file is written from the
    /// defaults only when missing; an unreadable or invalid file is reported, left alone,
    /// and the defaults are used.
    static func load(bundled: [String: Any]?, file: URL = file) -> Self? {
        let defaults = bundled.flatMap(decode)
        guard let data = try? Data(contentsOf: file) else {
            if let defaults {
                let encoder = JSONEncoder()
                encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
                try? FileManager.default.createDirectory(
                    at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
                try? encoder.encode(defaults).write(to: file, options: .withoutOverwriting)
            }
            return defaults
        }
        guard let user = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let config = decode((bundled ?? [:]).merging(user) { $1 })
        else {
            NSLog("möbius: ignoring invalid %@", file.path)
            return defaults
        }
        return config
    }

    private static func decode(_ values: [String: Any]) -> Self? {
        guard JSONSerialization.isValidJSONObject(values),
            let data = try? JSONSerialization.data(withJSONObject: values),
            let config = try? JSONDecoder().decode(Self.self, from: data), config.isValid
        else { return nil }
        return config
    }

    /// The gateway this account's Cloud must hand out; pins the bearer to it.
    func gatewayEndpoint(for userID: UUID) -> String {
        let digest = SHA256.hash(data: Data(userID.uuidString.lowercased().utf8))
            .map { String(format: "%02x", $0) }.joined()
        return gateway.replacing("{id}", with: digest.prefix(24))
    }

    func ownsGateway(_ endpoint: String) -> Bool {
        let parts = gateway.components(separatedBy: "{id}")
        guard parts.count == 2, endpoint.hasPrefix(parts[0]), endpoint.hasSuffix(parts[1]) else {
            return false
        }
        let id = endpoint.dropFirst(parts[0].count).dropLast(parts[1].count)
        return id.utf8.count == 24
            && id.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }
    }

    /// Bind the saved bearer to the complete API URL, including its port and service path.
    var keychainService: String {
        let digest = SHA256.hash(data: Data(api.absoluteString.utf8))
            .map { String(format: "%02x", $0) }.joined()
        return "app.mobius.desktop.cloud.\(digest)"
    }

    /// What Rust needs for copy, links and sign-in buttons.
    var publicJSON: String {
        let value: [String: Any] = [
            "site": site, "providers": providers,
            "age_gate": ageGate.map { $0 as Any } ?? NSNull(),
        ]
        let data = try? JSONSerialization.data(withJSONObject: value)
        return data.flatMap { String(data: $0, encoding: .utf8) } ?? "{}"
    }
}

/// Null when this build has no Cloud.
@_cdecl("mobius_native_cloud_config")
public func nativeCloudConfig() -> UnsafeMutablePointer<CChar>? {
    CloudConfig.current.flatMap { strdup($0.publicJSON) }
}

/// Uses the same exact endpoint match as bearer admission.
@_cdecl("mobius_native_cloud_owns_gateway")
public func nativeCloudOwnsGateway(_ endpoint: UnsafePointer<CChar>) -> Bool {
    CloudConfig.current?.ownsGateway(String(cString: endpoint)) ?? false
}

/// One browser sign-in, bound to this process by state and an S256 PKCE verifier.
struct CloudSignIn: Equatable, Sendable {
    let state: String
    let codeVerifier: String
    let callbackScheme: String

    var codeChallenge: String {
        Self.base64URL(Data(SHA256.hash(data: Data(codeVerifier.utf8))))
    }

    static func make(callbackScheme: String) throws -> Self {
        try Self(state: randomValue(), codeVerifier: randomValue(), callbackScheme: callbackScheme)
    }

    private static func randomValue() throws -> String {
        var bytes = [UInt8](repeating: 0, count: 32)
        guard SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess else {
            throw CloudFailure("A secure sign-in could not be started.")
        }
        return base64URL(Data(bytes))
    }

    private static func base64URL(_ data: Data) -> String {
        data.base64EncodedString()
            .replacing("+", with: "-").replacing("/", with: "_").replacing("=", with: "")
    }

    func code(from url: URL) throws -> String {
        guard let callback = URLComponents(url: url, resolvingAgainstBaseURL: false),
            callback.scheme == callbackScheme, callback.percentEncodedHost == "auth",
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
    /// The provider this sign-in used, so a re-authentication reuses it.
    var provider: String?

    var isValid: Bool {
        isCloudToken(token)
    }

    func gatewayToken(for endpoint: String, config: CloudConfig, now: Date = .now) throws
        -> String
    {
        guard isValid, expiresAt > now, endpoint == config.gatewayEndpoint(for: userID) else {
            throw CloudFailure.expired
        }
        return token
    }
}

/// Called on Rust's socket worker; reads Keychain afresh for pairing, reconnects and transfers.
/// 0 means self-hosted, 1 means a credential was copied, -1 means Cloud sign-in is required.
@_cdecl("mobius_native_cloud_gateway_token")
public func nativeCloudGatewayToken(
    _ endpoint: UnsafePointer<CChar>, _ output: UnsafeMutablePointer<UInt8>, _ capacity: Int
) -> Int32 {
    let endpoint = String(cString: endpoint)
    guard let config = CloudConfig.current, config.ownsGateway(endpoint) else { return 0 }
    do {
        guard let credential = try CloudKeychain(service: config.keychainService).load()
        else { return -1 }
        let bytes = Array(try credential.gatewayToken(for: endpoint, config: config).utf8)
        guard bytes.count == capacity else { return -1 }
        output.update(from: bytes, count: bytes.count)
        return 1
    } catch { return -1 }
}

struct CloudKeychain {
    let service: String
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
    private let config: CloudConfig
    private var base: URL { config.api }
    private let transport: Transport
    private let decoder: JSONDecoder

    init(config: CloudConfig, transport: Transport? = nil) {
        self.config = config
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

    func signInURL(_ authorization: CloudSignIn, provider: String) -> URL {
        base.appending(path: "api/desktop/auth/\(provider)").appending(queryItems: [
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
                    "This Cloud sign-in link expired or was already used. Sign in again."
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
            config.ownsGateway(grant.endpoint),
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
