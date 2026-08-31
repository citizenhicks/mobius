use super::*;
use crate::backend::model::PromptCacheIdentity;
use crate::backend::model::REPLAY_REASONING_FIELD;
use crate::backend::model::transport::capture_http_request;

fn model_request() -> ModelRequest<'static> {
    ModelRequest {
        session_id: "test-session",
        prompt_cache: Some(PromptCacheIdentity {
            key: "hashed-cache-key",
            context_epoch: 3,
        }),
        instructions: "Test instructions",
        input: &[],
        catalog_revision: "catalog-1",
        tools: &[],
        deferred_tools: &[],
        allow_hosted_tools: false,
        allow_continuation: false,
    }
}

fn tool_definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: format!("Use {name}"),
        parameters: serde_json::json!({"type": "object"}),
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
fn only_the_first_party_endpoint_reports_known_gpt_56_pricing() {
    let usage = TokenUsage {
        input_tokens: 100_000,
        total_tokens: 100_000,
        ..TokenUsage::default()
    };
    let official = OpenAi::new("test-key", "https://api.openai.com/v1", "gpt-5.6-luna")
        .expect("official provider");
    let compatible = OpenAi::new("test-key", "https://example.com/v1", "gpt-5.6-luna")
        .expect("compatible provider");

    assert_eq!(
        official
            .pricing()
            .and_then(|pricing| pricing.estimate_microusd(&usage)),
        Some(20_000)
    );
    assert_eq!(compatible.pricing(), None);
}

#[test]
fn endpoint_url_does_not_infer_reasoning_summary_support() {
    let provider = OpenAi::new("test-key", "https://api.openai.com/v1/", "test-model")
        .expect("provider")
        .with_reasoning_effort("medium")
        .expect("reasoning effort");

    assert_eq!(
        provider
            .response_body(model_request())
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
            .response_body(model_request())
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
            .response_body(model_request())
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
            .response_body(model_request())
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
        wire_input_with_cache(&input, true, false, "catalog-1", &[]).expect("wire input"),
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
        prompt_cache: Some(PromptCacheIdentity {
            key: "opaque-cache-key",
            context_epoch: 4,
        }),
        instructions: "Instructions",
        input: &input,
        catalog_revision: "catalog-1",
        tools: &[],
        deferred_tools: &[],
        allow_hosted_tools: false,
        allow_continuation: true,
    };

    let compatible = OpenAi::new("test-key", "https://example.com/v1", "test-model")
        .expect("compatible provider");
    let compatible_body = compatible.response_body(request).expect("compatible body");
    assert_eq!(compatible_body["prompt_cache_key"], "opaque-cache-key");
    assert!(compatible_body.get("prompt_cache_options").is_none());
    assert_eq!(
        compatible_body["input"][0]["content"][0],
        serde_json::json!({"type": "input_text", "text": "stable prefix"})
    );

    let request = ModelRequest {
        session_id: "local-session",
        prompt_cache: Some(PromptCacheIdentity {
            key: "opaque-cache-key",
            context_epoch: 4,
        }),
        instructions: "Instructions",
        input: &input,
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
        .response_body(request)
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
fn compact_decode_normalizes_provider_wire_items() {
    let decoded = decode_compact_response(serde_json::json!({
        "output": [
            {
                "type": "message",
                "id": "message-1",
                "role": "user",
                "status": "completed",
                "content": [{"type": "input_text", "text": "inspect"}]
            },
            {
                "type": "reasoning",
                "format": "openai-responses-v1",
                "status": "completed",
                "encrypted_content": "reasoning"
            },
            {
                "type": "compaction_summary",
                "encrypted_content": "opaque"
            }
        ]
    }))
    .expect("decode compact response");

    assert_eq!(
        decoded.output(),
        &[
            serde_json::json!({
                "type": "message",
                "id": "message-1",
                "role": "user",
                "content": [{"type": "input_text", "text": "inspect"}]
            }),
            serde_json::json!({
                "type": "reasoning",
                "encrypted_content": "reasoning"
            }),
            serde_json::json!({
                "type": "compaction",
                "encrypted_content": "opaque"
            })
        ]
    );
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

    let wired = wire_input_with_cache(&input, true, false, "catalog-1", &[]).expect("wire image");
    assert_eq!(
        wired[0]["content"][1],
        serde_json::json!({
            "type": "input_image",
            "image_url": "data:image/png;base64,aGVsbG8="
        })
    );
    assert!(
        wire_input_with_cache(&input, false, false, "catalog-1", &[])
            .expect_err("disabled image input")
            .to_string()
            .contains("does not support image attachments")
    );
}

#[test]
fn hosted_tools_can_be_disabled_per_request() {
    let hosted = [serde_json::json!({"type": "web_search"})];

    assert!(wire_tools(&[], &hosted, false).is_empty());
    assert_eq!(wire_tools(&[], &hosted, true), hosted);
}

#[test]
fn rebuild_filters_tool_load_control_items() {
    let loaded = [tool_definition("swarm_post")];
    let input = [ToolLoad {
        catalog_revision: "catalog-1".into(),
        tools: vec!["swarm_post".into()],
    }
    .into_input()];
    let body = OpenAi::new("test-key", "https://example.com/v1", "test-model")
        .expect("provider")
        .response_body(ModelRequest {
            input: &input,
            tools: &loaded,
            ..model_request()
        })
        .expect("response body");

    assert_eq!(
        (&body["input"], &body["tools"][0]["name"]),
        (&serde_json::json!([]), &serde_json::json!("swarm_post"))
    );

    let native = OpenAi::new("test-key", "https://api.openai.com/v1", "test-model")
        .expect("provider")
        .with_tool_discovery(ToolDiscoveryMode::Native)
        .response_body(ModelRequest {
            input: &input,
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
    let deferred = [tool_definition("swarm_post")];
    let input = [ToolLoad {
        catalog_revision: "catalog-0".into(),
        tools: vec!["swarm_post".into()],
    }
    .into_input()];

    let body = OpenAi::new("test-key", "https://api.openai.com/v1", "test-model")
        .expect("provider")
        .with_tool_discovery(ToolDiscoveryMode::Native)
        .response_body(ModelRequest {
            input: &input,
            deferred_tools: &deferred,
            ..model_request()
        })
        .expect("response body");

    assert_eq!(body["input"], serde_json::json!([]));
}

#[test]
fn openrouter_defers_optional_tools_and_uses_hosted_search() {
    let direct = [
        tool_definition(TOOLS_SEARCH_NAME),
        tool_definition("read_file"),
    ];
    let deferred = [tool_definition("swarm_post")];
    let input = [ToolLoad {
        catalog_revision: "catalog-1".into(),
        tools: vec!["swarm_post".into()],
    }
    .into_input()];
    let body = OpenAi::new("test-key", "https://openrouter.ai/api/v1", "test-model")
        .expect("provider")
        .with_openrouter_tool_search()
        .response_body(ModelRequest {
            input: &input,
            tools: &direct,
            deferred_tools: &deferred,
            ..model_request()
        })
        .expect("response body");

    assert_eq!(body["input"], serde_json::json!([]));
    assert_eq!(
        body["tools"],
        serde_json::json!([
            {"type": "openrouter:tool_search"},
            {
                "type": "function",
                "name": "read_file",
                "description": "Use read_file",
                "parameters": {"type": "object"},
                "strict": false
            },
            {
                "type": "function",
                "name": "swarm_post",
                "description": "Use swarm_post",
                "parameters": {"type": "object"},
                "strict": false,
                "defer_loading": true
            }
        ])
    );
}

#[test]
fn openrouter_materializes_only_deferred_tools_it_calls() {
    let deferred = [
        tool_definition("swarm_post"),
        tool_definition("scratchpad_write"),
    ];
    let output = OpenAi::new("test-key", "https://openrouter.ai/api/v1", "test-model")
        .expect("provider")
        .with_openrouter_tool_search()
        .decode_response(
            serde_json::json!({
                "output": [
                    {
                        "type": "openrouter:tool_search",
                        "status": "completed",
                        "query": "swarm"
                    },
                    {
                        "type": "function_call",
                        "call_id": "call-1",
                        "name": "swarm_post",
                        "arguments": "{\"message\":\"hello\"}"
                    }
                ]
            }),
            &deferred,
        )
        .expect("response");

    assert_eq!(
        output.materialized_tools(),
        &std::collections::BTreeSet::from(["swarm_post".to_string()])
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

#[test]
fn responses_emits_reasoning_text_deltas() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Ok(())
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
        .expect("reasoning event")
    );
    assert_eq!(
        *seen.lock().expect("events lock"),
        vec![ModelEvent::ReasoningDelta("Plan.".into())]
    );
}

#[test]
fn responses_emits_reasoning_summary_deltas() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Ok(())
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
        .expect("reasoning summary event")
    );
    assert_eq!(
        *seen.lock().expect("events lock"),
        vec![ModelEvent::ReasoningDelta(
            "**Checking the request**".into()
        )]
    );
}

#[test]
fn responses_preserves_reasoning_summary_part_boundaries() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Ok(())
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

#[test]
fn responses_emits_commentary_text_deltas() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Ok(())
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
    .expect("commentary delta");

    assert_eq!(
        *seen.lock().expect("events lock"),
        vec![ModelEvent::CommentaryDelta("Checking.".into())]
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

#[test]
fn compaction_shape_matches_the_responses_contract() {
    let provider = OpenAi::new("test-key", "https://api.openai.com/v1", "test-model")
        .expect("provider")
        .with_reasoning_effort("medium")
        .expect("reasoning effort")
        .with_reasoning_summary()
        .with_web_search();
    assert!(!provider.compaction_endpoint());
    let input = [serde_json::json!({
        "role": "user",
        "content": "hello",
        "_private": true
    })];
    let tools = [ToolDefinition {
        name: "read".into(),
        description: "Read a file".into(),
        parameters: serde_json::json!({"type": "object"}),
    }];
    let body = provider
        .with_compaction_endpoint()
        .compact_body(CompactRequest {
            session_id: "test-session",
            prompt_cache: Some(PromptCacheIdentity {
                key: "hashed-cache-key",
                context_epoch: 3,
            }),
            instructions: "Compact the conversation",
            input: &input,
            catalog_revision: "catalog-1",
            tools: &tools,
            deferred_tools: &[],
        })
        .expect("compact body");

    assert_eq!(
        body,
        serde_json::json!({
            "model": "test-model",
            "instructions": "Compact the conversation",
            "input": [{"role": "user", "content": "hello"}],
            "tools": [
                {
                    "type": "function",
                    "name": "read",
                    "description": "Read a file",
                    "parameters": {"type": "object"},
                    "strict": false
                },
                {"type": "web_search"}
            ],
            "parallel_tool_calls": true,
            "prompt_cache_key": "hashed-cache-key",
            "reasoning": {"effort": "medium", "summary": "auto"}
        })
    );
}

#[test]
fn native_compaction_ignores_tool_loads_from_an_old_catalog() {
    let deferred = [tool_definition("swarm_post")];
    let input = [ToolLoad {
        catalog_revision: "catalog-0".into(),
        tools: vec!["swarm_post".into()],
    }
    .into_input()];
    let body = OpenAi::new("test-key", "https://api.openai.com/v1", "test-model")
        .expect("provider")
        .with_tool_discovery(ToolDiscoveryMode::Native)
        .with_compaction_endpoint()
        .compact_body(CompactRequest {
            session_id: "test-session",
            prompt_cache: None,
            instructions: "Compact the conversation",
            input: &input,
            catalog_revision: "catalog-1",
            tools: &[],
            deferred_tools: &deferred,
        })
        .expect("compaction body");

    assert_eq!(body["input"], serde_json::json!([]));
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

    fn resolved(&self) -> ResolvedAuthorization {
        ResolvedAuthorization {
            token: self.token.lock().expect("token lock").clone(),
            headers: Vec::new(),
        }
    }
}

impl OpenAiAuthorization for HttpRefreshingAuthorization {
    fn authorize_http<'a>(
        &'a self,
        _streaming: bool,
        _session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ResolvedAuthorization>> {
        let authorization = self.resolved();
        Box::pin(async move { Ok(authorization) })
    }

    fn authorize_websocket<'a>(
        &'a self,
        _session_id: &'a str,
    ) -> BoxFuture<'a, Result<ResolvedAuthorization>> {
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
        Ok(())
    });
    let response =
        tokio::spawn(async move { provider.send_response(model_request(), event_sink).await });

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
        for response in [
            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".as_slice(),
        ] {
            let (mut stream, _) = listener.accept().await.expect("HTTP connection");
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut chunk = [0; 1_024];
                let count = stream.read(&mut chunk).await.expect("HTTP request");
                assert_ne!(count, 0, "request ended before its headers");
                request.extend_from_slice(&chunk[..count]);
            }
            requests.push(String::from_utf8_lossy(&request).into_owned());
            stream.write_all(response).await.expect("HTTP response");
        }
        requests
    });

    let auth = Arc::new(HttpRefreshingAuthorization::new());
    let provider = OpenAi::with_authorization(
        auth.clone(),
        format!("http://{address}"),
        "test-model",
        reqwest::Client::new(),
    )
    .expect("provider");
    let response = provider
        .send_authorized(
            "responses",
            &serde_json::json!({}),
            true,
            Some("test-session"),
        )
        .await
        .expect("request should recover");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let requests = server.await.expect("HTTP server");
    assert!(requests[0].contains("Bearer rejected-token"));
    assert!(requests[1].contains("Bearer fresh-token"));
    assert_eq!(auth.refreshes.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[tokio::test]
async fn http_transport_failure_retries_the_same_authorized_request() {
    use tokio::io::AsyncReadExt as _;
    use tokio::io::AsyncWriteExt as _;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for attempt in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("HTTP connection");
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut chunk = [0; 1_024];
                let count = stream.read(&mut chunk).await.expect("HTTP request");
                assert_ne!(count, 0, "request ended before its headers");
                request.extend_from_slice(&chunk[..count]);
            }
            requests.push(String::from_utf8_lossy(&request).into_owned());
            if attempt == 1 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await
                    .expect("HTTP response");
            }
        }
        requests
    });
    let provider = OpenAi::with_client(
        Some("test-key".into()),
        format!("http://{address}"),
        "test-model",
        reqwest::Client::new(),
    )
    .expect("provider");

    let response = provider
        .send_authorized("responses", &serde_json::json!({}), false, None)
        .await
        .expect("request should recover");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let requests = server.await.expect("HTTP server");
    assert_eq!(requests[0], requests[1]);
    assert!(requests[0].contains("Bearer test-key"));
}

#[tokio::test]
async fn credentialless_http_omits_authorization() {
    let (address, server) = capture_http_request().await;
    let provider = OpenAi::with_client(
        None,
        format!("http://{address}"),
        "test-model",
        reqwest::Client::new(),
    )
    .expect("credentialless provider");

    provider
        .send_authorized("responses", &serde_json::json!({}), false, None)
        .await
        .expect("credentialless request");

    let request = server.await.expect("HTTP server");
    assert!(!request.to_ascii_lowercase().contains("authorization:"));
}
