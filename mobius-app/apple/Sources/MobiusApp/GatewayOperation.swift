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

struct MessageReply: Codable, Hashable, Sendable {
    let target: MessageTarget
    let text: String

    init(target: MessageTarget, text: String) {
        self.target = target
        self.text = text
    }

    init(json: JSONValue) throws {
        guard let targetValue = json["target"],
              let target = MessageTarget(json: targetValue),
              let text = json["text"]?.stringValue,
              !text.isEmpty,
              text.utf8.count <= maximumComposerBytes
        else {
            throw GatewayWireError.invalidFrame("message reply is invalid")
        }
        self.init(target: target, text: text)
    }
}

enum ActiveMessageDelivery: String, CaseIterable, Codable, Hashable, Sendable {
    case steer
    case queue
}

enum MessageDelivery: String, Codable, Hashable, Sendable {
    case turn
    case steer
    case queue

    var startsTurn: Bool { self != .steer }
}

enum MessageAuthor: Codable, Hashable, Sendable {
    case user
    case peer(messageID: String, sessionID: String, handle: String, symbol: String?)

    init(from decoder: Decoder) throws {
        try self.init(json: JSONValue(from: decoder))
    }

    init(json: JSONValue) throws {
        guard let type = json["type"]?.stringValue else {
            throw GatewayWireError.invalidFrame("message author has no type")
        }
        switch type {
        case "user":
            self = .user
        case "peer":
            guard let messageID = json["messageId"]?.stringValue,
                  !messageID.isEmpty,
                  let sessionID = json["sessionId"]?.stringValue,
                  !sessionID.isEmpty,
                  let handle = json["handle"]?.stringValue,
                  !handle.isEmpty
            else {
                throw GatewayWireError.invalidFrame("peer message author is incomplete")
            }
            let symbol = json["symbol"]
            guard symbol == nil || symbol == .null || symbol?.stringValue != nil else {
                throw GatewayWireError.invalidFrame("peer message author has an invalid symbol")
            }
            if let value = symbol?.stringValue,
               value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || value.utf8.count > 256 {
                throw GatewayWireError.invalidFrame("peer message author has an invalid symbol")
            }
            self = .peer(
                messageID: messageID, sessionID: sessionID, handle: handle,
                symbol: symbol?.stringValue
            )
        default:
            throw GatewayWireError.invalidFrame("unknown message author \(type)")
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: DynamicCodingKey.self)
        switch self {
        case .user:
            try container.encode("user", forKey: "type")
        case .peer(let messageID, let sessionID, let handle, let symbol):
            try container.encode("peer", forKey: "type")
            try container.encode(messageID, forKey: "messageId")
            try container.encode(sessionID, forKey: "sessionId")
            try container.encode(handle, forKey: "handle")
            try container.encodeIfPresent(symbol, forKey: "symbol")
        }
    }
}

extension MessageAuthor {
    var peerFields: (messageID: String, sessionID: String, handle: String, symbol: String?)? {
        guard case .peer(let messageID, let sessionID, let handle, let symbol) = self else { return nil }
        return (messageID, sessionID, handle, symbol)
    }
}

struct MessageSubmission: Codable, Hashable, Sendable {
    let author: MessageAuthor
    let text: String
    let attachments: [SessionFileReference]
    let reply: MessageReply?
    let requestedDelivery: ActiveMessageDelivery?
    let targetTurnId: String?

    init(
        author: MessageAuthor,
        text: String,
        attachments: [SessionFileReference],
        reply: MessageReply? = nil,
        requestedDelivery: ActiveMessageDelivery?,
        targetTurnId: String?
    ) {
        self.author = author
        self.text = text
        self.attachments = attachments
        self.reply = reply
        self.requestedDelivery = requestedDelivery
        self.targetTurnId = targetTurnId
    }

    init(from decoder: Decoder) throws {
        try self.init(json: JSONValue(from: decoder))
    }

    init(json: JSONValue) throws {
        guard let author = json["author"],
              let text = json["text"]?.stringValue,
              let attachmentValues = json["attachments"]?.arrayValue,
              attachmentValues.count <= maximumWireSessionFileReferences,
              let replyValue = json["reply"],
              let requestedDeliveryValue = json["requestedDelivery"],
              let targetTurnValue = json["targetTurnId"]
        else {
            throw GatewayWireError.invalidFrame("message submission is incomplete")
        }
        let requestedDelivery: ActiveMessageDelivery?
        if requestedDeliveryValue != .null {
            guard let rawValue = requestedDeliveryValue.stringValue,
                  let decoded = ActiveMessageDelivery(rawValue: rawValue)
            else {
                throw GatewayWireError.invalidFrame("message submission has invalid delivery")
            }
            requestedDelivery = decoded
        } else {
            requestedDelivery = nil
        }
        let targetTurnId: String?
        if targetTurnValue != .null {
            guard let decoded = targetTurnValue.stringValue, !decoded.isEmpty else {
                throw GatewayWireError.invalidFrame("message submission has invalid target turn")
            }
            targetTurnId = decoded
        } else {
            targetTurnId = nil
        }
        let reply: MessageReply? = if replyValue == .null {
            nil
        } else {
            try MessageReply(json: replyValue)
        }
        self.init(
            author: try MessageAuthor(json: author),
            text: text,
            attachments: try attachmentValues.map(SessionFileReference.init(json:)),
            reply: reply,
            requestedDelivery: requestedDelivery,
            targetTurnId: targetTurnId
        )
    }

    func encode(to encoder: Encoder) throws {
        guard attachments.count <= maximumWireSessionFileReferences else {
            throw GatewayWireError.invalidFrame("message submission has too many attachments")
        }
        var container = encoder.container(keyedBy: DynamicCodingKey.self)
        try container.encode(author, forKey: "author")
        try container.encode(text, forKey: "text")
        try container.encode(attachments, forKey: "attachments")
        try container.encode(reply, forKey: "reply")
        try container.encode(requestedDelivery, forKey: "requestedDelivery")
        try container.encode(targetTurnId, forKey: "targetTurnId")
    }
}

struct MessageEventPayload: Hashable, Sendable {
    let author: MessageAuthor
    let delivery: MessageDelivery
    let text: String
    let attachments: [SessionFileReference]
    let reply: MessageReply?
    let messageTarget: MessageTarget?

    init(json: JSONValue) throws {
        guard let author = json["author"],
              let deliveryValue = json["delivery"]?.stringValue,
              let delivery = MessageDelivery(rawValue: deliveryValue),
              let text = json["text"]?.stringValue,
              let attachmentValues = json["attachments"]?.arrayValue,
              attachmentValues.count <= maximumWireSessionFileReferences,
              let targetValue = json["messageTarget"]
        else {
            throw GatewayWireError.invalidFrame("message event is incomplete")
        }
        let messageTarget: MessageTarget?
        if targetValue == .null {
            messageTarget = nil
        } else {
            guard let decoded = MessageTarget(json: targetValue) else {
                throw GatewayWireError.invalidFrame("message event has invalid target")
            }
            messageTarget = decoded
        }
        self.author = try MessageAuthor(json: author)
        self.delivery = delivery
        self.text = text
        self.attachments = try attachmentValues.map(SessionFileReference.init(json:))
        if let replyValue = json["reply"], replyValue != .null {
            reply = try MessageReply(json: replyValue)
        } else {
            reply = nil
        }
        self.messageTarget = messageTarget
    }
}

enum AgentOperation: Codable, Sendable {
    case message(MessageSubmission)
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
        case "message":
            guard let message = value["message"] else {
                throw GatewayWireError.invalidFrame("message operation has no message")
            }
            self = .message(try MessageSubmission(json: message))
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
        case .message(let message):
            try container.encode("message", forKey: "type")
            try container.encode(message, forKey: "message")
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
