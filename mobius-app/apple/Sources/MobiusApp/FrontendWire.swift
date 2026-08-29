import Foundation

func frontendPresentationText(_ value: String) -> LocalizedStringResource {
    LocalizedStringResource(String.LocalizationValue(value))
}

struct FrontendContribution: Decodable, Sendable {
    let capability: String
    let acceptsFileAttachments: Bool
    let count: Int?
    let commands: [FrontendCommand]
    let widgets: [FrontendWidget]
    let references: [FrontendReference]
}
extension FrontendContribution {
    private enum CodingKeys: String, CodingKey {
        case capability
        case acceptsFileAttachments
        case count
        case commands
        case widgets
        case references
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        guard container.contains(.count) else {
            throw DecodingError.keyNotFound(
                CodingKeys.count,
                .init(
                    codingPath: container.codingPath,
                    debugDescription: "Frontend contribution requires count."
                )
            )
        }
        capability = try container.decode(String.self, forKey: .capability)
        acceptsFileAttachments = try container.decode(
            Bool.self,
            forKey: .acceptsFileAttachments
        )
        count = try container.decodeIfPresent(Int.self, forKey: .count)
        commands = try container.decode([FrontendCommand].self, forKey: .commands)
        widgets = try container.decode([FrontendWidget].self, forKey: .widgets)
        references = try container.decode([FrontendReference].self, forKey: .references)
    }
}

struct FrontendCommand: Codable, Hashable, Sendable {
    let name: String
    let arguments: String
    let description: String
}

struct FrontendProgress: Decodable, Sendable {
    let completed: Int
    let total: Int

    var fraction: Double { Double(completed) / Double(total) }
}

struct FrontendContentBlock: Identifiable, Sendable {
    let id = UUID()
    let block: FrontendBlock
}

enum FrontendWidgetContent: Sendable {
    case blocks(title: String, blocks: [FrontendContentBlock])
    case picker(title: String, options: [FrontendPickerOption])
    case actionList(title: String, items: [FrontendActionListItem])

    var title: String {
        switch self {
        case .blocks(let title, _), .picker(let title, _), .actionList(let title, _): title
        }
    }
}

struct FrontendActionListItem: Identifiable, Sendable {
    let id: String
    let text: String
    let state: FrontendListItemState
    let actions: [FrontendActionListAction]

    init(json: JSONValue) throws {
        guard let id = json["id"]?.stringValue,
              !id.isEmpty,
              let text = json["text"]?.stringValue,
              !text.isEmpty,
              let state = json["state"]?.stringValue.flatMap(FrontendListItemState.init(rawValue:)),
              let values = json["actions"]?.arrayValue
        else {
            throw GatewayWireError.invalidFrame("frontend action list item is missing a required field")
        }
        let actions = try values.map(FrontendActionListAction.init(json:))
        guard Set(actions.map(\.id)).count == actions.count else {
            throw GatewayWireError.invalidFrame("frontend action list item has duplicate action IDs")
        }
        self.id = id
        self.text = text
        self.state = state
        self.actions = actions
    }
}

enum FrontendListItemState: String, Equatable, Sendable {
    case plain
    case pending
    case inProgress = "in_progress"
    case completed
}

struct FrontendActionListAction: Identifiable, Sendable {
    let id: String
    let label: String
    let symbol: String
    let tone: String
    let op: AgentOperation

    init(json: JSONValue) throws {
        guard let id = json["id"]?.stringValue,
              !id.isEmpty,
              let label = json["label"]?.stringValue,
              !label.isEmpty,
              let symbol = json["symbol"]?.stringValue,
              !symbol.isEmpty,
              let tone = json["tone"]?.stringValue,
              ["neutral", "success", "warning", "error"].contains(tone),
              let op = json["op"]
        else {
            throw GatewayWireError.invalidFrame("frontend action list action is missing a required field")
        }
        self.id = id
        self.label = label
        self.symbol = symbol
        self.tone = tone
        self.op = try AgentOperation(json: op)
    }
}

enum FrontendSlot: String, Decodable, Equatable, Sendable {
    case header
    case transcriptTail = "transcript_tail"
    case composerHeader = "composer_header"
    case composerFooter = "composer_footer"
    case messageActions = "message_actions"
    case navigation
    case chatMenu = "chat_menu"
}

struct FrontendWidget: Identifiable, Decodable, Sendable {
    let id: String
    let slot: FrontendSlot
    let text: String
    let tone: String
    let symbol: String?
    let iconOnly: Bool
    let progress: FrontendProgress?
    let content: FrontendWidgetContent?
    let action: AgentOperation?
}

extension FrontendWidget {
    init(from decoder: Decoder) throws {
        try self.init(json: JSONValue(from: decoder))
    }

    init(json: JSONValue) throws {
        guard let id = json["id"]?.stringValue,
              let slot = json["slot"]?.stringValue,
              let text = json["text"]?.stringValue,
              let tone = json["tone"]?.stringValue,
              let iconOnly = json["iconOnly"]?.boolValue,
              json["symbol"] != nil,
              json["progress"] != nil,
              json["content"] != nil,
              json["action"] != nil
        else {
            throw GatewayWireError.invalidFrame("frontend widget is missing a required field")
        }
        guard let slot = FrontendSlot(rawValue: slot),
              ["neutral", "success", "warning", "error"].contains(tone)
        else {
            throw GatewayWireError.invalidFrame("frontend widget has an unknown slot or tone")
        }
        self.id = id
        self.slot = slot
        self.text = text
        self.tone = tone
        self.iconOnly = iconOnly
        switch json["symbol"] {
        case .some(.string(let symbol)): self.symbol = symbol
        case .some(.null): self.symbol = nil
        default: throw GatewayWireError.invalidFrame("frontend widget has an invalid symbol")
        }
        switch json["progress"] {
        case .some(.object(let value)):
            guard let completed = value["completed"]?.intValue,
                  let total = value["total"]?.intValue,
                  total > 0,
                  completed >= 0,
                  completed <= total
            else {
                throw GatewayWireError.invalidFrame("frontend widget has invalid progress")
            }
            progress = FrontendProgress(completed: completed, total: total)
        case .some(.null): progress = nil
        default: throw GatewayWireError.invalidFrame("frontend widget has invalid progress")
        }
        switch json["content"] {
        case .some(.object(let value)):
            guard let type = value["type"]?.stringValue,
                  let title = value["title"]?.stringValue
            else {
                throw GatewayWireError.invalidFrame("frontend widget content is missing a required field")
            }
            switch type {
            case "blocks":
                guard let values = value["blocks"]?.arrayValue else {
                    throw GatewayWireError.invalidFrame("frontend widget blocks are missing")
                }
                content = .blocks(
                    title: title,
                    blocks: try values.map { value in
                        FrontendContentBlock(block: try FrontendBlock(json: value))
                    }
                )
            case "picker":
                guard let values = value["options"]?.arrayValue else {
                    throw GatewayWireError.invalidFrame("frontend widget options are missing")
                }
                content = .picker(title: title, options: try values.map(FrontendPickerOption.init(json:)))
            case "action_list":
                guard let values = value["items"]?.arrayValue else {
                    throw GatewayWireError.invalidFrame("frontend widget action list items are missing")
                }
                let items = try values.map(FrontendActionListItem.init(json:))
                guard Set(items.map(\.id)).count == items.count else {
                    throw GatewayWireError.invalidFrame("frontend widget action list has duplicate item IDs")
                }
                content = .actionList(title: title, items: items)
            default:
                throw GatewayWireError.invalidFrame("frontend widget has unknown content")
            }
        case .some(.null): content = nil
        default: throw GatewayWireError.invalidFrame("frontend widget has invalid content")
        }
        if let action = json["action"], action != .null {
            self.action = try AgentOperation(json: action)
        } else {
            self.action = nil
        }
    }
}

struct FrontendReference: Codable, Hashable, Sendable {
    let trigger: Character
    let value: String
    let description: String

    init(trigger: Character, value: String, description: String) {
        self.trigger = trigger
        self.value = value
        self.description = description
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let encodedTrigger = try container.decode(String.self, forKey: .trigger)
        guard encodedTrigger.count == 1, let trigger = encodedTrigger.first else {
            throw GatewayWireError.invalidFrame("frontend reference trigger must be one character")
        }
        self.trigger = trigger
        self.value = try container.decode(String.self, forKey: .value)
        self.description = try container.decode(String.self, forKey: .description)
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(String(trigger), forKey: .trigger)
        try container.encode(value, forKey: .value)
        try container.encode(description, forKey: .description)
    }

    private enum CodingKeys: String, CodingKey {
        case trigger
        case value
        case description
    }
}

enum FrontendBlockUpdate: String, Codable, Hashable, Sendable {
    case replace
    case append
}

enum FrontendBlockState: String, Codable, Hashable, Sendable {
    case pending
    case complete
}

enum FrontendBlockRole: String, Codable, Hashable, Sendable {
    case activity
    case tool
    case webSearch = "web_search"
    case artifact
    case approval
    case notice
}

struct FrontendBlock: Codable, Hashable, Sendable {
    let id: String?
    let group: String?
    let update: FrontendBlockUpdate
    let state: FrontendBlockState
    let role: FrontendBlockRole
    let title: String
    let text: String
    let symbol: String?
    let format: String
    let tone: String
    let files: [SessionFileReference]

    var pending: Bool { state == .pending }
}

extension FrontendBlock {
    init(from decoder: Decoder) throws {
        try self.init(json: JSONValue(from: decoder))
    }

    init(json: JSONValue) throws {
        func optionalString(_ key: String) throws -> String? {
            switch json[key] {
            case nil, .some(.null): return nil
            case .some(.string(let value)): return value
            default: throw GatewayWireError.invalidFrame("frontend block has invalid \(key)")
            }
        }

        guard let encodedUpdate = json["update"]?.stringValue,
              let update = FrontendBlockUpdate(rawValue: encodedUpdate),
              let encodedState = json["state"]?.stringValue,
              let state = FrontendBlockState(rawValue: encodedState),
              let encodedRole = json["role"]?.stringValue,
              let role = FrontendBlockRole(rawValue: encodedRole),
              let title = json["title"]?.stringValue,
              let text = json["text"]?.stringValue,
              let format = json["format"]?.stringValue,
              ["plain_text", "unified_diff"].contains(format),
              let tone = json["tone"]?.stringValue,
              ["neutral", "success", "warning", "error"].contains(tone),
              let files = json["files"]?.arrayValue,
              files.count <= maximumWireSessionFileReferences
        else {
            throw GatewayWireError.invalidFrame("frontend block is missing a required field")
        }
        id = try optionalString("id")
        group = try optionalString("group")
        self.update = update
        self.state = state
        self.role = role
        self.title = title
        self.text = text
        symbol = try optionalString("symbol")
        self.format = format
        self.tone = tone
        self.files = try files.map(SessionFileReference.init(json:))
    }
}

struct RenderedBlock: Decodable, Sendable {
    let capability: String
    let block: FrontendBlock
}

struct RenderedPreview: Decodable, Sendable {
    let id: String
    let title: String
    let subtitle: String
    let pageId: String
    let update: FrontendPreviewUpdate
    let events: [RenderedEventRecord]
    let next: AgentOperation?
}

enum FrontendPreviewUpdate: String, Decodable, Sendable {
    case replace
    case prepend
}

struct RenderedEventRecord: Decodable, Sendable {
    let recordedAtMs: Int64
    let event: JSONValue
    let blocks: [RenderedBlock]
}

extension RenderedEventRecord {
    init(event: JSONValue, blocks: [RenderedBlock], recordedAtMs: Int64 = 0) {
        self.recordedAtMs = recordedAtMs
        self.event = event
        self.blocks = blocks
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: DynamicCodingKey.self)
        recordedAtMs = try container.decode(Int64.self, forKey: "recordedAtMs")
        let event = try container.decode(JSONValue.self, forKey: "event")
        try AgentEventRecord.validate(event)
        self.event = event
        blocks = try container.decode([RenderedBlock].self, forKey: "blocks")
    }
}

struct RecordedEvent: Decodable, Sendable {
    let sequence: UInt64
    let recordedAtMs: Int64
    let event: AgentEventRecord
    let streamMetrics: [StreamMetrics]
    let blocks: [RenderedBlock]
    let preview: RenderedPreview?
}

enum ModelStepContentPhase: String, Decodable, Sendable {
    case reasoning
    case commentary
    case finalAnswer = "final_answer"
}

struct StreamMetrics: Decodable, Sendable {
    let phase: ModelStepContentPhase
    let firstDeltaAtMs: Int64
    let lastDeltaAtMs: Int64
    let chunkCount: UInt64
    let utf8Bytes: UInt64
    let longestGapMs: UInt64
}

struct FrontendPickerOption: Identifiable, Sendable {
    let id = UUID()
    let label: String
    let description: String
    let detail: String
    let symbol: String?
    let showsDetail: Bool
    let op: AgentOperation

    init(json: JSONValue) throws {
        guard let label = json["label"]?.stringValue,
              let description = json["description"]?.stringValue,
              let detail = json["detail"]?.stringValue,
              let symbolValue = json["symbol"],
              let showsDetail = json["showsDetail"]?.boolValue,
              let op = json["op"]
        else {
            throw GatewayWireError.invalidFrame("frontend picker option is missing a required field")
        }
        let symbol: String?
        switch symbolValue {
        case .string(let value) where !value.isEmpty: symbol = value
        case .null: symbol = nil
        default:
            throw GatewayWireError.invalidFrame("frontend picker option has an invalid symbol")
        }
        self.label = label
        self.description = description
        self.detail = detail
        self.symbol = symbol
        self.showsDetail = showsDetail
        self.op = try AgentOperation(json: op)
    }
}
