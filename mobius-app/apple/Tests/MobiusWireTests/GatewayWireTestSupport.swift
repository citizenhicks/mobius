import Foundation
import XCTest

final class GatewayWireTests: XCTestCase {
    func decoder() -> JSONDecoder {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return decoder
    }

    func encoder() -> JSONEncoder {
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        encoder.outputFormatting = [.sortedKeys]
        return encoder
    }

    func requestObject(_ request: GatewayRequest) throws -> [String: Any] {
        let data = try encoder().encode(request)
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(object["version"] as? Int, gatewayProtocolVersion)
        return object
    }

    func decodeEnvelope(_ fixture: String) throws -> GatewayEnvelope {
        let currentFixture = fixture.replacingOccurrences(
            of: #""version":\d+"#,
            with: "\"version\":\(gatewayProtocolVersion)",
            options: .regularExpression
        )
        return try decoder().decode(GatewayEnvelope.self, from: Data(currentFixture.utf8))
    }

    var configJSON: String {
        #"{"revision":4,"config":{"provider":{"instance":"openai-work","provider":"openai_socket","model":"gpt-5.6-sol","endpoint_auth":"provider_default","reasoning_effort":"high","web_search":"cached"},"middleware":{"enabled":["extensions","subagents"],"settings":{"context_offloading":{"stale_after_tokens":50000},"subagents":{"model_route":"openai_socket/gpt-5.6-sol"}}},"extensions":["plugin:ponytail"],"system_prompt":"Stay focused.","max_model_steps":256}}"#
    }

    var usageJSON: String {
        #"{"input_tokens":0,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0,"total_tokens":0}"#
    }

    var executionStatsJSON: String {
        #"{"run_count":2,"failed_run_count":1,"aborted_run_count":0,"model_calls":4,"tool_calls":6,"failed_tool_calls":1,"elapsed_ms":9000,"usage":\#(usageJSON)}"#
    }

    var runStatsJSON: String {
        #"{"run_count":2,"failed_run_count":1,"aborted_run_count":0,"model_calls":4,"tool_calls":6,"failed_tool_calls":1,"elapsed_ms":9000,"usage":\#(usageJSON),"active":null}"#
    }

    var sessionRecordJSON: String {
        #"{"session_id":"chat-1","session_context":{"bot_id":"bot-1","workspace_id":"workspace-1"},"parent_session_id":null,"parent_sequence":null,"sequence":7,"first_user_message":"Review this","execution_stats":\#(executionStatsJSON),"created_at":100,"updated_at":200,"title":"Review","pinned":true,"activity":{"state":"running","turn_id":"turn-1","approval_request_id":null,"started_at":190,"last_outcome":null,"message":null}}"#
    }

    var botJSON: String {
        #"{"id":"bot-1","handle":"helper","name":"Helper","description":"You are möbius, a concise coding agent. Inspect the real code path before editing, make the smallest focused change, and preserve unrelated work.","tint":"blue","config":\#(configJSON)}"#
    }

    var readyPayloadJSON: String {
        #"{"machine_name":"snowwhite.local","bots":[\#(botJSON)],"sessions":[\#(sessionRecordJSON)],"background_approvals":[],"providers":[{"provider":"openai_socket","label":"OpenAI","symbol":"chat_gpt","description":"Persistent Responses API","auth":"api_key","default_base_url":null,"default_api_key_env":"OPENAI_API_KEY","models":[{"id":"gpt-5.6-sol","label":"Sol","description":"Frontier capability for complex work","context_window":1050000,"reasoning":[{"id":"medium","label":"Medium","description":"Balanced reasoning and latency"}],"default_reasoning":"medium"}],"model_ids_configurable":false,"web_search":[{"value":"off","label":"Off","description":"Do not use provider-hosted web search","symbol":null,"tone":"neutral"},{"value":"cached","label":"Cached","description":"Allow cached provider-hosted search","symbol":null,"tone":"neutral"},{"value":"live","label":"Live","description":"Allow live provider-hosted search","symbol":null,"tone":"neutral"}]}],"provider_instances":[{"label":"Work","tint":"blue","configured":true,"credential_hint":"a8f2","selection":{"instance":"openai-work","provider":"openai_socket","model":"gpt-5.6-sol","endpoint_auth":"provider_default","reasoning_effort":"high","web_search":"cached"},"model_ids":[],"reasoning_efforts":[]}],"bot_defaults":\#(configJSON),"models":[{"route":"openai_socket/gpt-5.6-sol","group":"OpenAI","model":"gpt-5.6-sol","reasoning_effort":"medium","context_window":200000,"supports_image_input":true}],"model_providers":{"openai_socket/gpt-5.6-sol":"openai-work"},"middleware_features":[{"id":"extensions","label":"Extensions","description":"Load installed skills and plugins.","required":false,"settings":[{"id":"limit","label":"Limit","description":"Maximum items","composer":false,"type":"integer","min":1,"step":10},{"id":"route","label":"Route","description":"Default route","composer":false,"type":"select","options":[{"value":"route-a","label":"Route A","description":"First route","symbol":null,"tone":"neutral"}],"unset_label":"Inherit"}]}],"extensions":[{"id":"plugin:ponytail","capability":"extensions","kind":"plugin","name":"ponytail","description":"Minimal coding guidance.","version":"4.9.0","source":"https://github.com/DietrichGebert/ponytail.git","reference":"main","subdirectory":null,"resolved_revision":"0123456789abcdef","digest":"abcdef0123456789","skills":["ponytail"],"hooks":[{"event":"pre_tool_use","matcher":"shell","command":"bin/review","timeout_seconds":10}],"hooks_trusted":true}],"contributions":[{"capability":"extensions","accepts_file_attachments":false,"count":1,"commands":[],"widgets":[],"references":[{"trigger":"$","value":"planning","description":"Planning skill"}]}],"max_active_sessions":4,"session_file_limits":{"max_attachment_references":16,"max_file_bytes":52428800,"max_session_files":128,"max_session_bytes":262144000,"max_upload_chunk_bytes":262144}}"#
            .replacingOccurrences(
                of: #""providers":"#,
                with: #""swarm_attentions":[],"swarms":[],"providers":"#
            )
            .replacingOccurrences(
                of: #""default_reasoning":"medium"}]"#,
                with: #""default_reasoning":"medium","tool_discovery":"native"}]"#
            )
            .replacingOccurrences(
                of: #""model_ids_configurable":false,"web_search":"#,
                with: #""model_ids_configurable":false,"tool_discovery":"native","custom_endpoint_tool_discovery":"rebuild","web_search":"#
            )
            .replacingOccurrences(
                of: #""supports_image_input":true}"#,
                with: #""supports_image_input":true,"tool_discovery":"native"}"#
            )
    }

    var sessionReadyPayloadJSON: String {
        #"{"latest_sequence":7,"next_before_sequence":2,"workspace":{"id":"workspace-1","path":"/srv/mobius"},"git":{"current_branch":"main","branches":["feature","main"]},"session":{"session_id":"chat-1","context":{"bot_id":"bot-1","workspace_id":"workspace-1"},"model":{"route":"openai_socket/gpt-5.6-sol","model":"gpt-5.6-sol","reasoning_effort":"high","model_context_window":200000}},"contributions":[{"capability":"subagents","accepts_file_attachments":false,"count":2,"commands":[],"widgets":[{"id":"subagents","slot":"composer_footer","text":"Subagents","tone":"neutral","symbol":"agent","icon_only":true,"progress":null,"content":{"type":"picker","title":"Subagents","options":[{"label":"reviewer","description":"running","detail":"gpt-5.6-sol","symbol":"agent","shows_detail":false,"op":{"type":"capability_command","capability":"subagents","command":"subagents","arguments":"reviewer","input":null,"target":null}}]},"action":null}],"references":[]}],"widgets":[],"tool_count":7,"compaction_count":2,"run_stats":\#(runStatsJSON)}"#
    }

    var composition: AgentComposition {
        AgentComposition(
            provider: ProviderConfig(
                instance: "openai-work",
                provider: "openai_socket",
                model: "gpt-5.6-sol",
                baseUrl: nil,
                reasoningEffort: "high",
                webSearch: .cached
            ),
            middleware: MiddlewareConfig(
                enabled: ["extensions", "subagents"],
                settings: [
                    "context_offloading": ["stale_after_tokens": .integer(50_000)],
                    "subagents": ["model_route": .string("openai_socket/gpt-5.6-sol")]
                ]
            ),
            extensions: ["plugin:ponytail"],
            systemPrompt: "Stay focused.",
            maxModelSteps: 256
        )
    }

}
