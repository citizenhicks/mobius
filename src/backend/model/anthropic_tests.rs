use super::*;
use crate::backend::model::user_message;

#[tokio::test]
async fn hosted_results_resume_pending_calls_from_native_replay() {
    for reason in ["tool_use", "pause_turn"] {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&seen);
        let sink: ModelEventSink = Arc::new(move |event| {
            captured.lock().expect("events").push(event);
            Box::pin(async { Ok(()) })
        });
        let mut first = StreamState::default();
        for event in [
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"search","name":"web_search","input":{}}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"mobius\",\"filters\":{\"allowed_domains\":[\"example.com\"]}}"}}),
            serde_json::json!({"type":"content_block_stop","index":0}),
        ] {
            first.apply(event, &sink).await.expect("pending search");
        }
        if reason == "tool_use" {
            for event in [
                serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"read","name":"read","input":{"path":"README.md"}}}),
                serde_json::json!({"type":"content_block_stop","index":1}),
            ] {
                first.apply(event, &sink).await.expect("client call");
            }
        }
        first
            .apply(
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":reason}}),
                &sink,
            )
            .await
            .expect("first response end");
        first
            .apply(serde_json::json!({"type":"message_stop"}), &sink)
            .await
            .expect("first response stop");
        let mut history = vec![user_message("Search after reading")];
        history.extend(first.finish().expect("first output").output);
        if reason == "tool_use" {
            history.push(serde_json::json!({"type":"function_call_output","call_id":"read","output":[{"type":"input_text","text":"contents"}]}));
        }
        let replay = translate_messages(
            history.as_slice().into(),
            ToolDiscoveryMode::Rebuild,
            "catalog",
            &[],
        )
        .expect("continuation request");
        assert_eq!(replay[1].content[0]["id"], "search");
        assert_eq!(replay[1].content[0]["input"]["query"], "mobius");
        seen.lock().expect("events").clear();

        let mut second = StreamState::new(history.as_slice().into());
        assert!(seen.lock().expect("events").is_empty());
        let result = serde_json::json!({"type":"web_search_tool_result","tool_use_id":"search","content":[{"type":"web_search_result","url":"https://example.com","title":"Source","content":"answer","encrypted_index":"opaque"}]});
        second
            .apply(
                serde_json::json!({"type":"content_block_start","index":0,"content_block":result}),
                &sink,
            )
            .await
            .expect("result-only continuation");
        assert!(matches!(
            seen.lock().expect("events").as_slice(),
            [ModelEvent::HostedToolStarted { call_id }, ModelEvent::WebSearchStarted { call_id: web_id }]
                if call_id.as_str() == "search" && web_id.as_ref() == "search"
        ));
        second
            .apply(
                serde_json::json!({"type":"content_block_stop","index":0}),
                &sink,
            )
            .await
            .expect("continued search completion");
        {
            let events = seen.lock().expect("events");
            let execution = events
                .iter()
                .find_map(|event| match event {
                    ModelEvent::HostedToolCompleted(execution) => Some(execution),
                    _ => None,
                })
                .expect("actual execution");
            assert_eq!(execution.call_id.as_str(), "search");
            assert_eq!(&*execution.name, "web_search");
            assert_eq!(
                execution.arguments.as_ref(),
                Some(&replay[1].content[0]["input"])
            );
            assert_eq!(
                execution.result_fingerprint,
                hosted_result_fingerprint("web_search", &result["content"])
            );
            assert!(execution.result_fingerprint.is_some());
            drop(events);
        }
        let output = second.finish().expect("continued output");
        assert_eq!(output.output[0][RAW_CONTENT][0], result);
        history.extend(output.output);

        // Completed history cannot authorize a second result for the same call.
        let mut unmatched = StreamState::new(history.as_slice().into());
        unmatched
            .apply(
                serde_json::json!({"type":"content_block_start","index":0,"content_block":result}),
                &sink,
            )
            .await
            .expect("unmatched result start");
        let error = unmatched
            .apply(
                serde_json::json!({"type":"content_block_stop","index":0}),
                &sink,
            )
            .await
            .expect_err("completed call is no longer pending");
        assert!(error.to_string().contains("unknown call"));
    }
}

#[tokio::test]
async fn hosted_results_use_final_streamed_inputs_and_stable_visible_evidence() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);
    let sink: ModelEventSink = Arc::new(move |event| {
        captured.lock().expect("events").push(event);
        Box::pin(async { Ok(()) })
    });
    let mut stream = StreamState::default();
    for event in [
        serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"search","name":"web_search","input":{}}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"mobius\",\"filters\":{\"allowed_domains\":[\"example.com\"]}}"}}),
        serde_json::json!({"type":"content_block_stop","index":0}),
        serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"search","content":[{"type":"web_search_result","url":"https://example.com","title":"Source","content":"answer","encrypted_index":"opaque"}]}}),
    ] {
        stream.apply(event, &sink).await.expect("stream event");
    }
    assert!(
        !seen
            .lock()
            .expect("events")
            .iter()
            .any(|event| matches!(event, ModelEvent::HostedToolCompleted(_)))
    );
    assert_eq!(
        seen.lock()
            .expect("events")
            .iter()
            .filter(|event| matches!(event, ModelEvent::HostedToolStarted { .. }))
            .count(),
        1
    );
    stream
        .apply(
            serde_json::json!({"type":"content_block_stop","index":1}),
            &sink,
        )
        .await
        .expect("completed search");
    {
        let events = seen.lock().expect("events");
        let execution = events
            .iter()
            .find_map(|event| match event {
                ModelEvent::HostedToolCompleted(execution) => Some(execution),
                _ => None,
            })
            .expect("actual hosted execution");
        assert_eq!(execution.arguments.as_ref().unwrap()["query"], "mobius");
        assert_eq!(
            execution.arguments.as_ref().unwrap()["filters"]["allowed_domains"],
            serde_json::json!(["example.com"])
        );
        assert_eq!(execution.sources, ["https://example.com"]);
        assert!(execution.result_fingerprint.is_some());
        drop(events);
    }
    assert_eq!(stream.blocks[&0]["input"]["query"], "mobius");
    assert_eq!(stream.blocks[&1]["content"][0]["encrypted_index"], "opaque");
    let blocked: ModelEventSink = Arc::new(|event| {
        Box::pin(async move {
            if matches!(event, ModelEvent::HostedToolCompleted(_)) {
                Err(Error::Stopped(
                    "shared tool policy stopped execution".into(),
                ))
            } else {
                Ok(())
            }
        })
    });
    assert!(matches!(
        stream.emit_hosted_completion(1, &blocked).await,
        Err(Error::Stopped(_))
    ));

    let first = serde_json::json!([
        {"url":"https://a.example","title":"A","content":"one","encrypted_index":"first"},
        {"url":"https://b.example","title":"B","content":"two","encrypted_index":"second"}
    ]);
    let reordered = serde_json::json!([
        {"url":"https://b.example","title":"B","content":"two","encrypted_index":"different"},
        {"url":"https://a.example","title":"A","content":"one","encrypted_index":"other"}
    ]);
    assert_eq!(
        hosted_result_fingerprint("web_search", &first),
        hosted_result_fingerprint("web_search", &reordered)
    );
    let mut changed = reordered;
    changed[0]["content"] = "new evidence".into();
    assert_ne!(
        hosted_result_fingerprint("web_search", &first),
        hosted_result_fingerprint("web_search", &changed)
    );
    assert_eq!(
        hosted_result_fingerprint(
            "web_search",
            &serde_json::json!([{"url":"https://example.com","title":"Source","encrypted_content":"opaque"}])
        ),
        None
    );
    assert_eq!(
        hosted_result_fingerprint(
            "code_execution",
            &serde_json::json!({"content":[{"encrypted_content":"opaque"}]})
        ),
        None
    );
}

#[test]
fn explicit_cache_endpoint_advances_and_keeps_the_stable_anchor() {
    let provider = Anthropic::new("test-key", MANIFEST.base_url.share(), "claude-sonnet-5-5")
        .expect("provider");
    let mut first = user_message("stable prefix");
    assert!(crate::backend::model::mark_prompt_cache_breakpoint(
        &mut first
    ));
    let history = [
        first,
        serde_json::json!({"role":"assistant","content":[{"type":"output_text","text":"answer"}]}),
        user_message("follow-up"),
    ];
    let cache_points = |body: &Value| {
        body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .filter(|block| block.get("cache_control").is_some())
            .count()
    };

    let short = provider
        .request_body_value(
            "instructions",
            (&history[..1]).into(),
            "catalog-1",
            &[],
            &[],
            false,
        )
        .expect("first request");
    let long = provider
        .request_body_value(
            "instructions",
            (&history).into(),
            "catalog-1",
            &[],
            &[],
            false,
        )
        .expect("follow-up request");

    assert_eq!(cache_points(&short), 1);
    assert_eq!(
        long["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(
        long["messages"][2]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert!(
        long["messages"][1]["content"][0]
            .get("cache_control")
            .is_none()
    );
    assert!(cache_points(&long) <= 4);
}

#[test]
fn tool_schemas_preserve_nested_combinators_and_other_root_fields() {
    let tool = Arc::new(ToolDefinition {
        name: "custom".into(),
        description: "test tool".into(),
        parameters: serde_json::json!({
            "type": "object", "required": ["input"], "additionalProperties": false,
            "oneOf": [{"required": ["input"]}],
            "allOf": [{"required": ["input"]}],
            "anyOf": [{"required": ["input"]}],
            "properties": {"input": {"anyOf": [{"type": "string"}, {"type": "number"}]}},
            "$defs": {"choice": {"oneOf": [{"type": "string"}, {"type": "number"}]}}
        }),
    });
    let wire = serde_json::to_value(wire_tools(std::slice::from_ref(&tool), &[], false))
        .expect("wire tools");
    assert_eq!(
        wire[0]["input_schema"],
        serde_json::json!({
            "type": "object", "required": ["input"], "additionalProperties": false,
            "properties": &tool.parameters["properties"], "$defs": &tool.parameters["$defs"]
        })
    );
}

#[test]
fn native_discovery_defers_schemas_and_replays_tool_references() {
    let direct = [discovery_tool(TOOLS_SEARCH_NAME)];
    let deferred = [discovery_tool("notebook_post")];
    let input = discovery_history();

    for base_url in [MANIFEST.base_url.as_str(), "https://proxy.example/v1"] {
        let provider = Anthropic::new(
            "test-key",
            BaseUrl::new(base_url).unwrap(),
            "claude-sonnet-5-5",
        )
        .expect("provider");
        let body = provider
            .request_body_value(
                "instructions",
                (&input).into(),
                "catalog-1",
                &direct,
                &deferred,
                false,
            )
            .expect("request body");

        assert_eq!(
            (
                body["tools"][0].get("defer_loading"),
                body["tools"][1]["defer_loading"].as_bool(),
                &body["messages"][2]["content"][0]["content"],
                body.to_string().contains("tool_load"),
            ),
            (
                None,
                Some(true),
                &serde_json::json!([{"type": "tool_reference", "tool_name": "notebook_post"}]),
                false,
            ),
            "{base_url}"
        );
    }
}

fn discovery_tool(name: &str) -> Arc<ToolDefinition> {
    Arc::new(ToolDefinition {
        name: name.into(),
        description: "test tool".into(),
        parameters: serde_json::json!({"type": "object"}),
    })
}

fn discovery_history() -> Vec<Value> {
    vec![
        user_message("Find a collaboration tool."),
        serde_json::json!({
            "type": "function_call",
            "call_id": "search-1",
            "name": TOOLS_SEARCH_NAME,
            "arguments": "{\"query\":\"notebook\"}"
        }),
        serde_json::json!({
            "type": "function_call_output",
            "call_id": "search-1",
            "output": [{"type": "input_text", "text": "Found notebook_post"}]
        }),
        ToolLoad {
            catalog_revision: "catalog-1".into(),
            tools: vec!["notebook_post".into()],
        }
        .to_input(),
    ]
}

#[test]
fn responses_history_translates_to_anthropic_tool_messages() {
    let messages = translate_messages(
        (&[
            user_message("inspect it"),
            serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Checking."}]
            }),
            serde_json::json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "read_file",
                "arguments": "{\"path\":\"README.md\"}"
            }),
            serde_json::json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": [{"type": "input_text", "text": "contents"}]
            }),
        ])
            .into(),
        ToolDiscoveryMode::Rebuild,
        "catalog-1",
        &[],
    )
    .map(|messages| {
        messages
            .into_iter()
            .map(|message| serde_json::to_value(message).unwrap())
            .collect::<Vec<_>>()
    })
    .expect("translate history");

    assert_eq!(
        messages,
        vec![
            serde_json::json!({
                "role": "user",
                "content": [{"type": "text", "text": "inspect it"}]
            }),
            serde_json::json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Checking."},
                    {
                        "type": "tool_use",
                        "id": "call_1",
                        "name": "read_file",
                        "input": {"path": "README.md"}
                    }
                ]
            }),
            serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call_1",
                    "content": [{"type":"text", "text":"contents"}],
                    "is_error": false,
                    "cache_control": {"type": "ephemeral"}
                }]
            })
        ]
    );
}

#[tokio::test]
async fn stream_normalizes_deltas_tools_usage_and_errors() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Box::pin(async { Ok(()) })
    });
    let mut stream = StreamState::default();
    for event in [
        serde_json::json!({
            "type": "message_start",
            "message": {"usage": {
                "input_tokens": 6,
                "cache_read_input_tokens": 4,
                "cache_creation_input_tokens": 2
            }}
        }),
        serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": ""}
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "Reading."}
        }),
        serde_json::json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {"type": "thinking", "thinking": ""}
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": {"type": "thinking_delta", "thinking": "Plan."}
        }),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
        serde_json::json!({"type": "content_block_stop", "index": 1}),
        serde_json::json!({
            "type": "content_block_start",
            "index": 2,
            "content_block": {
                "type": "tool_use",
                "id": "call-1",
                "name": "read",
                "input": {}
            }
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 2,
            "delta": {"type": "input_json_delta", "partial_json": "{\"path\":\"README.md\"}"}
        }),
        serde_json::json!({"type": "content_block_stop", "index": 2}),
        serde_json::json!({
            "type": "content_block_start", "index": 3,
            "content_block": {"type": "tool_use", "id": "call-2", "name": "read", "input": {}}
        }),
        serde_json::json!({
            "type": "content_block_delta", "index": 3,
            "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}
        }),
        serde_json::json!({"type": "content_block_stop", "index": 3}),
        serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use"},
            "usage": {"output_tokens": 3}
        }),
        serde_json::json!({"type": "message_stop"}),
    ] {
        stream.apply(event, &events).await.expect("stream event");
    }

    let output = stream.finish().expect("normalized output");
    assert_eq!(output.text(), "");
    assert_eq!(output.content()[0].text, "Reading.");
    assert_eq!(output.content()[0].phase, ModelStepContentPhase::Commentary);
    assert_eq!(output.content()[1].phase, ModelStepContentPhase::Reasoning);
    assert_eq!(output.tool_calls()[0].arguments["path"], "README.md");
    assert_eq!(output.usage().input_tokens, 12);
    assert_eq!(output.usage().cached_input_tokens, 4);
    assert!(matches!(
        seen.lock().expect("events lock").as_slice(),
        [
            ModelEvent::TextDelta(_),
            ModelEvent::ReasoningDelta(_),
            ModelEvent::ToolCallReady(_),
            ModelEvent::ToolCallReady(call)
        ] if call == &output.tool_calls()[1]
    ));

    let repaired = &output.tool_calls()[1].arguments;
    assert!(repaired.get(super::super::INVALID_TOOL_ARGUMENTS).is_some());
    let replay = translate_messages(
        output.output().into(),
        ToolDiscoveryMode::Rebuild,
        "catalog-1",
        &[],
    )
    .expect("native replay");
    assert_eq!(&replay[0].content[3]["input"], repaired);

    let error = StreamState::default()
        .apply(
            serde_json::json!({"type": "error", "error": {"message": "quota"}}),
            &events,
        )
        .await
        .expect_err("stream error");
    assert!(error.to_string().contains("quota"));
}

#[tokio::test]
async fn empty_refusals_are_non_retryable_errors_and_visible_refusals_are_preserved() {
    let events: ModelEventSink = Arc::new(|_| Box::pin(async { Ok(()) }));
    for (block, answer) in [
        (None, None),
        (
            Some(serde_json::json!({"type": "thinking", "thinking": "", "signature": "signed"})),
            None,
        ),
        (Some(serde_json::json!({"type": "text", "text": " "})), None),
        (
            Some(serde_json::json!({"type": "text", "text": "I cannot help with this request."})),
            Some("I cannot help with this request."),
        ),
    ] {
        let mut stream = StreamState::default();
        stream
            .apply(
                serde_json::json!({
                    "type": "message_start",
                    "message": {"content": [], "usage": {"input_tokens": 1, "output_tokens": 0}}
                }),
                &events,
            )
            .await
            .expect("message start");
        if let Some(block) = block {
            stream
                .apply(
                    serde_json::json!({
                        "type": "content_block_start", "index": 0, "content_block": block
                    }),
                    &events,
                )
                .await
                .expect("content block");
            stream
                .apply(
                    serde_json::json!({"type": "content_block_stop", "index": 0}),
                    &events,
                )
                .await
                .expect("block stop");
        }
        stream
            .apply(
                serde_json::json!({
                    "type": "message_delta", "delta": {"stop_reason": "refusal"},
                    "usage": {"output_tokens": 0}
                }),
                &events,
            )
            .await
            .expect("refusal");
        stream
            .apply(serde_json::json!({"type": "message_stop"}), &events)
            .await
            .expect("message stop");
        if let Some(answer) = answer {
            assert_eq!(stream.finish().expect("visible refusal").text(), answer);
        } else {
            let Error::Provider(error) = stream.finish().expect_err("empty refusal") else {
                panic!("expected provider error");
            };
            assert_eq!(
                error.to_string(),
                "Anthropic refused the request without an answer"
            );
            assert!(!error.is_retryable());
        }
    }
}

#[test]
fn route_replay_strips_private_reasoning_without_copying_untouched_items() {
    let visible = std::sync::Arc::new(serde_json::json!({
        "type": "message", "role": "assistant", "content": "Visible answer"
    }));
    let private = std::sync::Arc::new(serde_json::json!({
        "type": "message", "role": "assistant", "content": "Visible answer",
        RAW_CONTENT: [{"type": "thinking", "thinking": "private"}]
    }));
    let call = std::sync::Arc::new(serde_json::json!({
        "type": "function_call", "call_id": "call", "name": "read", "arguments": "{}"
    }));
    let result = std::sync::Arc::new(serde_json::json!({
        "type": "function_call_output", "call_id": "call", "output": []
    }));
    let mut input = vec![
        std::sync::Arc::clone(&private),
        std::sync::Arc::clone(&call),
        std::sync::Arc::clone(&result),
        std::sync::Arc::clone(&visible),
    ];
    assert!(super::super::strip_shared_provider_reasoning(&mut input));
    assert_eq!(input[0]["content"], "Visible answer");
    assert!(!super::super::has_provider_reasoning(&input[0]));
    assert!(super::super::has_provider_reasoning(&private));
    assert!(std::sync::Arc::ptr_eq(&input[1], &call));
    assert!(std::sync::Arc::ptr_eq(&input[2], &result));
    assert!(std::sync::Arc::ptr_eq(&input[3], &visible));
    assert!(
        !input
            .iter()
            .any(|item| super::super::has_provider_reasoning(item))
    );
    assert!(!super::super::strip_shared_provider_reasoning(&mut input));
    assert!(std::sync::Arc::ptr_eq(&input[3], &visible));
}
