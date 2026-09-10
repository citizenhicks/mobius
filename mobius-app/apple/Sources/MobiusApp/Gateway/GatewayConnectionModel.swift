import Foundation
import Observation

@MainActor
@Observable
final class GatewayConnectionModel {
    var accounts: [GatewayAccount]
    var selectedAccountID: UUID?
    var connectionState: ConnectionState = .disconnected
    var gatewayMachineName = ""
    var pairingEndpoint = "wss://"
    var pairingCode = ""
    var pairingError: String?
    var locale = Locale.current

    @ObservationIgnored private let client: GatewayClient
    @ObservationIgnored private let store: GatewayStore
    @ObservationIgnored private let requestSender:
        @MainActor @Sendable (GatewayRequest) async throws -> Void
    @ObservationIgnored private let connectionOpener:
        @MainActor @Sendable (GatewayEndpoint) async throws
            -> AsyncThrowingStream<GatewayEnvelope, Error>
    @ObservationIgnored private let reconnectDelay: @Sendable (Int) -> Duration
    @ObservationIgnored private var openingTask: Task<Void, Never>?
    @ObservationIgnored private var eventTask: Task<Void, Never>?
    @ObservationIgnored private var reconnectTask: Task<Void, Never>?
    @ObservationIgnored private var disconnectTask: Task<Void, Never>?
    @ObservationIgnored private(set) var reconnectAttempt = 0
    @ObservationIgnored private(set) var automaticReconnectBlocked = false
    private var pendingPairingAccount: GatewayAccount?
    @ObservationIgnored private(set) var reconnectsOnActivation = false
    @ObservationIgnored private var reconnectsUntilReady = false
    @ObservationIgnored private(set) var connectionGeneration = UUID()
    @ObservationIgnored private var appIsInBackground = true

    var onConnectionReplacement: (@MainActor (GatewayAccount, Bool) -> Void)?
    var onEnvelope: (@MainActor (GatewayEnvelope) -> Void)?
    var onDisconnected: (@MainActor (String) -> Void)?
    var onUpdateRequired: (@MainActor () -> Void)?
    var onPairingRepairRequired: (@MainActor () -> Void)?

    init(
        client: GatewayClient,
        store: GatewayStore,
        requestSender: (@MainActor @Sendable (GatewayRequest) async throws -> Void)? = nil,
        connectionOpener: (
            @MainActor @Sendable (GatewayEndpoint) async throws
                -> AsyncThrowingStream<GatewayEnvelope, Error>
        )? = nil,
        reconnectDelay: (@Sendable (Int) -> Duration)? = nil
    ) {
        self.client = client
        self.store = store
        self.requestSender = requestSender ?? { request in try await client.send(request) }
        self.connectionOpener =
            connectionOpener ?? { endpoint in
                try await client.connect(to: endpoint)
            }
        self.reconnectDelay =
            reconnectDelay ?? { attempt in
                let seconds = min(
                    8,
                    0.5 * pow(2, Double(min(attempt, 4))) * Double.random(in: 0.75...1.25)
                )
                return .milliseconds(Int64(seconds * 1_000))
            }
        accounts = store.loadAccounts()
        selectedAccountID = store.selectedAccountID()
        if selectedAccountID == nil { selectedAccountID = accounts.first?.id }
    }

    isolated deinit {
        openingTask?.cancel()
        eventTask?.cancel()
        reconnectTask?.cancel()
        disconnectTask?.cancel()
        Task { [client] in await client.disconnect() }
    }

    var selectedAccount: GatewayAccount? {
        accounts.first { $0.id == selectedAccountID }
    }

    func connect(
        to account: GatewayAccount,
        retrying: Bool = false,
        reconnectsUntilReady: Bool = false
    ) {
        cancelReconnect()
        let sameGateway = account.id == selectedAccountID
        reset(preservingDrafts: sameGateway)
        onConnectionReplacement?(account, sameGateway)
        if !retrying {
            reconnectAttempt = 0
            automaticReconnectBlocked = false
            self.reconnectsUntilReady = reconnectsUntilReady
        }
        reconnectsOnActivation = false
        selectedAccountID = account.id
        store.select(account)
        connectionState = .connecting
        let generation = connectionGeneration
        openingTask = Task { [weak self] in
            guard let self, generation == self.connectionGeneration else { return }
            await self.client.disconnect()
            guard !Task.isCancelled, generation == self.connectionGeneration else { return }
            do {
                let token = try self.store.token(for: account)
                self.beginConnection(to: account.endpoint, generation: generation) { [weak self] in
                    guard let self, self.connectionGeneration == generation else { return }
                    try await self.requestSender(
                        .authenticate(
                            token: token,
                            clientKind: .currentApplePlatform
                        ))
                }
            } catch {
                self.blockAutomaticReconnect()
                self.connectionState = .failed(self.localizedErrorDescription(error))
                if case GatewayStore.StoreError.missingToken = error {
                    self.repairSelectedGateway()
                    self.onPairingRepairRequired?()
                }
                self.onDisconnected?(self.localizedErrorDescription(error))
            }
        }
    }

    func pair() {
        cancelReconnect()
        pairingError = nil
        do {
            let code = pairingCode.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !code.isEmpty else {
                pairingError = localizedString("Enter the one-time code shown by the gateway.")
                return
            }
            let setup = try GatewayPairingSetup(endpoint: pairingEndpoint, code: code)
            let endpoint = setup.endpoint
            let endpointName = endpoint.displayName(locale: locale)
            let account =
                accounts.first(where: { $0.endpoint == endpoint })
                ?? GatewayAccount(
                    endpoint: endpoint,
                    displayName: endpointName,
                    machineName: endpointName
                )
            let sameGateway = account.id == selectedAccountID
            reset(preservingDrafts: sameGateway)
            onConnectionReplacement?(account, sameGateway)
            automaticReconnectBlocked = false
            reconnectAttempt = 0
            reconnectsOnActivation = false
            reconnectsUntilReady = false
            pendingPairingAccount = account
            connectionState = .connecting
            let generation = connectionGeneration
            beginConnection(to: endpoint, generation: generation) { [weak self] in
                guard let self, self.connectionGeneration == generation else { return }
                try await self.requestSender(
                    .pair(
                        code: setup.code,
                        clientLabel: "möbius Apple",
                        clientKind: .currentApplePlatform
                    ))
            }
        } catch {
            pairingError = localizedErrorDescription(error)
        }
    }

    func applyPairingSetup(_ setup: GatewayPairingSetup) {
        cancelReconnect()
        pairingEndpoint = setup.endpoint.rawValue
        pairingCode = setup.code
        pairingError = nil
    }

    func repairSelectedGateway() {
        guard let account = selectedAccount else {
            pairingError = nil
            return
        }
        pairingEndpoint = account.endpoint.rawValue
        pairingCode = ""
        pairingError = localizedString("Enter a new one-time code to repair this pairing.")
    }

    func rename(_ account: GatewayAccount, to name: String) throws {
        let renamed = try store.rename(account, to: name)
        guard let index = accounts.firstIndex(where: { $0.id == renamed.id }) else { return }
        accounts[index] = renamed
    }

    func prepareAccountRemoval(_ account: GatewayAccount) -> Bool {
        let wasSelected = account.id == selectedAccountID
        guard wasSelected else {
            accounts.removeAll { $0.id == account.id }
            return false
        }
        reset(preservingDrafts: false)
        selectedAccountID = nil
        accounts.removeAll { $0.id == account.id }
        return true
    }

    func updatePendingPairingAccount(displayName: String, cloudUserID: UUID?) {
        pendingPairingAccount?.displayName = displayName
        pendingPairingAccount?.cloudUserID = cloudUserID
    }

    var hasPendingPairing: Bool { pendingPairingAccount != nil }

    var pairingConnectionState: ConnectionState? {
        hasPendingPairing ? connectionState : nil
    }

    func reloadAccounts() {
        accounts = store.loadAccounts()
        selectedAccountID = store.selectedAccountID() ?? accounts.first?.id
    }

    func clearAccounts() {
        accounts = []
        selectedAccountID = nil
    }

    func cancelPendingPairing() {
        pairingEndpoint = "wss://"
        pairingCode = ""
        pendingPairingAccount = nil
        blockAutomaticReconnect()
    }

    func allowAutomaticReconnect() {
        automaticReconnectBlocked = false
    }

    func blockAutomaticReconnect() {
        automaticReconnectBlocked = true
        reconnectsOnActivation = false
        cancelReconnect()
    }

    func setSceneActive(_ active: Bool, reconnectWhenActive: Bool = true) {
        guard active else {
            cancelReconnect()
            reconnectsOnActivation = !automaticReconnectBlocked
            return
        }
        guard reconnectWhenActive, reconnectsOnActivation,
            pendingPairingAccount == nil, !automaticReconnectBlocked
        else { return }
        guard let account = selectedAccount else { return }
        connect(to: account, reconnectsUntilReady: reconnectsUntilReady)
    }

    func setAppInBackground(_ value: Bool) {
        appIsInBackground = value
    }

    @discardableResult
    func shutdown(
        endVoiceRequest: GatewayRequest? = nil,
        state: ConnectionState? = nil
    ) -> Task<Void, Never> {
        resetTransport()
        if let state { connectionState = state }
        let generation = connectionGeneration
        let task = Task { [weak self, client] in
            guard self?.connectionGeneration == generation else { return }
            if let endVoiceRequest {
                try? await self?.send(endVoiceRequest, generation: generation)
            }
            guard !Task.isCancelled, self?.connectionGeneration == generation else { return }
            await client.disconnect()
        }
        disconnectTask = task
        return task
    }

    @discardableResult
    func reset(preservingDrafts: Bool) -> UUID {
        resetTransport()
        pendingPairingAccount = nil
        pairingError = preservingDrafts ? pairingError : nil
        if !preservingDrafts {
            pairingCode = ""
            gatewayMachineName = ""
        }
        connectionState = .disconnected
        return connectionGeneration
    }

    func transmit(
        _ request: GatewayRequest,
        onFailure: (@MainActor (String) -> Void)? = nil
    ) {
        let generation = connectionGeneration
        Task { [weak self] in
            guard let self, generation == self.connectionGeneration else { return }
            do {
                try await self.send(request, generation: generation)
            } catch {
                guard generation == self.connectionGeneration else { return }
                let message = GatewayWireError.disconnected.localizedDescription
                onFailure?(message)
                if !self.connectionState.isConnecting {
                    self.connectionEnded(generation: generation, message: message)
                }
            }
        }
    }

    func send(_ request: GatewayRequest) async throws {
        try await send(request, generation: connectionGeneration)
    }

    func handle(_ envelope: GatewayEnvelope) {
        let generation = connectionGeneration
        switch envelope {
        case .paired(_, let token):
            guard let account = pendingPairingAccount else { return }
            do {
                try store.save(account, token: token)
                accounts = store.loadAccounts()
                selectedAccountID = account.id
                pendingPairingAccount = nil
                pairingCode = ""
                pairingError = nil
                onEnvelope?(envelope)
            } catch {
                pairingError = localizedErrorDescription(error)
                onDisconnected?(localizedErrorDescription(error))
            }
        case .authenticated:
            connectionState = .loading
            onEnvelope?(envelope)
        case .ready:
            connectionState = .ready
            cancelReconnect()
            reconnectAttempt = 0
            automaticReconnectBlocked = false
            onEnvelope?(envelope)
        case .rejected(let rejection) where rejection.fatal:
            blockAutomaticReconnect()
            onEnvelope?(envelope)
            connectionEnded(generation: generation, message: rejection.message)
        case .error(let failure):
            if failure.code == "unauthorized", pendingPairingAccount == nil {
                blockAutomaticReconnect()
                repairSelectedGateway()
                onPairingRepairRequired?()
            }
            if failure.fatal { blockAutomaticReconnect() }
            onEnvelope?(envelope)
            if failure.fatal {
                connectionEnded(generation: generation, message: failure.message)
            }
        default:
            onEnvelope?(envelope)
        }
    }

    private func beginConnection(
        to endpoint: GatewayEndpoint,
        generation: UUID,
        authenticate: @escaping @MainActor @Sendable () async throws -> Void
    ) {
        guard generation == connectionGeneration else { return }
        connectionState = .connecting
        let connectionOpener = connectionOpener
        openingTask = Task { [weak self] in
            do {
                let stream = try await connectionOpener(endpoint)
                guard let self, !Task.isCancelled,
                    generation == self.connectionGeneration
                else { return }
                self.connectionState = .authenticating
                self.eventTask = Task { [weak self] in
                    do {
                        var handledFrames = 0
                        for try await frame in stream {
                            guard let self, generation == self.connectionGeneration else { return }
                            self.handle(frame)
                            handledFrames += 1
                            if handledFrames.isMultiple(of: 32) { await Task.yield() }
                        }
                        self?.connectionEnded(
                            generation: generation,
                            message: "The gateway closed the connection."
                        )
                    } catch {
                        self?.connectionEnded(generation: generation, error: error)
                    }
                }
                if self.reconnectsUntilReady { self.scheduleReconnect() }
                try await authenticate()
            } catch {
                self?.connectionEnded(generation: generation, error: error)
            }
        }
    }

    private func send(_ request: GatewayRequest, generation: UUID) async throws {
        guard generation == connectionGeneration, !connectionState.isConnecting else {
            throw GatewayWireError.disconnected
        }
        try await requestSender(request)
        guard generation == connectionGeneration else { throw GatewayWireError.disconnected }
    }

    func connectionEnded(generation: UUID, error: Error) {
        guard generation == connectionGeneration else { return }
        if case .unsupportedVersion(let version) = error as? GatewayWireError,
            version > gatewayProtocolVersion
        {
            automaticReconnectBlocked = true
            onUpdateRequired?()
            connectionEnded(
                generation: generation,
                message: "Update möbius to connect to this gateway."
            )
            return
        }
        connectionEnded(generation: generation, message: error.localizedDescription)
    }

    func connectionEnded(generation: UUID, message: String) {
        guard generation == connectionGeneration else { return }
        let wasPairing = pendingPairingAccount != nil
        resetTransport()
        connectionState = .failed(message)
        if wasPairing { pairingError = message }
        onDisconnected?(message)
        scheduleReconnect()
    }

    private func scheduleReconnect() {
        guard reconnectTask == nil,
            !automaticReconnectBlocked,
            pendingPairingAccount == nil,
            let account = selectedAccount
        else { return }
        guard !appIsInBackground else {
            reconnectsOnActivation = true
            return
        }
        let attempt = reconnectAttempt
        reconnectAttempt += 1
        let generation = connectionGeneration
        let delay = reconnectDelay(attempt)
        reconnectTask = Task { [weak self] in
            do {
                try await Task.sleep(for: delay)
            } catch {
                return
            }
            guard let self, !Task.isCancelled,
                generation == connectionGeneration,
                selectedAccountID == account.id
            else { return }
            reconnectTask = nil
            connect(to: account, retrying: true)
        }
    }

    private func resetTransport() {
        connectionGeneration = UUID()
        openingTask?.cancel()
        openingTask = nil
        eventTask?.cancel()
        eventTask = nil
        reconnectTask?.cancel()
        reconnectTask = nil
        disconnectTask?.cancel()
        disconnectTask = nil
    }

    func updateMachineName(_ machineName: String) {
        gatewayMachineName = machineName
        guard let account = selectedAccount,
            account.machineName != machineName,
            let index = accounts.firstIndex(where: { $0.id == account.id })
        else { return }
        accounts[index].machineName = machineName
        try? store.recordMachineName(machineName, for: account)
    }

    func cancelReconnect() {
        reconnectTask?.cancel()
        reconnectTask = nil
    }

    private func localizedString(_ resource: LocalizedStringResource) -> String {
        var resource = resource
        resource.locale = locale
        return String(localized: resource)
    }

    private func localizedErrorDescription(_ error: Error) -> String {
        if let error = error as? GatewayWireError {
            return localizedString(error.localizedDescriptionResource)
        }
        if let resource = (error as? GatewayStore.StoreError)?.localizedDescriptionResource {
            return localizedString(resource)
        }
        return error.localizedDescription
    }

}
