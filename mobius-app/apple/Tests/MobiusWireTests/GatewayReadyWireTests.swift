import Foundation
import XCTest

extension GatewayWireTests {
    func testGatewayWideReadyPayloadDefaultsMissingCredentialHint() throws {
        let legacyPayload = readyPayloadJSON.replacingOccurrences(
            of: "\"credential_hint\":\"a8f2\",",
            with: ""
        )
        let envelope = try decodeEnvelope(
            #"{"version":28,"type":"ready","payload":\#(legacyPayload)}"#
        )
        guard case .ready(let payload) = envelope else {
            return XCTFail("Expected ready envelope")
        }

        XCTAssertNil(payload.providerInstances.first?.credentialHint)
    }

    func testGatewayWideReadyPayloadDecodesV28State() throws {
        let envelope = try decodeEnvelope(
            #"{"version":28,"type":"ready","payload":\#(readyPayloadJSON)}"#
        )
        guard case .ready(let payload) = envelope else {
            return XCTFail("Expected ready envelope")
        }

        XCTAssertEqual(payload.sessions.first?.sessionId, "chat-1")
        XCTAssertEqual(payload.sessions.first?.title, "Review")
        XCTAssertEqual(payload.sessions.first?.activity.state, .running)
        XCTAssertEqual(payload.sessions.first?.activity.turnId, "turn-1")
        XCTAssertEqual(payload.sessions.first?.executionStats.toolCalls, 6)
        XCTAssertEqual(payload.providers.first?.defaultApiKeyEnv, "OPENAI_API_KEY")
        XCTAssertEqual(payload.providers.first?.symbol, "chat_gpt")
        XCTAssertEqual(payload.providers.first?.description, "Persistent Responses API")
        XCTAssertEqual(payload.providers.first?.auth, .apiKey)
        XCTAssertEqual(payload.providers.first?.models.first?.label, "Sol")
        XCTAssertEqual(payload.providers.first?.models.first?.defaultReasoning, "medium")
        XCTAssertEqual(payload.providers.first?.modelIdsConfigurable, false)
        XCTAssertEqual(payload.providerInstances.first?.instance, "openai-work")
        XCTAssertEqual(payload.providerInstances.first?.provider, "openai_socket")
        XCTAssertEqual(payload.providerInstances.first?.tint, .blue)
        XCTAssertEqual(payload.providerInstances.first?.credentialHint, "a8f2")
        XCTAssertEqual(payload.providerInstances.first?.reasoningEfforts, [])
        XCTAssertEqual(payload.providers.first?.models.first?.reasoning.first?.description, "Balanced reasoning and latency")
        XCTAssertEqual(payload.providers.first?.webSearch.map(\.value), ["off", "cached", "live"])
        XCTAssertEqual(
            payload.providers.first?.webSearch.first?.description,
            "Do not use provider-hosted web search"
        )
        XCTAssertEqual(payload.defaultConfig?.revision, 4)
        XCTAssertEqual(payload.defaultConfig?.config.maxModelSteps, 256)
        XCTAssertEqual(payload.models.first?.route, "openai_socket/gpt-5.6-sol")
        XCTAssertEqual(payload.models.first?.supportsImageInput, true)
        XCTAssertEqual(payload.modelProviders["openai_socket/gpt-5.6-sol"], "openai-work")
        XCTAssertEqual(payload.middlewareFeatures.first?.id, "extensions")
        XCTAssertEqual(payload.extensions.first?.id, "plugin:ponytail")
        XCTAssertEqual(payload.extensions.first?.capability, "extensions")
        XCTAssertEqual(payload.extensions.first?.kind, .plugin)
        XCTAssertEqual(payload.extensions.first?.hooks.first?.timeoutSeconds, 10)
        XCTAssertEqual(payload.contributions.first?.references.first?.value, "planning")
        XCTAssertEqual(payload.defaultConfig?.config.extensions, ["plugin:ponytail"])
        XCTAssertEqual(payload.maxActiveSessions, 4)
        XCTAssertEqual(payload.sessionFileLimits.maxAttachmentReferences, 16)
        XCTAssertEqual(payload.sessionFileLimits.maxFileBytes, 50 * 1024 * 1024)
        XCTAssertEqual(payload.sessionFileLimits.maxSessionFiles, 128)
        XCTAssertEqual(payload.sessionFileLimits.maxSessionBytes, 250 * 1024 * 1024)
        XCTAssertEqual(payload.sessionFileLimits.maxUploadChunkBytes, 256 * 1024)
        XCTAssertEqual(payload.machineName, "snowwhite.local")
        let settings = try XCTUnwrap(payload.middlewareFeatures.first?.settings)
        guard case .integer(let minimum, let maximum, let step) = settings[0].kind else {
            return XCTFail("Expected integer setting")
        }
        XCTAssertEqual(minimum, 1)
        XCTAssertNil(maximum)
        XCTAssertEqual(step, 10)
        guard case .select(let options, let unsetLabel) = settings[1].kind else {
            return XCTFail("Expected select setting")
        }
        XCTAssertEqual(options.first?.value, "route-a")
        XCTAssertNil(options.first?.symbol)
        XCTAssertEqual(options.first?.tone, "neutral")
        XCTAssertEqual(unsetLabel, "Inherit")

        let configured = try decodeEnvelope(
            #"{"version":28,"type":"gateway_configured","request_id":"gateway-1","payload":\#(readyPayloadJSON)}"#
        )
        guard case .gatewayConfigured(let requestID, let refreshed) = configured else {
            return XCTFail("Expected gateway configured envelope")
        }
        XCTAssertEqual(requestID, "gateway-1")
        XCTAssertEqual(refreshed.maxActiveSessions, 4)
    }

    func testGatewayMachineNameRejectsControlCharacters() {
        let payload = readyPayloadJSON.replacingOccurrences(
            of: "snowwhite.local",
            with: #"snowwhite\nlocal"#
        )
        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":28,"type":"ready","payload":\#(payload)}"#
        ))
    }

    func testReadyRejectsInvalidSearchOptionsAndSessionFileLimits() {
        let invalidSearch = readyPayloadJSON.replacingOccurrences(
            of: #""value":"off""#,
            with: #""value":"unknown""#
        )
        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":28,"type":"ready","payload":\#(invalidSearch)}"#
        ))

        let invalidLimits = readyPayloadJSON.replacingOccurrences(
            of: #""max_upload_chunk_bytes":262144"#,
            with: #""max_upload_chunk_bytes":0"#
        )
        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":28,"type":"ready","payload":\#(invalidLimits)}"#
        ))
    }

    func testV28RejectsLegacyProviderMetadata() {
        let legacyProvider = #"{"provider":"openai_socket","label":"OpenAI","configured":true,"auth":"api_key","default_model":"gpt-5.6-sol","default_base_url":null,"default_api_key_env":"OPENAI_API_KEY","default_reasoning_effort":"medium","default_web_search":"off"}"#
        let payload = #"{"sessions":[],"providers":[\#(legacyProvider)],"default_config":\#(configJSON),"models":[],"middleware_features":[],"max_active_sessions":4}"#

        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":28,"type":"ready","payload":\#(payload)}"#
        ))
    }

    func testV39RequiresProviderInstanceReasoningCatalogMetadata() {
        let payload = readyPayloadJSON.replacingOccurrences(
            of: #""reasoning_efforts":[]"#,
            with: ""
        )

        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":28,"type":"ready","payload":\#(payload)}"#
        ))
    }

    func testSessionOpenedAndChangedDecodeSessionReadyPayload() throws {
        let payloadJSON = sessionReadyPayloadJSON.replacingOccurrences(
            of: #""compaction_count":2"#,
            with: #""compaction_count":2,"context_limit_tokens":200000"#
        )
        let opened = try decodeEnvelope(
            #"{"version":28,"type":"session_opened","request_id":"open-1","payload":\#(payloadJSON)}"#
        )
        guard case .sessionOpened(let requestID, let payload) = opened else {
            return XCTFail("Expected session opened envelope")
        }
        XCTAssertEqual(requestID, "open-1")
        XCTAssertEqual(payload.latestSequence, 7)
        XCTAssertEqual(payload.nextBeforeSequence, 2)
        XCTAssertEqual(payload.compactionCount, 2)
        XCTAssertEqual(payload.contextLimitTokens, 200_000)
        XCTAssertEqual(payload.workspace.path, "/srv/mobius")
        XCTAssertEqual(payload.git?.currentBranch, "main")
        XCTAssertEqual(payload.git?.branches, ["feature", "main"])
        XCTAssertEqual(payload.session.sessionId, "chat-1")
        XCTAssertEqual(payload.toolCount, 7)
        XCTAssertEqual(payload.runStats.elapsedMs, 9_000)
        XCTAssertEqual(payload.contributions.first?.count, 2)
        XCTAssertEqual(payload.contributions.first?.acceptsFileAttachments, false)
        XCTAssertEqual(payload.config.config.middleware.enabled, ["cron", "extensions", "subagents"])
        XCTAssertEqual(payload.config.config.maxModelSteps, 256)
        guard let widget = payload.contributions.first?.widgets.first,
              case .picker(let title, let options) = widget.content,
              let option = options.first,
              case .capabilityCommand(let capability, let command, let arguments, let input, let target) =
                  option.op
        else { return XCTFail("Expected widget picker") }
        XCTAssertEqual(title, "Subagents")
        XCTAssertTrue(widget.iconOnly)
        XCTAssertEqual(widget.symbol, "agent")
        XCTAssertEqual(option.description, "running")
        XCTAssertEqual(option.detail, "gpt-5.6-sol")
        XCTAssertEqual(capability, "subagents")
        XCTAssertEqual(command, "subagents")
        XCTAssertEqual(arguments, "reviewer")
        XCTAssertNil(input)
        XCTAssertNil(target)

        let changed = try decodeEnvelope(
            #"{"version":28,"type":"session_changed","payload":\#(payloadJSON)}"#
        )
        guard case .sessionChanged(let changedPayload) = changed else {
            return XCTFail("Expected session changed envelope")
        }
        XCTAssertEqual(changedPayload.workspace.id, "workspace-1")

        let replayComplete = try decodeEnvelope(
            #"{"version":28,"type":"session_replay_complete","request_id":"open-1","session_id":"chat-1"}"#
        )
        guard case .sessionReplayComplete(let completedRequestID, let sessionID) = replayComplete else {
            return XCTFail("Expected session replay completion envelope")
        }
        XCTAssertEqual(completedRequestID, "open-1")
        XCTAssertEqual(sessionID, "chat-1")

        let history = try decodeEnvelope(
            #"{"version":28,"type":"session_history","request_id":"history-1","session_id":"chat-1","records":[{"sequence":3,"recorded_at_ms":1000,"event":{"submission_id":null,"msg":{"type":"context_compacted"}},"stream_metrics":[],"blocks":[],"preview":null}],"next_before_sequence":4}"#
        )
        guard case .sessionHistory(
            let historyRequestID,
            let historySessionID,
            let records,
            let nextBeforeSequence
        ) = history else { return XCTFail("Expected session history page") }
        XCTAssertEqual(historyRequestID, "history-1")
        XCTAssertEqual(historySessionID, "chat-1")
        XCTAssertEqual(records.first?.sequence, 3)
        XCTAssertEqual(records.first?.recordedAtMs, 1_000)
        XCTAssertEqual(records.first?.event.msg["type"]?.stringValue, "context_compacted")
        XCTAssertEqual(nextBeforeSequence, 4)
    }

    func testV28RequiresSessionActivityAndToolCount() {
        let sessionWithoutActivity = #"{"version":28,"type":"sessions","sessions":[{"session_id":"chat-1","session_context":{},"parent_session_id":null,"parent_sequence":null,"sequence":0,"first_user_message":null,"created_at":100,"updated_at":100,"title":null,"pinned":false}]}"#
        XCTAssertThrowsError(try decodeEnvelope(sessionWithoutActivity))

        let payloadWithoutToolCount = sessionReadyPayloadJSON.replacingOccurrences(
            of: #","tool_count":7"#,
            with: ""
        )
        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":28,"type":"session_opened","request_id":"open-1","payload":\#(payloadWithoutToolCount)}"#
        ))

        let payloadWithoutCompactionCount = sessionReadyPayloadJSON.replacingOccurrences(
            of: #","compaction_count":2"#,
            with: ""
        )
        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":28,"type":"session_opened","request_id":"open-1","payload":\#(payloadWithoutCompactionCount)}"#
        ))

        let payloadWithoutContributionCount = sessionReadyPayloadJSON.replacingOccurrences(
            of: #""count":2,"#,
            with: ""
        )
        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":28,"type":"session_opened","request_id":"open-1","payload":\#(payloadWithoutContributionCount)}"#
        ))
    }

    func testV28RequiresGenericSettingsAndScalarValues() {
        let withoutSettings = configJSON.replacingOccurrences(
            of: #","settings":{"context_offloading":{"stale_after_tokens":50000},"subagents":{"model_route":"openai_socket/gpt-5.6-sol"}}"#,
            with: ""
        )
        let payloadWithoutSettings = readyPayloadJSON.replacingOccurrences(
            of: configJSON,
            with: withoutSettings
        )
        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":28,"type":"ready","payload":\#(payloadWithoutSettings)}"#
        ))

        let invalidScalar = configJSON.replacingOccurrences(
            of: #""stale_after_tokens":50000"#,
            with: #""stale_after_tokens":true"#
        )
        let payloadWithInvalidScalar = readyPayloadJSON.replacingOccurrences(
            of: configJSON,
            with: invalidScalar
        )
        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":28,"type":"ready","payload":\#(payloadWithInvalidScalar)}"#
        ))
    }

    func testV28RequiresTheModelStepLimit() {
        let withoutLimit = configJSON.replacingOccurrences(
            of: #","max_model_steps":256"#,
            with: ""
        )
        let payload = readyPayloadJSON.replacingOccurrences(
            of: configJSON,
            with: withoutLimit
        )

        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":28,"type":"ready","payload":\#(payload)}"#
        ))
    }

    func testV39RequiresAnExplicitExtensionSelection() {
        let withoutExtensions = configJSON.replacingOccurrences(
            of: #","extensions":["plugin:ponytail"]"#,
            with: ""
        )
        let payload = readyPayloadJSON.replacingOccurrences(
            of: configJSON,
            with: withoutExtensions
        )

        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":39,"type":"ready","payload":\#(payload)}"#
        ))
    }

    func testV39RequiresGatewayContributions() {
        let payload = readyPayloadJSON.replacingOccurrences(
            of: #","contributions":[{"capability":"extensions","accepts_file_attachments":false,"count":1,"commands":[],"widgets":[],"references":[{"trigger":"$","value":"planning","description":"Planning skill"}],"active_input":null}]"#,
            with: ""
        )

        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":39,"type":"ready","payload":\#(payload)}"#
        ))
    }

    func testV28RequiresAPositiveModelStepLimitWithoutAnUpperPolicyBound() throws {
        let zeroLimit = configJSON.replacingOccurrences(
            of: #""max_model_steps":256"#,
            with: #""max_model_steps":0"#
        )
        let zeroPayload = readyPayloadJSON.replacingOccurrences(
            of: configJSON,
            with: zeroLimit
        )
        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":28,"type":"ready","payload":\#(zeroPayload)}"#
        ))

        let maximumLimit = configJSON.replacingOccurrences(
            of: #""max_model_steps":256"#,
            with: #""max_model_steps":18446744073709551615"#
        )
        let maximumPayload = readyPayloadJSON.replacingOccurrences(
            of: configJSON,
            with: maximumLimit
        )
        guard case .ready(let payload) = try decodeEnvelope(
            #"{"version":28,"type":"ready","payload":\#(maximumPayload)}"#
        ) else { return XCTFail("Expected ready envelope") }
        XCTAssertEqual(payload.defaultConfig?.config.maxModelSteps, UInt64.max)
    }

    func testFrontendIntegerSettingsRejectInvalidBoundsAndStep() {
        let fixtures = [
            (
                #"{"id":"limit","label":"Limit","description":"Maximum items","composer":false,"type":"integer","min":2,"max":1,"step":1}"#,
                "frontend integer setting maximum is below minimum"
            ),
            (
                #"{"id":"limit","label":"Limit","description":"Maximum items","composer":false,"type":"integer","min":1,"max":2,"step":0}"#,
                "frontend integer setting step must be positive"
            ),
            (
                #"{"id":"limit","label":"Limit","description":"Maximum items","composer":false,"type":"integer","min":1,"max":2,"step":-1}"#,
                "frontend integer setting step must be positive"
            ),
        ]

        for (fixture, message) in fixtures {
            XCTAssertThrowsError(
                try decoder().decode(FrontendSetting.self, from: Data(fixture.utf8))
            ) { error in
                XCTAssertEqual(error as? GatewayWireError, .invalidFrame(message))
            }
        }
    }

    func testFrontendSelectSettingsRejectDuplicateOptionValues() {
        let fixture = #"{"id":"route","label":"Route","description":"Default route","composer":false,"type":"select","options":[{"value":"route-a","label":"Route A","description":"First route","symbol":null,"tone":"neutral"},{"value":"route-a","label":"Route A again","description":"Duplicate route","symbol":null,"tone":"neutral"}]}"#

        XCTAssertThrowsError(
            try decoder().decode(FrontendSetting.self, from: Data(fixture.utf8))
        ) { error in
            XCTAssertEqual(
                error as? GatewayWireError,
                .invalidFrame("frontend select setting has duplicate option values")
            )
        }
    }

    func testFrontendComposerSettingDecodesSemanticOptions() throws {
        let fixture = #"{"id":"policy","label":"Access","description":"Execution access","composer":true,"type":"select","options":[{"value":"safe","label":"Safe","description":"Use bounded access","symbol":"shield_check","tone":"neutral"},{"value":"full","label":"Full access","description":"Use host access","symbol":"shield_off","tone":"error"}]}"#

        let setting = try decoder().decode(FrontendSetting.self, from: Data(fixture.utf8))

        XCTAssertTrue(setting.composer)
        guard case .select(let options, _) = setting.kind else {
            return XCTFail("Expected select setting")
        }
        XCTAssertEqual(options.map(\.symbol), ["shield_check", "shield_off"])
        XCTAssertEqual(options.map(\.tone), ["neutral", "error"])
    }

    func testFrontendSettingOptionRejectsUnknownTone() {
        let fixture = #"{"id":"policy","label":"Access","description":"Execution access","composer":true,"type":"select","options":[{"value":"safe","label":"Safe","description":"Use bounded access","symbol":"shield_check","tone":"loud"}]}"#

        XCTAssertThrowsError(
            try decoder().decode(FrontendSetting.self, from: Data(fixture.utf8))
        ) { error in
            XCTAssertEqual(
                error as? GatewayWireError,
                .invalidFrame("frontend setting option has an unknown tone")
            )
        }
    }

    func testV28RequiresCacheWriteInputTokens() {
        let usage = #"{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0,"total_tokens":2}"#
        let fixture = #"{"version":28,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"token_count","info":{"total_token_usage":\#(usage),"last_token_usage":\#(usage),"model_context_window":200}}},"stream_metrics":[],"blocks":[],"preview":null}}"#

        XCTAssertThrowsError(try decodeEnvelope(fixture))
    }

}
