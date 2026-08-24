import Foundation

struct Submission: Encodable, Sendable {
    let id: String
    let op: AgentOperation
}
struct SessionFileReference: Identifiable, Codable, Hashable, Sendable {
    private enum CodingKeys: String, CodingKey { case id, name, size, mediaType }

    let id: String
    let name: String
    let size: Int64
    let mediaType: String

    init(id: String, name: String, size: Int64, mediaType: String) {
        self.id = id
        self.name = name
        self.size = size
        self.mediaType = mediaType
    }

    init(json: JSONValue) throws {
        guard let id = json["id"]?.stringValue,
              !id.isEmpty,
              let name = json["name"]?.stringValue,
              !name.isEmpty,
              let size = json["size"]?.intValue,
              size >= 0,
              let mediaType = json["mediaType"]?.stringValue,
              !mediaType.isEmpty
        else {
            throw GatewayWireError.invalidFrame("session file is missing a required field")
        }
        self.init(id: id, name: name, size: Int64(size), mediaType: mediaType)
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let id = try container.decode(String.self, forKey: .id)
        let name = try container.decode(String.self, forKey: .name)
        let size = try container.decode(Int64.self, forKey: .size)
        let mediaType = try container.decode(String.self, forKey: .mediaType)
        guard !id.isEmpty, !name.isEmpty, size >= 0, !mediaType.isEmpty else {
            throw GatewayWireError.invalidFrame("session file is missing a required field")
        }
        self.init(id: id, name: name, size: size, mediaType: mediaType)
    }
}

enum SessionFileOrigin: String, Codable, Hashable, Sendable {
    case user
    case agent
}

struct SessionFileRecord: Identifiable, Codable, Hashable, Sendable {
    let origin: SessionFileOrigin
    let file: SessionFileReference

    var id: String { file.id }
}

struct MessageTarget: Codable, Hashable, Sendable {
    let checkpointSequence: UInt64
    let batchItemCount: Int

    init(checkpointSequence: UInt64, batchItemCount: Int) {
        self.checkpointSequence = checkpointSequence
        self.batchItemCount = batchItemCount
    }

    init?(json: JSONValue) {
        guard let sequenceValue = json["checkpointSequence"],
              case .number(let sequence) = sequenceValue,
              let checkpointSequence = UInt64(exactly: sequence),
              let countValue = json["batchItemCount"],
              case .number(let count) = countValue,
              let batchItemCount = Int(exactly: count),
              batchItemCount > 0
        else { return nil }
        self.init(checkpointSequence: checkpointSequence, batchItemCount: batchItemCount)
    }
}

enum AgentOperation: Codable, Sendable {
    case userInput(text: String, attachments: [SessionFileReference])
    case activeInput(operation: String, turnID: String, text: String)
    case interrupt(turnID: String)
    case execApproval(id: String, decision: ReviewDecision)
    case capabilityCommand(
        capability: String,
        command: String,
        arguments: String,
        input: String?,
        target: MessageTarget?
    )
    case setModel(route: String)
    case resumeSession(sessionID: String)

    init(from decoder: Decoder) throws {
        try self.init(json: JSONValue(from: decoder))
    }

    init(json value: JSONValue) throws {
        guard let type = value["type"]?.stringValue else {
            throw GatewayWireError.invalidFrame("agent operation has no type")
        }
        func required(_ key: String) throws -> String {
            guard let string = value[key]?.stringValue else {
                throw GatewayWireError.invalidFrame("\(type) has no \(key)")
            }
            return string
        }
        switch type {
        case "user_input":
            guard let values = value["attachments"]?.arrayValue,
                  values.count <= maximumWireSessionFileReferences
            else {
                throw GatewayWireError.invalidFrame("user_input has invalid attachments")
            }
            self = .userInput(
                text: try required("text"),
                attachments: try values.map(SessionFileReference.init(json:))
            )
        case "active_input":
            self = .activeInput(
                operation: try required("operation"),
                turnID: try required("turnId"),
                text: try required("text")
            )
        case "interrupt":
            self = .interrupt(turnID: try required("turnId"))
        case "exec_approval":
            guard let decision = value["decision"] else {
                throw GatewayWireError.invalidFrame("exec_approval has no decision")
            }
            self = .execApproval(
                id: try required("id"),
                decision: try ReviewDecision(json: decision)
            )
        case "capability_command":
            guard let inputValue = value["input"], let targetValue = value["target"] else {
                throw GatewayWireError.invalidFrame("capability_command has no input or target")
            }
            let input: String?
            switch inputValue {
            case .string(let value): input = value
            case .null: input = nil
            default: throw GatewayWireError.invalidFrame("capability_command has invalid input")
            }
            let target: MessageTarget?
            if targetValue != .null {
                guard let decoded = MessageTarget(json: targetValue) else {
                    throw GatewayWireError.invalidFrame("capability_command has invalid target")
                }
                target = decoded
            } else {
                target = nil
            }
            self = .capabilityCommand(
                capability: try required("capability"),
                command: try required("command"),
                arguments: try required("arguments"),
                input: input,
                target: target
            )
        case "set_model":
            self = .setModel(route: try required("route"))
        case "resume_session":
            self = .resumeSession(sessionID: try required("sessionId"))
        default:
            throw GatewayWireError.invalidFrame("unknown agent operation \(type)")
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: DynamicCodingKey.self)
        switch self {
        case .userInput(let text, let attachments):
            guard attachments.count <= maximumWireSessionFileReferences else {
                throw GatewayWireError.invalidFrame("user_input has too many attachments")
            }
            try container.encode("user_input", forKey: "type")
            try container.encode(text, forKey: "text")
            try container.encode(attachments, forKey: "attachments")
        case .activeInput(let operation, let turnID, let text):
            try container.encode("active_input", forKey: "type")
            try container.encode(operation, forKey: "operation")
            try container.encode(turnID, forKey: "turnId")
            try container.encode(text, forKey: "text")
        case .interrupt(let turnID):
            try container.encode("interrupt", forKey: "type")
            try container.encode(turnID, forKey: "turnId")
        case .execApproval(let id, let decision):
            try container.encode("exec_approval", forKey: "type")
            try container.encode(id, forKey: "id")
            try container.encode(decision, forKey: "decision")
        case .capabilityCommand(let capability, let command, let arguments, let input, let target):
            try container.encode("capability_command", forKey: "type")
            try container.encode(capability, forKey: "capability")
            try container.encode(command, forKey: "command")
            try container.encode(arguments, forKey: "arguments")
            try container.encode(input, forKey: "input")
            try container.encode(target, forKey: "target")
        case .setModel(let route):
            try container.encode("set_model", forKey: "type")
            try container.encode(route, forKey: "route")
        case .resumeSession(let sessionID):
            try container.encode("resume_session", forKey: "type")
            try container.encode(sessionID, forKey: "sessionId")
        }
    }
}

extension AgentOperation {
    var capabilityInput: String? {
        guard case .capabilityCommand(_, _, _, let input, _) = self else { return nil }
        return input
    }

    func replacingCapabilityInput(with input: String) -> Self {
        guard case .capabilityCommand(
            let capability,
            let command,
            let arguments,
            _,
            let target
        ) = self else { return self }
        return .capabilityCommand(
            capability: capability,
            command: command,
            arguments: arguments,
            input: input,
            target: target
        )
    }
}

enum ReviewDecision: Codable, Sendable {
    case approved
    case approvedForSession
    case denied(rejection: String)
    case abort

    init(from decoder: Decoder) throws {
        self = try Self(json: JSONValue(from: decoder))
    }

    init(json: JSONValue) throws {
        if let value = json.stringValue {
            switch value {
            case "approved": self = .approved
            case "approved_for_session": self = .approvedForSession
            case "abort": self = .abort
            default: throw GatewayWireError.invalidFrame("unknown review decision \(value)")
            }
            return
        }
        if let rejection = json["denied"]?["rejection"]?.stringValue {
            self = .denied(rejection: rejection)
            return
        }
        throw GatewayWireError.invalidFrame("invalid review decision")
    }

    func encode(to encoder: Encoder) throws {
        switch self {
        case .approved:
            var container = encoder.singleValueContainer()
            try container.encode("approved")
        case .approvedForSession:
            var container = encoder.singleValueContainer()
            try container.encode("approved_for_session")
        case .denied(let rejection):
            var container = encoder.container(keyedBy: DynamicCodingKey.self)
            try container.encode(["rejection": rejection], forKey: "denied")
        case .abort:
            var container = encoder.singleValueContainer()
            try container.encode("abort")
        }
    }
}
