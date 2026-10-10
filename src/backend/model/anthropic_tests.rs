use super::*;

#[test]
fn replay_borrows_preserved_assistant_blocks() {
    let input = [
        serde_json::json!({
            "type": "message", "role": "assistant",
            RAW_CONTENT: [
                {"type": "thinking", "thinking": "retained reasoning", "signature": "signed"},
                {"type": "text", "text": "answer"}
            ]
        }),
        user_message("continue"),
    ];
    let messages = translate_messages((&input).into(), ToolDiscoveryMode::Rebuild, "catalog", &[])
        .expect("borrowed replay");
    for (actual, original) in messages[0]
        .content
        .iter()
        .zip(input[0][RAW_CONTENT].as_array().unwrap())
    {
        assert!(matches!(actual, Cow::Borrowed(value) if std::ptr::eq(*value, original)));
    }
    assert!(input[0][RAW_CONTENT][1].get("cache_control").is_none());
}
use crate::backend::model::provider::ProviderCredential;
use crate::backend::model::transport::capture_http_request;
use crate::backend::model::user_message;

#[test]
fn advertised_web_search_modes_build() {
    let definition = provider();
    for web_search in definition.web_search().iter().copied() {
        definition
            .build(ProviderBuildConfig {
                capability: None,
                tool_discovery: None,
                credential: ProviderCredential::ApiKey("test-key".into()),
                model: definition.default_model().expect("default model").into(),
                base_url: Some(MANIFEST.base_url.as_str().into()),
                reasoning_effort: None,
                service_tier: None,
                web_search,
                http: reqwest::Client::new(),
                transport: crate::backend::model::ModelTransportSettings::default(),
            })
            .expect("advertised web search mode builds");
    }
}

#[test]
fn catalog_advertises_all_efforts_for_every_model_and_haiku_5_5_defaults() {
    let definition = provider();
    let model = definition
        .model("claude-haiku-5-5")
        .expect("Haiku 5.5 preset");
    assert!(definition.model("claude-haiku-4-5").is_none());
    assert_eq!(model.context_window, 1_000_000);
    assert_eq!(model.default_reasoning.as_deref(), Some("medium"));
    for model in definition.models() {
        assert_eq!(
            model
                .reasoning
                .iter()
                .map(|effort| effort.id.as_str())
                .collect::<Vec<_>>(),
            ["low", "medium", "high", "xhigh", "max"],
            "{}",
            model.id
        );
    }
}

#[test]
fn native_discovery_builds_on_equivalent_and_custom_endpoints() {
    for base_url in [
        "https://api.anthropic.com:443/v1/",
        "https://proxy.example/v1",
    ] {
        let model = provider()
            .build(ProviderBuildConfig {
                capability: None,
                tool_discovery: None,
                credential: ProviderCredential::ApiKey("test-key".into()),
                model: "claude-haiku-5-5".into(),
                base_url: Some(base_url.into()),
                reasoning_effort: None,
                service_tier: None,
                web_search: HostedWebSearch::Off,
                http: reqwest::Client::new(),
                transport: crate::backend::model::ModelTransportSettings::default(),
            })
            .expect("native endpoint builds");

        assert_eq!(
            model.tool_discovery(),
            ToolDiscoveryMode::Native,
            "{base_url}"
        );
    }
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
        .post(b"{}".to_vec())
        .await
        .expect("credentialless request");

    let request = server.await.expect("HTTP server").to_ascii_lowercase();
    assert!(request.starts_with("post /proxy/messages http/1.1\r\n"));
    assert!(!request.contains("x-api-key:"));
}

#[test]
fn anthropic_uses_explicit_prompt_cache() {
    let provider = Anthropic::new("test-key", MANIFEST.base_url.as_str(), "claude-haiku-4-5")
        .expect("provider");
    assert_eq!(
        provider.prompt_cache_capability(),
        PromptCacheMode::Explicit
    );
}

#[test]
fn explicit_prompt_cache_breakpoint_is_sent_on_the_marked_content_block() {
    let provider = Anthropic::new("test-key", MANIFEST.base_url.as_str(), "claude-sonnet-5-5")
        .expect("provider");
    let mut input = user_message("stable prefix");
    assert!(crate::backend::model::mark_prompt_cache_breakpoint(
        &mut input
    ));

    let body = provider
        .request_body_value(
            "instructions",
            (&[input]).into(),
            "catalog-1",
            &[],
            &[],
            false,
        )
        .expect("request body");

    assert!(body.get("cache_control").is_none());
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
}

#[test]
fn explicit_cache_endpoint_advances_and_keeps_the_stable_anchor() {
    let provider = Anthropic::new("test-key", MANIFEST.base_url.as_str(), "claude-sonnet-5-5")
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
fn hosted_search_can_be_disabled_per_request() {
    assert_eq!(
        serde_json::to_value(wire_tools(&[], &[], false)).expect("tools"),
        serde_json::json!([])
    );
    assert_eq!(
        serde_json::to_value(wire_tools(&[], &[], true)).expect("tools")[0]["name"],
        "web_search"
    );
}

#[test]
fn tool_schemas_omit_only_unsupported_root_combinators() {
    let from_manifest = |text: &str, name: &str| {
        let mut manifest: Value = toml::from_str(text).expect("tool manifest");
        Arc::new(
            serde_json::from_value::<ToolDefinition>(manifest[name]["tool"].take())
                .expect("tool definition"),
        )
    };
    let direct = [from_manifest(
        include_str!("../../middleware/sessions.toml"),
        "message_chat",
    )];
    let deferred = [
        from_manifest(
            include_str!("../../middleware/artifacts.toml"),
            "send_artifact",
        ),
        from_manifest(
            include_str!("../../middleware/tools/coding.toml"),
            "view_image",
        ),
    ];
    let original = serde_json::to_string(&(&direct, &deferred)).expect("original schemas");
    let provider = Anthropic::new("test-key", MANIFEST.base_url.as_str(), "claude-haiku-5-5")
        .expect("provider");
    let input = [user_message("hello there")];
    let body = provider
        .request_body_value(
            "instructions",
            (&input).into(),
            "catalog",
            &direct,
            &deferred,
            false,
        )
        .expect("request body");

    for (index, tool) in direct.iter().chain(&deferred).enumerate() {
        let schema = &body["tools"][index]["input_schema"];
        assert_eq!(body["tools"][index]["name"], tool.name);
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"], tool.parameters["properties"]);
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema.get("oneOf").is_none(), "{}", tool.name);
        assert_eq!(
            body["tools"][index]["defer_loading"].as_bool(),
            (index > 0).then_some(true)
        );
    }
    assert!(direct[0].parameters.get("oneOf").is_some());
    assert!(deferred[0].parameters.get("oneOf").is_some());
    assert!(
        body["tools"][2]["input_schema"]["properties"]["images"]["items"]
            .get("oneOf")
            .is_some()
    );
    assert_eq!(
        body["tools"][2]["input_schema"]["required"],
        serde_json::json!(["images"])
    );
    assert_eq!(
        serde_json::to_string(&(&direct, &deferred)).expect("shared schemas"),
        original
    );
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
        let provider = Anthropic::new("test-key", base_url, "claude-sonnet-5-5").expect("provider");
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

#[test]
fn native_discovery_replays_a_standalone_compacted_tool_load() {
    let provider = Anthropic::new("test-key", MANIFEST.base_url.as_str(), "claude-haiku-5-5")
        .expect("provider");
    let direct = [discovery_tool(TOOLS_SEARCH_NAME)];
    let deferred = [discovery_tool("notebook_post")];
    let input = [
        user_message("Continue after compaction."),
        ToolLoad {
            catalog_revision: "catalog-1".into(),
            tools: vec!["notebook_post".into()],
        }
        .to_input(),
    ];

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
            &body["messages"][1]["content"][0]["name"],
            &body["messages"][2]["content"][0]["content"],
        ),
        (
            &serde_json::json!(TOOLS_SEARCH_NAME),
            &serde_json::json!([{"type": "tool_reference", "tool_name": "notebook_post"}]),
        )
    );
}

#[test]
fn native_discovery_ignores_tool_loads_from_an_old_catalog() {
    let provider = Anthropic::new("test-key", MANIFEST.base_url.as_str(), "claude-haiku-5-5")
        .expect("provider");
    let direct = [discovery_tool(TOOLS_SEARCH_NAME)];
    let deferred = [discovery_tool("notebook_post")];
    let mut input = discovery_history();
    input.last_mut().expect("tool load")["catalog_revision"] = "catalog-0".into();

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
        body["messages"][2]["content"][0]["content"],
        serde_json::json!([{"type":"text", "text":"Found notebook_post"}])
    );
}

#[test]
fn rebuild_discovery_omits_deferred_schemas_and_internal_markers() {
    let provider =
        Anthropic::new("test-key", MANIFEST.base_url.as_str(), "custom-model").expect("provider");
    assert_eq!(provider.tool_discovery(), ToolDiscoveryMode::Rebuild);
    let direct = [discovery_tool(TOOLS_SEARCH_NAME)];
    let deferred = [discovery_tool("notebook_post")];
    let input = discovery_history();

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
            body["tools"].as_array().map(Vec::len),
            body["messages"][2]["content"][0]["content"][0]["text"].as_str(),
            body.to_string().contains("tool_load"),
        ),
        (Some(1), Some("Found notebook_post"), false)
    );
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

#[tokio::test]
async fn anthropic_web_search_normalizes_query_to_a_singleton() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Box::pin(async { Ok(()) })
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
        stream.apply(event, &events).await.expect("stream event");
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

#[tokio::test]
async fn anthropic_web_search_with_an_empty_streamed_query_is_other() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Box::pin(async { Ok(()) })
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
        stream.apply(event, &events).await.expect("stream event");
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

#[test]
fn neutral_image_becomes_anthropic_base64_source() {
    let input = [serde_json::json!({
        "role": "user",
        "content": [
            {"type": "input_text", "text": "Describe it."},
            {"type": "input_image", "media_type": "image/webp", "data": "aGVsbG8="}
        ]
    })];
    let messages = translate_messages(
        (&input).into(),
        ToolDiscoveryMode::Rebuild,
        "catalog-1",
        &[],
    )
    .expect("translate image");

    assert_eq!(
        *messages[0].content[1],
        serde_json::json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/webp",
                "data": "aGVsbG8="
            },
            "cache_control": {"type": "ephemeral"}
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
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use"},
            "usage": {"output_tokens": 3}
        }),
        serde_json::json!({"type": "message_stop"}),
    ] {
        stream.apply(event, &events).await.expect("stream event");
    }

    let output = stream.finish().expect("normalized output");
    assert_eq!(output.text(), "Reading.");
    assert_eq!(output.tool_calls()[0].arguments["path"], "README.md");
    assert_eq!(output.usage().input_tokens, 12);
    assert_eq!(output.usage().cached_input_tokens, 4);
    assert!(matches!(
        seen.lock().expect("events lock").as_slice(),
        [
            ModelEvent::TextDelta(_),
            ModelEvent::ReasoningDelta(_),
            ModelEvent::ToolCallReady(crate::protocol::ToolCall { .. })
        ]
    ));

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
        match answer {
            Some(answer) => assert_eq!(stream.finish().expect("visible refusal").text(), answer),
            None => {
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
}

#[tokio::test]
async fn stream_accepts_empty_thinking_and_json_fragments_without_losing_signatures() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Box::pin(async { Ok(()) })
    });
    let mut stream = StreamState::default();
    for event in [
        serde_json::json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "thinking", "thinking": ""}
        }),
        serde_json::json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "thinking_delta", "thinking": ""}
        }),
        serde_json::json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "signature_delta", "signature": "signed"}
        }),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
        serde_json::json!({
            "type": "content_block_start", "index": 1,
            "content_block": {"type": "tool_use", "id": "call-1", "name": "probe_echo", "input": {}}
        }),
        serde_json::json!({
            "type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": ""}
        }),
        serde_json::json!({
            "type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": "{\"token\":\"QUERY\"}"}
        }),
        serde_json::json!({"type": "content_block_stop", "index": 1}),
        serde_json::json!({
            "type": "content_block_start", "index": 2,
            "content_block": {"type": "tool_use", "id": "call-2", "name": "observe_probe", "input": {}}
        }),
        serde_json::json!({
            "type": "content_block_delta", "index": 2,
            "delta": {"type": "input_json_delta", "partial_json": ""}
        }),
        serde_json::json!({"type": "content_block_stop", "index": 2}),
        serde_json::json!({"type": "message_stop"}),
    ] {
        stream.apply(event, &events).await.expect("stream event");
    }

    let output = stream.finish().expect("normalized output");
    assert_eq!(output.tool_calls()[0].arguments["token"], "QUERY");
    assert_eq!(output.tool_calls()[1].arguments, serde_json::json!({}));
    assert_eq!(output.output()[0][RAW_CONTENT][0]["thinking"], "");
    assert_eq!(output.output()[0][RAW_CONTENT][0]["signature"], "signed");
    assert!(matches!(
        seen.lock().expect("events lock").as_slice(),
        [ModelEvent::ToolCallReady(_), ModelEvent::ToolCallReady(_)]
    ));
}

#[tokio::test]
async fn stream_rejects_missing_or_non_string_delta_fragments() {
    let events: ModelEventSink = Arc::new(|_| Box::pin(async { Ok(()) }));
    for (block_type, delta_type, field) in [
        ("text", "text_delta", "text"),
        ("thinking", "thinking_delta", "thinking"),
        ("tool_use", "input_json_delta", "partial_json"),
    ] {
        for value in [None, Some(Value::Null), Some(serde_json::json!(7))] {
            let mut stream = StreamState::default();
            stream
                .apply(
                    serde_json::json!({
                        "type": "content_block_start", "index": 0,
                        "content_block": {"type": block_type}
                    }),
                    &events,
                )
                .await
                .expect("block start");
            let mut delta = serde_json::json!({"type": delta_type});
            if let Some(value) = value {
                delta[field] = value;
            }
            let error = stream
                .apply(
                    serde_json::json!({"type": "content_block_delta", "index": 0, "delta": delta}),
                    &events,
                )
                .await
                .expect_err("malformed fragment must fail");
            assert!(error.to_string().contains(field));
        }
    }
}

#[tokio::test]
async fn stream_rejects_mutation_or_duplicate_stop_after_block_completion() {
    let events: ModelEventSink = Arc::new(|_| Box::pin(async { Ok(()) }));
    let mut stream = StreamState::default();
    stream
        .apply(
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": "done"}
            }),
            &events,
        )
        .await
        .expect("block start");
    stream
        .apply(
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            &events,
        )
        .await
        .expect("block stop");

    assert!(
        stream
            .apply(
                serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": "mutated"}
                }),
                &events,
            )
            .await
            .is_err()
    );
    assert!(
        stream
            .apply(
                serde_json::json!({"type": "content_block_stop", "index": 0}),
                &events,
            )
            .await
            .is_err()
    );
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

#[test]
fn ordered_tool_images_keep_error_status_and_image_cache_marker() {
    let item = serde_json::json!({
        "type":"function_call_output", "call_id":"screen", (TOOL_ERROR_FIELD):true,
        "output":[
            {"type":"input_text", "text":"before"},
            {"type":"input_image", "media_type":"image/png", "data":"AA=="},
            {"type":"input_text", "text":"after"},
            {"type":"input_image", "media_type":"image/png", "data":"AQ==", (PROMPT_CACHE_BREAKPOINT_FIELD):true}
        ]
    });
    let result = tool_result_block(
        &item,
        None,
        ToolDiscoveryMode::Rebuild,
        "catalog",
        &BTreeSet::new(),
        &BTreeSet::new(),
    )
    .expect("native tool result");
    assert_eq!(result["is_error"], true);
    assert_eq!(result["content"][0]["text"], "before");
    assert_eq!(result["content"][1]["source"]["data"], "AA==");
    assert_eq!(result["content"][2]["text"], "after");
    assert_eq!(result["content"][3]["source"]["data"], "AQ==");
    assert_eq!(result["content"][3]["cache_control"]["type"], "ephemeral");
}

#[test]
fn selected_output_token_budget_reaches_the_native_request() {
    let provider = Anthropic::new("test-key", MANIFEST.base_url.as_str(), "claude-haiku-4-5")
        .expect("provider")
        .with_max_output_tokens(2048)
        .expect("output policy");
    let body = provider
        .request_body_value(
            "test",
            (&[user_message("test")]).into(),
            "test",
            &[],
            &[],
            false,
        )
        .expect("native request");
    assert_eq!(body["max_tokens"], 2048);
    assert!(provider.with_max_output_tokens(0).is_err());
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
        private.clone(),
        call.clone(),
        result.clone(),
        visible.clone(),
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
