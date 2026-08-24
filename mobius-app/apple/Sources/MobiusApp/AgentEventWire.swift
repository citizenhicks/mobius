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

        func requireString(_ key: String, in value: JSONValue = msg) throws {
            guard value[key]?.stringValue != nil else {
                throw GatewayWireError.invalidFrame("\(type) has invalid \(key)")
            }
        }

        func requireBool(_ key: String, in value: JSONValue = msg) throws {
            guard value[key]?.boolValue != nil else {
                throw GatewayWireError.invalidFrame("\(type) has invalid \(key)")
            }
        }

        func requireInteger(_ key: String, in value: JSONValue = msg) throws {
            guard value[key]?.intValue != nil else {
                throw GatewayWireError.invalidFrame("\(type) has invalid \(key)")
            }
        }

        func optionalString(_ key: String, in value: JSONValue = msg) throws {
            guard let field = value[key], field != .null else { return }
            guard field.stringValue != nil else {
                throw GatewayWireError.invalidFrame("\(type) has invalid \(key)")
            }
        }

        func optionalInteger(_ key: String, in value: JSONValue = msg) throws {
            guard let field = value[key], field != .null else { return }
            guard field.intValue != nil else {
                throw GatewayWireError.invalidFrame("\(type) has invalid \(key)")
            }
        }

        func validateContext(_ value: JSONValue) throws {
            guard value.objectValue != nil else {
                throw GatewayWireError.invalidFrame("\(type) has invalid context")
            }
            for key in ["tenantId", "userId", "userName", "workspaceId", "workspaceLabel", "originLabel"] {
                try optionalString(key, in: value)
            }
        }

        func validateModel(_ value: JSONValue) throws {
            guard value.objectValue != nil else {
                throw GatewayWireError.invalidFrame("\(type) has invalid model")
            }
            try requireString("route", in: value)
            try requireString("model", in: value)
            try optionalString("reasoningEffort", in: value)
            try optionalInteger("modelContextWindow", in: value)
        }

        func validatePhase(in value: JSONValue = msg) throws {
            guard let phase = value["phase"]?.stringValue,
                  ["commentary", "final_answer"].contains(phase)
            else {
                throw GatewayWireError.invalidFrame("\(type) has invalid phase")
            }
        }

        func validateMessageTarget() throws {
            guard let value = msg["messageTarget"],
                  value == .null || MessageTarget(json: value) != nil
            else {
                throw GatewayWireError.invalidFrame("\(type) has invalid message target")
            }
        }

        func validateUsage(_ value: JSONValue) throws {
            guard value.objectValue != nil else {
                throw GatewayWireError.invalidFrame("\(type) has invalid token usage")
            }
            for key in [
                "inputTokens", "cachedInputTokens", "outputTokens",
                "cacheWriteInputTokens", "reasoningOutputTokens", "totalTokens"
            ] {
                try requireInteger(key, in: value)
            }
        }

        func validateAttachments() throws {
            guard let attachments = msg["attachments"]?.arrayValue,
                  attachments.count <= maximumWireSessionFileReferences
            else {
                throw GatewayWireError.invalidFrame("\(type) has invalid attachments")
            }
            try attachments.forEach { _ = try SessionFileReference(json: $0) }
        }

        func validateModelStepAnnotation(_ annotation: JSONValue) throws {
            guard annotation.objectValue != nil,
                  let annotationType = annotation["type"]?.stringValue
            else {
                throw GatewayWireError.invalidFrame(
                    "model_step_completed has invalid content annotation"
                )
            }
            switch annotationType {
            case "url_citation":
                for key in ["url", "title"] { try requireString(key, in: annotation) }
                for key in ["startIndex", "endIndex"] {
                    try requireInteger(key, in: annotation)
                }
            case "file_citation":
                for key in ["fileId", "filename"] { try requireString(key, in: annotation) }
                try requireInteger("index", in: annotation)
            case "container_file_citation":
                for key in ["containerId", "fileId", "filename"] {
                    try requireString(key, in: annotation)
                }
                for key in ["startIndex", "endIndex"] {
                    try requireInteger(key, in: annotation)
                }
            case "file_path":
                try requireString("fileId", in: annotation)
                try requireInteger("index", in: annotation)
            case "document_character_citation":
                try requireString("citedText", in: annotation)
                try requireInteger("documentIndex", in: annotation)
                try optionalString("documentTitle", in: annotation)
                try optionalString("fileId", in: annotation)
                for key in ["startCharIndex", "endCharIndex"] {
                    try requireInteger(key, in: annotation)
                }
            case "document_page_citation":
                try requireString("citedText", in: annotation)
                try requireInteger("documentIndex", in: annotation)
                try optionalString("documentTitle", in: annotation)
                try optionalString("fileId", in: annotation)
                for key in ["startPageNumber", "endPageNumber"] {
                    try requireInteger(key, in: annotation)
                }
            case "document_content_block_citation":
                try requireString("citedText", in: annotation)
                try requireInteger("documentIndex", in: annotation)
                try optionalString("documentTitle", in: annotation)
                try optionalString("fileId", in: annotation)
                for key in ["startBlockIndex", "endBlockIndex"] {
                    try requireInteger(key, in: annotation)
                }
            case "search_result_citation":
                for key in ["citedText", "source"] { try requireString(key, in: annotation) }
                try requireInteger("searchResultIndex", in: annotation)
                try optionalString("title", in: annotation)
                for key in ["startBlockIndex", "endBlockIndex"] {
                    try requireInteger(key, in: annotation)
                }
            case "web_search_result_citation":
                for key in ["citedText", "encryptedIndex", "url"] {
                    try requireString(key, in: annotation)
                }
                try optionalString("title", in: annotation)
            default:
                throw GatewayWireError.invalidFrame(
                    "model_step_completed has unknown content annotation \(annotationType)"
                )
            }
        }

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
                throw GatewayWireError.invalidFrame("session_configured is missing context or model")
            }
            try validateContext(context)
            try validateModel(model)
        case "task_started":
            try requireString("turnId")
            try optionalInteger("modelContextWindow")
        case "task_complete":
            try requireString("turnId")
        case "turn_aborted":
            try requireString("turnId")
            try requireString("reason")
        case "agent_message":
            for key in ["sessionId", "turnId", "modelStepId"] { try requireString(key) }
            try requireString("message")
            try validatePhase()
            try validateMessageTarget()
        case "agent_message_content_delta":
            for key in ["sessionId", "turnId", "modelStepId", "delta"] {
                try requireString(key)
            }
            try validatePhase()
        case "agent_reasoning_content_delta":
            for key in ["sessionId", "turnId", "modelStepId", "delta"] {
                try requireString(key)
            }
        case "model_step_started":
            for key in ["sessionId", "turnId", "modelStepId"] { try requireString(key) }
            try requireInteger("stepIndex")
            try requireInteger("startedAtMs")
        case "model_step_completed":
            for key in ["sessionId", "turnId", "modelStepId"] { try requireString(key) }
            for key in ["stepIndex", "startedAtMs", "completedAtMs"] {
                try requireInteger(key)
            }
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
                    try requireInteger("outputIndex", in: item)
                    try requireInteger("partIndex", in: item)
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
        case "session_history":
            throw GatewayWireError.invalidFrame("session_history cannot cross the gateway")
        case "model_changed":
            try validateModel(msg)
        case "session_resume_requested":
            try requireString("sessionId")
            guard let context = msg["context"] else {
                throw GatewayWireError.invalidFrame("session_resume_requested has invalid context")
            }
            try validateContext(context)
        case "tool_call_begin":
            for key in ["turnId", "callId", "name"] { try requireString(key) }
            guard msg["arguments"] != nil else {
                throw GatewayWireError.invalidFrame("tool_call_begin has invalid arguments")
            }
        case "tool_call_end":
            for key in ["turnId", "callId", "name", "output"] { try requireString(key) }
            try requireBool("isError")
        case "exec_approval_request":
            for key in ["id", "turnId", "reason"] { try requireString(key) }
            guard let calls = msg["calls"]?.arrayValue else {
                throw GatewayWireError.invalidFrame("exec_approval_request has invalid calls")
            }
            for call in calls {
                try requireString("callId", in: call)
                try requireString("name", in: call)
                guard call["arguments"] != nil else {
                    throw GatewayWireError.invalidFrame("exec_approval_request has invalid arguments")
                }
            }
        case "exec_approval_review":
            for key in ["id", "turnId"] { try requireString(key) }
            guard let calls = msg["calls"]?.arrayValue else {
                throw GatewayWireError.invalidFrame("exec_approval_review has invalid calls")
            }
            for call in calls {
                try requireString("callId", in: call)
                try requireString("name", in: call)
                guard call["arguments"] != nil else {
                    throw GatewayWireError.invalidFrame("exec_approval_review has invalid arguments")
                }
            }
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
        case "token_count":
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
        case "context_compacted":
            break
        case "web_search_begin":
            for key in ["sessionId", "turnId", "modelStepId", "callId"] {
                try requireString(key)
            }
        case "web_search_end":
            for key in ["sessionId", "turnId", "modelStepId", "callId"] {
                try requireString(key)
            }
            guard let action = msg["action"], action.objectValue != nil else {
                throw GatewayWireError.invalidFrame("web_search_end has invalid action")
            }
            switch action["type"]?.stringValue {
            case "search":
                guard let queries = action["queries"]?.arrayValue,
                      !queries.isEmpty,
                      queries.allSatisfy({ query in
                          query.stringValue?.isEmpty == false
                      })
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
        case "frontend":
            guard let frontendType = msg["frontendType"]?.stringValue else {
                throw GatewayWireError.invalidFrame("frontend event has no frontend_type")
            }
            switch frontendType {
            case "render":
                guard msg["capability"]?.stringValue != nil, let block = msg["block"] else {
                    throw GatewayWireError.invalidFrame("frontend render is missing a required field")
                }
                _ = try FrontendBlock(json: block)
            case "widget":
                guard msg["capability"]?.stringValue != nil, let item = msg["item"] else {
                    throw GatewayWireError.invalidFrame("frontend widget is missing a required field")
                }
                _ = try FrontendWidget(json: item)
            case "remove_widget":
                guard msg["capability"]?.stringValue != nil, msg["id"]?.stringValue != nil else {
                    throw GatewayWireError.invalidFrame("frontend remove_widget is missing a required field")
                }
            case "picker":
                guard msg["title"]?.stringValue != nil, let options = msg["options"]?.arrayValue else {
                    throw GatewayWireError.invalidFrame("frontend picker is missing a required field")
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
                    throw GatewayWireError.invalidFrame("frontend preview is missing a required field")
                }
                if next != .null { _ = try AgentOperation(json: next) }
                try events.forEach(validate)
            default:
                throw GatewayWireError.invalidFrame("unknown frontend event \(frontendType)")
            }
        default:
            throw GatewayWireError.invalidFrame("unknown agent event \(type)")
        }
    }
}
