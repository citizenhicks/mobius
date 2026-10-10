use super::super::*;
use super::support::model_request;
use crate::backend::model::{ModelTransportSettings, ToolDefinition};
use crate::protocol::ToolLoad;

#[test]
fn native_socket_reasoning_uses_its_locked_text_catalog() {
    for (model, effort, valid) in [
        ("gpt-6.1-sol", "medium", true),
        ("gpt-6.1-sol", "operator-effort", false),
        ("operator-model", "medium", false),
    ] {
        let result = OpenAiSocket::new("test-key", model)
            .unwrap()
            .with_reasoning_effort(effort);
        assert_eq!(result.is_ok(), valid, "{model}/{effort}");
        if let Ok(provider) = result {
            assert_eq!(provider.info().reasoning_effort.as_deref(), Some(effort));
        }
    }
}

#[test]
fn implicit_prompt_cache_omits_options() {
    let authorized = OpenAiSocket::with_authorization(
        Arc::new(ApiKeyAuthorization::new("test-key".into())),
        "https://example.com/v1",
        "wss://example.com/v1/responses",
        "test-model",
        reqwest::Client::new(),
        crate::backend::model::ModelTransportSettings::default(),
    )
    .expect("provider");
    let api_key = OpenAiSocket::new("test-key", "test-model").expect("API-key provider");
    for provider in [authorized, api_key] {
        let body = response_body(
            "test-model",
            &model_request(),
            (&[]).into(),
            None,
            None,
            &[],
            provider.explicit_prompt_cache,
        )
        .and_then(|body| Ok(serde_json::to_value(body)?))
        .expect("response body");

        assert_eq!(
            provider.prompt_cache_capability(),
            PromptCacheMode::Implicit
        );
        assert_eq!(
            provider.http.prompt_cache_capability(),
            PromptCacheMode::Implicit
        );
        assert!(body.get("prompt_cache_options").is_none());
        assert_eq!(body["prompt_cache_key"], "hashed-cache-key");
    }
}

#[test]
fn tool_load_becomes_additional_tools_at_its_context_position() {
    let direct = [Arc::new(ToolDefinition {
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: serde_json::json!({"type": "object"}),
    })];
    let deferred = [Arc::new(ToolDefinition {
        name: "notebook_post".into(),
        description: "Post to the notebook".into(),
        parameters: serde_json::json!({"type": "object"}),
    })];
    let input = [
        serde_json::json!({"role": "user", "content": "before"}),
        ToolLoad {
            catalog_revision: "catalog-1".into(),
            tools: vec!["notebook_post".into()],
        }
        .to_input(),
        serde_json::json!({"role": "user", "content": "after"}),
    ];
    let request = ModelRequest {
        input: (&input).into(),
        tools: &direct,
        deferred_tools: &deferred,
        ..model_request()
    };

    let body = response_body(
        "test-model",
        &request,
        (&input).into(),
        None,
        None,
        &[],
        false,
    )
    .and_then(|body| Ok(serde_json::to_value(body)?))
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
                        "name": "notebook_post",
                        "description": "Post to the notebook",
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
fn payment_denials_override_stream_recovery_hints() {
    for (status, code, kind) in [
        (402, "rate_limit_exceeded", "rate_limit_exceeded"),
        (
            402,
            "previous_response_not_found",
            "previous_response_not_found",
        ),
        (429, "insufficient_quota", "insufficient_quota"),
        (429, "rate_limit_exceeded", "insufficient_quota"),
    ] {
        let event = serde_json::json!({
            "type": "error",
            "status": status,
            "error": {"code": code, "type": kind, "message": "Balance exhausted"}
        });
        let Err(Error::Provider(error)) = failed_exchange(&event, false) else {
            panic!("expected payment denial");
        };
        assert_eq!(error.status(), Some(status));
        assert!(!error.is_retryable());
        assert!(!error.is_stream_interrupted());
    }
}

#[test]
fn stream_failures_do_not_enable_http_fallback() {
    let mut state = SocketState {
        connection: None,
        continuation: None,
        use_http: false,
        last_used_at: Instant::now(),
    };

    for _ in 0..ModelTransportSettings::default().stream_retry_limit {
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
        serde_json::json!({"type":"function_call_output", "call_id":"screenshot-1", "output":[{"type":"input_text","text":"before"},{"type":"input_image","media_type":"image/png","data":"immutable-encoded-pixels","detail":"high"}]}),
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
    let (response, input) =
        continuation_input(&mut state, (&continued).into(), envelope).expect("continue");
    assert_eq!(response.as_deref(), Some("resp-1"));
    assert_eq!(
        serde_json::to_value(input).expect("input"),
        serde_json::json!([{ "type": "function_call_output" }])
    );

    let deferred_tools = [Arc::new(ToolDefinition {
        name: "notebook_post".into(),
        description: "Post to the notebook".into(),
        parameters: serde_json::json!({"type": "object"}),
    })];
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
    let (response, input) = continuation_input(&mut state, (&continued).into(), inventory_envelope)
        .expect("inventory continuation");
    assert_eq!(response.as_deref(), Some("resp-1"));
    assert_eq!(
        serde_json::to_value(input).expect("input"),
        serde_json::json!([{ "type": "function_call_output" }])
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
    let (response, input) = continuation_input(&mut state, (&continued).into(), changed_envelope)
        .expect("catalog reset");
    assert_eq!(response, None);
    assert_eq!(
        input.iter().collect::<Vec<_>>(),
        continued.iter().collect::<Vec<_>>()
    );
    assert!(state.continuation.is_none());

    let rewritten = vec![serde_json::json!({"role": "user", "content": "Working checkpoint"})];
    let (response, input) =
        continuation_input(&mut state, (&rewritten).into(), envelope).expect("reset");
    assert_eq!(response, None);
    assert_eq!(
        input.iter().collect::<Vec<_>>(),
        rewritten.iter().collect::<Vec<_>>()
    );
    assert!(state.continuation.is_none());

    state.continuation = Some(Continuation {
        response_id: "resp-2".into(),
        known_items: known.len(),
        fingerprint: fingerprint(known.iter()).expect("fingerprint"),
        envelope_fingerprint: envelope,
    });
    let (response, input) =
        response_input(&mut state, (&known).into(), false, envelope).expect("stateless request");
    assert_eq!(response, None);
    assert_eq!(
        input.iter().collect::<Vec<_>>(),
        known.iter().collect::<Vec<_>>()
    );
    assert!(state.continuation.is_none());
}

#[test]
fn borrowed_settings_keep_the_existing_schema_and_cache_fingerprint() {
    let tools = [Arc::new(ToolDefinition {
        name: "inspect".into(),
        description: "Inspect a file".into(),
        parameters: serde_json::json!({"type":"object", "properties":{"path":{"type":"string"}}}),
    })];
    let hosted = [serde_json::json!({"type":"web_search", "search_context_size":"medium"})];
    let request = ModelRequest {
        tools: &tools,
        allow_hosted_tools: true,
        ..model_request()
    };
    let legacy = serde_json::json!({
        "model":"test-model",
        "instructions": request.instructions,
        "catalog_revision": request.catalog_revision,
        "tools":[{
            "type":"function",
            "name":tools[0].name,
            "description":tools[0].description,
            "parameters":tools[0].parameters,
            "strict":false,
        }, hosted[0]],
        "reasoning_effort":"high",
        "prompt_cache":{"key":"hashed-cache-key", "context_epoch":1, "mode":"explicit"},
    });
    assert_eq!(
        envelope_fingerprint("test-model", &request, Some("high"), &hosted)
            .expect("borrowed settings"),
        fingerprint(std::iter::once(&legacy)).expect("existing JSON settings"),
    );
}
