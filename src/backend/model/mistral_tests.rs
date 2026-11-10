use std::fmt::Write as _;

use serde_json::json;
use tokio::io::AsyncWriteExt as _;

use super::*;
use crate::backend::model::Model as _;
use crate::backend::model::chat_completions::request_body;
use crate::backend::model::provider::BaseUrl;
use crate::backend::model::transport::ApiKeyModel;
use crate::backend::model::{
    ModelRequest, ModelTransportSettings, PromptCacheIdentity, ToolDefinition, user_message,
};
use std::sync::Arc;

fn model() -> Mistral {
    Mistral::new("test-key", MANIFEST.base_url.share(), "mistral-large-4-0")
        .expect("Mistral provider")
        .with_reasoning_effort("high")
        .expect("Mistral effort")
}

fn request<'a>(input: &'a [Value], tools: &'a [Arc<ToolDefinition>]) -> ModelRequest<'a> {
    ModelRequest {
        session_id: None,
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
        let request = crate::backend::model::test_support::read_http_request(&mut socket).await;
        socket
            .write_all(response.as_bytes())
            .await
            .expect("HTTP response");
        request
    });
    (root, server)
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
    let body = request_body(&model, request(&input, &tools)).expect("request");
    let MessageContent::Thinking(content) = &MistralDialect::message(MessageSource::History {
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
        &MistralDialect::message(MessageSource::History {
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
    let mut body = frames.iter().fold(String::new(), |mut body, frame| {
        write!(body, "data: {frame}\n\n").expect("write frame");
        body
    });
    body.push_str("data: [DONE]\n\n");
    let (root, server) =
        mock_response("200 OK", "Content-Type: text/event-stream\r\n", &body).await;
    let model = Mistral::from_config(
        ApiKeyModel::new(
            Some("test-key".into()),
            BaseUrl::new(root.as_str()).unwrap(),
            "mistral-large-4-0".into(),
            reqwest::Client::new(),
            ModelTransportSettings::default(),
        )
        .unwrap(),
    );
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
    assert_eq!(output.text(), "");
    assert_eq!(output.output()[0]["phase"], "commentary");
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
    let replay =
        serde_json::to_value(request_body(&model, request(&history, &[])).unwrap()).unwrap();
    assert_eq!(
        replay["messages"][2]["content"][0]["signature"],
        "sig-final"
    );
    assert_eq!(replay["messages"][2]["tool_calls"][0]["id"], "call-a");
    assert_eq!(replay["messages"][3]["tool_call_id"], "call-a");
}

#[tokio::test]
async fn generation_error_never_commits_partial_tools_or_text() {
    let mut stream = MistralStream::default();
    let error = stream.apply_data(&json!({
        "choices": [{"finish_reason": "error", "delta": {
            "content": "Partial",
            "tool_calls": [{"index": 0, "id": "call-a", "function": {"name": "read", "arguments": "{}"}}]
        }}]
    }).to_string(), &sink()).await.expect_err("failed generation");
    assert!(error.to_string().contains("Mistral generation failed"));
    assert!(stream.text.is_empty());
    assert!(stream.tools.finish("Mistral").unwrap().is_empty());
    let mut stream = MistralStream::default();
    assert!(
        stream
            .apply_data(r#"{"choices":[{"finish_reason":"error"}]}"#, &sink())
            .await
            .is_err()
    );
}
