import Foundation

struct GatewayInstallation: Sendable {
    let executable: URL
    let stateDirectory: String?

    init(arguments: [String] = CommandLine.arguments, bundle: Bundle = .main) throws {
        var executable = bundle.url(forAuxiliaryExecutable: "mobius-gateway")
        var stateDirectory: String?
        var arguments = arguments.dropFirst().makeIterator()
        while let argument = arguments.next() {
            guard let value = arguments.next(), value.hasPrefix("/"), value.utf8.count <= 4096
            else {
                throw GatewayWireError.invalidFrame("The gateway launch arguments are invalid.")
            }
            switch argument {
            case "--gateway-executable": executable = URL(fileURLWithPath: value)
            case "--state-dir": stateDirectory = value
            default:
                throw GatewayWireError.invalidFrame("The gateway launch arguments are invalid.")
            }
        }
        guard let executable, FileManager.default.isExecutableFile(atPath: executable.path) else {
            throw GatewayWireError.invalidFrame(
                "Reinstall möbius-app; its gateway executable is missing.")
        }
        self.executable = executable
        self.stateDirectory = stateDirectory
    }

    func connection() async throws -> LocalGatewayConnection {
        let process = Process()
        process.executableURL = executable
        process.arguments = ["__menu-bar-connect"]
        if let stateDirectory {
            process.arguments?.append(contentsOf: ["--state-dir", stateDirectory])
        }
        var environment = ProcessInfo.processInfo.environment
        environment["MOBIUS_GATEWAY_NO_MENU_BAR"] = "1"
        process.environment = environment
        process.standardInput = FileHandle.nullDevice
        let output = Pipe()
        let errors = Pipe()
        process.standardOutput = output
        process.standardError = errors
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                process.terminationHandler = { process in
                    do {
                        let data = try output.fileHandleForReading.read(upToCount: 8193) ?? Data()
                        guard process.terminationStatus == 0 else {
                            let errorData =
                                try errors.fileHandleForReading.read(upToCount: 16384) ?? Data()
                            let message = String(decoding: errorData, as: UTF8.self)
                                .trimmingCharacters(in: .whitespacesAndNewlines)
                            throw GatewayWireError.invalidFrame(
                                message.isEmpty ? "The gateway could not start." : message)
                        }
                        guard data.count <= 8192 else {
                            throw GatewayWireError.oversizedFrame(data.count)
                        }
                        continuation.resume(returning: try LocalGatewayConnection(data: data))
                    } catch { continuation.resume(throwing: error) }
                }
                do {
                    try Task.checkCancellation()
                    try process.run()
                } catch { continuation.resume(throwing: error) }
            }
        } onCancel: {
            if process.isRunning { process.terminate() }
        }
    }
}

struct LocalGatewayConnection: Sendable {
    let endpoint: GatewayEndpoint
    let token: String

    init(data: Data) throws {
        struct Handoff: Decodable {
            let endpoint: String
            let token: String
            let protocolVersion: Int
        }
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let handoff = try decoder.decode(Handoff.self, from: data)
        guard handoff.protocolVersion == gatewayProtocolVersion else {
            throw GatewayWireError.invalidFrame("Update the gateway and menu bar app together.")
        }
        guard !handoff.token.isEmpty, handoff.token.utf8.count <= 512,
            handoff.token == handoff.token.trimmingCharacters(in: .whitespacesAndNewlines)
        else { throw GatewayWireError.invalidFrame("The local gateway credential is invalid.") }
        endpoint = try GatewayEndpoint(handoff.endpoint)
        token = handoff.token
    }
}
