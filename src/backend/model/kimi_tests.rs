use serde_json::json;

use super::*;
use crate::backend::model::PromptCacheIdentity;
use crate::backend::model::transport::capture_http_request;
use crate::backend::model::user_message;
use crate::protocol::FrontendSymbol;

#[test]
fn provider_advertises_kimi_identity() {
    assert_eq!(provider().symbol(), FrontendSymbol::Custom("kimi".into()));
}

#[tokio::test]
async fn credentialless_post_uses_custom_path_and_omits_authorization() {
    let (address, server) = capture_http_request().await;
    let provider = Kimi::with_client(
        None,
        format!("http://{address}/proxy"),
        "test-model",
        reqwest::Client::new(),
    )
    .expect("credentialless provider");

    provider
        .post(&json!({}))
        .await
        .expect("credentialless request");

    let request = server.await.expect("HTTP server").to_ascii_lowercase();
    assert!(request.starts_with("post /proxy/chat/completions http/1.1\r\n"));
    assert!(!request.contains("authorization:"));
}

#[test]
fn responses_history_becomes_kimi_messages_and_tools() {
    let provider = Kimi::new("test-key", DEFAULT_BASE_URL, "kimi-k3")
        .expect("provider")
        .with_reasoning_effort("high")
        .expect("reasoning");
    let input = vec![
        user_message("Inspect both files."),
        json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "I will inspect them."}],
            (REPLAY_REASONING_FIELD): "Use parallel reads."
        }),
        json!({
            "type": "function_call",
            "call_id": "call-a",
            "name": "read",
            "arguments": "{\"path\":\"a.rs\"}"
        }),
        json!({
            "type": "function_call",
            "call_id": "call-b",
            "name": "read",
            "arguments": "{\"path\":\"b.rs\"}"
        }),
        json!({"type": "function_call_output", "call_id": "call-a", "output": "A"}),
        json!({"type": "function_call_output", "call_id": "call-b", "output": "B"}),
    ];
    let tools = [ToolDefinition {
        name: "read".into(),
        description: "Read a file".into(),
        parameters: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
    }];
    let request = ModelRequest {
        session_id: "session-7",
        prompt_cache: Some(PromptCacheIdentity {
            key: "hashed-session-7",
            context_epoch: 0,
        }),
        instructions: "Be precise.",
        input: &input,
        catalog_revision: "catalog-1",
        tools: &tools,
        deferred_tools: &[],
        allow_hosted_tools: true,
        allow_continuation: true,
    };

    let body = provider.request_body(&request).expect("request body");

    assert_eq!(body["model"], "kimi-k3");
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["prompt_cache_key"], "hashed-session-7");
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(
        body["messages"],
        json!([
            {"role": "system", "content": "Be precise."},
            {"role": "user", "content": "Inspect both files."},
            {
                "role": "assistant",
                "content": "I will inspect them.",
                "reasoning_content": "Use parallel reads.",
                "tool_calls": [
                    {
                        "id": "call-a",
                        "type": "function",
                        "function": {"name": "read", "arguments": "{\"path\":\"a.rs\"}"}
                    },
                    {
                        "id": "call-b",
                        "type": "function",
                        "function": {"name": "read", "arguments": "{\"path\":\"b.rs\"}"}
                    }
                ]
            },
            {"role": "tool", "tool_call_id": "call-a", "content": "A"},
            {"role": "tool", "tool_call_id": "call-b", "content": "B"}
        ])
    );
    assert_eq!(
        body["tools"],
        json!([{
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read a file",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }
        }])
    );
}

#[test]
fn neutral_image_becomes_kimi_image_url() {
    let content = wire_content(Some(&json!([
        {"type": "input_text", "text": "Describe it."},
        {"type": "input_image", "media_type": "image/jpeg", "data": "aGVsbG8="}
    ])))
    .expect("wire image");

    assert_eq!(
        content,
        json!([
            {"type": "text", "text": "Describe it."},
            {
                "type": "image_url",
                "image_url": {"url": "data:image/jpeg;base64,aGVsbG8="}
            }
        ])
    );
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
    stream
        .apply_data(
            &json!({
                "choices": [{
                    "delta": {
                        "reasoning_content": "Plan.",
                        "content": "Reading.",
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-1",
                            "function": {"name": "read", "arguments": "{\"path\":"}
                        }]
                    }
                }]
            })
            .to_string(),
            &events,
        )
        .expect("first delta");
    stream
        .apply_data(
            &json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {"arguments": "\"README.md\"}"}
                        }]
                    }
                }],
                "usage": {
                    "prompt_tokens": 10,
                    "prompt_tokens_details": {"cached_tokens": 4},
                    "completion_tokens": 3,
                    "total_tokens": 13
                }
            })
            .to_string(),
            &events,
        )
        .expect("second delta");
    stream.apply_data("[DONE]", &events).expect("done");

    let output = stream.finish().expect("normalized output");
    assert_eq!(output.text(), "Reading.");
    assert_eq!(output.tool_calls()[0].arguments["path"], "README.md");
    assert_eq!(output.usage().cached_input_tokens, 4);
    assert!(matches!(
        seen.lock().expect("events lock").as_slice(),
        [ModelEvent::ReasoningDelta(_), ModelEvent::TextDelta(_)]
    ));

    let mut failed = StreamState::default();
    let error = failed
        .apply_data(r#"{"error":{"message":"quota"}} "#, &events)
        .expect_err("stream error");
    assert!(error.to_string().contains("quota"));
}
