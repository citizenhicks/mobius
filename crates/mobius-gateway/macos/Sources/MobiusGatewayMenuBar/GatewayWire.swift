import Foundation

// Checked against the Rust gateway by build.sh. This frontend consumes only voice and session data.
let gatewayProtocolVersion = 77
let maximumGatewayFrameBytes = 50 * 1024 * 1024

enum GatewayWireError: LocalizedError {
    case invalidEndpoint(String)
    case invalidFrame(String)
    case oversizedFrame(Int)
    case disconnected

    var errorDescription: String? {
        switch self {
        case .invalidFrame(let message), .invalidEndpoint(let message): message
        case .oversizedFrame: "The gateway sent an oversized message."
        case .disconnected: "The gateway disconnected."
        }
    }
}

/// The desktop gateway hands off only its authenticated loopback listener.
struct GatewayEndpoint: Sendable {
    let rawValue: String
    let host: String
    let port: UInt16
    var usesTLS: Bool { false }
    var usesWebSocket: Bool { false }

    init(_ value: String) throws {
        guard let url = URLComponents(string: value), url.scheme == "tcp",
            let host = url.host, ["127.0.0.1", "[::1]", "::1"].contains(host),
            let port = url.port, let port = UInt16(exactly: port), port > 0,
            url.user == nil, url.password == nil, url.query == nil, url.fragment == nil,
            url.path.isEmpty
        else { throw GatewayWireError.invalidFrame("Voice requires a local gateway connection.") }
        rawValue = value
        self.host = host.trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        self.port = port
    }
}

struct GatewayRequest: Encodable, Sendable {
    let body: JSONValue

    init(_ type: String, _ fields: [String: JSONValue] = [:]) {
        var fields = fields
        fields["type"] = .string(type)
        fields["version"] = .integer(Int64(gatewayProtocolVersion))
        body = .object(fields)
    }

    func encode(to encoder: Encoder) throws { try body.encode(to: encoder) }
}

struct GatewayEnvelope: Decodable, Sendable {
    let type: String
    let body: JSONValue

    init(from decoder: Decoder) throws {
        body = try JSONValue(from: decoder)
        guard body["version"]?.intValue == gatewayProtocolVersion else {
            throw GatewayWireError.invalidFrame("Update the gateway and menu bar app together.")
        }
        type = try body.requiredString("type")
    }
}

extension JSONValue {
    func requiredString(_ key: String) throws -> String {
        guard let value = self[key]?.stringValue, !value.isEmpty else {
            throw GatewayWireError.invalidFrame("The gateway response is missing \(key).")
        }
        return value
    }

    func decode<T: Decodable>(_ type: T.Type) throws -> T {
        try JSONDecoder().decode(type, from: JSONEncoder().encode(self))
    }
}

struct VoiceChat: Decodable, Identifiable, Equatable {
    var id: String { sessionId }
    let sessionId: String
    let sessionContext: Context
    let parentSessionId: String?
    let title: String?
    let firstUserMessage: String?
    let updatedAt: Int64
    let activity: Activity

    var name: String { title ?? firstUserMessage.map { String($0.prefix(80)) } ?? "New chat" }
    var workspace: String { sessionContext.workspaceLabel ?? "." }

    struct Context: Decodable, Equatable {
        let botId: String
        let workspaceId: String?
        let workspaceLabel: String?
    }

    struct Activity: Decodable, Equatable {
        let state: String
        let approvalRequestId: String?
    }
}

struct VoiceBot: Decodable, Identifiable {
    let id: String
    let handle: String
    let name: String
    let description: String
    let tint: String
}

struct VoiceModel: Decodable {
    let route: String
    let supportsRealtimeVoice: Bool
}

struct VoiceCatalog: Decodable {
    let sessions: [VoiceChat]
    let bots: [VoiceBot]
    let models: [VoiceModel]
}

struct VoiceApproval: Identifiable {
    let id: String
    let reason: String
    let calls: [Call]

    struct Call: Identifiable {
        let id: String
        let name: String
        let arguments: String
    }

    init(_ event: JSONValue) throws {
        id = try event.requiredString("id")
        reason = try event.requiredString("reason")
        guard let calls = event["calls"]?.arrayValue, !calls.isEmpty else {
            throw GatewayWireError.invalidFrame("An approval request has no calls.")
        }
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        self.calls = try calls.map { call in
            guard let arguments = call["arguments"] else {
                throw GatewayWireError.invalidFrame("An approval request has no arguments.")
            }
            return Call(
                id: try call.requiredString("callId"), name: try call.requiredString("name"),
                arguments: String(decoding: try encoder.encode(arguments), as: UTF8.self)
            )
        }
    }
}
