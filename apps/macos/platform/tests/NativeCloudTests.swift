import Foundation
import Testing

@testable import MobiusNative

private let token = String(repeating: "a", count: 43)
private let userID = "7f1f7d2e-3c4b-4a5d-9e6f-0a1b2c3d4e5f"
private let gatewayEndpoint = "wss://mobius-2fffaade2469f7ccbd9a1305.thinkingsand.dev"

private let config = CloudConfig(
    api: URL(string: "https://cloud.invalid/")!, site: "https://cloud.invalid",
    gateway: "wss://mobius-{id}.thinkingsand.dev", providers: ["apple"],
    callbackScheme: "app.mobius.desktop", ageGate: 18)

private func keychain() -> CloudKeychain {
    CloudKeychain(service: "app.mobius.desktop.cloud.test.\(UUID().uuidString)")
}

@MainActor
private func client(_ status: Int, _ body: String) -> CloudClient {
    CloudClient(
        config: config,
        transport: { request in
            #expect(request.url?.host == "cloud.invalid")
            let response = HTTPURLResponse(
                url: request.url!, statusCode: status, httpVersion: nil, headerFields: nil)!
            return (Data(body.utf8), response)
        })
}

@Test func browserSignInUsesIndependentStateAndS256PKCE() throws {
    let authorization = try CloudSignIn.make(callbackScheme: config.callbackScheme)
    for value in [authorization.state, authorization.codeVerifier, authorization.codeChallenge] {
        #expect(value.range(of: #"\A[A-Za-z0-9_-]{43}\z"#, options: .regularExpression) != nil)
    }
    #expect(authorization.state != authorization.codeVerifier)
    let vector = CloudSignIn(
        state: token, codeVerifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        callbackScheme: config.callbackScheme)
    #expect(vector.codeChallenge == "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM")
}

@MainActor @Test func exchangeKeepsTheBearerOutOfBrowserURLs() async throws {
    let authorization = try CloudSignIn.make(callbackScheme: config.callbackScheme)
    let body =
        #"{"token":"\#(token)","userId":"\#(userID)","expiresAt":"2999-01-01T00:00:00.000Z"}"#
    let exchange = CloudClient(
        config: config,
        transport: { request in
            #expect(
                request.httpMethod == "POST" && request.url?.path == "/api/desktop/auth/exchange")
            #expect(
                request.url?.query == nil
                    && request.value(forHTTPHeaderField: "Authorization") == nil)
            let sent = try #require(request.httpBody)
            let payload = try JSONDecoder().decode([String: String].self, from: sent)
            #expect(payload == ["code": token, "codeVerifier": authorization.codeVerifier])
            let response = HTTPURLResponse(
                url: request.url!, statusCode: 200, httpVersion: nil, headerFields: nil)!
            return (Data(body.utf8), response)
        })
    let start = try #require(
        URLComponents(
            url: exchange.signInURL(authorization, provider: "apple"),
            resolvingAgainstBaseURL: false))
    #expect(
        start.scheme == "https" && start.host == "cloud.invalid"
            && start.path == "/api/desktop/auth/apple")
    #expect(
        exchange.signInURL(authorization, provider: "github").path == "/api/desktop/auth/github")
    #expect(
        start.queryItems == [
            URLQueryItem(name: "state", value: authorization.state),
            URLQueryItem(name: "code_challenge", value: authorization.codeChallenge),
        ])
    let credential = try await exchange.authenticate(
        code: token, codeVerifier: authorization.codeVerifier)
    #expect(credential.token == token && credential.isValid)
    await #expect(throws: CloudFailure.self) {
        do {
            _ = try await client(401, "{}").authenticate(
                code: token, codeVerifier: authorization.codeVerifier)
        } catch let failure as CloudFailure {
            #expect(failure.kind == .other)
            throw failure
        }
    }
}

@Test func browserCallbackRequiresExactDestinationAndState() throws {
    let authorization = CloudSignIn(
        state: token, codeVerifier: token, callbackScheme: config.callbackScheme)
    let callback = "app.mobius.desktop://auth/callback?state=\(token)&code=\(token)"
    #expect(try authorization.code(from: URL(string: callback)!) == token)
    for invalid in [
        callback.replacing("app.mobius.desktop:", with: "https:"),
        callback.replacing("//auth/", with: "//wrong/"),
        callback.replacing("//auth/", with: "//user@auth/"),
        callback.replacing("//auth/", with: "//auth:443/"),
        callback.replacing("/callback?", with: "/callback/extra?"),
        callback.replacing("/callback?", with: "/%63allback?"),
        callback.replacing("state=\(token)", with: "state=wrong"),
        callback.replacing("code=\(token)", with: "code=short"),
        callback + "&state=\(token)", callback + "&code=\(token)",
        callback + "&error=cancelled", callback + "#fragment",
    ] {
        #expect(throws: CloudFailure.invalidSignIn) {
            try authorization.code(from: URL(string: invalid)!)
        }
    }
}

@Test func browserCallbackErrorsNeverBypassStateValidation() throws {
    let authorization = CloudSignIn(
        state: token, codeVerifier: token, callbackScheme: config.callbackScheme)
    let callback = "app.mobius.desktop://auth/callback?state=\(token)&error="
    #expect(throws: CancellationError.self) {
        try authorization.code(from: URL(string: callback + "cancelled")!)
    }
    for error in ["pending", "unavailable", "failed", "arbitrary-server-message"] {
        #expect(throws: CloudFailure.self) {
            try authorization.code(from: URL(string: callback + error)!)
        }
    }
    #expect(throws: CloudFailure.invalidSignIn) {
        try authorization.code(
            from: URL(string: "app.mobius.desktop://auth/callback?state=wrong&error=cancelled")!)
    }
}

@MainActor @Test func signOutRejectsAnOldBrowserCompletionBeforeExchange() throws {
    let keychain = keychain()
    defer { try? keychain.remove() }
    let transport = CloudClient(
        config: config,
        transport: { _ in
            Issue.record("An old browser callback must not exchange credentials")
            throw CloudFailure.invalidSignIn
        })
    let cloud = NativeCloud(config: config, client: transport, keychain: keychain, emit: { _ in })
    cloud.signOut()
    let callback = URL(string: "app.mobius.desktop://auth/callback?state=\(token)&code=\(token)")!
    cloud.completeSignIn(
        url: callback, error: nil,
        authorization: CloudSignIn(
            state: token, codeVerifier: token, callbackScheme: config.callbackScheme), generation: 0
    )
    #expect(cloud.status == CloudStatus())
    #expect(try keychain.load() == nil)
}

@MainActor @Test func signOutWhileAccountLoadsCannotRestoreOldState() async throws {
    let keychain = keychain()
    defer { try? keychain.remove() }
    try keychain.save(
        CloudCredential(token: token, userID: UUID(uuidString: userID)!, expiresAt: .distantFuture))
    let (started, signal) = AsyncStream<Void>.makeStream()
    var waiting: CheckedContinuation<(Data, HTTPURLResponse), Never>?
    var requests = 0
    let transport = CloudClient(
        config: config,
        transport: { request in
            requests += 1
            if request.url?.path == "/api/mobile/sign-out" {
                #expect(request.httpMethod == "POST")
                #expect(request.value(forHTTPHeaderField: "Authorization") == "Bearer \(token)")
                return (
                    Data(),
                    HTTPURLResponse(
                        url: request.url!, statusCode: 204, httpVersion: nil, headerFields: nil)!
                )
            }
            guard requests == 1 else { throw CloudFailure.invalidAccount }
            #expect(request.url?.path == "/api/mobile/account")
            // Deliberately ignores cancellation: the response may already be in flight.
            return await withCheckedContinuation { continuation in
                waiting = continuation
                signal.yield()
                signal.finish()
            }
        })
    let cloud = NativeCloud(config: config, client: transport, keychain: keychain, emit: { _ in })
    cloud.refresh()
    let operation = try #require(cloud.task)
    for await _ in started {}
    cloud.signOut()
    await cloud.task?.value
    let body =
        #"{"userId":"\#(userID)","email":"old@example.com","subscribed":true,"ageEligibility":{"eligible":true}}"#
    let response = HTTPURLResponse(
        url: URL(string: "https://cloud.invalid/")!, statusCode: 200,
        httpVersion: nil, headerFields: nil)!
    waiting?.resume(returning: (Data(body.utf8), response))
    await operation.value
    #expect(requests == 2 && cloud.status == CloudStatus())
    #expect(try keychain.load() == nil)
}

@Test func gatewayAdmissionIsBoundToTheAccountAndExpiresWithItsSession() throws {
    let expiry = Date(timeIntervalSince1970: 2_000_000_000)
    let credential = CloudCredential(
        token: token, userID: UUID(uuidString: userID)!, expiresAt: expiry)
    #expect(config.gatewayEndpoint(for: credential.userID) == gatewayEndpoint)
    #expect(
        try credential.gatewayToken(
            for: gatewayEndpoint, config: config, now: expiry.addingTimeInterval(-1))
            == token)
    #expect(throws: CloudFailure.expired) {
        try credential.gatewayToken(for: gatewayEndpoint, config: config, now: expiry)
    }
    for endpoint in [
        "wss://gateway.example", "wss://mobius-000000000000000000000000.thinkingsand.dev",
        gatewayEndpoint + ".evil.example", gatewayEndpoint + ":8443", gatewayEndpoint + "/",
        gatewayEndpoint.replacing("wss://", with: "ws://"),
    ] {
        #expect(throws: CloudFailure.expired) {
            try credential.gatewayToken(
                for: endpoint, config: config, now: expiry.addingTimeInterval(-1))
        }
    }
    #expect(!config.ownsGateway("wss://gateway.example"))
    #expect(!config.ownsGateway(gatewayEndpoint + ".evil.example"))
    #expect(!config.ownsGateway(gatewayEndpoint + "\n"))
    #expect(config.ownsGateway(gatewayEndpoint))
    var output = [UInt8](repeating: 0, count: 43)
    let result = "wss://gateway.example".withCString { endpoint in
        nativeCloudGatewayToken(endpoint, &output, output.count)
    }
    #expect(result == 0 && output.allSatisfy { $0 == 0 })
}

@MainActor @Test func failedRevocationKeepsTheCredentialForSignOutRetry() async throws {
    let keychain = keychain()
    defer { try? keychain.remove() }
    let credential = CloudCredential(
        token: token, userID: UUID(uuidString: userID)!, expiresAt: .distantFuture)
    try keychain.save(credential)
    var events: [JSONValue] = []
    let cloud = NativeCloud(
        config: config, client: client(503, "{}"), keychain: keychain, emit: { events.append($0) })
    cloud.signOut()
    await cloud.task?.value
    #expect(try keychain.load() == credential)
    #expect(cloud.status.error != nil)
    #expect(!events.contains { $0["type"]?.stringValue == "cloud_forget" })
}

@MainActor @Test func differentAccountReauthenticationForgetsOldGatewayEvenIfRevocationFails()
    async throws
{
    let newToken = String(repeating: "b", count: 43)
    let newUserID = "9c75115e-8650-44c6-b33d-255abcf830e7"
    let keychain = keychain()
    defer { try? keychain.remove() }
    var previous = CloudCredential(
        token: token, userID: UUID(uuidString: userID)!, expiresAt: .distantFuture)
    previous.name = "Old Mac"
    previous.gatewayEndpoint = gatewayEndpoint
    try keychain.save(previous)
    let savedKeychain = keychain
    var events: [JSONValue] = []
    let transport = CloudClient(
        config: config,
        transport: { request in
            let body: String
            let status: Int
            switch request.url?.path {
            case "/api/desktop/auth/exchange":
                body =
                    #"{"token":"\#(newToken)","userId":"\#(newUserID)","expiresAt":"2999-01-01T00:00:00Z"}"#
                status = 200
            case "/api/mobile/sign-out":
                #expect(request.value(forHTTPHeaderField: "Authorization") == "Bearer \(token)")
                #expect(try savedKeychain.load()?.token == newToken)
                body = "{}"
                status = 503
            case "/api/mobile/account":
                #expect(request.value(forHTTPHeaderField: "Authorization") == "Bearer \(newToken)")
                body =
                    #"{"userId":"\#(newUserID)","email":"new@example.com","subscribed":false,"ageEligibility":{"eligible":true}}"#
                status = 200
            default:
                throw CloudFailure.unavailable
            }
            return (
                Data(body.utf8),
                HTTPURLResponse(
                    url: request.url!, statusCode: status, httpVersion: nil, headerFields: nil)!
            )
        })
    let cloud = NativeCloud(
        config: config, client: transport, keychain: keychain, emit: { events.append($0) })
    cloud.completeSignIn(
        url: URL(string: "app.mobius.desktop://auth/callback?state=\(token)&code=\(token)"),
        error: nil,
        authorization: CloudSignIn(
            state: token, codeVerifier: token, callbackScheme: config.callbackScheme), generation: 0
    )
    await cloud.task?.value
    #expect(try keychain.load()?.userID == UUID(uuidString: newUserID))
    #expect(try keychain.load()?.name == nil)
    #expect(try keychain.load()?.gatewayEndpoint == nil)
    #expect(cloud.status.email == "new@example.com" && cloud.status.gatewayEndpoint == nil)
    #expect(
        events.contains {
            $0["type"]?.stringValue == "cloud_forget"
                && $0["endpoint"]?.stringValue == gatewayEndpoint
        })
}

@MainActor @Test func oldBearerRevocationDoesNotHoldUpOrSurviveIntoReauthenticatedAccount()
    async throws
{
    let newToken = String(repeating: "b", count: 43)
    let keychain = keychain()
    defer { try? keychain.remove() }
    try keychain.save(
        CloudCredential(
            token: token, userID: UUID(uuidString: userID)!, expiresAt: .distantFuture))
    let reply = { (request: URLRequest, status: Int, body: String) in
        (
            Data(body.utf8),
            HTTPURLResponse(
                url: request.url!, statusCode: status, httpVersion: nil, headerFields: nil)!
        )
    }
    var oldRevocation: CheckedContinuation<(Data, HTTPURLResponse), Never>?
    var oldRevocationCancelled = false
    var oldRevocationFinished = false
    var pendingAccount: CheckedContinuation<(Data, HTTPURLResponse), Never>?
    var accountRequests = 0
    let transport = CloudClient(
        config: config,
        transport: { request in
            switch request.url?.path {
            case "/api/desktop/auth/exchange":
                return reply(
                    request, 200,
                    #"{"token":"\#(newToken)","userId":"\#(userID)","expiresAt":"2999-01-01T00:00:00Z"}"#
                )
            case "/api/mobile/sign-out"
            where request.value(forHTTPHeaderField: "Authorization") == "Bearer \(token)":
                let response = await withCheckedContinuation { continuation in
                    oldRevocation = continuation
                }
                oldRevocationCancelled = Task.isCancelled
                oldRevocationFinished = true
                return response
            case "/api/mobile/sign-out":
                #expect(request.value(forHTTPHeaderField: "Authorization") == "Bearer \(newToken)")
                return reply(request, 204, "")
            case "/api/mobile/account":
                accountRequests += 1
                return await withCheckedContinuation { continuation in
                    pendingAccount = continuation
                }
            default:
                throw CloudFailure.unavailable
            }
        })
    let cloud = NativeCloud(config: config, client: transport, keychain: keychain, emit: { _ in })
    cloud.completeSignIn(
        url: URL(string: "app.mobius.desktop://auth/callback?state=\(token)&code=\(token)"),
        error: nil,
        authorization: CloudSignIn(
            state: token, codeVerifier: token, callbackScheme: config.callbackScheme), generation: 0
    )
    let reauthentication = try #require(cloud.task)
    // The new account loads with the old token's revocation still unanswered.
    try await waitUntil { oldRevocation != nil && accountRequests == 1 }
    #expect(!oldRevocationFinished)
    #expect(try keychain.load()?.token == newToken)

    // Signing out neither cancels that revocation nor lets it end in a reload.
    cloud.signOut()
    await cloud.task?.value
    oldRevocation?.resume(
        returning: reply(
            URLRequest(url: URL(string: "https://cloud.invalid/api/mobile/sign-out")!), 204, ""))
    try await waitUntil { oldRevocationFinished }
    #expect(!oldRevocationCancelled)
    pendingAccount?.resume(
        returning: reply(
            URLRequest(url: URL(string: "https://cloud.invalid/api/mobile/account")!), 200,
            #"{"userId":"\#(userID)","email":"new@example.com","subscribed":false,"ageEligibility":{"eligible":true}}"#
        ))
    await reauthentication.value
    #expect(accountRequests == 1 && cloud.status == CloudStatus())
    #expect(try keychain.load() == nil)
}

/// Lets concurrent work run until `condition` holds, failing rather than hanging when it never does.
@MainActor private func waitUntil(_ condition: () -> Bool) async throws {
    for _ in 0..<500 where !condition() {
        try await Task.sleep(for: .milliseconds(10))
    }
    #expect(condition())
}

private final class OversizedCloudResponseProtocol: URLProtocol {
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        let response = HTTPURLResponse(
            url: request.url!, statusCode: 200, httpVersion: nil, headerFields: nil)!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(repeating: 0, count: 64 * 1024 + 1))
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}
}

@MainActor @Test func liveCloudTransportBoundsTheResponseWhileStreaming() async throws {
    let configuration = URLSessionConfiguration.ephemeral
    configuration.protocolClasses = [OversizedCloudResponseProtocol.self]
    let session = URLSession(configuration: configuration)
    defer { session.invalidateAndCancel() }
    let transport = CloudClient.liveTransport(session: session)
    let request = URLRequest(url: URL(string: "https://cloud.invalid/")!)
    await #expect(throws: CloudFailure("möbius Cloud returned too much data.")) {
        _ = try await transport(request)
    }
}

@Test func cloudConfigLayersTheUserFileOverTheBundledDefaults() throws {
    let folder = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: folder) }
    let file = folder.appending(path: ".mobius/cloud.json")
    let bundled: [String: Any] = [
        "api": "https://mobius.example/", "site": "https://mobius.example",
        "gateway": "wss://mobius-{id}.example", "providers": ["apple"],
        "callbackScheme": "app.mobius.desktop", "ageGate": 18,
    ]
    #expect(CloudConfig.load(bundled: nil, file: file) == nil)
    #expect(!FileManager.default.fileExists(atPath: file.path))

    let defaults = try #require(CloudConfig.load(bundled: bundled, file: file))
    #expect(defaults.api.host() == "mobius.example" && defaults.ageGate == 18)
    #expect(try JSONDecoder().decode(CloudConfig.self, from: Data(contentsOf: file)) == defaults)

    let custom =
        #"{"api":"https://cloud.example.com/","gateway":"wss://gw-{id}.example.com","providers":["github","google"],"ageGate":null}"#
    try Data(custom.utf8).write(to: file)
    let config = try #require(CloudConfig.load(bundled: bundled, file: file))
    #expect(config.api.host() == "cloud.example.com" && config.providers == ["github", "google"])
    #expect(config.ageGate == nil && config.site == "https://mobius.example")
    #expect(try Data(contentsOf: file) == Data(custom.utf8))
    #expect(config.keychainService != defaults.keychainService)

    // A user file alone configures a build without bundled defaults, e.g. an unbundled run.
    let full =
        #"{"api":"https://cloud.example.com/","site":"https://example.com","gateway":"wss://gw-{id}.example.com","providers":["github"],"callbackScheme":"app.example"}"#
    try Data(full.utf8).write(to: file)
    #expect(CloudConfig.load(bundled: nil, file: file)?.providers == ["github"])

    try Data(#"{"api":"http://cloud.example.com/"}"#.utf8).write(to: file)
    #expect(CloudConfig.load(bundled: bundled, file: file) == defaults)

    let id = UUID(uuidString: userID)!
    #expect(config.ownsGateway(config.gatewayEndpoint(for: id)))
    #expect(!config.ownsGateway(gatewayEndpoint))
    #expect(!config.ownsGateway("wss://gw-2fffaade2469f7ccbd9a1305.example.com.evil"))
}

@Test func cloudConfigRejectsUnsafeValues() throws {
    var valid = config
    #expect(valid.isValid)
    valid.api = URL(string: "http://localhost:8787/")!
    #expect(valid.isValid)
    for change in [
        { (c: inout CloudConfig) in c.api = URL(string: "http://cloud.example.com/")! },
        { $0.gateway = "ws://gw-{id}.example.com" }, { $0.gateway = "wss://gw.example.com" },
        { $0.providers = [] }, { $0.providers = ["../admin"] },
        { $0.providers = ["apple", "apple"] },
        { $0.providers = ["one", "two", "three", "four", "five"] },
        { $0.api = URL(string: "https://user:password@cloud.example.com/")! },
        { $0.api = URL(string: "https://cloud.example.com/?query=1")! },
        { $0.site = "https://cloud.example.com/#fragment" },
        { $0.gateway = "wss://gw-{id}.example.com?query=1" },
        { $0.gateway = "ws://gw-{id}.localhost:8787" },
        { $0.gateway = "wss://gw-{id}.example.com/" },
        { $0.gateway = "wss://example.com/{id}" },
        { $0.gateway = "wss://gw-{id}.example.com:443" },
        { $0.gateway = "wss://gw-{id}.example.com:0" },
        { $0.callbackScheme = "Bad Scheme" }, { $0.ageGate = 0 },
    ] {
        var invalid = config
        change(&invalid)
        #expect(!invalid.isValid)
    }
}

@Test func savedSignInsAreIsolatedByTheCompleteServiceURL() {
    for url in ["https://cloud.invalid:8443/", "https://cloud.invalid/other/", "http://localhost/"]
    {
        var other = config
        other.api = URL(string: url)!
        #expect(other.keychainService != config.keychainService)
    }
}

@Test func hostedGatewayRecognitionRequiresTheExactConfiguredEndpoint() {
    let id = "2fffaade2469f7ccbd9a1305"
    for template in [
        "wss://gw-{id}.example.com", "wss://{id}.example.com:8787",
    ] {
        var custom = config
        custom.gateway = template
        let endpoint = template.replacing("{id}", with: id)
        #expect(custom.isValid)
        #expect(custom.ownsGateway(endpoint))
        for invalid in [
            endpoint + "/", endpoint + ":443", endpoint + "?x=1",
            endpoint.replacing(id, with: id.uppercased()), endpoint.replacing(id, with: "short"),
        ] {
            #expect(!custom.ownsGateway(invalid))
        }
    }
}

@MainActor @Test func cloudWithoutAnAgeGateConnectsWithoutAnAppleAgeDecision() async throws {
    var custom = config
    custom.ageGate = nil
    custom.providers = ["github"]
    let keychain = keychain()
    defer { try? keychain.remove() }
    try keychain.save(
        CloudCredential(
            token: token, userID: UUID(uuidString: userID)!, expiresAt: .distantFuture,
            provider: "github"))
    var requests: [String] = []
    var events: [JSONValue] = []
    let transport = CloudClient(
        config: custom,
        transport: { request in
            let path = request.url!.path
            requests.append(path)
            let body: String
            switch path {
            case "/api/mobile/account":
                body = #"{"userId":"\#(userID)","email":"user@example.com","subscribed":true}"#
            case "/api/mobile/gateway" where request.httpMethod == "GET":
                body = #"{"status":"ready"}"#
            case "/api/mobile/gateway":
                body =
                    #"{"endpoint":"\#(gatewayEndpoint)","pairingCode":"one-time-code","expiresAt":"2999-01-01T00:00:00Z"}"#
            default:
                Issue.record("Unexpected Cloud request: \(path)")
                throw CloudFailure.unavailable
            }
            return (
                Data(body.utf8),
                HTTPURLResponse(
                    url: request.url!, statusCode: 200, httpVersion: nil, headerFields: nil)!
            )
        })
    let cloud = NativeCloud(
        config: custom, client: transport, keychain: keychain, emit: { events.append($0) })
    cloud.refresh(connect: true)
    await cloud.task?.value
    #expect(requests == ["/api/mobile/account", "/api/mobile/gateway", "/api/mobile/gateway"])
    #expect(cloud.status.error == nil && cloud.status.provider == "github")
    #expect(events.contains { $0["type"]?.stringValue == "cloud_pairing" })
}
