use super::*;
use crate::backend::model::ModelInput;
use crate::backend::model::PromptCacheIdentity;
use crate::backend::model::test_support::{
    RefreshingAuthorization, capture_http_request, read_http_request,
};

fn wire_input(
    input: ModelInput<'_>,
    allow_images: bool,
    catalog_revision: &str,
    additional_tools: &[Arc<ToolDefinition>],
) -> Result<Vec<Value>> {
    let input = WireInput::new(input, allow_images, catalog_revision, additional_tools)?;
    Ok(serde_json::from_value(serde_json::to_value(input)?)?)
}

fn body_value(provider: &OpenAi, request: ModelRequest<'_>) -> Value {
    serde_json::to_value(provider.response_body(request).expect("request body")).expect("JSON")
}
use crate::protocol::ModelStepContentPhase;

#[tokio::test]
async fn actual_hosted_completions_are_distinct_from_local_calls_and_citation_activity() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);
    let sink: ModelEventSink = Arc::new(move |event| {
        captured.lock().expect("events").push(event);
        Box::pin(async { Ok(()) })
    });
    let mut searches = BTreeSet::new();
    for (id, action, name) in [
        (
            "search",
            serde_json::json!({"type":"search","query":"mobius","filters":{"allowed_domains":["example.com"]},"sources":[{"url":"https://example.com"}]}),
            "web_search",
        ),
        (
            "open",
            serde_json::json!({"type":"open_page","url":"https://example.com"}),
            "open_page",
        ),
        (
            "find",
            serde_json::json!({"type":"find_in_page","url":"https://example.com","pattern":"answer"}),
            "find_in_page",
        ),
    ] {
        let event = serde_json::json!({"type":"response.output_item.done","item":{"type":"web_search_call","id":id,"action":action}});
        let added = serde_json::json!({"type":"response.output_item.added","item":{"type":"web_search_call","id":id}});
        emit_web_event(&added, &mut searches, &sink, "web_search_call")
            .await
            .expect("hosted start");
        emit_web_event(&event, &mut searches, &sink, "web_search_call")
            .await
            .expect("hosted completion");
        let mut expected = action;
        expected.as_object_mut().unwrap().remove("type");
        expected.as_object_mut().unwrap().remove("sources");
        let events = seen.lock().expect("events");
        let execution = events
            .iter()
            .rev()
            .find_map(|event| match event {
                ModelEvent::HostedToolCompleted(execution) => Some(execution),
                _ => None,
            })
            .expect("actual execution");
        assert_eq!(&*execution.name, name);
        assert_eq!(execution.arguments, Some(expected));
        assert_eq!(execution.result_fingerprint, None);
        drop(events);
    }
    for kind in [
        "function_call",
        "custom_tool_call",
        "computer_call",
        "mcp_approval_request",
        "shell_call",
        "message",
    ] {
        for phase in ["response.output_item.added", "response.output_item.done"] {
            emit_web_event(
                &serde_json::json!({"type":phase,"item":{"type":kind,"id":"local"}}),
                &mut searches,
                &sink,
                "web_search_call",
            )
            .await
            .expect("local item");
        }
    }
    for item in [
        serde_json::json!({"type":"tool_search_call","id":"discovery","arguments":{"query":"read"}}),
        serde_json::json!({"type":"file_search_call","id":"files","queries":["release notes"]}),
        serde_json::json!({"type":"mcp_list_tools","id":"listing","server_label":"docs"}),
        serde_json::json!({"type":"program","id":"program-item","call_id":"program-call","code":"text(1)"}),
        serde_json::json!({"type":"program_output","id":"program-result","call_id":"program-call","result":"1","status":"completed"}),
        serde_json::json!({"type":"shell_call_output","id":"shell-result","call_id":"shell-call","output":[{"stdout":"ok","stderr":"","outcome":{"type":"exit","exit_code":0}}]}),
    ] {
        for phase in ["response.output_item.added", "response.output_item.done"] {
            emit_web_event(
                &serde_json::json!({"type":phase,"item":item}),
                &mut searches,
                &sink,
                "web_search_call",
            )
            .await
            .expect("native execution");
        }
    }
    let executions = seen
        .lock()
        .expect("events")
        .iter()
        .filter_map(|event| match event {
            ModelEvent::HostedToolCompleted(execution) => Some(execution.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(executions.len(), 8);
    assert_eq!(executions[0].sources, ["https://example.com"]);
    assert_eq!(&*executions[3].name, super::super::TOOLS_SEARCH_NAME);
    assert_eq!(&*executions[4].name, "file_search");
    assert_eq!(
        executions[4].arguments,
        Some(serde_json::json!({"queries":["release notes"]}))
    );
    assert_eq!(&*executions[5].name, "mcp_list_tools");
    assert_eq!(
        executions[5].arguments,
        Some(serde_json::json!({"server_label":"docs"}))
    );
    assert_eq!(&*executions[6].name, "programmatic_tool_calling");
    assert_eq!(executions[6].call_id.as_str(), "program-call");
    assert_eq!(executions[6].arguments, None);
    assert_eq!(&*executions[7].name, "shell");
    assert_eq!(executions[7].call_id.as_str(), "shell-call");
    assert_eq!(executions[7].arguments, None);
    for kind in ["file_search_call", "mcp_list_tools"] {
        assert_eq!(
            hosted_execution(
                Some(&serde_json::json!({"type":kind,"id":"undisclosed"})),
                "web_search_call"
            )
            .unwrap()
            .unwrap()
            .arguments,
            None
        );
    }
    assert_eq!(
        hosted_execution(
            Some(
                &serde_json::json!({"type":"image_generation_call","id":"image","result":"opaque"})
            ),
            "web_search_call"
        )
        .unwrap()
        .unwrap()
        .arguments,
        None
    );

    let cited = ModelOutput::from_output(vec![serde_json::json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[{"type":"url_citation","url":"https://example.com","title":"source","start_index":0,"end_index":6}]}]})], true, TokenUsage::default()).expect("cited output");
    emit_citation_web_search(&cited, false, &sink)
        .await
        .expect("inferred activity");
    assert_eq!(
        seen.lock()
            .expect("events")
            .iter()
            .filter(|event| matches!(event, ModelEvent::HostedToolCompleted(_)))
            .count(),
        8
    );
    assert_eq!(
        seen.lock()
            .expect("events")
            .iter()
            .filter(|event| matches!(event, ModelEvent::HostedToolStarted { .. }))
            .count(),
        7
    );
    let completed_before_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let captured_completion = Arc::clone(&completed_before_stop);
    let blocked: ModelEventSink = Arc::new(move |event| {
        let result = match event {
            ModelEvent::WebSearchCompleted { .. } => {
                captured_completion.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            ModelEvent::HostedToolCompleted(_) => Err(Error::Stopped(
                "shared tool policy stopped execution".into(),
            )),
            _ => Ok(()),
        };
        Box::pin(async move { result })
    });
    assert!(matches!(emit_web_event(
        &serde_json::json!({"type":"response.output_item.done","item":{"type":"tool_search_call","id":"blocked","arguments":{"query":"read"}}}),
        &mut searches,
        &blocked,
        "web_search_call",
    ).await, Err(Error::Stopped(_))));
    assert!(matches!(emit_web_event(
        &serde_json::json!({"type":"response.output_item.done","item":{"type":"web_search_call","id":"blocked-search","action":{"type":"search","query":"mobius"}}}),
        &mut searches,
        &blocked,
        "web_search_call",
    ).await, Err(Error::Stopped(_))));
    assert!(completed_before_stop.load(std::sync::atomic::Ordering::SeqCst));
}

fn url(url: &str) -> BaseUrl {
    BaseUrl::new(url).expect("base URL")
}

fn max_request_bytes(
    settings: crate::backend::model::ModelTransportSettings,
    max_request_bytes: usize,
) -> crate::backend::model::ModelTransportSettings {
    crate::backend::model::RawModelTransportSettings {
        max_request_bytes,
        ..*settings
    }
    .try_into()
    .expect("transport settings")
}

fn model_request() -> ModelRequest<'static> {
    ModelRequest {
        session_id: None,
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
fn base_url_rejects_serializable_secret_locations() {
    for url in [
        "https://secret@example.com/v1",
        "https://example.com/v1?key=secret",
        "https://example.com/v1#secret",
    ] {
        assert!(BaseUrl::new(url).is_err());
        assert!(serde_json::from_value::<BaseUrl>(url.into()).is_err());
    }
    let spelling = "https://EXAMPLE.com:443/v1/";
    let url = BaseUrl::new(spelling).expect("endpoint");
    assert_eq!(url.as_str(), spelling);
    assert_eq!(serde_json::to_value(&url).unwrap(), spelling);
    assert_eq!(
        serde_json::from_value::<BaseUrl>(spelling.into()).unwrap(),
        url
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
        wire_input((&input).into(), true, "catalog-1", &[]).expect("wire input"),
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
        session_id: None,
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

    let compatible = OpenAi::new("test-key", url("https://example.com/v1"), "test-model")
        .expect("compatible provider");
    let compatible_body = body_value(&compatible, request);
    assert_eq!(compatible_body["prompt_cache_key"], "opaque-cache-key");
    assert!(compatible_body.get("max_output_tokens").is_none());
    assert_eq!(
        compatible_body["input"][0]["content"][0],
        serde_json::json!({"type": "input_text", "text": "stable prefix"})
    );

    let capped = OpenAi::new("test-key", url("https://api.openai.com/v1"), "test-model")
        .expect("first-party provider")
        .with_max_output_tokens(Some(32_000));
    let capped_body = body_value(&capped, request);
    assert_eq!(capped_body["max_output_tokens"], 32_000);
}

#[test]
fn inline_search_materializes_only_deferred_tools_it_calls() {
    let deferred = [
        tool_definition("notebook_post"),
        tool_definition("memory_write"),
    ];
    let output = OpenAi::new("test-key", url("https://vendor.example/v1"), "test-model")
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
        &std::collections::BTreeSet::from(["notebook_post".to_owned()])
    );
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

    assert_eq!(decoded.text(), "");
    assert_eq!(decoded.content()[0].text, "Checking.");
    assert_eq!(
        decoded.content()[0].phase,
        ModelStepContentPhase::Commentary
    );
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
async fn http_unauthorized_refreshes_and_retries_once() {
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
            requests.push(read_http_request(&mut stream).await);
            stream
                .write_all(response.as_bytes())
                .await
                .expect("HTTP response");
        }
        requests
    });

    let auth = Arc::new(RefreshingAuthorization::new());
    let mut provider = OpenAi::from_authorization(
        Some(Arc::<RefreshingAuthorization>::clone(&auth)),
        url(&format!("http://{address}")),
        "test-model".into(),
        reqwest::Client::new(),
        crate::backend::model::ModelTransportSettings::default(),
    )
    .expect("provider");
    let expected =
        serde_json::to_vec(&provider.response_body(model_request()).expect("body")).expect("JSON");
    provider.transport = max_request_bytes(provider.transport, expected.len());
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
    provider.transport = max_request_bytes(provider.transport, expected.len() - 1);
    let error = provider
        .respond(model_request(), Arc::new(|_| Box::pin(async { Ok(()) })))
        .await
        .expect_err("direct requests also reject one byte over budget before sending");
    assert!(error.to_string().contains("request budget"));
    assert!(requests[0].contains("Bearer rejected-token"));
    assert!(requests[1].contains("Bearer fresh-token"));
    assert_eq!(auth.refreshes(), 1);
}

#[tokio::test]
async fn http_transport_failure_does_not_replay_accepted_post() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("HTTP connection");
        let request = read_http_request(&mut stream).await;
        drop(stream);
        let replayed =
            tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept())
                .await
                .is_ok();
        (request, replayed)
    });
    let provider = OpenAi::from_api_key(
        Some("test-key".into()),
        url(&format!("http://{address}")),
        "test-model".into(),
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
    assert!(request.ends_with("{}"));
    assert!(request.contains("Bearer test-key"));
}

#[tokio::test]
async fn eof_without_response_completed_is_a_retryable_interruption() {
    let (address, server) = capture_http_request().await;
    let provider = OpenAi::new("test-key", url(&format!("http://{address}")), "test-model")
        .expect("provider")
        .with_headers(&crate::backend::model::openrouter::MANIFEST.headers);
    let error = provider
        .send_response(
            model_request(),
            Arc::new(|_| Box::pin(async { Ok(()) })),
            None,
        )
        .await
        .expect_err("missing completion");
    assert!(matches!(error, Error::Provider(error) if error.is_stream_interrupted()));
    let request = server.await.expect("HTTP server").to_lowercase();
    assert!(request.contains("http-referer: https://github.com/citizenhicks/mobius\r\n"));
    assert!(request.contains("x-openrouter-title: möbius\r\n"));
    assert!(request.contains("x-openrouter-categories: personal-agent\r\n"));
}

#[test]
fn a_tool_loaded_twice_is_sent_once() {
    let load = || {
        crate::protocol::ToolLoad {
            catalog_revision: "catalog-1".into(),
            tools: vec!["show_ui".into()],
        }
        .to_input()
    };
    let tool = Arc::new(ToolDefinition {
        name: "show_ui".into(),
        description: "test tool".into(),
        parameters: serde_json::json!({"type": "object"}),
    });
    let input = vec![load(), load()];
    let wire = wire_input((&input).into(), true, "catalog-1", &[tool]).expect("wire input");
    assert_eq!(wire.len(), 1, "{wire:?}");
    assert_eq!(wire[0]["type"], "additional_tools");
}
