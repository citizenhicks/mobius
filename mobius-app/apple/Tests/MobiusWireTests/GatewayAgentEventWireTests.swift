import Foundation
import XCTest

extension GatewayWireTests {
    func testSubmissionRejectedRequiresMessage() throws {
        let fixture = #"{"version":55,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1234,"event":{"submission_id":"input-1","msg":{"type":"submission_rejected","message":"Message queue is full"}},"stream_metrics":[],"blocks":[],"preview":null}}"#
        guard case .agentEvent(_, let record) = try decodeEnvelope(fixture) else {
            return XCTFail("Expected submission rejection event")
        }
        XCTAssertEqual(record.event.msg["message"]?.stringValue, "Message queue is full")

        let malformed = fixture.replacingOccurrences(
            of: ",\"message\":\"Message queue is full\"",
            with: ""
        )
        XCTAssertThrowsError(try decodeEnvelope(malformed)) { error in
            XCTAssertEqual(
                error as? GatewayWireError,
                .invalidFrame("submission_rejected has invalid message")
            )
        }
    }

    func testAgentEventFixtureIncludesSessionScope() throws {
        let fixture = #"{"version":28,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1234,"event":{"submission_id":"input-1","msg":{"type":"context_compacted"}},"stream_metrics":[{"phase":"reasoning","first_delta_at_ms":1000,"last_delta_at_ms":1200,"chunk_count":3,"utf8_bytes":12,"longest_gap_ms":150}],"blocks":[],"preview":null}}"#
        let envelope = try decodeEnvelope(fixture)

        guard case .agentEvent(let sessionID, let record) = envelope else {
            return XCTFail("Expected agent event envelope")
        }
        XCTAssertEqual(sessionID, "chat-1")
        XCTAssertEqual(record.sequence, 8)
        XCTAssertEqual(record.recordedAtMs, 1_234)
        XCTAssertEqual(record.event.submissionId, "input-1")
        XCTAssertEqual(record.event.msg["type"]?.stringValue, "context_compacted")
        XCTAssertEqual(record.streamMetrics.first?.phase, .reasoning)
        XCTAssertEqual(record.streamMetrics.first?.chunkCount, 3)
        XCTAssertTrue(record.blocks.isEmpty)
        XCTAssertNil(record.preview)
    }

    func testTypedModelCompletionAndWebActionDecode() throws {
        let completion = try decodeEnvelope(
            #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1400,"event":{"msg":{"type":"model_step_completed","session_id":"chat-1","turn_id":"turn-1","model_step_id":"step-1","step_index":0,"started_at_ms":1000,"completed_at_ms":1400,"outcome":{"status":"completed","end_turn":true,"tool_call_ids":["call-1"],"usage":{"input_tokens":10,"cached_input_tokens":2,"cache_write_input_tokens":0,"output_tokens":3,"reasoning_output_tokens":1,"total_tokens":13}}}},"stream_metrics":[{"phase":"reasoning","first_delta_at_ms":1100,"last_delta_at_ms":1200,"chunk_count":2,"utf8_bytes":7,"longest_gap_ms":100},{"phase":"final_answer","first_delta_at_ms":1300,"last_delta_at_ms":1350,"chunk_count":1,"utf8_bytes":4,"longest_gap_ms":0}],"blocks":[],"preview":null}}"#
        )
        guard case .agentEvent(_, let completionRecord) = completion else {
            return XCTFail("Expected model completion")
        }
        XCTAssertEqual(
            completionRecord.event.msg["outcome"]?["toolCallIds"]?.arrayValue?.first?.stringValue,
            "call-1"
        )
        XCTAssertEqual(completionRecord.streamMetrics.map(\.phase), [.reasoning, .finalAnswer])
        XCTAssertEqual(completionRecord.streamMetrics.last?.utf8Bytes, 4)

        let assistant = try decodeEnvelope(
            #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":9,"recorded_at_ms":1401,"event":{"msg":{"type":"assistant_message","session_id":"chat-1","turn_id":"turn-1","model_step_id":"step-1","content":[{"output_index":0,"part_index":0,"phase":"reasoning","text":"Checked","annotations":[]},{"output_index":1,"part_index":0,"phase":"final_answer","text":"Done","annotations":[{"type":"url_citation","url":"https://example.com","title":"Example","content":"Relevant excerpt.","start_index":0,"end_index":4}]}],"message_target":null}},"stream_metrics":[],"blocks":[],"preview":null}}"#
        )
        guard case .agentEvent(_, let assistantRecord) = assistant else {
            return XCTFail("Expected assistant message")
        }
        XCTAssertEqual(assistantRecord.event.msg["content"]?.arrayValue?.count, 2)
        XCTAssertEqual(
            assistantRecord.event.msg["content"]?.arrayValue?.last?["annotations"]?
                .arrayValue?.first?["content"]?.stringValue,
            "Relevant excerpt."
        )

        let invalidCitationContent = #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":9,"recorded_at_ms":1401,"event":{"msg":{"type":"assistant_message","session_id":"chat-1","turn_id":"turn-1","model_step_id":"step-1","content":[{"output_index":0,"part_index":0,"phase":"final_answer","text":"Done","annotations":[{"type":"url_citation","url":"https://example.com","title":"Example","content":7,"start_index":0,"end_index":4}]}],"message_target":null}},"stream_metrics":[],"blocks":[],"preview":null}}"#
        XCTAssertThrowsError(try decodeEnvelope(invalidCitationContent))

        let retry = try decodeEnvelope(
            #"{"version":28,"type":"agent_event","session_id":"chat-1","record":{"sequence":9,"recorded_at_ms":1450,"event":{"msg":{"type":"model_step_completed","session_id":"chat-1","turn_id":"turn-1","model_step_id":"step-1","step_index":0,"started_at_ms":1000,"completed_at_ms":1450,"outcome":{"status":"retrying"}}},"stream_metrics":[],"blocks":[],"preview":null}}"#
        )
        guard case .agentEvent(_, let retryRecord) = retry else {
            return XCTFail("Expected retrying model completion")
        }
        XCTAssertEqual(retryRecord.event.msg["outcome"]?["status"]?.stringValue, "retrying")

        let unknownCitation = #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1400,"event":{"msg":{"type":"assistant_message","session_id":"chat-1","turn_id":"turn-1","model_step_id":"step-1","content":[{"output_index":0,"part_index":0,"phase":"final_answer","text":"Done","annotations":[{"type":"future"}]}],"message_target":null}},"stream_metrics":[],"blocks":[],"preview":null}}"#
        XCTAssertThrowsError(try decodeEnvelope(unknownCitation)) { error in
            XCTAssertEqual(
                error as? GatewayWireError,
                .invalidFrame(
                    "assistant_message has unknown content annotation future"
                )
            )
        }

        let search = try decodeEnvelope(
            #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":9,"recorded_at_ms":1500,"event":{"msg":{"type":"web_search_end","session_id":"chat-1","turn_id":"turn-1","model_step_id":"step-1","call_id":"search-1","action":{"type":"find_in_page","url":"https://example.com","pattern":"answer"}}},"stream_metrics":[],"blocks":[],"preview":null}}"#
        )
        guard case .agentEvent(_, let searchRecord) = search else {
            return XCTFail("Expected web search event")
        }
        XCTAssertEqual(searchRecord.event.msg["action"]?["type"]?.stringValue, "find_in_page")
        XCTAssertEqual(searchRecord.event.msg["action"]?["url"]?.stringValue, "https://example.com")
        XCTAssertEqual(searchRecord.event.msg["action"]?["pattern"]?.stringValue, "answer")

        let query = try decodeEnvelope(
            #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":10,"recorded_at_ms":1600,"event":{"msg":{"type":"web_search_end","session_id":"chat-1","turn_id":"turn-1","model_step_id":"step-1","call_id":"search-2","action":{"type":"search","queries":["first query","second query"]}}},"stream_metrics":[],"blocks":[],"preview":null}}"#
        )
        guard case .agentEvent(_, let queryRecord) = query else {
            return XCTFail("Expected web query event")
        }
        XCTAssertEqual(
            queryRecord.event.msg["action"]?["queries"]?.arrayValue?.compactMap(\.stringValue),
            ["first query", "second query"]
        )
    }

    func testGatewayOnlyEventInvariantsAreRejected() {
        let fixtures = [
            #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"session_history","events":[]}},"stream_metrics":[],"blocks":[],"preview":null}}"#,
        ]

        for fixture in fixtures {
            XCTAssertThrowsError(try decodeEnvelope(fixture))
        }
    }

    func testUnknownAgentEventIsRejected() {
        let fixture = #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"future_event"}},"stream_metrics":[],"blocks":[],"preview":null}}"#
        XCTAssertThrowsError(try decodeEnvelope(fixture)) { error in
            XCTAssertEqual(
                error as? GatewayWireError,
                .invalidFrame("unknown agent event future_event")
            )
        }
    }

    func testInvalidFrontendEventSubtypeIsRejected() {
        let fixtures = [
            (#"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"frontend"}},"stream_metrics":[],"blocks":[],"preview":null}}"#, "frontend event has no frontend_type"),
            (#"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"frontend","frontend_type":"future_frontend"}},"stream_metrics":[],"blocks":[],"preview":null}}"#, "unknown frontend event future_frontend")
        ]

        for (fixture, message) in fixtures {
            XCTAssertThrowsError(try decodeEnvelope(fixture)) { error in
                XCTAssertEqual(error as? GatewayWireError, .invalidFrame(message))
            }
        }
    }

    func testMalformedFrontendEventPayloadIsRejected() {
        let fixtures = [
            (#"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"frontend","frontend_type":"render","capability":"tools"}},"stream_metrics":[],"blocks":[],"preview":null}}"#, "frontend render is missing a required field"),
            (#"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"frontend","frontend_type":"picker","title":"Choose","options":[{"label":"One","description":"First"}]}},"stream_metrics":[],"blocks":[],"preview":null}}"#, "frontend picker option is missing a required field")
        ]

        for (fixture, message) in fixtures {
            XCTAssertThrowsError(try decodeEnvelope(fixture)) { error in
                XCTAssertEqual(error as? GatewayWireError, .invalidFrame(message))
            }
        }
    }

    func testUnknownRenderedPresentationValuesAreRejected() {
        let outerBlock = #"{"version":27,"type":"agent_event","session_id":"chat-1","record":{"sequence":8,"recorded_at_ms":1000,"event":{"msg":{"type":"turn_complete","turn_id":"turn-1"}},"stream_metrics":[],"blocks":[{"capability":"tools","block":{"id":null,"group":null,"update":"replace","state":"complete","role":"tool","title":"Done","text":"","symbol":null,"format":"future_format","tone":"neutral","files":[]}}],"preview":null}}"#
        XCTAssertThrowsError(try decodeEnvelope(outerBlock))

        let invalidWidgetPayload = sessionReadyPayloadJSON.replacingOccurrences(
            of: #""slot":"composer_footer""#,
            with: #""slot":"future_slot""#
        )
        XCTAssertThrowsError(try decodeEnvelope(
            #"{"version":27,"type":"session_opened","request_id":"open-1","payload":\#(invalidWidgetPayload)}"#
        ))
    }

    func testShellWidgetSlotsAreAccepted() throws {
        for (rawSlot, expected): (String, FrontendSlot) in [
            ("transcript_tail", .transcriptTail),
            ("message_actions", .messageActions),
            ("navigation", .navigation),
            ("chat_menu", .chatMenu)
        ] {
            let payload = sessionReadyPayloadJSON.replacingOccurrences(
                of: #""slot":"composer_footer""#,
                with: #""slot":"\#(rawSlot)""#
            )
            let envelope = try decodeEnvelope(
                #"{"version":27,"type":"session_opened","request_id":"open-1","payload":\#(payload)}"#
            )
            guard case .sessionOpened(_, let ready) = envelope else {
                return XCTFail("Expected session opened envelope")
            }

            XCTAssertEqual(ready.contributions.first?.widgets.first?.slot, expected)
        }
    }

    func testActionListDecodesStableRowsActionsAndEditableInput() throws {
        let widget = try FrontendWidget(json: .object([
            "id": .string("notes"),
            "slot": .string("navigation"),
            "text": .string("Notes"),
            "tone": .string("neutral"),
            "symbol": .string("brain"),
            "iconOnly": .bool(false),
            "progress": .null,
            "content": .object([
                "type": .string("action_list"),
                "title": .string("Notes"),
                "items": .array([.object([
                    "id": .string("note-1"),
                    "text": .string("Prefer small native controls."),
                    "state": .string("completed"),
                    "actions": .array([.object([
                        "id": .string("edit:note-1"),
                        "label": .string("Edit"),
                        "symbol": .string("edit"),
                        "tone": .string("neutral"),
                        "op": .object([
                            "type": .string("capability_command"),
                            "capability": .string("notes"),
                            "command": .string("edit"),
                            "arguments": .string("note-1"),
                            "input": .string("Prefer small native controls."),
                            "target": .null
                        ])
                    ])])
                ])])
            ]),
            "action": .null
        ]))

        guard case .actionList(let title, let items) = widget.content,
              let item = items.first,
              let action = item.actions.first,
              case .capabilityCommand(_, _, _, let input, _) = action.op
        else { return XCTFail("Expected action list") }
        XCTAssertEqual(title, "Notes")
        XCTAssertEqual(item.id, "note-1")
        XCTAssertEqual(item.text, "Prefer small native controls.")
        XCTAssertEqual(item.state, .completed)
        XCTAssertEqual(action.id, "edit:note-1")
        XCTAssertEqual(action.symbol, "edit")
        XCTAssertEqual(input, "Prefer small native controls.")

        guard case .capabilityCommand(_, _, _, let edited, _) =
            action.op.replacingCapabilityInput(with: "Use one row.")
        else { return XCTFail("Expected edited capability command") }
        XCTAssertEqual(edited, "Use one row.")
    }

}
