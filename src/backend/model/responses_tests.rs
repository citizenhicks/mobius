use super::*;
use crate::backend::model::PromptCacheIdentity;
use crate::backend::model::transport::capture_http_request;

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
