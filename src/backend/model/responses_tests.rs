use super::*;
use crate::backend::model::ImageGenerationReference;
use crate::backend::model::PromptCacheIdentity;
use crate::backend::model::REPLAY_REASONING_FIELD;
use crate::backend::model::transport::capture_http_request;
use crate::protocol::PROMPT_CACHE_BREAKPOINT_FIELD;

fn model_request() -> ModelRequest<'static> {
    ModelRequest {
        session_id: "test-session",
        cancellation: None,
        prompt_cache: Some(PromptCacheIdentity {
            key: "hashed-cache-key",
            context_epoch: 3,
        }),
        instructions: "Test instructions",
        input: (&[]).into(),
        catalog_revision: "catalog-1",
        tools: &[],
        deferred_tools: &[],
        allow_hosted_tools: false,
        allow_continuation: false,
    }
}

fn tool_definition(name: &str) -> Arc<ToolDefinition> {
    Arc::new(ToolDefinition {
        name: name.into(),
        description: format!("Use {name}"),
        parameters: serde_json::json!({"type": "object"}),
    })
}

#[test]
fn service_tier_is_optional_and_preserves_native_values() {
    for tier in [None, Some("default"), Some("priority")] {
        let provider = OpenAi::new("key", "https://example.com/v1", "model")
            .expect("provider")
            .with_service_tier(tier.map(str::to_owned));
        let body = provider
            .response_body_value(model_request())
            .expect("request body");
        assert_eq!(body.get("service_tier").and_then(Value::as_str), tier);
    }
}

#[test]
fn base_url_rejects_serializable_secret_locations() {
    for url in [
        "https://secret@example.com/v1",
        "https://example.com/v1?key=secret",
        "https://example.com/v1#secret",
    ] {
        assert!(OpenAi::new("test-key", url, "test-model").is_err());
    }
}

#[test]
fn custom_openai_endpoints_keep_native_capabilities() {
    for model in ["gpt-6-luna", "gpt-6.1-sol", "gpt-6-astra"] {
        let official = OpenAi::new("test-key", "https://api.openai.com:443/v1/", model)
            .expect("official provider");
        let compatible =
            OpenAi::new("test-key", "https://example.com/v1", model).expect("compatible provider");
        assert!(official.supports_realtime_voice());
        assert!(official.supports_image_generation());
        assert!(compatible.supports_realtime_voice());
        assert!(compatible.supports_image_generation());
    }
}

#[test]
fn endpoint_url_does_not_infer_reasoning_summary_support() {
    let provider = OpenAi::new("test-key", "https://api.openai.com/v1/", "test-model")
        .expect("provider")
        .with_reasoning_effort("medium")
        .expect("reasoning effort");

    assert_eq!(
        provider
            .response_body_value(model_request())
            .expect("response body")["reasoning"],
        serde_json::json!({"effort": "medium"})
    );
}

#[test]
fn compatible_endpoint_does_not_assume_reasoning_summary_support() {
    let provider = OpenAi::new("test-key", "https://example.com/v1", "test-model")
        .expect("provider")
        .with_reasoning_effort("medium")
        .expect("reasoning effort");

    assert_eq!(
        provider
            .response_body_value(model_request())
            .expect("response body")["reasoning"],
        serde_json::json!({"effort": "medium"})
    );
}

#[test]
fn compatible_endpoint_can_opt_into_automatic_reasoning_summaries() {
    let provider = OpenAi::new("test-key", "https://example.com/v1", "test-model")
        .expect("provider")
        .with_reasoning_effort("medium")
        .expect("reasoning effort")
        .with_reasoning_summary();

    assert_eq!(
        provider
            .response_body_value(model_request())
            .expect("response body")["reasoning"],
        serde_json::json!({"effort": "medium", "summary": "auto"})
    );
}

#[test]
fn reasoning_summary_opt_in_can_use_the_models_default_effort() {
    let provider = OpenAi::new("test-key", "https://example.com/v1", "test-model")
        .expect("provider")
        .with_reasoning_summary();

    assert_eq!(
        provider
            .response_body_value(model_request())
            .expect("response body")["reasoning"],
        serde_json::json!({"summary": "auto"})
    );
}

#[test]
fn responses_input_strips_only_top_level_provider_metadata() {
    let input = vec![
        serde_json::json!({
            "type": "function_call",
            "arguments": {"_keep": true},
            "_mobius_reasoning": "Plan.",
            "_provider_internal": [{"type": "thinking"}]
        }),
        serde_json::json!({
            "type": "reasoning",
            "encrypted_content": "opaque",
            "format": "openai-responses-v1",
            "status": "completed",
            "summary": []
        }),
        serde_json::json!({
            "type": "function_call",
            "call_id": "call-1",
            "name": "inspect",
            "arguments": "{}",
            "status": "completed"
        }),
        serde_json::json!({
            "type": "message",
            "role": "assistant",
            "phase": "commentary",
            "status": "completed",
            "content": [{"type": "output_text", "text": "done"}]
        }),
        serde_json::json!({
            "type": "web_search_call",
            "status": "completed"
        }),
    ];

    assert_eq!(
        wire_input_with_cache((&input).into(), true, false, "catalog-1", &[]).expect("wire input"),
        vec![
            serde_json::json!({
                "type": "function_call",
                "arguments": {"_keep": true}
            }),
            serde_json::json!({
                "type": "reasoning",
                "encrypted_content": "opaque",
                "summary": []
            }),
            serde_json::json!({
                "type": "function_call",
                "call_id": "call-1",
                "name": "inspect",
                "arguments": "{}"
            }),
            serde_json::json!({
                "type": "message",
                "role": "assistant",
                "phase": "commentary",
                "content": [{"type": "output_text", "text": "done"}]
            }),
            serde_json::json!({
                "type": "web_search_call",
                "status": "completed"
            }),
        ]
    );
}

#[test]
fn compatible_responses_are_implicit_while_first_party_breakpoints_are_explicit() {
    let input = [serde_json::json!({
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": "stable prefix",
            "_mobius_prompt_cache_breakpoint": true
        }]
    })];
    let request = ModelRequest {
        session_id: "local-session",
        cancellation: None,
        prompt_cache: Some(PromptCacheIdentity {
            key: "opaque-cache-key",
            context_epoch: 4,
        }),
        instructions: "Instructions",
        input: (&input).into(),
        catalog_revision: "catalog-1",
        tools: &[],
        deferred_tools: &[],
        allow_hosted_tools: false,
        allow_continuation: true,
    };

    let compatible = OpenAi::new("test-key", "https://example.com/v1", "test-model")
        .expect("compatible provider");
    let compatible_body = compatible
        .response_body_value(request)
        .expect("compatible body");
    assert_eq!(compatible_body["prompt_cache_key"], "opaque-cache-key");
    assert!(compatible_body.get("prompt_cache_options").is_none());
    assert_eq!(
        compatible_body["input"][0]["content"][0],
        serde_json::json!({"type": "input_text", "text": "stable prefix"})
    );

    let request = ModelRequest {
        session_id: "local-session",
        cancellation: None,
        prompt_cache: Some(PromptCacheIdentity {
            key: "opaque-cache-key",
            context_epoch: 4,
        }),
        instructions: "Instructions",
        input: (&input).into(),
        catalog_revision: "catalog-1",
        tools: &[],
        deferred_tools: &[],
        allow_hosted_tools: false,
        allow_continuation: true,
    };
    let first_party = OpenAi::new("test-key", "https://api.openai.com/v1", "test-model")
        .expect("first-party provider")
        .with_explicit_prompt_cache();
    let first_party_body = first_party
        .response_body_value(request)
        .expect("first-party body");
    assert_eq!(
        first_party_body["prompt_cache_options"],
        serde_json::json!({"mode": "explicit"})
    );
    assert_eq!(
        first_party_body["input"][0]["content"][0],
        serde_json::json!({
            "type": "input_text",
            "text": "stable prefix",
            "prompt_cache_breakpoint": {"mode": "explicit"}
        })
    );
}

#[test]
fn explicit_cache_endpoints_advance_and_preserve_previous_lookup_boundaries() {
    let input = serde_json::json!([
        {"role":"user", "content":[{"type":"input_text", "text":"first", "_mobius_prompt_cache_breakpoint":true}]},
        {"role":"assistant", "content":[{"type":"output_text", "text":"answer"}]},
        {"role":"user", "content":"next turn"},
        {"type":"function_call", "call_id":"call-1", "name":"inspect", "arguments":"{}"},
        {"type":"function_call_output", "call_id":"call-1", "output":"tool result"},
        {"type":"function_call_output", "call_id":"call-2", "output":[
            {"type":"input_text", "text":"screenshot"},
            {"type":"input_image", "media_type":"image/png", "data":"pixels"}
        ]},
        {"role":"developer", "content":[{"type":"input_text", "text":"continue"}]},
        {"type":"reasoning", "encrypted_content":"opaque"}
    ]);
    let original = input.to_string();
    let history = input.as_array().expect("history");
    let endpoints = [
        (0, "content", 0),
        (2, "content", 0),
        (4, "output", 0),
        (5, "output", 1),
        (6, "content", 0),
    ];
    for end in 1..=history.len() {
        let wired = wire_input_with_cache((&history[..end]).into(), true, true, "catalog-1", &[])
            .expect("explicit replay");
        for &(index, field, part) in &endpoints {
            if index < end {
                assert_eq!(
                    wired[index][field][part]["prompt_cache_breakpoint"],
                    serde_json::json!({"mode":"explicit"})
                );
            }
        }
        let expected_count = endpoints
            .iter()
            .filter(|(index, _, _)| *index < end)
            .count();
        assert_eq!(
            serde_json::to_string(&wired)
                .expect("wire JSON")
                .matches("prompt_cache_breakpoint")
                .count(),
            expected_count
        );
    }
    let suffix = wire_input_with_cache((&history[4..6]).into(), true, true, "catalog-1", &[])
        .expect("continuation suffix");
    assert_eq!(
        suffix[1]["output"][1]["prompt_cache_breakpoint"],
        serde_json::json!({"mode":"explicit"})
    );
    let implicit = wire_input_with_cache((history).into(), true, false, "catalog-1", &[])
        .expect("implicit replay");
    assert!(
        !serde_json::to_string(&implicit)
            .expect("wire JSON")
            .contains("prompt_cache_breakpoint")
    );
    assert_eq!(
        input.to_string(),
        original,
        "durable history must stay unchanged"
    );
}

#[test]
fn responses_decode_strips_reasoning_wire_metadata() {
    let decoded = decode_response(serde_json::json!({
        "output": [{
            "type": "reasoning",
            "format": "openai-responses-v1",
            "status": "completed",
            "summary": [{"type": "summary_text", "text": "Plan."}]
        }]
    }))
    .expect("decode response");

    assert_eq!(decoded.output()[0].get("format"), None);
    assert_eq!(decoded.output()[0].get("status"), None);
    assert_eq!(decoded.output()[0][REPLAY_REASONING_FIELD], "Plan.");
}

#[test]
fn responses_converts_neutral_images_and_rejects_them_when_disabled() {
    let input = [serde_json::json!({
        "role": "user",
        "content": [
            {"type": "input_text", "text": "What is this?"},
            {"type": "input_image", "media_type": "image/png", "data": "aGVsbG8="}
        ]
    })];

    let wired =
        wire_input_with_cache((&input).into(), true, false, "catalog-1", &[]).expect("wire image");
    assert_eq!(
        wired[0]["content"][1],
        serde_json::json!({
            "type": "input_image",
            "image_url": "data:image/png;base64,aGVsbG8="
        })
    );
    assert!(
        wire_input_with_cache((&input).into(), false, false, "catalog-1", &[])
            .expect_err("disabled image input")
            .to_string()
            .contains("does not support image attachments")
    );
}

#[test]
fn responses_applies_cache_markers_after_mapping_each_observation_kind() {
    let input = [serde_json::json!({
        "type": "function_call_output", "call_id": "call",
        "output": [
            {"type": "input_text", "text": "before", (PROMPT_CACHE_BREAKPOINT_FIELD): true},
            {"type": "file", "file": {"id": "report"}, (PROMPT_CACHE_BREAKPOINT_FIELD): true},
            {"type": "input_image", "media_type": "image/png", "data": "aGVsbG8=", "detail": "high", (PROMPT_CACHE_BREAKPOINT_FIELD): true}
        ]
    })];
    for explicit in [false, true] {
        let wired = wire_input_with_cache((&input).into(), true, explicit, "catalog", &[])
            .expect("wire observations");
        let parts = wired[0]["output"].as_array().expect("ordered content");
        for (index, part) in parts.iter().enumerate() {
            assert!(part.get(PROMPT_CACHE_BREAKPOINT_FIELD).is_none());
            assert_eq!(
                part.get("prompt_cache_breakpoint"),
                (explicit && index + 1 == parts.len())
                    .then_some(&serde_json::json!({"mode": "explicit"}))
            );
        }
        assert_eq!(parts[1]["type"], "input_text");
        assert_eq!(parts[2]["image_url"], "data:image/png;base64,aGVsbG8=");
        assert_eq!(parts[2]["detail"], "high");
    }
}

#[test]
fn hosted_tools_can_be_disabled_per_request() {
    let hosted = [serde_json::json!({"type": "web_search"})];

    assert_eq!(
        serde_json::to_value(wire_tools(&[], &hosted, false)).expect("tools"),
        serde_json::json!([])
    );
    assert_eq!(
        serde_json::to_value(wire_tools(&[], &hosted, true)).expect("tools"),
        serde_json::json!(hosted)
    );
}

#[test]
fn rebuild_filters_tool_load_control_items() {
    let loaded = [tool_definition("notebook_post")];
    let input = [ToolLoad {
        catalog_revision: "catalog-1".into(),
        tools: vec!["notebook_post".into()],
    }
    .to_input()];
    let provider =
        OpenAi::new("test-key", "https://example.com/v1", "test-model").expect("provider");
    let borrowed = provider
        .response_body(ModelRequest {
            input: (&input).into(),
            tools: &loaded,
            ..model_request()
        })
        .expect("borrowed response body");
    assert!(std::ptr::eq(
        &borrowed.tools.functions[0].parameters,
        &loaded[0].parameters
    ));
    let body = serde_json::to_value(borrowed).expect("response body");
    assert_eq!(
        body["tools"][0],
        serde_json::json!({
            "type": "function",
            "name": loaded[0].name,
            "description": loaded[0].description,
            "parameters": loaded[0].parameters,
            "strict": false,
        })
    );

    assert_eq!(
        (&body["input"], &body["tools"][0]["name"]),
        (&serde_json::json!([]), &serde_json::json!("notebook_post"))
    );

    let native = OpenAi::new("test-key", "https://api.openai.com/v1", "test-model")
        .expect("provider")
        .with_tool_discovery(ToolDiscoveryMode::Native)
        .response_body_value(ModelRequest {
            input: (&input).into(),
            deferred_tools: &loaded,
            ..model_request()
        })
        .expect("native response body");
    assert_eq!(
        native["input"][0]["type"],
        serde_json::json!("additional_tools")
    );
}

#[test]
fn native_discovery_ignores_tool_loads_from_an_old_catalog() {
    let deferred = [tool_definition("notebook_post")];
    let input = [ToolLoad {
        catalog_revision: "catalog-0".into(),
        tools: vec!["notebook_post".into()],
    }
    .to_input()];

    let body = OpenAi::new("test-key", "https://api.openai.com/v1", "test-model")
        .expect("provider")
        .with_tool_discovery(ToolDiscoveryMode::Native)
        .response_body_value(ModelRequest {
            input: (&input).into(),
            deferred_tools: &deferred,
            ..model_request()
        })
        .expect("response body");

    assert_eq!(body["input"], serde_json::json!([]));
}

#[test]
fn inline_search_defers_optional_tools_and_uses_hosted_search() {
    let direct = [
        tool_definition(TOOLS_SEARCH_NAME),
        tool_definition("read_file"),
    ];
    let deferred = [tool_definition("notebook_post")];
    let input = [ToolLoad {
        catalog_revision: "catalog-1".into(),
        tools: vec!["notebook_post".into()],
    }
    .to_input()];
    let body = OpenAi::new("test-key", "https://vendor.example/v1", "test-model")
        .expect("provider")
        .with_inline_tool_search("vendor:tool_search")
        .response_body_value(ModelRequest {
            input: (&input).into(),
            tools: &direct,
            deferred_tools: &deferred,
            allow_hosted_tools: false,
            ..model_request()
        })
        .expect("response body");

    assert_eq!(body["input"], serde_json::json!([]));
    assert_eq!(
        body["tools"],
        serde_json::json!([
            {"type": "vendor:tool_search"},
            {
                "type": "function",
                "name": "read_file",
                "description": "Use read_file",
                "parameters": {"type": "object"},
                "strict": false
            },
            {
                "type": "function",
                "name": "notebook_post",
                "description": "Use notebook_post",
                "parameters": {"type": "object"},
                "strict": false,
                "defer_loading": true
            }
        ])
    );
}

#[test]
fn inline_search_omits_tool_search_without_deferred_tools() {
    let body = OpenAi::new("test-key", "https://vendor.example/v1", "test-model")
        .expect("provider")
        .with_inline_tool_search("vendor:tool_search")
        .response_body_value(ModelRequest {
            tools: &[tool_definition("write_handoff")],
            allow_hosted_tools: false,
            ..model_request()
        })
        .expect("checkpoint request");

    assert_eq!(
        (
            body["tools"].as_array().expect("tools").len(),
            body["tools"][0]["name"].as_str(),
        ),
        (1, Some("write_handoff")),
    );
}

#[test]
fn inline_search_materializes_only_deferred_tools_it_calls() {
    let deferred = [
        tool_definition("notebook_post"),
        tool_definition("scratchpad_write"),
    ];
    let output = OpenAi::new("test-key", "https://vendor.example/v1", "test-model")
        .expect("provider")
        .with_inline_tool_search("vendor:tool_search")
        .decode_response(
            serde_json::json!({
                "output": [
                    {
                        "type": "vendor:tool_search",
                        "status": "completed",
                        "query": "notebook"
                    },
                    {
                        "type": "function_call",
                        "call_id": "call-1",
                        "name": "notebook_post",
                        "arguments": "{\"message\":\"hello\"}"
                    }
                ]
            }),
            &deferred,
        )
        .expect("response");

    assert_eq!(
        output.materialized_tools(),
        &std::collections::BTreeSet::from(["notebook_post".to_string()])
    );
}

#[test]
fn responses_decode_preserves_reasoning_content_for_replay() {
    let decoded = decode_response(serde_json::json!({
        "output": [{
            "type": "reasoning",
            "content": [{"type": "reasoning_text", "text": "Plan."}]
        }]
    }))
    .expect("decode response");

    assert_eq!(decoded.output()[0][REPLAY_REASONING_FIELD], "Plan.");
}

#[test]
fn stream_completion_preserves_final_annotations_and_completion_only_items() {
    let streamed = serde_json::json!({
        "id": "message-1",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "Source.", "annotations": []}]
    });
    let annotation = serde_json::json!({
        "type": "url_citation",
        "url": "https://example.com/source",
        "title": "Source",
        "start_index": 0,
        "end_index": 6
    });
    let mut completed = streamed.clone();
    completed["content"][0]["annotations"] = serde_json::json!([annotation]);
    let response = serde_json::json!({
        "id": "response-1",
        "output": [completed, {
            "id": "search-1",
            "type": "web_search_call",
            "status": "completed",
            "action": {"type": "search", "query": "source"}
        }]
    });

    let attached = attach_stream_output(response.clone(), BTreeMap::from([(0, streamed)]));

    assert_eq!(attached, response);
    let decoded = decode_response(attached).expect("completed output");
    assert_eq!(
        serde_json::to_value(&decoded.content()[0].annotations).expect("annotations"),
        serde_json::json!([annotation])
    );
}

#[test]
fn stream_completion_falls_back_only_for_missing_or_empty_output() {
    let item = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "Done."}]
    });
    let streamed = BTreeMap::from([(0, item.clone())]);
    for response in [serde_json::json!({}), serde_json::json!({"output": []})] {
        let attached = attach_stream_output(response, streamed.clone());
        assert_eq!(attached["output"], serde_json::json!([item]));
    }
    for response in [
        Value::Null,
        serde_json::json!([]),
        serde_json::json!({"output": null}),
        serde_json::json!({"output": {}}),
        serde_json::json!({"output": "invalid"}),
    ] {
        let attached = attach_stream_output(response.clone(), streamed.clone());
        assert_eq!(attached, response);
        assert!(decode_response(attached).is_err());
    }
}

#[test]
fn responses_decode_preserves_text_part_boundaries_and_annotations() {
    let decoded = decode_response(serde_json::json!({
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [
                {
                    "type": "output_text",
                    "text": "Source one.",
                    "annotations": [
                        {
                            "type": "url_citation",
                            "url": "https://example.com",
                            "title": "Example",
                            "content": "Relevant excerpt.",
                            "start_index": 0,
                            "end_index": 10
                        },
                        {
                            "type": "file_citation",
                            "file_id": "file-1",
                            "filename": "notes.txt",
                            "index": 2
                        }
                    ]
                },
                {"type": "refusal", "refusal": "unused boundary"},
                {
                    "type": "output_text",
                    "text": "Source two.",
                    "annotations": [
                        {
                            "type": "container_file_citation",
                            "container_id": "container-1",
                            "file_id": "file-2",
                            "filename": "report.pdf",
                            "start_index": 0,
                            "end_index": 10
                        },
                        {
                            "type": "file_path",
                            "file_id": "file-3",
                            "index": 4
                        }
                    ]
                }
            ]
        }]
    }))
    .expect("decode response");

    assert_eq!(
        serde_json::to_value(decoded.content()).expect("serialize normalized content"),
        serde_json::json!([
            {
                "output_index": 0,
                "part_index": 0,
                "phase": "final_answer",
                "text": "Source one.",
                "annotations": [
                    {
                        "type": "url_citation",
                        "url": "https://example.com",
                        "title": "Example",
                        "content": "Relevant excerpt.",
                        "start_index": 0,
                        "end_index": 10
                    },
                    {
                        "type": "file_citation",
                        "file_id": "file-1",
                        "filename": "notes.txt",
                        "index": 2
                    }
                ]
            },
            {
                "output_index": 0,
                "part_index": 2,
                "phase": "final_answer",
                "text": "Source two.",
                "annotations": [
                    {
                        "type": "container_file_citation",
                        "container_id": "container-1",
                        "file_id": "file-2",
                        "filename": "report.pdf",
                        "start_index": 0,
                        "end_index": 10
                    },
                    {"type": "file_path", "file_id": "file-3", "index": 4}
                ]
            }
        ])
    );
}

#[test]
fn responses_decode_normalizes_tool_calls_usage_and_errors() {
    let decoded = decode_response(serde_json::json!({
        "output": [
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Checking."}]
            },
            {
                "type": "function_call",
                "call_id": "call-1",
                "name": "read",
                "arguments": "{\"path\":\"README.md\"}"
            }
        ],
        "usage": {
            "input_tokens": 10,
            "input_tokens_details": {"cached_tokens": 4},
            "output_tokens": 3,
            "output_tokens_details": {"reasoning_tokens": 1},
            "total_tokens": 13
        }
    }))
    .expect("decode response");

    assert_eq!(decoded.text(), "Checking.");
    assert_eq!(decoded.tool_calls()[0].arguments["path"], "README.md");
    assert_eq!(decoded.usage().cached_input_tokens, 4);
    assert_eq!(
        response_error(&serde_json::json!({"error": {"message": "bad request"}})),
        "bad request"
    );
}

#[tokio::test]
async fn responses_emits_complete_tool_calls_in_output_order() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Box::pin(async { Ok(()) })
    });
    let mut output = BTreeMap::new();
    let mut next_output_index = 0;

    output.insert(
        1,
        serde_json::json!({
            "type": "function_call",
            "call_id": "call-1",
            "name": "read_file",
            "arguments": "{\"path\":\"README.md\"}"
        }),
    );
    emit_ready_tool_calls(&output, None, &mut next_output_index, &events)
        .await
        .expect("incomplete prefix is held");
    assert!(seen.lock().expect("events lock").is_empty());

    output.insert(
        0,
        serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": []
        }),
    );
    emit_ready_tool_calls(&output, None, &mut next_output_index, &events)
        .await
        .expect("complete prefix emits");

    assert_eq!(
        *seen.lock().expect("events lock"),
        vec![ModelEvent::ToolCallReady(crate::protocol::ToolCall {
            call_id: "call-1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "README.md"}),
        })]
    );
}

#[tokio::test]
async fn responses_emits_reasoning_text_deltas() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Box::pin(async { Ok(()) })
    });
    let mut previous_part = None;

    assert!(
        emit_reasoning_event(
            &serde_json::json!({
                "type": "response.reasoning_text.delta",
                "delta": "Plan."
            }),
            &mut previous_part,
            &events,
        )
        .await
        .expect("reasoning event")
    );
    assert_eq!(
        *seen.lock().expect("events lock"),
        vec![ModelEvent::ReasoningDelta("Plan.".into())]
    );
}

#[tokio::test]
async fn responses_emits_reasoning_summary_deltas() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Box::pin(async { Ok(()) })
    });
    let mut previous_part = None;

    assert!(
        emit_reasoning_event(
            &serde_json::json!({
                "type": "response.reasoning_summary_text.delta",
                "output_index": 0,
                "summary_index": 0,
                "delta": "**Checking the request**"
            }),
            &mut previous_part,
            &events,
        )
        .await
        .expect("reasoning summary event")
    );
    assert_eq!(
        *seen.lock().expect("events lock"),
        vec![ModelEvent::ReasoningDelta(
            "**Checking the request**".into()
        )]
    );
}

#[tokio::test]
async fn responses_preserves_reasoning_summary_part_boundaries() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Box::pin(async { Ok(()) })
    });
    let mut previous_part = None;

    for (summary_index, delta) in [
        (0, "**Planning file creation"),
        (0, " and editing methods**"),
        (1, "**Implementing file generation**"),
    ] {
        emit_reasoning_event(
            &serde_json::json!({
                "type": "response.reasoning_summary_text.delta",
                "output_index": 0,
                "summary_index": summary_index,
                "delta": delta,
            }),
            &mut previous_part,
            &events,
        )
        .await
        .expect("reasoning summary event");
    }

    assert_eq!(
        *seen.lock().expect("events lock"),
        vec![
            ModelEvent::ReasoningDelta("**Planning file creation".into()),
            ModelEvent::ReasoningDelta(" and editing methods**".into()),
            ModelEvent::ReasoningDelta("\n**Implementing file generation**".into()),
        ]
    );
}

#[tokio::test]
async fn responses_emits_commentary_text_deltas() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Box::pin(async { Ok(()) })
    });
    let mut commentary = BTreeSet::new();

    emit_text_event(
        &serde_json::json!({
            "type": "response.output_item.added",
            "item": {
                "id": "message-1",
                "type": "message",
                "phase": "commentary"
            }
        }),
        &mut commentary,
        &events,
    )
    .await
    .expect("commentary item");
    emit_text_event(
        &serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "message-1",
            "delta": "Checking."
        }),
        &mut commentary,
        &events,
    )
    .await
    .expect("commentary delta");

    assert_eq!(
        *seen.lock().expect("events lock"),
        vec![ModelEvent::CommentaryDelta("Checking.".into())]
    );
}

#[tokio::test]
async fn inline_search_web_search_uses_native_search_events() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Box::pin(async { Ok(()) })
    });
    let mut searches = BTreeSet::new();

    assert!(
        emit_web_event(
            &serde_json::json!({
                "type": "response.output_item.done",
                "item": {
                    "id": "search-1",
                    "type": "vendor:web_search",
                    "status": "completed",
                    "action": {
                        "type": "search",
                        "query": "möbius framework"
                    }
                }
            }),
            &mut searches,
            &events,
            "vendor:web_search",
        )
        .await
        .expect("configured web search event")
    );
    assert_eq!(
        *seen.lock().expect("events lock"),
        vec![
            ModelEvent::WebSearchStarted {
                call_id: "search-1".into()
            },
            ModelEvent::WebSearchCompleted {
                call_id: "search-1".into(),
                action: WebSearchAction::Search {
                    queries: vec!["möbius framework".into()]
                }
            }
        ]
    );
}

#[test]
fn responses_web_search_preserves_every_query() {
    let action = decode_web_action(&serde_json::json!({
        "action": {
            "type": "search",
            "queries": ["möbius framework", "möbius gateway"]
        }
    }));

    assert_eq!(
        action,
        WebSearchAction::Search {
            queries: vec!["möbius framework".into(), "möbius gateway".into()]
        }
    );
}

#[test]
fn responses_web_search_accepts_a_singular_query() {
    let action = decode_web_action(&serde_json::json!({
        "action": {
            "type": "search",
            "query": "möbius framework"
        }
    }));

    assert_eq!(
        action,
        WebSearchAction::Search {
            queries: vec!["möbius framework".into()]
        }
    );
}

#[test]
fn responses_web_search_without_queries_is_other() {
    let action = decode_web_action(&serde_json::json!({
        "action": {
            "type": "search"
        }
    }));

    assert_eq!(action, WebSearchAction::Other);
}

struct HttpRefreshingAuthorization {
    token: std::sync::Mutex<String>,
    refreshes: std::sync::atomic::AtomicUsize,
}

impl HttpRefreshingAuthorization {
    fn new() -> Self {
        Self {
            token: std::sync::Mutex::new("rejected-token".into()),
            refreshes: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn resolved(&self) -> ResolvedAuthorization<'static> {
        ResolvedAuthorization {
            token: self.token.lock().expect("token lock").clone().into(),
            headers: Vec::new(),
        }
    }
}

impl OpenAiAuthorization for HttpRefreshingAuthorization {
    fn authorize_http<'a>(
        &'a self,
        _streaming: bool,
        _session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ResolvedAuthorization<'a>>> {
        let authorization = self.resolved();
        Box::pin(async move { Ok(authorization) })
    }

    fn authorize_websocket<'a>(
        &'a self,
        _session_id: &'a str,
    ) -> BoxFuture<'a, Result<ResolvedAuthorization<'a>>> {
        let authorization = self.resolved();
        Box::pin(async move { Ok(authorization) })
    }

    fn recover_unauthorized<'a>(&'a self, rejected_token: &'a str) -> BoxFuture<'a, Result<bool>> {
        let mut token = self.token.lock().expect("token lock");
        if token.as_str() == rejected_token {
            *token = "fresh-token".into();
            self.refreshes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Box::pin(async { Ok(true) })
    }
}

#[tokio::test]
async fn http_stream_emits_citation_search_before_completion_and_eof() {
    use tokio::io::AsyncReadExt as _;
    use tokio::io::AsyncWriteExt as _;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let (finish, finished) = tokio::sync::oneshot::channel();
    let (close, closed) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("HTTP connection");
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut chunk = [0; 1_024];
            let count = stream.read(&mut chunk).await.expect("HTTP request");
            assert_ne!(count, 0, "request ended before its headers");
            request.extend_from_slice(&chunk[..count]);
        }
        let delta = format!(
            "data: {}\n\n",
            serde_json::json!({
                "type": "response.output_text.delta",
                "item_id": "message-1",
                "delta": "D"
            })
        );
        let completion = [
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "message-1",
                    "type": "message",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": "Done.",
                        "annotations": [{
                            "type": "url_citation",
                            "url": "https://example.com",
                            "title": "Example",
                            "start_index": 0,
                            "end_index": 4
                        }]
                    }]
                }
            }),
            serde_json::json!({
                "type": "response.completed",
                "response": {"id": "response-1", "output": []}
            }),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            delta.len() + completion.len() + 1_024
        );
        stream
            .write_all(format!("{headers}{delta}").as_bytes())
            .await
            .expect("HTTP delta");
        stream.flush().await.expect("flush HTTP delta");
        let _ = finished.await;
        stream
            .write_all(completion.as_bytes())
            .await
            .expect("HTTP completion");
        stream.flush().await.expect("flush HTTP completion");
        let _ = closed.await;
    });
    let provider =
        OpenAi::new("test-key", format!("http://{address}"), "test-model").expect("provider");
    let (event_sender, mut events) = tokio::sync::mpsc::unbounded_channel();
    let event_sink: ModelEventSink = Arc::new(move |event| {
        let _ = event_sender.send(event);
        Box::pin(async { Ok(()) })
    });
    let response = tokio::spawn(async move {
        provider
            .send_response(model_request(), event_sink, None)
            .await
    });

    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("delta was buffered until completion"),
        Some(ModelEvent::TextDelta("D".into()))
    );
    let _ = finish.send(());
    let output = tokio::time::timeout(std::time::Duration::from_secs(1), response)
        .await
        .expect("completed response waited for EOF")
        .expect("response task")
        .expect("completed response");
    let _ = close.send(());
    server.await.expect("HTTP server");

    assert_eq!(output.text(), "Done.");
    assert_eq!(
        events.recv().await,
        Some(ModelEvent::WebSearchStarted {
            call_id: "citations".into()
        })
    );
    assert_eq!(
        events.recv().await,
        Some(ModelEvent::WebSearchCompleted {
            call_id: "citations".into(),
            action: WebSearchAction::Other
        })
    );
}

#[tokio::test]
async fn http_unauthorized_refreshes_and_retries_once() {
    use tokio::io::AsyncReadExt as _;
    use tokio::io::AsyncWriteExt as _;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        let completed = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"response-test\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}],\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"total_tokens\":0}}}\n\n";
        for response in [
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_owned(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{completed}",
                completed.len()
            ),
        ] {
            let (mut stream, _) = listener.accept().await.expect("HTTP connection");
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut chunk = [0; 1_024];
                let count = stream.read(&mut chunk).await.expect("HTTP request");
                assert_ne!(count, 0, "request ended before its headers");
                request.extend_from_slice(&chunk[..count]);
            }
            let header_end = request
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
                .expect("headers")
                + 4;
            let headers = std::str::from_utf8(&request[..header_end]).expect("headers UTF-8");
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .map(str::to_owned)
                })
                .expect("content length")
                .parse()
                .expect("body length");
            while request.len() < header_end + length {
                let mut chunk = [0; 1024];
                let count = stream.read(&mut chunk).await.expect("request body");
                assert_ne!(count, 0);
                request.extend_from_slice(&chunk[..count]);
            }
            requests.push(String::from_utf8(request).expect("request UTF-8"));
            stream
                .write_all(response.as_bytes())
                .await
                .expect("HTTP response");
        }
        requests
    });

    let auth = Arc::new(HttpRefreshingAuthorization::new());
    let mut provider = OpenAi::with_authorization(
        auth.clone(),
        format!("http://{address}"),
        "test-model",
        reqwest::Client::new(),
        crate::backend::model::ModelTransportSettings::default(),
    )
    .expect("provider");
    let expected =
        serde_json::to_vec(&provider.response_body(model_request()).expect("body")).expect("JSON");
    provider.transport.max_request_bytes = expected.len();
    provider
        .respond_prepared(
            model_request(),
            Arc::new(|_| Box::pin(async { Ok(()) })),
            super::super::MediaPreparation {
                files: None,
                limits: super::super::ImageInputLimits::default(),
            },
        )
        .await
        .expect("exact-budget request should refresh and recover");

    let requests = server.await.expect("HTTP server");
    for request in &requests {
        let (headers, body) = request.split_once("\r\n\r\n").expect("HTTP body");
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        assert_eq!(body.as_bytes(), expected);
    }
    provider.transport.max_request_bytes = expected.len() - 1;
    let error = provider
        .respond(model_request(), Arc::new(|_| Box::pin(async { Ok(()) })))
        .await
        .expect_err("direct requests also reject one byte over budget before sending");
    assert!(error.to_string().contains("request budget"));
    assert!(requests[0].contains("Bearer rejected-token"));
    assert!(requests[1].contains("Bearer fresh-token"));
    assert_eq!(auth.refreshes.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[tokio::test]
async fn http_transport_failure_does_not_replay_accepted_post() {
    use tokio::io::AsyncReadExt as _;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("HTTP connection");
        let mut request = Vec::new();
        let header_end = loop {
            if let Some(position) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break position;
            }
            let mut chunk = [0; 1_024];
            let count = stream.read(&mut chunk).await.expect("HTTP request");
            assert_ne!(count, 0, "request ended before its headers");
            request.extend_from_slice(&chunk[..count]);
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .expect("content length header");
        while request.len() < header_end + 4 + content_length {
            let mut chunk = [0; 1_024];
            let count = stream.read(&mut chunk).await.expect("HTTP request body");
            assert_ne!(count, 0, "request ended before its body");
            request.extend_from_slice(&chunk[..count]);
        }
        drop(stream);
        let replayed =
            tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept())
                .await
                .is_ok();
        (request, replayed)
    });
    let provider = OpenAi::with_client(
        Some("test-key".into()),
        format!("http://{address}"),
        "test-model",
        reqwest::Client::new(),
        crate::backend::model::ModelTransportSettings::default(),
    )
    .expect("provider");

    let result = provider
        .send_authorized("responses", &serde_json::json!({}), false, None)
        .await;

    assert!(result.is_err());
    let (request, replayed) = server.await.expect("HTTP server");
    assert!(!replayed);
    assert!(request.ends_with(b"{}"));
    assert!(String::from_utf8_lossy(&request).contains("Bearer test-key"));
}

#[tokio::test]
async fn credentialless_http_omits_authorization() {
    let (address, server) = capture_http_request().await;
    let provider = OpenAi::with_client(
        None,
        format!("http://{address}"),
        "test-model",
        reqwest::Client::new(),
        crate::backend::model::ModelTransportSettings::default(),
    )
    .expect("credentialless provider");

    provider
        .send_authorized("responses", &serde_json::json!({}), false, None)
        .await
        .expect("credentialless request");

    let request = server.await.expect("HTTP server");
    assert!(!request.to_ascii_lowercase().contains("authorization:"));
}

#[tokio::test]
async fn compatible_images_use_json_for_generation_and_multipart_for_public_edits() {
    use crate::backend::model::provider::{
        HostedWebSearch, ProviderBuildConfig, ProviderCredential,
    };
    use base64::Engine as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let server = tokio::spawn(async move {
        let response = serde_json::json!({
            "data": [{"b64_json": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/sZkAAAAASUVORK5CYII="}],
            "usage": {"input_tokens": 2, "output_tokens": 3, "total_tokens": 5}
        });
        let response = response.to_string();
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("HTTP connection");
            let mut request = Vec::new();
            let body_start = loop {
                if let Some(start) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break start + 4;
                }
                let mut chunk = [0_u8; 1024];
                let read = stream.read(&mut chunk).await.expect("request headers");
                assert_ne!(read, 0);
                request.extend_from_slice(&chunk[..read]);
            };
            let headers = String::from_utf8_lossy(&request[..body_start]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().expect("content length"))
                    })
                })
                .expect("content length header");
            while request.len() < body_start + length {
                let mut chunk = [0_u8; 1024];
                let read = stream.read(&mut chunk).await.expect("request body");
                assert_ne!(read, 0);
                request.extend_from_slice(&chunk[..read]);
            }
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                        response.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("response");
            requests.push(request);
        }
        requests
    });
    let provider = crate::backend::model::provider::provider("responses")
        .expect("Responses registration")
        .build(ProviderBuildConfig {
            capability: None,
            tool_discovery: None,
            credential: ProviderCredential::ApiKey("test-key".into()),
            base_url: Some(format!("http://{address}/api/native/v1/")),
            model: "chat-model".into(),
            reasoning_effort: None,
            service_tier: None,
            web_search: HostedWebSearch::Off,
            http: reqwest::Client::new(),
            transport: Default::default(),
        })
        .expect("provider");
    let generated = provider
        .generate_image(ImageGenerationRequest {
            model: "gpt-image-2.5-sunburst",
            prompt: "a red fox",
            image_aspect: crate::backend::model::ImageAspect::Square,
            quality: None,
            references: &[],
        })
        .await
        .expect("image");
    assert_eq!(generated.media_type, "image/png");
    assert_eq!(generated.usage.expect("usage").total_tokens, 5);
    let source = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/sZkAAAAASUVORK5CYII=")
        .expect("source image");
    let references = [ImageGenerationReference {
        media_type: "image/png",
        bytes: &source,
    }];
    provider
        .generate_image(ImageGenerationRequest {
            model: "gpt-image-2.5-flare",
            prompt: "make the fox blue",
            image_aspect: crate::backend::model::ImageAspect::Landscape,
            quality: None,
            references: &references,
        })
        .await
        .expect("edited image");
    let requests = server.await.expect("server");
    let generation = String::from_utf8_lossy(&requests[0]);
    assert!(generation.starts_with("POST /api/native/v1/images/generations HTTP/1.1"));
    assert!(generation.contains("Bearer test-key"));
    assert!(generation.contains("\"model\":\"gpt-image-2.5-sunburst\""));
    assert!(generation.contains("\"size\":\"1024x1024\""));
    let edit = String::from_utf8_lossy(&requests[1]);
    assert!(edit.starts_with("POST /api/native/v1/images/edits HTTP/1.1"));
    assert!(edit.contains("Bearer test-key"));
    assert!(edit.contains("multipart/form-data; boundary="));
    assert!(edit.contains("name=\"image[]\"; filename=\"reference-0.png\""));
    assert!(edit.contains("name=\"model\""));
    assert!(edit.contains("gpt-image-2.5-flare"));
    assert!(edit.contains("name=\"prompt\""));
    assert!(edit.contains("make the fox blue"));
    assert!(edit.contains("name=\"size\""));
    assert!(edit.contains("1536x1024"));
    assert!(
        requests[1]
            .windows(source.len())
            .any(|bytes| bytes == source)
    );
}

#[tokio::test]
async fn eof_without_response_completed_is_a_retryable_interruption() {
    let (address, server) = capture_http_request().await;
    let provider =
        OpenAi::new("test-key", format!("http://{address}"), "test-model").expect("provider");
    let error = provider
        .send_response(
            model_request(),
            Arc::new(|_| Box::pin(async { Ok(()) })),
            None,
        )
        .await
        .expect_err("missing completion");
    assert!(matches!(error, Error::Provider(error) if error.is_stream_interrupted()));
    server.await.expect("HTTP server");
}

#[test]
fn completed_stream_items_move_payloads_and_validate_before_collection() {
    let mut event = serde_json::json!({
        "type": "response.output_item.done",
        "item": {"text": "large payload".repeat(1024)}
    });
    let payload = event["item"]["text"].as_str().expect("payload").as_ptr();
    let mut output = BTreeMap::new();
    let index = validate_stream_output(&event, &output).expect("valid item");
    assert_eq!(index, Some(0));
    collect_stream_output(&mut event, &mut output, index);
    assert_eq!(
        output[&0]["text"]
            .as_str()
            .expect("stored payload")
            .as_ptr(),
        payload
    );
    assert!(event["item"].is_null());

    let duplicate = serde_json::json!({
        "type": "response.output_item.done", "output_index": 0, "item": {}
    });
    assert!(validate_stream_output(&duplicate, &output).is_err());
    let missing = serde_json::json!({"type": "response.output_item.done"});
    assert!(validate_stream_output(&missing, &output).is_err());
    for index in 1..MAX_STREAM_OUTPUT_ITEMS as u64 {
        output.insert(index, Value::Null);
    }
    let overflow = serde_json::json!({"type": "response.output_item.done", "item": {}});
    assert!(validate_stream_output(&overflow, &output).is_err());
}
