import Foundation
import Network

actor GatewayClient {
    private let queue = DispatchQueue(label: "app.mobius.gateway", qos: .userInitiated)
    private let encoder: JSONEncoder
    private let decoder: JSONDecoder
    private var connection: NWConnection?
    private var webSocketSession: URLSession?
    private var webSocketTask: URLSessionWebSocketTask?
    private var streamContinuation: AsyncThrowingStream<GatewayEnvelope, Error>.Continuation?
    private var connectionGeneration = UUID()

    init() {
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        self.encoder = encoder

        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        self.decoder = decoder
    }

    func connect(to endpoint: GatewayEndpoint) async throws -> AsyncThrowingStream<GatewayEnvelope, Error> {
        disconnect()
        if endpoint.usesWebSocket {
            return try await connectWebSocket(to: endpoint)
        }
        guard let port = NWEndpoint.Port(rawValue: endpoint.port) else {
            throw GatewayWireError.invalidEndpoint("Gateway port is invalid.")
        }

        let parameters: NWParameters
        if endpoint.usesTLS {
            parameters = NWParameters(
                tls: NWProtocolTLS.Options(),
                tcp: NWProtocolTCP.Options()
            )
        } else {
            parameters = .tcp
        }

        let connection = NWConnection(
            host: NWEndpoint.Host(endpoint.host),
            port: port,
            using: parameters
        )
        let generation = UUID()
        connectionGeneration = generation
        self.connection = connection
        do {
            try await start(connection)
        } catch {
            cancel(generation: generation)
            throw error
        }
        guard connectionGeneration == generation else {
            connection.cancel()
            throw GatewayWireError.disconnected
        }

        var continuation: AsyncThrowingStream<GatewayEnvelope, Error>.Continuation!
        let stream = AsyncThrowingStream<GatewayEnvelope, Error> { continuation = $0 }
        continuation.onTermination = { [weak self] _ in
            Task { await self?.cancel(generation: generation) }
        }
        streamContinuation = continuation
        Task { await receiveFrames(from: connection, generation: generation, into: continuation) }
        return stream
    }

    func send(_ request: GatewayRequest) async throws {
        let payload = try encoder.encode(request)
        guard !payload.isEmpty, payload.count <= maximumGatewayFrameBytes else {
            throw GatewayWireError.oversizedFrame(payload.count)
        }
        if let webSocketTask {
            try await webSocketTask.send(.data(payload))
            return
        }
        guard let connection else { throw GatewayWireError.disconnected }
        guard let length = UInt32(exactly: payload.count) else {
            throw GatewayWireError.oversizedFrame(payload.count)
        }
        var prefix = length.bigEndian
        var frame = withUnsafeBytes(of: &prefix) { Data($0) }
        frame.append(payload)
        try await send(frame, over: connection)
    }

    func disconnect() {
        streamContinuation?.finish()
        streamContinuation = nil
        connection?.cancel()
        connection = nil
        webSocketTask?.cancel(with: .goingAway, reason: nil)
        webSocketTask = nil
        webSocketSession?.invalidateAndCancel()
        webSocketSession = nil
        connectionGeneration = UUID()
    }

    private func connectWebSocket(
        to endpoint: GatewayEndpoint
    ) async throws -> AsyncThrowingStream<GatewayEnvelope, Error> {
        guard let url = URL(string: endpoint.rawValue) else {
            throw GatewayWireError.invalidEndpoint("The WebSocket gateway address is invalid.")
        }
        let generation = UUID()
        connectionGeneration = generation
        do {
            try await withCheckedThrowingContinuation {
                (continuation: CheckedContinuation<Void, Error>) in
                let delegate = WebSocketStartDelegate(continuation)
                let session = URLSession(
                    configuration: .default,
                    delegate: delegate,
                    delegateQueue: nil
                )
                let task = session.webSocketTask(with: url)
                task.maximumMessageSize = maximumGatewayFrameBytes
                webSocketSession = session
                webSocketTask = task
                task.resume()
            }
        } catch {
            cancel(generation: generation)
            throw error
        }
        guard connectionGeneration == generation, let task = webSocketTask else {
            throw GatewayWireError.disconnected
        }

        var continuation: AsyncThrowingStream<GatewayEnvelope, Error>.Continuation!
        let stream = AsyncThrowingStream<GatewayEnvelope, Error> { continuation = $0 }
        continuation.onTermination = { [weak self] _ in
            Task { await self?.cancel(generation: generation) }
        }
        streamContinuation = continuation
        Task { await receiveFrames(from: task, generation: generation, into: continuation) }
        return stream
    }

    private func start(_ connection: NWConnection) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            let gate = ConnectionStartGate(continuation)
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    if gate.succeed() { connection.stateUpdateHandler = nil }
                case .failed(let error), .waiting(let error):
                    if gate.fail(error) {
                        connection.stateUpdateHandler = nil
                        connection.cancel()
                    }
                case .cancelled:
                    if gate.fail(GatewayWireError.disconnected) {
                        connection.stateUpdateHandler = nil
                    }
                default:
                    break
                }
            }
            connection.start(queue: queue)
        }
    }

    private func send(_ data: Data, over connection: NWConnection) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            connection.send(content: data, completion: .contentProcessed { error in
                if let error { continuation.resume(throwing: error) }
                else { continuation.resume() }
            })
        }
    }

    private func receiveFrames(
        from connection: NWConnection,
        generation: UUID,
        into continuation: AsyncThrowingStream<GatewayEnvelope, Error>.Continuation
    ) async {
        do {
            while !Task.isCancelled {
                let prefix = try await receiveExactly(4, from: connection)
                let length = prefix.reduce(UInt32.zero) { ($0 << 8) | UInt32($1) }
                guard length > 0, length <= maximumGatewayFrameBytes else {
                    throw GatewayWireError.oversizedFrame(Int(length))
                }
                let payload = try await receiveExactly(Int(length), from: connection)
                continuation.yield(try decoder.decode(GatewayEnvelope.self, from: payload))
            }
        } catch {
            continuation.finish(throwing: error)
        }
        cancel(generation: generation)
    }

    private func receiveFrames(
        from task: URLSessionWebSocketTask,
        generation: UUID,
        into continuation: AsyncThrowingStream<GatewayEnvelope, Error>.Continuation
    ) async {
        do {
            while !Task.isCancelled {
                let message = try await task.receive()
                guard case .data(let payload) = message else {
                    throw GatewayWireError.invalidFrame(
                        "WebSocket gateways must send binary messages."
                    )
                }
                guard !payload.isEmpty, payload.count <= maximumGatewayFrameBytes else {
                    throw GatewayWireError.oversizedFrame(payload.count)
                }
                continuation.yield(try decoder.decode(GatewayEnvelope.self, from: payload))
            }
        } catch {
            continuation.finish(throwing: error)
        }
        cancel(generation: generation)
    }

    private func receiveExactly(_ count: Int, from connection: NWConnection) async throws -> Data {
        var data = Data()
        while data.count < count {
            let remaining = count - data.count
            let chunk = try await receive(maximumLength: remaining, from: connection)
            guard !chunk.isEmpty else { throw GatewayWireError.disconnected }
            data.append(chunk)
        }
        return data
    }

    private func receive(maximumLength: Int, from connection: NWConnection) async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            connection.receive(
                minimumIncompleteLength: 1,
                maximumLength: maximumLength
            ) { data, _, isComplete, error in
                if let error { continuation.resume(throwing: error) }
                else if let data, !data.isEmpty { continuation.resume(returning: data) }
                else if isComplete { continuation.resume(throwing: GatewayWireError.disconnected) }
                else { continuation.resume(returning: Data()) }
            }
        }
    }

    private func cancel(generation: UUID) {
        guard connectionGeneration == generation else { return }
        disconnect()
    }
}

private final class WebSocketStartDelegate: NSObject, URLSessionWebSocketDelegate,
    @unchecked Sendable {
    private let gate: ConnectionStartGate

    init(_ continuation: CheckedContinuation<Void, Error>) {
        gate = ConnectionStartGate(continuation)
    }

    func urlSession(
        _ session: URLSession,
        webSocketTask: URLSessionWebSocketTask,
        didOpenWithProtocol protocol: String?
    ) {
        _ = gate.succeed()
    }

    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        didCompleteWithError error: Error?
    ) {
        _ = gate.fail(error ?? GatewayWireError.disconnected)
    }
}

private final class ConnectionStartGate: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<Void, Error>?

    init(_ continuation: CheckedContinuation<Void, Error>) {
        self.continuation = continuation
    }

    func succeed() -> Bool {
        resume(with: .success(()))
    }

    func fail(_ error: Error) -> Bool {
        resume(with: .failure(error))
    }

    private func resume(with result: Result<Void, Error>) -> Bool {
        lock.lock()
        guard let continuation else {
            lock.unlock()
            return false
        }
        self.continuation = nil
        lock.unlock()
        continuation.resume(with: result)
        return true
    }
}
