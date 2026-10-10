use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;
use crate::backend::model::{PromptCacheIdentity, ToolDefinition, user_message};
use crate::protocol::FrontendSymbol;

fn model() -> Mistral {
    Mistral::new("test-key", MANIFEST.base_url.as_str(), "mistral-large-4-0")
        .expect("Mistral provider")
        .with_reasoning_effort("high")
        .expect("Mistral effort")
}

#[test]
fn provider_debug_omits_credentials_and_endpoint() {
    let provider =
        Mistral::new("secret-api-key", "https://private.example/v1///", "model").expect("provider");
    assert_eq!(provider.config.base_url, "https://private.example/v1");
    assert_eq!(
        format!("{provider:?}"),
        "Mistral { config: ApiKeyModel { model: \"model\", reasoning_effort: None, .. } }"
    );
}

fn request<'a>(input: &'a [Value], tools: &'a [Arc<ToolDefinition>]) -> ModelRequest<'a> {
    ModelRequest {
        session_id: "session",
        cancellation: None,
        prompt_cache: Some(PromptCacheIdentity {
            key: "cache-key",
            context_epoch: 0,
        }),
        instructions: "Be precise.",
        input: input.into(),
        catalog_revision: "tools-1",
        tools,
        deferred_tools: &[],
        allow_hosted_tools: true,
        allow_continuation: true,
    }
}

fn sink() -> ModelEventSink {
    Arc::new(|_| Box::pin(async { Ok(()) }))
}

async fn mock_response(
    status: &str,
    headers: &str,
    body: &str,
) -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("HTTP listener");
    let root = format!(
        "http://{}/proxy",
        listener.local_addr().expect("HTTP address")
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
        body.len()
    );
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("HTTP connection");
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0; 4_096];
            let read = socket.read(&mut buffer).await.expect("request bytes");
            assert_ne!(read, 0);
            bytes.extend_from_slice(&buffer[..read]);
            let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let length = std::str::from_utf8(&bytes[..end])
                .expect("headers")
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|length| length.trim().parse::<usize>().expect("content length"))
                })
                .expect("request content length");
            if bytes.len() >= end + 4 + length {
                break;
            }
        }
        socket
            .write_all(response.as_bytes())
            .await
            .expect("HTTP response");
        String::from_utf8(bytes).expect("request UTF-8")
    });
    (root, server)
}

#[test]
fn provider_advertises_native_catalog_and_only_supported_capabilities() {
    let definition = provider();
    assert_eq!(
        definition.symbol(),
        FrontendSymbol::Custom("mistral".into())
    );
    assert_eq!(definition.default_model(), Some("mistral-large-4-0"));
    assert_eq!(
        definition
            .models()
            .iter()
            .map(|model| (model.id.as_str(), model.context_window))
            .collect::<Vec<_>>(),
        [
            ("mistral-large-4-0", 1_000_000),
            ("mistral-small-2603", 256_000)
        ]
    );
    assert!(definition.supports_image_input());
    for preset in definition.models() {
        assert_eq!(preset.default_reasoning.as_deref(), Some("high"));
        assert_eq!(
            preset
                .reasoning
                .iter()
                .map(|effort| effort.id.as_str())
                .collect::<Vec<_>>(),
            ["none", "minimal", "low", "medium", "high", "xhigh"]
        );
    }
    let model = model();
    assert!(model.supports_image_input());
    assert!(model.supports_tool_image_input());
    assert!(!model.supports_image_generation());
    assert!(!model.supports_realtime_voice());
}

#[test]
fn request_borrows_native_thinking_tools_and_arguments_without_kimi_fields() {
    let thinking = json!([{"type": "thinking", "thinking": [{"type": "text", "text": "Read both."}], "signature": "opaque-signature", "closed": true}]);
    let input = vec![
        user_message("Read both."),
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Reading."}], (REPLAY_REASONING_FIELD): "Read both.", (RAW_THINKING): thinking}),
        json!({"type": "function_call", "call_id": "call-a", "name": "read", "arguments": "{\"path\":\"a.rs\"}"}),
        json!({"type": "function_call", "call_id": "call-b", "name": "read", "arguments": {"path": "b.rs"}}),
        json!({"type": "function_call_output", "call_id": "call-a", "output": [{"type": "input_text", "text": "A"}]}),
        json!({"type": "function_call_output", "call_id": "call-b", "output": [{"type": "input_text", "text": "B"}]}),
    ];
    let tools = [Arc::new(ToolDefinition {
        name: "read".into(),
        description: "Read a file".into(),
        parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    })];
    let model = model();
    let body = model
        .request_body(request(&input, &tools))
        .expect("request");
    let MessageContent::Thinking(content) = &message_content(MessageSource::History {
        role: "assistant",
        item: &input[1],
    })
    .unwrap()
    .content
    else {
        panic!("borrowed native thinking");
    };
    assert!(std::ptr::eq(
        content.blocks,
        input[1][RAW_THINKING].as_array().unwrap().as_slice()
    ));
    assert!(matches!(&content.text, Cow::Borrowed("Reading.")));
    assert!(matches!(
        &message_content(MessageSource::History {
            role: "user",
            item: &input[0]
        })
        .unwrap()
        .content,
        MessageContent::Neutral(WireContent::Text(Cow::Borrowed("Read both.")))
    ));
    assert!(matches!(
        super::super::chat_completions::argument_text(input[2].get("arguments")).unwrap(),
        Cow::Borrowed(_)
    ));
    let body = serde_json::to_value(body).expect("wire body");
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["prompt_cache_key"], "cache-key");
    assert!(body.get("stream_options").is_none());
    assert!(body["messages"][2].get("reasoning_content").is_none());
    assert_eq!(body["messages"][2]["content"][0], thinking[0]);
    assert_eq!(
        body["messages"][2]["content"][1],
        json!({"type": "text", "text": "Reading."})
    );
    assert_eq!(
        body["messages"][2]["tool_calls"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        body["messages"][3],
        json!({"role": "tool", "tool_call_id": "call-a", "content": "A"})
    );
    assert_eq!(
        body["tools"][0]["function"]["parameters"],
        tools[0].parameters
    );
}

#[test]
fn neutral_images_use_native_image_url_and_reject_malformed_input() {
    let input = [json!({"type": "message", "role": "user", "content": [
        {"type": "input_text", "text": "Describe it."},
        {"type": "input_image", "media_type": "image/jpeg", "data": "aGVsbG8="}
    ]})];
    let model = model();
    let body = serde_json::to_value(model.request_body(request(&input, &[])).unwrap()).unwrap();
    assert_eq!(
        body["messages"][1]["content"][1],
        json!({"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,aGVsbG8="}})
    );
    let bad = [json!({"role": "user", "content": [{"type": "input_image", "data": "aGVsbG8="}]})];
    assert!(model.request_body(request(&bad, &[])).is_err());
}

#[tokio::test]
async fn native_stream_round_trip_preserves_thinking_signatures_tools_and_usage() {
    let frames = [
        json!({"choices": [{"delta": {"content": [{"type": "thinking", "thinking": [{"type": "text", "text": "Read "}], "signature": "sig-", "closed": false}]}}]}),
        json!({"choices": [{"delta": {"content": [
            {"type": "thinking", "thinking": [{"type": "text", "text": "both."}], "signature": "final", "closed": true},
            {"type": "text", "text": "Reading."}
        ], "tool_calls": [{"index": 0, "id": "call-a", "function": {"name": "read", "arguments": "{\"path\":"}}]}}]}),
        json!({"choices": [{"delta": {"content": " Now.", "tool_calls": [{"index": 0, "function": {"arguments": "\"a.rs\"}"}}]}}], "usage": {"prompt_tokens": 12, "completion_tokens": 8, "total_tokens": 20}}),
    ];
    let body = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    let (root, server) =
        mock_response("200 OK", "Content-Type: text/event-stream\r\n", &body).await;
    let model = Mistral::with_client(
        Some("test-key".into()),
        root,
        "mistral-large-4-0",
        Client::new(),
    )
    .unwrap();
    let input = [user_message("Read a.rs.")];
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        capture.lock().unwrap().push(event);
        Box::pin(async { Ok(()) })
    });
    let output = model
        .respond(request(&input, &[]), events)
        .await
        .expect("native response");
    let wire = server.await.unwrap();
    assert!(wire.starts_with("POST /proxy/chat/completions HTTP/1.1\r\n"));
    assert!(
        wire.to_ascii_lowercase()
            .contains("authorization: bearer test-key\r\n")
    );
    let wire_body: Value = serde_json::from_str(wire.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert!(wire_body.get("stream_options").is_none());
    assert_eq!(output.text(), "Reading. Now.");
    assert_eq!(output.usage().total_tokens, 20);
    assert_eq!(output.tool_calls()[0].arguments, json!({"path": "a.rs"}));
    assert!(!output.end_turn());
    assert_eq!(
        output.output()[0][RAW_THINKING],
        json!([{"type": "thinking", "thinking": [{"type": "text", "text": "Read both."}], "signature": "sig-final", "closed": true}])
    );
    assert!(matches!(
        seen.lock().unwrap().as_slice(),
        [
            ModelEvent::ReasoningDelta(_),
            ModelEvent::ReasoningDelta(_),
            ModelEvent::TextDelta(_),
            ModelEvent::TextDelta(_)
        ]
    ));
    let mut history = input.to_vec();
    history.extend_from_slice(output.output());
    history.push(json!({"type": "function_call_output", "call_id": "call-a", "output": [{"type": "input_text", "text": "A"}]}));
    let replay = serde_json::to_value(model.request_body(request(&history, &[])).unwrap()).unwrap();
    assert_eq!(
        replay["messages"][2]["content"][0]["signature"],
        "sig-final"
    );
    assert_eq!(replay["messages"][2]["tool_calls"][0]["id"], "call-a");
    assert_eq!(replay["messages"][3]["tool_call_id"], "call-a");
}

#[tokio::test]
async fn credentialless_endpoint_and_http_errors_keep_native_labels() {
    let (root, server) = mock_response(
        "503 Service Unavailable",
        "Retry-After: 2\r\nContent-Type: application/json\r\n",
        r#"{"message":"temporarily unavailable"}"#,
    )
    .await;
    let model = Mistral::with_client(None, root, "model", Client::new()).unwrap();
    let error = super::super::chat_completions::post(
        &model.config.client,
        &model.config.base_url,
        model.config.api_key.as_deref(),
        b"{}".to_vec(),
        "Mistral",
    )
    .await
    .expect_err("HTTP rejection");
    assert!(error.to_string().contains("Mistral HTTP 503"));
    assert!(error.to_string().contains("temporarily unavailable"));
    let wire = server.await.unwrap();
    assert!(!wire.to_ascii_lowercase().contains("authorization:"));
}

#[tokio::test]
async fn malformed_streams_and_event_sink_failures_are_not_hidden() {
    let mut stream = StreamState::default();
    assert!(
        stream
            .apply_data(r#"{"error":{"message":"quota"}}"#, &sink())
            .await
            .unwrap_err()
            .to_string()
            .contains("quota")
    );
    assert!(
        stream
            .finish()
            .unwrap_err()
            .to_string()
            .contains("before the [DONE]")
    );
    let mut stream = StreamState::default();
    let failing: ModelEventSink =
        Arc::new(|_| Box::pin(async { Err(Error::Provider("sink ended".into())) }));
    let error = stream
        .apply_data(r#"{"choices":[{"delta":{"content":"Hello"}}]}"#, &failing)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("sink ended"));
    let mut stream = StreamState::default();
    stream
        .apply_data(
            r#"{"choices":[{"delta":{"content":[{"type":"text","text":""}]}}]}"#,
            &sink(),
        )
        .await
        .expect("valid empty text delta");
    assert!(
        stream
            .apply_data(
                r#"{"choices":[{"delta":{"content":[{"type":"unexpected"}]}}]}"#,
                &sink()
            )
            .await
            .is_err()
    );
    let mut stream = StreamState {
        thinking: (0..MAX_THINKING_BLOCKS)
            .map(|_| ThinkingBlock {
                kind: "thinking",
                closed: Some(true),
                finished: true,
                ..ThinkingBlock::default()
            })
            .collect(),
        ..StreamState::default()
    };
    assert!(
        stream
            .append_thinking(
                ThinkingChunk {
                    thinking: Vec::new(),
                    signature: None,
                    closed: Some(true)
                },
                &sink()
            )
            .await
            .is_err()
    );
}

#[test]
fn route_replay_strips_native_thinking_without_copying_unaffected_history() {
    let user = Arc::new(user_message("Read it."));
    let call = Arc::new(
        json!({"type": "function_call", "call_id": "a", "name": "read", "arguments": "{}"}),
    );
    let mut input = vec![
        Arc::clone(&user),
        Arc::new(json!({
            "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Reading."}],
            (REPLAY_REASONING_FIELD): "Read it.",
            (RAW_THINKING): [{"type": "thinking", "thinking": [{"type": "text", "text": "Read it."}], "signature": "opaque", "closed": true}]
        })),
        Arc::clone(&call),
    ];
    assert!(super::super::strip_shared_provider_reasoning(&mut input));
    assert!(Arc::ptr_eq(&input[0], &user));
    assert!(Arc::ptr_eq(&input[2], &call));
    assert!(input[1].get(RAW_THINKING).is_none());
    assert!(input[1].get(REPLAY_REASONING_FIELD).is_none());
    assert_eq!(input[1]["content"][0]["text"], "Reading.");
    assert!(!super::super::strip_shared_provider_reasoning(&mut input));
}

#[tokio::test]
async fn generation_error_never_commits_partial_tools_or_text() {
    let mut stream = StreamState::default();
    let error = stream.apply_data(&json!({
        "choices": [{"finish_reason": "error", "delta": {
            "content": "Partial",
            "tool_calls": [{"index": 0, "id": "call-a", "function": {"name": "read", "arguments": "{}"}}]
        }}]
    }).to_string(), &sink()).await.expect_err("failed generation");
    assert!(error.to_string().contains("Mistral generation failed"));
    assert!(stream.text.is_empty());
    assert!(stream.tools.finish("Mistral").unwrap().is_empty());
    let mut stream = StreamState::default();
    assert!(
        stream
            .apply_data(r#"{"choices":[{"finish_reason":"error"}]}"#, &sink())
            .await
            .is_err()
    );
}

#[test]
fn single_text_tool_results_borrow_without_weakening_validation() {
    for text in ["", "x".repeat(65_536).as_str()] {
        let output = json!([{"type": "input_text", "text": text}]);
        let WireContent::Text(Cow::Borrowed(borrowed)) =
            WireContent::tool_result(&output, "Mistral").unwrap()
        else {
            panic!("single text result should borrow its stored text");
        };
        assert!(std::ptr::eq(borrowed, output[0]["text"].as_str().unwrap()));
        assert_eq!(
            serde_json::to_value(WireContent::Text(borrowed.into())).unwrap(),
            text
        );
    }
    assert!(WireContent::tool_result(&json!([{"type": "input_text"}]), "Mistral").is_err());
}

#[test]
fn tool_observations_keep_text_images_and_file_references_together() {
    let mut input = vec![
        user_message("Inspect it."),
        json!({"type": "function_call", "call_id": "call-a", "name": "view_image", "arguments": "{}"}),
        json!({"type": "function_call_output", "call_id": "call-a", "output": [
            {"type": "input_text", "text": "Screenshot"},
            {"type": "file", "file": {"id": "original"}},
            {"type": "input_image", "media_type": "image/png", "data": "aGVsbG8="}
        ]}),
    ];
    let model = model();
    let body = serde_json::to_value(model.request_body(request(&input, &[])).unwrap()).unwrap();
    assert_eq!(
        body["messages"][3]["content"],
        json!([
            {"type": "text", "text": "Screenshot"},
            {"type": "text", "text": "Stored file: {\"id\":\"original\"}"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGVsbG8="}}
        ])
    );
    input[2]["output"] =
        json!([{"type": "input_text", "text": ""}, {"type": "file", "file": {"id": "original"}}]);
    let body = serde_json::to_value(model.request_body(request(&input, &[])).unwrap()).unwrap();
    assert_eq!(
        body["messages"][3]["content"],
        "\nStored file: {\"id\":\"original\"}"
    );
    input[2]["output"] = json!([{"type": "input_text"}, {"type": "input_image", "media_type": "image/png", "data": "aGVsbG8="}]);
    assert!(model.request_body(request(&input, &[])).is_err());
    input[2]["output"] = json!("unsupported tool scalar");
    assert!(model.request_body(request(&input, &[])).is_err());
}

#[tokio::test]
async fn missing_closed_metadata_is_not_invented_on_tool_only_thinking() {
    let mut stream = StreamState::default();
    let events = sink();
    for frame in [
        json!({"choices": [{"delta": {"content": [{"type": "thinking", "thinking": [{"type": "text", "text": "Read "}]}]}}]}),
        json!({"choices": [{"delta": {"content": [{"type": "text", "text": ""}]}}]}),
        json!({"choices": [{"delta": {
            "content": [{"type": "thinking", "thinking": [{"type": "text", "text": "it."}], "signature": "opaque"}],
            "tool_calls": [{"index": 0, "id": "call-a", "function": {"name": "read", "arguments": "{}"}}]
        }}]}),
    ] {
        stream
            .apply_data(&frame.to_string(), &events)
            .await
            .unwrap();
    }
    stream.apply_data("[DONE]", &events).await.unwrap();
    let output = stream.finish().unwrap();
    assert!(!output.end_turn());
    assert_eq!(
        output.output()[0][RAW_THINKING],
        json!([{"type": "thinking", "thinking": [{"type": "text", "text": "Read it."}], "signature": "opaque"}])
    );
}
