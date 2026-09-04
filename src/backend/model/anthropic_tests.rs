use super::*;
use crate::backend::model::provider::ProviderCredential;
use crate::backend::model::transport::capture_http_request;
use crate::backend::model::user_message;

#[test]
fn advertised_web_search_modes_build() {
    let definition = provider();
    for web_search in definition.web_search().iter().copied() {
        definition
            .build(ProviderBuildConfig {
                credential: ProviderCredential::ApiKey("test-key".into()),
                model: definition.default_model().expect("default model").into(),
                base_url: Some(DEFAULT_BASE_URL.into()),
                reasoning_effort: None,
                web_search,
                http: reqwest::Client::new(),
            })
            .expect("advertised web search mode builds");
    }
}

#[test]
fn equivalent_default_endpoint_preserves_native_tool_discovery() {
    let model = provider()
        .build(ProviderBuildConfig {
            credential: ProviderCredential::ApiKey("test-key".into()),
            model: "claude-haiku-4-5".into(),
            base_url: Some("https://api.anthropic.com:443/v1/".into()),
            reasoning_effort: None,
            web_search: HostedWebSearch::Off,
            http: reqwest::Client::new(),
        })
        .expect("equivalent default endpoint builds");

    assert_eq!(model.tool_discovery(), ToolDiscoveryMode::Native);
}

#[tokio::test]
async fn credentialless_post_uses_custom_path_and_omits_api_key() {
    let (address, server) = capture_http_request().await;
    let provider = Anthropic::with_client(
        None,
        format!("http://{address}/proxy"),
        "test-model",
        reqwest::Client::new(),
    )
    .expect("credentialless provider");

    provider
        .post(&serde_json::json!({}))
        .await
        .expect("credentialless request");

    let request = server.await.expect("HTTP server").to_ascii_lowercase();
    assert!(request.starts_with("post /proxy/messages http/1.1\r\n"));
    assert!(!request.contains("x-api-key:"));
}

#[test]
fn anthropic_reports_provider_owned_cache_pricing() {
    let provider =
        Anthropic::new("test-key", DEFAULT_BASE_URL, "claude-haiku-4-5").expect("provider");
    let usage = TokenUsage {
        input_tokens: 1_000_000,
        cached_input_tokens: 200_000,
        cache_write_input_tokens: 300_000,
        output_tokens: 1_000_000,
        total_tokens: 2_000_000,
        ..TokenUsage::default()
    };

    assert_eq!(
        provider
            .pricing()
            .and_then(|pricing| pricing.estimate_microusd(&usage)),
        Some(5_895_000)
    );
    assert_eq!(
        provider.prompt_cache_capability(),
        PromptCacheMode::Explicit
    );
}

#[test]
fn sonnet_5_pricing_changes_at_the_standard_rate_date() {
    let usage = TokenUsage {
        input_tokens: 1_000_000,
        cached_input_tokens: 200_000,
        cache_write_input_tokens: 300_000,
        output_tokens: 1_000_000,
        total_tokens: 2_000_000,
        ..TokenUsage::default()
    };
    let before = anthropic_model_pricing_at(
        "claude-sonnet-5",
        SONNET_5_STANDARD_PRICING_START_UNIX_SECONDS - 1,
    )
    .and_then(|pricing| pricing.estimate_microusd(&usage));
    let after = anthropic_model_pricing_at(
        "claude-sonnet-5",
        SONNET_5_STANDARD_PRICING_START_UNIX_SECONDS,
    )
    .and_then(|pricing| pricing.estimate_microusd(&usage));

    assert_eq!((before, after), (Some(11_790_000), Some(17_685_000)));
}

#[test]
fn explicit_prompt_cache_breakpoint_is_sent_on_the_marked_content_block() {
    let provider =
        Anthropic::new("test-key", DEFAULT_BASE_URL, "claude-sonnet-5").expect("provider");
    let mut input = user_message("stable prefix");
    assert!(crate::backend::model::mark_prompt_cache_breakpoint(
        &mut input
    ));

    let body = provider
        .request_body("instructions", &[input], "catalog-1", &[], &[], false)
        .expect("request body");

    assert!(body.get("cache_control").is_none());
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
}

#[test]
fn hosted_search_can_be_disabled_per_request() {
    assert!(wire_tools(&[], &[], false).is_empty());
    assert_eq!(wire_tools(&[], &[], true)[0]["name"], "web_search");
}

#[test]
fn native_discovery_defers_schemas_and_replays_tool_references() {
    let provider =
        Anthropic::new("test-key", DEFAULT_BASE_URL, "claude-haiku-4-5").expect("provider");
    let direct = [discovery_tool(TOOLS_SEARCH_NAME)];
    let deferred = [discovery_tool("swarm_post")];
    let input = discovery_history();

    let body = provider
        .request_body(
            "instructions",
            &input,
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
            body["messages"][2]["content"][0]["content"].clone(),
            body.to_string().contains("tool_load"),
        ),
        (
            None,
            Some(true),
            serde_json::json!([{"type": "tool_reference", "tool_name": "swarm_post"}]),
            false,
        )
    );
}

#[test]
fn native_discovery_replays_a_standalone_compacted_tool_load() {
    let provider =
        Anthropic::new("test-key", DEFAULT_BASE_URL, "claude-haiku-4-5").expect("provider");
    let direct = [discovery_tool(TOOLS_SEARCH_NAME)];
    let deferred = [discovery_tool("swarm_post")];
    let input = [
        user_message("Continue after compaction."),
        ToolLoad {
            catalog_revision: "catalog-1".into(),
            tools: vec!["swarm_post".into()],
        }
        .into_input(),
    ];

    let body = provider
        .request_body(
            "instructions",
            &input,
            "catalog-1",
            &direct,
            &deferred,
            false,
        )
        .expect("request body");

    assert_eq!(
        (
            &body["messages"][1]["content"][0]["name"],
            &body["messages"][2]["content"][0]["content"],
        ),
        (
            &serde_json::json!(TOOLS_SEARCH_NAME),
            &serde_json::json!([{"type": "tool_reference", "tool_name": "swarm_post"}]),
        )
    );
}

#[test]
fn native_discovery_ignores_tool_loads_from_an_old_catalog() {
    let provider =
        Anthropic::new("test-key", DEFAULT_BASE_URL, "claude-haiku-4-5").expect("provider");
    let direct = [discovery_tool(TOOLS_SEARCH_NAME)];
    let deferred = [discovery_tool("swarm_post")];
    let mut input = discovery_history();
    input.last_mut().expect("tool load")["catalog_revision"] = "catalog-0".into();

    let body = provider
        .request_body(
            "instructions",
            &input,
            "catalog-1",
            &direct,
            &deferred,
            false,
        )
        .expect("request body");

    assert_eq!(
        body["messages"][2]["content"][0]["content"],
        serde_json::json!("Found swarm_post")
    );
}

#[test]
fn rebuild_discovery_omits_deferred_schemas_and_internal_markers() {
    let provider =
        Anthropic::new("test-key", DEFAULT_BASE_URL, "claude-sonnet-5").expect("provider");
    let direct = [discovery_tool(TOOLS_SEARCH_NAME)];
    let deferred = [discovery_tool("swarm_post")];
    let input = discovery_history();

    let body = provider
        .request_body(
            "instructions",
            &input,
            "catalog-1",
            &direct,
            &deferred,
            false,
        )
        .expect("request body");

    assert_eq!(
        (
            body["tools"].as_array().map(Vec::len),
            body["messages"][2]["content"][0]["content"].as_str(),
            body.to_string().contains("tool_load"),
        ),
        (Some(1), Some("Found swarm_post"), false)
    );
}

fn discovery_tool(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: "test tool".into(),
        parameters: serde_json::json!({"type": "object"}),
    }
}

fn discovery_history() -> Vec<Value> {
    vec![
        user_message("Find a collaboration tool."),
        serde_json::json!({
            "type": "function_call",
            "call_id": "search-1",
            "name": TOOLS_SEARCH_NAME,
            "arguments": "{\"query\":\"swarm\"}"
        }),
        serde_json::json!({
            "type": "function_call_output",
            "call_id": "search-1",
            "output": "Found swarm_post"
        }),
        ToolLoad {
            catalog_revision: "catalog-1".into(),
            tools: vec!["swarm_post".into()],
        }
        .into_input(),
    ]
}

#[test]
fn anthropic_web_search_normalizes_query_to_a_singleton() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Ok(())
    });
    let mut stream = StreamState::default();

    for event in [
        serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "server_tool_use",
                "id": "search-1",
                "name": "web_search",
                "input": {"query": "möbius framework"}
            }
        }),
        serde_json::json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": "search-1",
                "content": []
            }
        }),
    ] {
        stream.apply(event, &events).expect("stream event");
    }

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
fn anthropic_web_search_with_an_empty_streamed_query_is_other() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Ok(())
    });
    let mut stream = StreamState::default();

    for event in [
        serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "server_tool_use",
                "id": "search-1",
                "name": "web_search",
                "input": {}
            }
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "input_json_delta",
                "partial_json": "{\"query\":\"\"}"
            }
        }),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
        serde_json::json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": "search-1",
                "content": []
            }
        }),
    ] {
        stream.apply(event, &events).expect("stream event");
    }

    assert_eq!(
        *seen.lock().expect("events lock"),
        vec![
            ModelEvent::WebSearchStarted {
                call_id: "search-1".into()
            },
            ModelEvent::WebSearchCompleted {
                call_id: "search-1".into(),
                action: WebSearchAction::Other
            }
        ]
    );
}

#[test]
fn responses_history_translates_to_anthropic_tool_messages() {
    let messages = translate_messages(
        &[
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
                "output": "contents"
            }),
        ],
        ToolDiscoveryMode::Rebuild,
        "catalog-1",
        &[],
    )
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
                    "content": "contents",
                    "is_error": false
                }]
            })
        ]
    );
}

#[test]
fn neutral_image_becomes_anthropic_base64_source() {
    let messages = translate_messages(
        &[serde_json::json!({
            "role": "user",
            "content": [
                {"type": "input_text", "text": "Describe it."},
                {"type": "input_image", "media_type": "image/webp", "data": "aGVsbG8="}
            ]
        })],
        ToolDiscoveryMode::Rebuild,
        "catalog-1",
        &[],
    )
    .expect("translate image");

    assert_eq!(
        messages[0]["content"][1],
        serde_json::json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/webp",
                "data": "aGVsbG8="
            }
        })
    );
}

#[test]
fn stream_preserves_text_part_boundaries_and_every_citation_location() {
    let stream = StreamState {
        blocks: BTreeMap::from([
            (
                1,
                serde_json::json!({"type": "thinking", "thinking": "Check sources."}),
            ),
            (
                3,
                serde_json::json!({
                    "type": "text",
                    "text": "Cited answer.",
                    "citations": [
                        {
                            "type": "char_location",
                            "cited_text": "characters",
                            "document_index": 0,
                            "document_title": "Notes",
                            "file_id": "file-1",
                            "start_char_index": 2,
                            "end_char_index": 12
                        },
                        {
                            "type": "page_location",
                            "cited_text": "pages",
                            "document_index": 1,
                            "document_title": null,
                            "file_id": "file-2",
                            "start_page_number": 5,
                            "end_page_number": 7
                        },
                        {
                            "type": "content_block_location",
                            "cited_text": "blocks",
                            "document_index": 2,
                            "document_title": "Chunks",
                            "file_id": null,
                            "start_block_index": 4,
                            "end_block_index": 6
                        },
                        {
                            "type": "search_result_location",
                            "cited_text": "search result",
                            "search_result_index": 3,
                            "source": "urn:source:3",
                            "title": "Result",
                            "start_block_index": 1,
                            "end_block_index": 2
                        },
                        {
                            "type": "web_search_result_location",
                            "cited_text": "web result",
                            "encrypted_index": "opaque-index",
                            "title": null,
                            "url": "https://example.com/web"
                        }
                    ]
                }),
            ),
        ]),
        ..StreamState::default()
    };

    let output = stream.finish().expect("normalized output");

    assert_eq!(
        serde_json::to_value(output.content()).expect("serialize normalized content"),
        serde_json::json!([
            {
                "output_index": 0,
                "part_index": 1,
                "phase": "reasoning",
                "text": "Check sources.",
                "annotations": []
            },
            {
                "output_index": 0,
                "part_index": 3,
                "phase": "final_answer",
                "text": "Cited answer.",
                "annotations": [
                    {
                        "type": "document_character_citation",
                        "cited_text": "characters",
                        "document_index": 0,
                        "document_title": "Notes",
                        "file_id": "file-1",
                        "start_char_index": 2,
                        "end_char_index": 12
                    },
                    {
                        "type": "document_page_citation",
                        "cited_text": "pages",
                        "document_index": 1,
                        "document_title": null,
                        "file_id": "file-2",
                        "start_page_number": 5,
                        "end_page_number": 7
                    },
                    {
                        "type": "document_content_block_citation",
                        "cited_text": "blocks",
                        "document_index": 2,
                        "document_title": "Chunks",
                        "file_id": null,
                        "start_block_index": 4,
                        "end_block_index": 6
                    },
                    {
                        "type": "search_result_citation",
                        "cited_text": "search result",
                        "search_result_index": 3,
                        "source": "urn:source:3",
                        "title": "Result",
                        "start_block_index": 1,
                        "end_block_index": 2
                    },
                    {
                        "type": "web_search_result_citation",
                        "cited_text": "web result",
                        "encrypted_index": "opaque-index",
                        "title": null,
                        "url": "https://example.com/web"
                    }
                ]
            }
        ])
    );
}

#[test]
fn stream_rejects_unmodeled_citation_fields() {
    let stream = StreamState {
        blocks: BTreeMap::from([(
            0,
            serde_json::json!({
                "type": "text",
                "text": "Cited answer.",
                "citations": [{
                    "type": "web_search_result_location",
                    "cited_text": "web result",
                    "encrypted_index": "opaque-index",
                    "title": null,
                    "url": "https://example.com/web",
                    "unmodeled": true
                }]
            }),
        )]),
        ..StreamState::default()
    };

    let error = stream
        .finish()
        .expect_err("unmodeled citation fields must fail");

    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn stream_normalizes_deltas_tools_usage_and_errors() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Ok(())
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
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use"},
            "usage": {"output_tokens": 3}
        }),
        serde_json::json!({"type": "message_stop"}),
    ] {
        stream.apply(event, &events).expect("stream event");
    }

    let output = stream.finish().expect("normalized output");
    assert_eq!(output.text(), "Reading.");
    assert_eq!(output.tool_calls()[0].arguments["path"], "README.md");
    assert_eq!(output.usage().input_tokens, 12);
    assert_eq!(output.usage().cached_input_tokens, 4);
    assert!(matches!(
        seen.lock().expect("events lock").as_slice(),
        [ModelEvent::TextDelta(_), ModelEvent::ReasoningDelta(_)]
    ));

    let error = StreamState::default()
        .apply(
            serde_json::json!({"type": "error", "error": {"message": "quota"}}),
            &events,
        )
        .expect_err("stream error");
    assert!(error.to_string().contains("quota"));
}

#[test]
fn usage_rejects_provider_integer_overflow() {
    let usage = Usage {
        input: i64::MAX,
        cache_read: 1,
        ..Usage::default()
    };

    assert!(usage.finish().is_err());
}
