import AppKit
import AuthenticationServices
@preconcurrency import DeclaredAgeRange
import Foundation

/// What the Account page shows; Rust receives it whole after every change.
struct CloudStatus: Encodable, Equatable {
    enum Stage: String, Encodable {
        case signingIn = "signing_in"
        case checking
        case provisioning
        case connecting
    }

    /// Cloud's age decision, or this Mac's attempt to share Apple's age range while none exists.
    enum Age: String, Encodable {
        case eligible
        case underage
        case checking
        case declined
        case failed
    }

    var signedIn = false
    var name: String?
    var email: String?
    var subscribed = false
    var plan: String?
    var subscription: String?
    var allowanceRenewsOn: String?
    var accessEndsOn: String?
    var allowanceLeft: Bool?
    var age: Age?
    var gateway: String?
    var gatewayEndpoint: String?
    var stage: Stage?
    var errorKind: CloudFailure.Kind?
    var error: String?
}

/// Sign in with Apple, the mobile bearer in Keychain, and the Cloud gateway pairing grant.
/// Pairing itself stays in Rust's ordinary gateway flow.
@MainActor
final class NativeCloud: NSObject {
    private let emit: (JSONValue) -> Void
    private let client: CloudClient
    private let keychain: CloudKeychain
    private(set) var status = CloudStatus()
    /// Declined or failed for this sign-in; Apple's sheet is not shown again until a retry.
    private var ageAnswer: CloudStatus.Age?
    private var webSession: ASWebAuthenticationSession?
    private var generation = 0
    private(set) var task: Task<Void, Never>?

    init(
        client: CloudClient = CloudClient(), keychain: CloudKeychain = CloudKeychain(),
        emit: @escaping (JSONValue) -> Void
    ) {
        self.client = client
        self.keychain = keychain
        self.emit = emit
    }

    func signIn(connect: Bool = false) {
        guard status.stage == nil else { return }
        guard (NSApplication.shared.keyWindow ?? NSApplication.shared.windows.first) != nil else {
            fail(CloudFailure("Open the möbius app window to sign in with Apple."))
            return
        }
        do {
            let authorization = try CloudSignIn.make()
            generation += 1
            let current = generation
            task?.cancel()
            let session = Self.webSession(url: client.signInURL(authorization)) {
                [weak self] url, error in
                Task { @MainActor in
                    self?.completeSignIn(
                        url: url, error: error, authorization: authorization, generation: current,
                        connect: connect)
                }
            }
            session.presentationContextProvider = self
            webSession = session
            update {
                $0.stage = .signingIn; $0.error = nil; $0.errorKind = nil
            }
            if !session.start() {
                generation += 1
                webSession = nil
                fail(CloudFailure("Apple sign-in could not be opened. Try again."))
            }
        } catch { fail(error) }
    }

    /// Apple's sign-in window answers on an XPC queue, so its handler must not be main-actor
    /// code: a closure written in this main-actor class is, and Swift aborts on the check.
    nonisolated private static func webSession(
        url: URL, handler: @escaping @Sendable (URL?, (any Error)?) -> Void
    ) -> ASWebAuthenticationSession {
        ASWebAuthenticationSession(url: url, callback: .customScheme(CloudSignIn.callbackScheme)) {
            url, error in handler(url, error)
        }
    }

    /// Reads the saved sign-in and, when signed in, the account and gateway status.
    func refresh(connect: Bool = false) {
        guard status.stage == nil else { return }
        run { [self] in try await load(connect: connect) }
    }

    /// Shows Apple's age sheet again after a decline or failure.
    func retryAge() {
        ageAnswer = nil
        refresh()
    }

    /// Account first; Apple's age sheet only while Cloud has no decision; the gateway only
    /// with both an adult decision and a subscription.
    private func load(connect: Bool) async throws {
        guard var credential = try keychain.load() else {
            status = CloudStatus()
            emitStatus()
            return
        }
        guard credential.expiresAt > .now else { throw CloudFailure.expired }
        update {
            $0.signedIn = true
            $0.name = credential.name
            $0.gatewayEndpoint = credential.gatewayEndpoint
            $0.stage = .checking
            $0.error = nil
            $0.errorKind = nil
        }
        var account = try await verifiedAccount(credential)
        if account.ageEligible == nil, ageAnswer == nil {
            update { $0.age = .checking }
            ageAnswer = try await shareAgeRange(token: credential.token)
            try Task.checkCancellation()
            if ageAnswer == nil {
                account = try await verifiedAccount(credential)
                if account.ageEligible == nil { ageAnswer = .failed }
            }
        }
        update {
            $0.email = account.email
            $0.subscribed = account.subscribed
            $0.plan = account.plan
            $0.subscription = account.subscription
            $0.allowanceRenewsOn = account.allowanceRenewsOn
            $0.accessEndsOn = account.accessEndsOn
            $0.allowanceLeft = account.allowanceLeft
            $0.age = account.ageEligible.map { $0 ? .eligible : .underage } ?? ageAnswer
        }
        guard account.subscribed, account.ageEligible == true else { return }
        try await checkGateway(&credential, connect: connect)
    }

    private func verifiedAccount(_ credential: CloudCredential) async throws -> CloudAccount {
        let account = try await client.account(token: credential.token)
        try Task.checkCancellation()
        guard account.userID == credential.userID else {
            throw CloudFailure(
                .session, "Your Cloud account did not match this sign-in. Sign in again.")
        }
        return account
    }

    /// Apple's age sheet, then Cloud records its decision from the range. Returns what this
    /// Mac keeps when nothing was recorded; an expired sign-in still throws.
    private func shareAgeRange(token: String) async throws -> CloudStatus.Age? {
        let response: AgeRangeService.Response
        do {
            guard let window = NSApplication.shared.keyWindow ?? NSApplication.shared.windows.first
            else { return .failed }
            response = try await AgeRangeService.shared.requestAgeRange(ageGates: 18, in: window)
        } catch {
            // This case exists only in the SDK 27 module, and only on macOS 27 and later.
            #if canImport(DeclaredAgeRange, _version: 28)
                if #available(macOS 27.0, *),
                    error as? AgeRangeService.Error == .declinedOnboarding
                {
                    return .declined
                }
            #endif
            return .failed
        }
        guard case .sharing(let range) = response else { return .declined }
        do {
            try await client.submitAgeRange(
                token: token, lowerBound: range.lowerBound, upperBound: range.upperBound)
            return nil
        } catch let failure as CloudFailure where failure.kind == .session {
            throw failure
        } catch {
            return .failed
        }
    }

    private func checkGateway(_ credential: inout CloudCredential, connect: Bool) async throws {
        let deadline = ContinuousClock().now.advanced(by: .seconds(300))
        while true {
            let gateway = try await client.gatewayStatus(
                token: credential.token,
                timeout: Self.gatewayRequestTimeout(until: deadline, maximum: 30))
            try Task.checkCancellation()
            _ = try Self.gatewayRequestTimeout(until: deadline, maximum: 30)
            update { $0.gateway = gateway }
            guard connect else { return }
            switch gateway {
            case "ready":
                update { $0.stage = .connecting }
                let grant = try await client.pairingGrant(
                    token: credential.token,
                    timeout: Self.gatewayRequestTimeout(until: deadline, maximum: 120))
                _ = try Self.gatewayRequestTimeout(until: deadline, maximum: 120)
                guard grant.endpoint == credential.expectedGatewayEndpoint else {
                    throw CloudFailure.invalidGateway
                }
                try Task.checkCancellation()
                credential.gatewayEndpoint = grant.endpoint
                try keychain.save(credential)
                update { $0.gatewayEndpoint = grant.endpoint }
                emit(
                    .object([
                        "type": .string("cloud_pairing"), "endpoint": .string(grant.endpoint),
                        "code": .string(grant.code),
                    ]))
                return
            case "waiting":
                update { $0.stage = .provisioning }
                try await Task.sleep(
                    for: .seconds(Self.gatewayRequestTimeout(until: deadline, maximum: 2)))
            case "expired":
                throw CloudFailure("An active Cloud subscription is required.")
            default:
                throw CloudFailure("möbius Cloud could not provision your gateway.")
            }
        }
    }

    /// HTTP calls and polling sleeps share the provisioning deadline.
    static func gatewayRequestTimeout(
        until deadline: ContinuousClock.Instant, maximum: TimeInterval
    ) throws -> TimeInterval {
        let remaining = ContinuousClock().now.duration(to: deadline)
        guard remaining > .zero else {
            throw CloudFailure("Gateway setup is taking longer than expected. Try again shortly.")
        }
        let parts = remaining.components
        return min(maximum, Double(parts.seconds) + Double(parts.attoseconds) / 1e18)
    }

    /// Revokes the bearer before forgetting it; a failed request can be retried.
    func signOut() {
        generation += 1
        webSession?.cancel()
        webSession = nil
        task?.cancel()
        ageAnswer = nil
        run { [self] in
            let credential = try keychain.load()
            let endpoint = credential?.expectedGatewayEndpoint ?? status.gatewayEndpoint
            if let credential, credential.expiresAt > .now {
                update { $0.stage = .checking }
                try await client.signOut(token: credential.token)
            }
            try Task.checkCancellation()
            try keychain.remove()
            status = CloudStatus()
            emitStatus()
            if let endpoint {
                emit(.object(["type": .string("cloud_forget"), "endpoint": .string(endpoint)]))
            }
        }
    }

    private func run(_ operation: @escaping @MainActor () async throws -> Void) {
        generation += 1
        let current = generation
        task?.cancel()
        task = Task { [weak self] in
            do {
                try await operation()
                guard let self, self.generation == current else { return }
                self.update { $0.stage = nil }
            } catch is CancellationError {
            } catch {
                guard let self, self.generation == current else { return }
                if error as? CloudFailure == .reauthenticationRequired {
                    self.update { $0.stage = nil }
                    self.signIn(connect: true)
                    return
                }
                self.fail(error)
            }
        }
    }

    private func fail(_ error: Error) {
        let failure =
            error as? CloudFailure ?? CloudFailure("Couldn’t connect to möbius Cloud. Try again.")
        if failure.kind == .session { try? keychain.remove() }
        update {
            if failure.kind == .session { $0 = CloudStatus() }
            if $0.age == .checking { $0.age = .failed }
            $0.stage = nil
            $0.errorKind = failure.kind
            $0.error = failure.message
        }
    }

    private func update(_ change: (inout CloudStatus) -> Void) {
        var next = status
        change(&next)
        guard next != status else { return }
        status = next
        emitStatus()
    }

    func emitStatus() {
        guard let data = try? JSONEncoder().encode(status),
            let value = try? JSONDecoder().decode(JSONValue.self, from: data)
        else { return }
        emit(.object(["type": .string("cloud_status"), "status": value]))
    }

    func completeSignIn(
        url: URL?, error: Error?, authorization: CloudSignIn, generation current: Int,
        connect: Bool = false
    ) {
        guard current == generation else { return }
        webSession = nil
        if (error as? ASWebAuthenticationSessionError)?.code == .canceledLogin {
            signInCancelled(connect: connect)
            return
        }
        do {
            guard error == nil, let url else { throw CloudFailure.invalidSignIn }
            let code = try authorization.code(from: url)
            ageAnswer = nil
            run { [self] in
                let previous = try keychain.load()
                var credential = try await client.authenticate(
                    code: code, codeVerifier: authorization.codeVerifier)
                if let previous, previous.userID == credential.userID {
                    credential.name = previous.name
                    if previous.gatewayEndpoint == credential.expectedGatewayEndpoint {
                        credential.gatewayEndpoint = previous.gatewayEndpoint
                    }
                }
                try Task.checkCancellation()
                try keychain.save(credential)
                if let previous {
                    if previous.userID != credential.userID {
                        status = CloudStatus()
                        status.stage = .checking
                        emitStatus()
                        emit(.object([
                            "type": .string("cloud_forget"),
                            "endpoint": .string(previous.expectedGatewayEndpoint),
                        ]))
                    }
                    if previous.token != credential.token {
                        // On its own, so signing in does not wait for it and neither a later
                        // sign-out nor another sign-in cancels it. A failure changes nothing
                        // for the new credential.
                        Task { [client, oldToken = previous.token] in
                            try? await client.signOut(token: oldToken)
                        }
                    }
                }
                try Task.checkCancellation()
                try await load(connect: connect)
            }
        } catch is CancellationError {
            signInCancelled(connect: connect)
        } catch { fail(error) }
    }

    private func signInCancelled(connect: Bool) {
        fail(CloudFailure(
            connect
                ? "Apple sign-in was canceled. Select Connect to try again."
                : "Apple sign-in was canceled. Select Sign in with Apple to try again."))
    }
}

extension NativeCloud: ASWebAuthenticationPresentationContextProviding {
    func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        NSApplication.shared.keyWindow ?? NSApplication.shared.windows.first ?? NSWindow()
    }
}

@_cdecl("mobius_native_cloud")
public func nativeCloud(_ action: UInt8) {
    MainActor.assumeIsolated {
        let cloud = NativeBridge.shared.cloud
        switch action {
        case 1: cloud.signIn()
        case 2: cloud.refresh(connect: true)
        case 3: cloud.signOut()
        case 4: cloud.retryAge()
        default: cloud.refresh()
        }
    }
}
