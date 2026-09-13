import Foundation

struct AssistantMessageContent: Sendable {
    let outputIndex: Int
    let partIndex: Int
    let phase: ModelStepContentPhase
    let text: String
    let annotations: [JSONValue]

    init(json: JSONValue) throws {
        guard let outputIndex = json["outputIndex"]?.intValue,
            let partIndex = json["partIndex"]?.intValue,
            let rawPhase = json["phase"]?.stringValue,
            let phase = ModelStepContentPhase(rawValue: rawPhase),
            let text = json["text"]?.stringValue,
            let annotations = json["annotations"]?.arrayValue
        else {
            throw GatewayWireError.invalidFrame("assistant_message has invalid content")
        }
        self.outputIndex = outputIndex
        self.partIndex = partIndex
        self.phase = phase
        self.text = text
        self.annotations = annotations
    }
}

enum ModelStepOutcomeStatus: String, Sendable {
    case completed
    case failed
    case interrupted
    case retrying
}

struct AgentEventRecord: Decodable, Sendable {
    let submissionId: String?
    let kind: AgentEventKind
    let msg: JSONValue

    #if DEBUG
        /// Test-only construction for synthetic records. Production records use the throwing decoder.
        init(submissionId: String?, msg: JSONValue) {
            guard let rawKind = msg["type"]?.stringValue,
                let kind = AgentEventKind(rawValue: rawKind)
            else { preconditionFailure("invalid trusted agent event") }
            self.submissionId = submissionId
            self.kind = kind
            self.msg = msg
        }
    #endif

    init(validating msg: JSONValue, submissionId: String?) throws {
        self.submissionId = submissionId
        kind = try Self.validate(msg, submissionId: submissionId)
        self.msg = msg
    }

    var message: MessageEventPayload? {
        guard kind == .message else { return nil }
        return try? MessageEventPayload(json: msg)
    }

    var turnID: String? { msg["turnId"]?.stringValue }
    var modelStepID: String? { msg["modelStepId"]?.stringValue }
    var delta: String? {
        switch kind {
        case .messageDelta: msg["text"]?.stringValue
        case .assistantContentDelta: msg["delta"]?.stringValue
        default: nil
        }
    }
    var phase: ModelStepContentPhase? {
        msg["phase"]?.stringValue.flatMap(ModelStepContentPhase.init(rawValue:))
    }
    var assistantContent: [AssistantMessageContent]? {
        guard kind == .assistantMessage, let content = msg["content"]?.arrayValue else {
            return nil
        }
        return try? content.map(AssistantMessageContent.init(json:))
    }
    var messageTarget: MessageTarget? {
        guard kind == .assistantMessage, let target = msg["messageTarget"], target != .null else {
            return nil
        }
        return MessageTarget(json: target)
    }
    var modelContextWindow: Int64? {
        let value =
            kind == .tokenCount ? msg["info"]?["modelContextWindow"] : msg["modelContextWindow"]
        return value?.intValue.map(Int64.init)
    }
    var modelRoute: String? {
        guard kind == .modelChanged else { return nil }
        return msg["route"]?.stringValue
    }
    var sessionID: String? {
        guard kind == .sessionResumeRequested else { return nil }
        return msg["sessionId"]?.stringValue
    }
    var toolCallFailed: Bool {
        kind == .toolCallEnd && msg["isError"]?.boolValue == true
    }
    var modelStepOutcomeStatus: ModelStepOutcomeStatus? {
        guard kind == .modelStepCompleted else { return nil }
        return msg["outcome"]?["status"]?.stringValue.flatMap(
            ModelStepOutcomeStatus.init(rawValue:))
    }
    var tokenUsage: (total: TokenUsage, last: TokenUsage)? {
        guard kind == .tokenCount, let info = msg["info"], info != .null,
            let total = info["totalTokenUsage"].flatMap(TokenUsage.init(json:)),
            let last = info["lastTokenUsage"].flatMap(TokenUsage.init(json:))
        else { return nil }
        return (total, last)
    }
    var frontendWidget: (capability: String, widget: FrontendWidget)? {
        guard kind == .frontend, frontendEvent == .widget,
            let capability = msg["capability"]?.stringValue,
            let item = msg["item"],
            let widget = try? FrontendWidget(json: item)
        else { return nil }
        return (capability, widget)
    }
    var removedFrontendWidget: (capability: String, id: String)? {
        guard kind == .frontend, frontendEvent == .removeWidget,
            let capability = msg["capability"]?.stringValue,
            let id = msg["id"]?.stringValue
        else { return nil }
        return (capability, id)
    }
    var frontendPicker: (title: String, options: [FrontendPickerOption])? {
        guard kind == .frontend, frontendEvent == .picker,
            let title = msg["title"]?.stringValue,
            let values = msg["options"]?.arrayValue,
            let options = try? values.map(FrontendPickerOption.init(json:))
        else { return nil }
        return (title, options)
    }
    var frontendEvent: FrontendAgentEventKind? {
        guard kind == .frontend else { return nil }
        return msg["frontendType"]?.stringValue.flatMap(FrontendAgentEventKind.init(rawValue:))
    }
}

extension AgentEventRecord {
    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: DynamicCodingKey.self)
        let msg = try container.decode(JSONValue.self, forKey: "msg")
        try self.init(
            validating: msg,
            submissionId: try container.decodeIfPresent(String.self, forKey: "submissionId")
        )
    }

    @discardableResult
    static func validate(_ msg: JSONValue, submissionId: String?) throws -> AgentEventKind {
        let kind = try validate(msg)
        if kind == .messageDelta, submissionId?.isEmpty != false {
            throw GatewayWireError.invalidFrame("message_delta has no submission identity")
        }
        return kind
    }

    @discardableResult
    static func validate(_ msg: JSONValue) throws -> AgentEventKind {
        guard let rawKind = msg["type"]?.stringValue else {
            throw GatewayWireError.invalidFrame("agent event has no type")
        }
        guard let kind = AgentEventKind(rawValue: rawKind) else {
            throw GatewayWireError.invalidFrame("unknown agent event \(rawKind)")
        }
        try AgentEventValidator(msg: msg, kind: kind).validate()
        return kind
    }
}

private struct AgentEventValidator {
    let msg: JSONValue
    let kind: AgentEventKind
    var type: String { kind.rawValue }

    func validate() throws {
        let validated: Bool
        switch kind {
        case .error, .warning, .submissionRejected, .message, .messageDelta, .sessionConfigured,
            .sessionHistory, .sessionResumeRequested, .contextCompacted:
            validated = try validateSessionEvent()
        case .turnStarted, .turnComplete, .turnAborted, .assistantMessage,
            .assistantContentDelta, .modelStepStarted, .modelStepCompleted, .modelChanged:
            validated = try validateModelEvent()
        case .toolCallBegin, .toolCallEnd, .toolLoad, .execApprovalRequest, .tokenCount,
            .webSearchBegin, .webSearchEnd, .frontend:
            validated = try validateCapabilityEvent()
        }
        guard validated else {
            throw GatewayWireError.invalidFrame("unknown agent event \(type)")
        }
    }

    private func validateSessionEvent() throws -> Bool {
        switch kind {
        case .error:
            try requireString("kind")
            try requireString("message")
            try requireBool("retryable")
            try optionalInteger("status")
            try optionalString("retryAfter")
        case .warning, .submissionRejected:
            try requireString("message")
        case .message:
            _ = try MessageEventPayload(json: msg)
        case .messageDelta:
            try requireString("text")
        case .sessionConfigured:
            try requireString("sessionId")
            guard let context = msg["context"], let model = msg["model"] else {
                throw GatewayWireError.invalidFrame(
                    "session_configured is missing context or model"
                )
            }
            try validateContext(context)
            try validateModel(model)
        case .sessionHistory:
            throw GatewayWireError.invalidFrame("session_history cannot cross the gateway")
        case .sessionResumeRequested:
            try requireString("sessionId")
            guard let context = msg["context"] else {
                throw GatewayWireError.invalidFrame(
                    "session_resume_requested has invalid context"
                )
            }
            try validateContext(context)
        case .contextCompacted:
            break
        default:
            return false
        }
        return true
    }

    private func validateModelEvent() throws -> Bool {
        switch kind {
        case .turnStarted:
            try requireString("turnId")
            try optionalInteger("modelContextWindow")
        case .turnComplete:
            try requireString("turnId")
        case .turnAborted:
            try requireString("turnId")
            try requireString("reason")
        case .assistantMessage:
            try requireStrings(["sessionId", "turnId", "modelStepId"])
            guard let content = msg["content"]?.arrayValue, !content.isEmpty else {
                throw GatewayWireError.invalidFrame("assistant_message has invalid content")
            }
            try validateAssistantContent(content)
            try validateMessageTarget()
        case .assistantContentDelta:
            try requireStrings(["sessionId", "turnId", "modelStepId", "delta"])
            try validatePhase()
        case .modelStepStarted:
            try requireStrings(["sessionId", "turnId", "modelStepId"])
            try requireInteger("stepIndex")
            try requireInteger("startedAtMs")
        case .modelStepCompleted:
            try validateModelStepCompletion()
        case .modelChanged:
            try validateModel(msg)
        default:
            return false
        }
        return true
    }

    private func validateCapabilityEvent() throws -> Bool {
        switch kind {
        case .toolCallBegin:
            try requireStrings(["turnId", "callId", "name"])
            guard msg["arguments"] != nil else {
                throw GatewayWireError.invalidFrame("tool_call_begin has invalid arguments")
            }
        case .toolCallEnd:
            try requireStrings(["turnId", "callId", "name"])
            guard let output = msg["output"]?.arrayValue else {
                throw GatewayWireError.invalidFrame("tool_call_end has invalid output")
            }
            _ = try output.map(ContentPart.init(json:))
            try requireBool("isError")
        case .toolLoad:
            try requireStrings(["turnId", "loadId", "catalogRevision"])
            guard let tools = msg["tools"]?.arrayValue,
                !tools.isEmpty,
                tools.allSatisfy({ $0.stringValue?.isEmpty == false })
            else {
                throw GatewayWireError.invalidFrame("tool_load has invalid tools")
            }
        case .execApprovalRequest:
            try validateApprovalCalls()
        case .tokenCount:
            try validateTokenCount()
        case .webSearchBegin:
            try requireStrings(["sessionId", "turnId", "modelStepId", "callId"])
        case .webSearchEnd:
            try requireStrings(["sessionId", "turnId", "modelStepId", "callId"])
            try validateWebSearchAction()
        case .frontend:
            try validateFrontendEvent()
        default:
            return false
        }
        return true
    }

    private func requireString(_ key: String, in value: JSONValue? = nil) throws {
        guard (value ?? msg)[key]?.stringValue != nil else {
            throw GatewayWireError.invalidFrame("\(type) has invalid \(key)")
        }
    }

    private func requireStrings(_ keys: [String], in value: JSONValue? = nil) throws {
        for key in keys { try requireString(key, in: value) }
    }

    private func requireBool(_ key: String, in value: JSONValue? = nil) throws {
        guard (value ?? msg)[key]?.boolValue != nil else {
            throw GatewayWireError.invalidFrame("\(type) has invalid \(key)")
        }
    }

    private func requireInteger(_ key: String, in value: JSONValue? = nil) throws {
        guard (value ?? msg)[key]?.intValue != nil else {
            throw GatewayWireError.invalidFrame("\(type) has invalid \(key)")
        }
    }

    private func requireIntegers(_ keys: [String], in value: JSONValue? = nil) throws {
        for key in keys { try requireInteger(key, in: value) }
    }

    private func optionalString(_ key: String, in value: JSONValue? = nil) throws {
        guard let field = (value ?? msg)[key], field != .null else { return }
        guard field.stringValue != nil else {
            throw GatewayWireError.invalidFrame("\(type) has invalid \(key)")
        }
    }

    private func optionalStrings(_ keys: [String], in value: JSONValue? = nil) throws {
        for key in keys { try optionalString(key, in: value) }
    }

    private func optionalInteger(_ key: String, in value: JSONValue? = nil) throws {
        guard let field = (value ?? msg)[key], field != .null else { return }
        guard field.intValue != nil else {
            throw GatewayWireError.invalidFrame("\(type) has invalid \(key)")
        }
    }

    private func validateContext(_ value: JSONValue) throws {
        guard value.objectValue != nil else {
            throw GatewayWireError.invalidFrame("\(type) has invalid context")
        }
        try requireString("ownerId", in: value)
        try optionalStrings(
            ["tenantId", "userId", "userName", "workspaceId", "workspaceLabel", "originLabel"],
            in: value
        )
    }

    private func validateModel(_ value: JSONValue) throws {
        guard value.objectValue != nil else {
            throw GatewayWireError.invalidFrame("\(type) has invalid model")
        }
        try requireString("route", in: value)
        try requireString("model", in: value)
        try optionalString("reasoningEffort", in: value)
        try optionalInteger("modelContextWindow", in: value)
    }

    private func validatePhase(in value: JSONValue? = nil) throws {
        let value = value ?? msg
        guard let phase = value["phase"]?.stringValue,
            ModelStepContentPhase(rawValue: phase) != nil
        else {
            throw GatewayWireError.invalidFrame("\(type) has invalid phase")
        }
    }

    private func validateMessageTarget() throws {
        guard let value = msg["messageTarget"],
            value == .null || MessageTarget(json: value) != nil
        else {
            throw GatewayWireError.invalidFrame("\(type) has invalid message target")
        }
    }

    private func validateUsage(_ value: JSONValue) throws {
        guard value.objectValue != nil else {
            throw GatewayWireError.invalidFrame("\(type) has invalid token usage")
        }
        try requireIntegers(
            [
                "inputTokens", "cachedInputTokens", "outputTokens",
                "cacheWriteInputTokens", "reasoningOutputTokens", "totalTokens",
            ],
            in: value
        )
    }

    private func validateModelStepCompletion() throws {
        try requireStrings(["sessionId", "turnId", "modelStepId"])
        try requireIntegers(["stepIndex", "startedAtMs", "completedAtMs"])
        guard let outcome = msg["outcome"], outcome.objectValue != nil else {
            throw GatewayWireError.invalidFrame("model_step_completed has invalid outcome")
        }
        guard let rawStatus = outcome["status"]?.stringValue,
            let status = ModelStepOutcomeStatus(rawValue: rawStatus)
        else {
            throw GatewayWireError.invalidFrame(
                "model_step_completed has invalid outcome status"
            )
        }
        switch status {
        case .completed:
            try requireBool("endTurn", in: outcome)
            guard let usage = outcome["usage"],
                let toolCallIDs = outcome["toolCallIds"]?.arrayValue,
                toolCallIDs.allSatisfy({ $0.stringValue != nil })
            else {
                throw GatewayWireError.invalidFrame(
                    "model_step_completed has incomplete output"
                )
            }
            try validateUsage(usage)
        case .failed, .interrupted, .retrying:
            break
        }
    }

    private func validateAssistantContent(_ content: [JSONValue]) throws {
        for value in content {
            let item = try AssistantMessageContent(json: value)
            try item.annotations.forEach(validateModelStepAnnotation)
        }
    }

    private func validateModelStepAnnotation(_ annotation: JSONValue) throws {
        guard annotation.objectValue != nil,
            let annotationType = annotation["type"]?.stringValue
        else {
            throw GatewayWireError.invalidFrame(
                "assistant_message has invalid content annotation"
            )
        }
        switch annotationType {
        case "url_citation":
            try requireStrings(["url", "title"], in: annotation)
            try optionalString("content", in: annotation)
            try requireIntegers(["startIndex", "endIndex"], in: annotation)
        case "file_citation":
            try requireStrings(["fileId", "filename"], in: annotation)
            try requireInteger("index", in: annotation)
        case "container_file_citation":
            try requireStrings(["containerId", "fileId", "filename"], in: annotation)
            try requireIntegers(["startIndex", "endIndex"], in: annotation)
        case "file_path":
            try requireString("fileId", in: annotation)
            try requireInteger("index", in: annotation)
        case "document_character_citation":
            try validateDocumentCitation(
                annotation,
                positionKeys: ["startCharIndex", "endCharIndex"]
            )
        case "document_page_citation":
            try validateDocumentCitation(
                annotation,
                positionKeys: ["startPageNumber", "endPageNumber"]
            )
        case "document_content_block_citation":
            try validateDocumentCitation(
                annotation,
                positionKeys: ["startBlockIndex", "endBlockIndex"]
            )
        case "search_result_citation":
            try requireStrings(["citedText", "source"], in: annotation)
            try requireInteger("searchResultIndex", in: annotation)
            try optionalString("title", in: annotation)
            try requireIntegers(["startBlockIndex", "endBlockIndex"], in: annotation)
        case "web_search_result_citation":
            try requireStrings(["citedText", "encryptedIndex", "url"], in: annotation)
            try optionalString("title", in: annotation)
        default:
            throw GatewayWireError.invalidFrame(
                "assistant_message has unknown content annotation \(annotationType)"
            )
        }
    }

    private func validateDocumentCitation(
        _ annotation: JSONValue,
        positionKeys: [String]
    ) throws {
        try requireString("citedText", in: annotation)
        try requireInteger("documentIndex", in: annotation)
        try optionalString("documentTitle", in: annotation)
        try optionalString("fileId", in: annotation)
        try requireIntegers(positionKeys, in: annotation)
    }

    private func validateApprovalCalls() throws {
        try requireStrings(["id", "turnId", "reason"])
        guard let calls = msg["calls"]?.arrayValue else {
            throw GatewayWireError.invalidFrame("\(type) has invalid calls")
        }
        for call in calls {
            try requireString("callId", in: call)
            try requireString("name", in: call)
            guard call["arguments"] != nil else {
                throw GatewayWireError.invalidFrame("\(type) has invalid arguments")
            }
        }
    }

    private func validateTokenCount() throws {
        if let info = msg["info"], info != .null {
            guard info.objectValue != nil,
                let total = info["totalTokenUsage"],
                let last = info["lastTokenUsage"]
            else {
                throw GatewayWireError.invalidFrame("token_count has invalid info")
            }
            try validateUsage(total)
            try validateUsage(last)
            try optionalInteger("modelContextWindow", in: info)
        }
    }

    private func validateWebSearchAction() throws {
        guard let action = msg["action"], action.objectValue != nil else {
            throw GatewayWireError.invalidFrame("web_search_end has invalid action")
        }
        switch action["type"]?.stringValue {
        case "search":
            guard let queries = action["queries"]?.arrayValue,
                !queries.isEmpty,
                queries.allSatisfy({ $0.stringValue?.isEmpty == false })
            else {
                throw GatewayWireError.invalidFrame("web_search_end has invalid queries")
            }
        case "open_page":
            try optionalString("url", in: action)
        case "find_in_page":
            try optionalString("url", in: action)
            try optionalString("pattern", in: action)
        case "interrupted", "other":
            break
        default:
            throw GatewayWireError.invalidFrame("web_search_end has unknown action")
        }
    }

    private func validateFrontendEvent() throws {
        guard let rawFrontendEvent = msg["frontendType"]?.stringValue else {
            throw GatewayWireError.invalidFrame("frontend event has no frontend_type")
        }
        guard let frontendEvent = FrontendAgentEventKind(rawValue: rawFrontendEvent) else {
            throw GatewayWireError.invalidFrame("unknown frontend event \(rawFrontendEvent)")
        }
        switch frontendEvent {
        case .render:
            guard msg["capability"]?.stringValue != nil, let block = msg["block"] else {
                throw GatewayWireError.invalidFrame(
                    "frontend render is missing a required field"
                )
            }
            _ = try FrontendBlock(json: block)
        case .widget:
            guard msg["capability"]?.stringValue != nil, let item = msg["item"] else {
                throw GatewayWireError.invalidFrame(
                    "frontend widget is missing a required field"
                )
            }
            _ = try FrontendWidget(json: item)
        case .removeWidget:
            guard msg["capability"]?.stringValue != nil, msg["id"]?.stringValue != nil else {
                throw GatewayWireError.invalidFrame(
                    "frontend remove_widget is missing a required field"
                )
            }
        case .picker:
            guard msg["title"]?.stringValue != nil,
                let options = msg["options"]?.arrayValue
            else {
                throw GatewayWireError.invalidFrame(
                    "frontend picker is missing a required field"
                )
            }
            try options.forEach { _ = try FrontendPickerOption(json: $0) }
        case .preview:
            guard let id = msg["id"]?.stringValue,
                !id.isEmpty,
                msg["title"]?.stringValue != nil,
                msg["subtitle"]?.stringValue != nil,
                let pageID = msg["pageId"]?.stringValue,
                !pageID.isEmpty,
                let update = msg["update"]?.stringValue,
                FrontendPreviewUpdate(rawValue: update) != nil,
                let events = msg["events"]?.arrayValue,
                let next = msg["next"]
            else {
                throw GatewayWireError.invalidFrame(
                    "frontend preview is missing a required field"
                )
            }
            if next != .null { _ = try AgentOperation(json: next) }
            try events.forEach { try AgentEventRecord.validate($0) }
        }
    }
}
