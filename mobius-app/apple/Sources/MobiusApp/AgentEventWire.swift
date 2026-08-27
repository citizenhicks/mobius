import Foundation

struct AgentEventRecord: Decodable, Sendable {
    let submissionId: String?
    let msg: JSONValue
}

extension AgentEventRecord {
    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: DynamicCodingKey.self)
        let msg = try container.decode(JSONValue.self, forKey: "msg")
        try Self.validate(msg)
        submissionId = try container.decodeIfPresent(String.self, forKey: "submissionId")
        self.msg = msg
    }

    static func validate(_ msg: JSONValue) throws {
        guard let type = msg["type"]?.stringValue else {
            throw GatewayWireError.invalidFrame("agent event has no type")
        }
        try AgentEventValidator(msg: msg, type: type).validate()
    }
}

private struct AgentEventValidator {
    let msg: JSONValue
    let type: String

    func validate() throws {
        if try validateSessionEvent() { return }
        if try validateModelEvent() { return }
        if try validateCapabilityEvent() { return }
        throw GatewayWireError.invalidFrame("unknown agent event \(type)")
    }

    private func validateSessionEvent() throws -> Bool {
        switch type {
        case "error":
            try requireString("kind")
            try requireString("message")
            try requireBool("retryable")
            try optionalInteger("status")
            try optionalString("retryAfter")
        case "warning":
            try requireString("message")
        case "user_message":
            try requireString("message")
            try validateAttachments()
            try validateMessageTarget()
        case "session_configured":
            try requireString("sessionId")
            guard let context = msg["context"], let model = msg["model"] else {
                throw GatewayWireError.invalidFrame(
                    "session_configured is missing context or model"
                )
            }
            try validateContext(context)
            try validateModel(model)
        case "session_history":
            throw GatewayWireError.invalidFrame("session_history cannot cross the gateway")
        case "session_resume_requested":
            try requireString("sessionId")
            guard let context = msg["context"] else {
                throw GatewayWireError.invalidFrame(
                    "session_resume_requested has invalid context"
                )
            }
            try validateContext(context)
        case "context_compacted":
            break
        default:
            return false
        }
        return true
    }

    private func validateModelEvent() throws -> Bool {
        switch type {
        case "task_started":
            try requireString("turnId")
            try optionalInteger("modelContextWindow")
        case "task_complete":
            try requireString("turnId")
        case "turn_aborted":
            try requireString("turnId")
            try requireString("reason")
        case "agent_message":
            try requireStrings(["sessionId", "turnId", "modelStepId"])
            try requireString("message")
            try validatePhase()
            try validateMessageTarget()
        case "agent_message_content_delta":
            try requireStrings(["sessionId", "turnId", "modelStepId", "delta"])
            try validatePhase()
        case "agent_reasoning_content_delta":
            try requireStrings(["sessionId", "turnId", "modelStepId", "delta"])
        case "model_step_started":
            try requireStrings(["sessionId", "turnId", "modelStepId"])
            try requireInteger("stepIndex")
            try requireInteger("startedAtMs")
        case "model_step_completed":
            try validateModelStepCompletion()
        case "model_changed":
            try validateModel(msg)
        default:
            return false
        }
        return true
    }

    private func validateCapabilityEvent() throws -> Bool {
        switch type {
        case "tool_call_begin":
            try requireStrings(["turnId", "callId", "name"])
            guard msg["arguments"] != nil else {
                throw GatewayWireError.invalidFrame("tool_call_begin has invalid arguments")
            }
        case "tool_call_end":
            try requireStrings(["turnId", "callId", "name", "output"])
            try requireBool("isError")
        case "exec_approval_request":
            try validateApprovalCalls(reasonRequired: true)
        case "exec_approval_review":
            try validateApprovalCalls(reasonRequired: false)
            try validateApprovalReviewStatus()
        case "token_count":
            try validateTokenCount()
        case "web_search_begin":
            try requireStrings(["sessionId", "turnId", "modelStepId", "callId"])
        case "web_search_end":
            try requireStrings(["sessionId", "turnId", "modelStepId", "callId"])
            try validateWebSearchAction()
        case "frontend":
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
            ["commentary", "final_answer"].contains(phase)
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

    private func validateAttachments() throws {
        guard let attachments = msg["attachments"]?.arrayValue,
            attachments.count <= maximumWireSessionFileReferences
        else {
            throw GatewayWireError.invalidFrame("\(type) has invalid attachments")
        }
        try attachments.forEach { _ = try SessionFileReference(json: $0) }
    }

    private func validateModelStepCompletion() throws {
        try requireStrings(["sessionId", "turnId", "modelStepId"])
        try requireIntegers(["stepIndex", "startedAtMs", "completedAtMs"])
        guard let outcome = msg["outcome"], outcome.objectValue != nil else {
            throw GatewayWireError.invalidFrame("model_step_completed has invalid outcome")
        }
        switch outcome["status"]?.stringValue {
        case "completed":
            try requireBool("endTurn", in: outcome)
            guard let usage = outcome["usage"],
                let content = outcome["content"]?.arrayValue,
                let toolCallIDs = outcome["toolCallIds"]?.arrayValue,
                toolCallIDs.allSatisfy({ $0.stringValue != nil })
            else {
                throw GatewayWireError.invalidFrame(
                    "model_step_completed has incomplete output"
                )
            }
            try validateUsage(usage)
            for item in content {
                try requireIntegers(["outputIndex", "partIndex"], in: item)
                guard let phase = item["phase"]?.stringValue,
                    ["reasoning", "commentary", "final_answer"].contains(phase)
                else {
                    throw GatewayWireError.invalidFrame(
                        "model_step_completed has invalid content phase"
                    )
                }
                try requireString("text", in: item)
                guard let annotations = item["annotations"]?.arrayValue else {
                    throw GatewayWireError.invalidFrame(
                        "model_step_completed has invalid content annotations"
                    )
                }
                try annotations.forEach(validateModelStepAnnotation)
            }
        case "failed", "interrupted", "retrying":
            break
        default:
            throw GatewayWireError.invalidFrame(
                "model_step_completed has invalid outcome status"
            )
        }
    }

    private func validateModelStepAnnotation(_ annotation: JSONValue) throws {
        guard annotation.objectValue != nil,
            let annotationType = annotation["type"]?.stringValue
        else {
            throw GatewayWireError.invalidFrame(
                "model_step_completed has invalid content annotation"
            )
        }
        switch annotationType {
        case "url_citation":
            try requireStrings(["url", "title"], in: annotation)
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
                "model_step_completed has unknown content annotation \(annotationType)"
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

    private func validateApprovalCalls(reasonRequired: Bool) throws {
        try requireStrings(["id", "turnId"])
        if reasonRequired { try requireString("reason") }
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

    private func validateApprovalReviewStatus() throws {
        switch msg["status"]?.stringValue {
        case "reviewing", "approved":
            if let reason = msg["reason"], reason != .null {
                throw GatewayWireError.invalidFrame(
                    "exec_approval_review has an unexpected reason"
                )
            }
        case "escalated":
            guard let reason = msg["reason"]?.stringValue,
                [
                    "reviewer_asked",
                    "review_data_unavailable",
                    "reviewer_unavailable",
                    "invalid_response",
                ].contains(reason)
            else {
                throw GatewayWireError.invalidFrame(
                    "exec_approval_review has an invalid reason"
                )
            }
        default:
            throw GatewayWireError.invalidFrame(
                "exec_approval_review has an invalid status"
            )
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
        guard let frontendType = msg["frontendType"]?.stringValue else {
            throw GatewayWireError.invalidFrame("frontend event has no frontend_type")
        }
        switch frontendType {
        case "render":
            guard msg["capability"]?.stringValue != nil, let block = msg["block"] else {
                throw GatewayWireError.invalidFrame(
                    "frontend render is missing a required field"
                )
            }
            _ = try FrontendBlock(json: block)
        case "widget":
            guard msg["capability"]?.stringValue != nil, let item = msg["item"] else {
                throw GatewayWireError.invalidFrame(
                    "frontend widget is missing a required field"
                )
            }
            _ = try FrontendWidget(json: item)
        case "remove_widget":
            guard msg["capability"]?.stringValue != nil, msg["id"]?.stringValue != nil else {
                throw GatewayWireError.invalidFrame(
                    "frontend remove_widget is missing a required field"
                )
            }
        case "picker":
            guard msg["title"]?.stringValue != nil,
                let options = msg["options"]?.arrayValue
            else {
                throw GatewayWireError.invalidFrame(
                    "frontend picker is missing a required field"
                )
            }
            try options.forEach { _ = try FrontendPickerOption(json: $0) }
        case "preview":
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
            try events.forEach(AgentEventRecord.validate)
        default:
            throw GatewayWireError.invalidFrame("unknown frontend event \(frontendType)")
        }
    }
}
