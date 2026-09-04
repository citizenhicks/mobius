use super::super::*;
use super::support::model_request;
use crate::backend::model::{STREAM_RETRY_LIMIT, ToolDefinition, ToolLoad};

#[test]
fn implicit_prompt_cache_omits_options() {
    let provider = OpenAiSocket::with_authorization(
        Arc::new(ApiKeyAuthorization::new("test-key".into())),
        "https://example.com/v1",
        "wss://example.com/v1/responses",
        "test-model",
        reqwest::Client::new(),
    )
    .expect("provider");
    let body = response_body(
        "test-model",
        &model_request(),
        &[],
        None,
        None,
        &[],
        provider.explicit_prompt_cache,
    )
    .expect("response body");

    assert_eq!(
        (
            provider.prompt_cache_capability(),
            body.get("prompt_cache_options")
        ),
        (PromptCacheMode::Implicit, None)
    );
}

#[test]
fn tool_load_becomes_additional_tools_at_its_context_position() {
    let direct = [ToolDefinition {
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: serde_json::json!({"type": "object"}),
    }];
    let deferred = [ToolDefinition {
        name: "swarm_post".into(),
        description: "Post to the swarm".into(),
        parameters: serde_json::json!({"type": "object"}),
    }];
    let input = [
        serde_json::json!({"role": "user", "content": "before"}),
        ToolLoad {
            catalog_revision: "catalog-1".into(),
            tools: vec!["swarm_post".into()],
        }
        .into_input(),
        serde_json::json!({"role": "user", "content": "after"}),
    ];
    let request = ModelRequest {
        input: &input,
        tools: &direct,
        deferred_tools: &deferred,
        ..model_request()
    };

    let body = response_body("test-model", &request, &input, None, None, &[], false)
        .expect("response body");

    assert_eq!(
        (&body["input"], &body["tools"]),
        (
            &serde_json::json!([
                {"role": "user", "content": "before"},
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [{
                        "type": "function",
                        "name": "swarm_post",
                        "description": "Post to the swarm",
                        "parameters": {"type": "object"},
                        "strict": false
                    }]
                },
                {"role": "user", "content": "after"}
            ]),
            &serde_json::json!([{
                "type": "function",
                "name": "read_file",
                "description": "Read a file",
                "parameters": {"type": "object"},
                "strict": false
            }])
        )
    );
}

#[test]
fn generic_processing_error_is_a_retryable_stream_failure() {
    let request_id = "922d2b28-14a7-4b76-be1e-ae6be18309b9";
    let event = serde_json::json!({
        "type": "response.failed",
        "response": {
            "error": {
                "message": format!(
                    "An error occurred while processing your request. You can retry your request. Please include the request ID {request_id} in your message."
                )
            }
        }
    });

    let Exchange::Retry { retry_after } =
        failed_exchange(&event, false).expect("retryable failure")
    else {
        panic!("expected retryable exchange");
    };
    assert_eq!(retry_after, None);
    assert!(matches!(
        failed_exchange(&event, true).expect("streamed failure"),
        Exchange::Retry { retry_after: None }
    ));
}

#[test]
fn stream_failures_do_not_enable_http_fallback() {
    let mut state = SocketState {
        connection: None,
        continuation: None,
        use_http: false,
        last_used_at: Instant::now(),
    };

    for _ in 0..STREAM_RETRY_LIMIT {
        let Error::Provider(error) = websocket_failure(&mut state, None) else {
            panic!("expected provider error");
        };
        assert!(error.is_stream_interrupted());
    }

    assert!(!state.use_http);
}

#[test]
fn continuation_ignores_searchable_inventory_and_resets_on_catalog_change() {
    let known = vec![
        serde_json::json!({"role": "user", "content": "one"}),
        serde_json::json!({"role": "assistant", "content": "two"}),
    ];
    let mut state = SocketState {
        connection: None,
        continuation: Some(Continuation {
            response_id: "resp-1".into(),
            known_items: known.len(),
            fingerprint: fingerprint(known.iter()).expect("fingerprint"),
            envelope_fingerprint: envelope_fingerprint("test-model", &model_request(), None, &[])
                .expect("envelope fingerprint"),
        }),
        use_http: false,
        last_used_at: Instant::now(),
    };
    let mut continued = known.clone();
    continued.push(serde_json::json!({"type": "function_call_output"}));
    let envelope = envelope_fingerprint("test-model", &model_request(), None, &[])
        .expect("envelope fingerprint");
    let (response, input) = continuation_input(&mut state, &continued, envelope).expect("continue");
    assert_eq!(response.as_deref(), Some("resp-1"));
    assert_eq!(
        input,
        &[serde_json::json!({"type": "function_call_output"})]
    );

    let deferred_tools = [ToolDefinition {
        name: "swarm_post".into(),
        description: "Post to the swarm".into(),
        parameters: serde_json::json!({"type": "object"}),
    }];
    let inventory_envelope = envelope_fingerprint(
        "test-model",
        &ModelRequest {
            deferred_tools: &deferred_tools,
            ..model_request()
        },
        None,
        &[],
    )
    .expect("searchable inventory fingerprint");
    assert_eq!(inventory_envelope, envelope);
    let (response, input) = continuation_input(&mut state, &continued, inventory_envelope)
        .expect("inventory continuation");
    assert_eq!(response.as_deref(), Some("resp-1"));
    assert_eq!(
        input,
        &[serde_json::json!({"type": "function_call_output"})]
    );

    let changed_envelope = envelope_fingerprint(
        "test-model",
        &ModelRequest {
            catalog_revision: "catalog-2",
            deferred_tools: &deferred_tools,
            ..model_request()
        },
        None,
        &[],
    )
    .expect("changed catalog fingerprint");
    let (response, input) =
        continuation_input(&mut state, &continued, changed_envelope).expect("catalog reset");
    assert_eq!(response, None);
    assert_eq!(input, continued);
    assert!(state.continuation.is_none());

    let rewritten = vec![serde_json::json!({"type": "compaction"})];
    let (response, input) = continuation_input(&mut state, &rewritten, envelope).expect("reset");
    assert_eq!(response, None);
    assert_eq!(input, rewritten);
    assert!(state.continuation.is_none());

    state.continuation = Some(Continuation {
        response_id: "resp-2".into(),
        known_items: known.len(),
        fingerprint: fingerprint(known.iter()).expect("fingerprint"),
        envelope_fingerprint: envelope,
    });
    let (response, input) =
        response_input(&mut state, &known, false, envelope).expect("stateless request");
    assert_eq!(response, None);
    assert_eq!(input, known);
    assert!(state.continuation.is_none());
}
