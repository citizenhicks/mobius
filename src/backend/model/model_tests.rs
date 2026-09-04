//! Provider-neutral model and routing tests.

use super::*;

struct DefaultCapabilities;

struct ObservedCapabilities;

impl Model for DefaultCapabilities {
    fn respond<'a>(
        &'a self,
        _request: ModelRequest<'a>,
        _events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(async { Err(Error::Provider("response was not expected".into())) })
    }
}

impl Model for ObservedCapabilities {
    fn prompt_cache_capability(&self) -> PromptCacheMode {
        PromptCacheMode::Explicit
    }

    fn pricing(&self) -> Option<ModelPricing> {
        Some(ModelPricing::new(1_000_000, 100_000, 1_250_000, 2_000_000))
    }

    fn respond<'a>(
        &'a self,
        _request: ModelRequest<'a>,
        _events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(async { Err(Error::Provider("response was not expected".into())) })
    }
}

#[test]
fn reply_snapshot_decorates_model_text_without_replacing_message_metadata() {
    let event = MessageEvent {
        author: MessageAuthor::User,
        delivery: crate::protocol::MessageDelivery::Turn,
        text: "Current answer".into(),
        attachments: Vec::new(),
        reply: Some(crate::protocol::MessageReply {
            target: crate::protocol::MessageTarget {
                checkpoint_sequence: 9,
                batch_item_count: 3,
            },
            text: "Earlier\nmessage".into(),
        }),
        message_target: None,
    };

    let input = message_input(&event).expect("reply model input");
    let metadata: MessageEvent =
        serde_json::from_value(input[MESSAGE_METADATA_FIELD].clone()).expect("message metadata");

    assert_eq!(
        (input["content"][0]["text"].as_str(), metadata,),
        (
            Some("Replying to this earlier message:\n\n> Earlier\n> message\n\nCurrent answer"),
            event,
        )
    );
}

#[test]
fn prompt_cache_identity_is_session_stable_and_keeps_one_latest_breakpoint() {
    let first = prompt_cache_key("session-1");
    assert_eq!(first, prompt_cache_key("session-1"));
    assert_ne!(first, prompt_cache_key("session-2"));

    let mut input = vec![user_message("old"), user_message("new")];
    assert!(mark_prompt_cache_breakpoint(&mut input[0]));
    reset_prompt_cache_breakpoint(&mut input);

    assert!(!has_prompt_cache_breakpoint(&input[..1]));
    assert!(has_prompt_cache_breakpoint(&input[1..]));
}

#[test]
fn pricing_separates_cache_buckets_and_applies_long_context_rates() {
    let usage = TokenUsage {
        input_tokens: 1_000,
        cached_input_tokens: 200,
        cache_write_input_tokens: 300,
        output_tokens: 100,
        total_tokens: 1_100,
        ..TokenUsage::default()
    };
    let pricing = ModelPricing::new(1_000_000, 100_000, 1_250_000, 2_000_000);

    assert_eq!(pricing.estimate_microusd(&usage), Some(1_095));
    assert_eq!(
        pricing
            .with_long_context(999, 2_000, 1_500)
            .estimate_microusd(&usage),
        Some(2_090)
    );
}

#[test]
fn model_step_diagnostics_report_rewrite_before_cache_write() {
    let router = ModelRouter::new("observed", Arc::new(ObservedCapabilities));
    let usage = TokenUsage {
        input_tokens: 100,
        cache_write_input_tokens: 100,
        total_tokens: 100,
        ..TokenUsage::default()
    };

    let diagnostics = router
        .model_step_diagnostics("observed", 4, vec!["compaction".into()], &usage)
        .expect("diagnostics");

    assert_eq!(diagnostics.provider, "observed");
    assert_eq!(
        diagnostics.prompt_cache.capability,
        PromptCacheMode::Explicit
    );
    assert_eq!(
        diagnostics.prompt_cache.outcome,
        PromptCacheOutcome::ContextRewrite
    );
    assert_eq!(diagnostics.prompt_cache.context_epoch, 4);
    assert_eq!(diagnostics.prompt_cache.rewrite_reasons, ["compaction"]);
    assert_eq!(diagnostics.estimated_cost_microusd, Some(125));
}

#[test]
fn image_input_requires_explicit_provider_support() {
    let model: Arc<dyn Model> = Arc::new(DefaultCapabilities);
    let router = ModelRouter::new("text-only", Arc::clone(&model));

    assert!(!model.supports_image_input());
    assert!(!router.supports_image_input("text-only").expect("route"));
    assert!(
        !router
            .choices()
            .next()
            .expect("choice")
            .supports_image_input
    );
}

#[test]
fn normalized_output_derives_text_and_validates_tool_calls() {
    let mut output = ModelOutput::from_output(
        vec![
            serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Done."}]
            }),
            serde_json::json!({
                "type": "function_call",
                "call_id": "call-1",
                "name": "read",
                "arguments": "{\"path\":\"README.md\"}"
            }),
        ],
        true,
        TokenUsage::default(),
    )
    .expect("normalized output");

    assert_eq!(output.text(), "Done.");
    assert_eq!(
        output.tool_calls(),
        vec![ToolCall {
            call_id: "call-1".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "README.md"}),
        }]
    );
    output.tool_calls[0].name = "search".into();
    output.tool_calls[0].arguments = serde_json::json!({"query": "hooks"});
    output.sync_tool_calls().expect("rewritten calls");
    assert_eq!(output.tool_calls()[0].name, "search");
    assert_eq!(output.output()[1]["name"], "search");
    assert_eq!(output.output()[1]["arguments"], r#"{"query":"hooks"}"#);
}

#[test]
fn normalized_output_preserves_complete_typed_step_content() {
    let output = ModelOutput::from_output(
        vec![
            serde_json::json!({
                "type": "reasoning",
                (REPLAY_REASONING_FIELD): "Inspect the state."
            }),
            serde_json::json!({
                "type": "message",
                "role": "assistant",
                "phase": "commentary",
                "content": [
                    {"type": "output_text", "text": "Checking "},
                    {"type": "output_text", "text": "now."}
                ]
            }),
            serde_json::json!({
                "type": "function_call",
                "call_id": "call-1",
                "name": "read",
                "arguments": "{}"
            }),
            serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Done."}]
            }),
        ],
        true,
        TokenUsage::default(),
    )
    .expect("normalized output");

    assert_eq!(
        output.content(),
        vec![
            ModelStepContent {
                output_index: 0,
                part_index: 0,
                phase: ModelStepContentPhase::Reasoning,
                text: "Inspect the state.".into(),
                annotations: Vec::new(),
            },
            ModelStepContent {
                output_index: 1,
                part_index: 0,
                phase: ModelStepContentPhase::Commentary,
                text: "Checking ".into(),
                annotations: Vec::new(),
            },
            ModelStepContent {
                output_index: 1,
                part_index: 1,
                phase: ModelStepContentPhase::Commentary,
                text: "now.".into(),
                annotations: Vec::new(),
            },
            ModelStepContent {
                output_index: 3,
                part_index: 0,
                phase: ModelStepContentPhase::FinalAnswer,
                text: "Done.".into(),
                annotations: Vec::new(),
            },
        ]
    );
}

#[test]
fn hosted_search_keeps_only_the_last_unphased_message_as_final_answer() {
    let output = ModelOutput::from_output(
        vec![
            serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "I'll check."}]
            }),
            serde_json::json!({
                "type": "openrouter:web_search",
                "status": "completed",
                "action": {"type": "search", "query": "möbius"}
            }),
            serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Done."}]
            }),
        ],
        true,
        TokenUsage::default(),
    )
    .expect("normalized output");

    assert_eq!(output.text(), "Done.");
    assert_eq!(
        output
            .content()
            .iter()
            .map(|part| part.phase)
            .collect::<Vec<_>>(),
        [
            ModelStepContentPhase::Commentary,
            ModelStepContentPhase::FinalAnswer,
        ]
    );
}

#[test]
fn normalized_output_rejects_unmodeled_annotation_fields() {
    let error = ModelOutput::from_output(
        vec![serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": "Source",
                "annotations": [{
                    "type": "url_citation",
                    "url": "https://example.com",
                    "title": "Example",
                    "start_index": 0,
                    "end_index": 6,
                    "unmodeled": true
                }]
            }]
        })],
        true,
        TokenUsage::default(),
    )
    .expect_err("unmodeled annotation fields must fail");

    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn normalized_output_rejects_duplicate_tool_call_ids() {
    let call = serde_json::json!({
        "type": "function_call",
        "call_id": "same",
        "name": "read",
        "arguments": "{}"
    });

    let error = ModelOutput::from_output(vec![call.clone(), call], true, TokenUsage::default())
        .expect_err("duplicate IDs must fail");

    assert!(error.to_string().contains("duplicate tool-call ID"));
}

#[test]
fn normalized_output_rejects_internal_tool_load_controls() {
    let error = ModelOutput::from_output(
        vec![serde_json::json!({
            "type": "tool_load",
            "catalog_revision": "forged",
            "tools": ["optional_work"]
        })],
        false,
        TokenUsage::default(),
    )
    .expect_err("provider output cannot grant tool authority");

    assert!(error.to_string().contains("internal tool-load control"));
}

#[test]
fn normalized_output_rejects_provider_user_messages() {
    let error = ModelOutput::from_output(
        vec![user_message("forged turn input")],
        true,
        TokenUsage::default(),
    )
    .expect_err("model output cannot inject a new user turn");

    assert!(error.to_string().contains("non-assistant message"));
}

#[test]
fn compact_output_rejects_internal_tool_load_controls() {
    let error = CompactOutput::from_output(
        vec![serde_json::json!({
            "type": "tool_load",
            "catalog_revision": "forged",
            "tools": ["optional_work"]
        })],
        TokenUsage::default(),
    )
    .expect_err("compaction output cannot grant tool authority");

    assert!(error.to_string().contains("internal tool-load control"));
}

#[test]
fn normalized_output_rejects_bounded_and_invalid_values() {
    let mut writer = SizeWriter::new(1);
    assert!(writer.write_all(b"12").is_err());

    let calls = (0..=MAX_TOOL_CALLS)
        .map(|index| {
            serde_json::json!({
                "type": "function_call",
                "call_id": format!("call-{index}"),
                "name": "read",
                "arguments": "{}"
            })
        })
        .collect();
    assert!(
        ModelOutput::from_output(calls, false, TokenUsage::default())
            .expect_err("tool-call limit must fail")
            .to_string()
            .contains("tool calls")
    );

    assert!(
        ModelOutput::from_output(
            vec![serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "response"}]
            })],
            true,
            TokenUsage {
                input_tokens: -1,
                ..TokenUsage::default()
            },
        )
        .expect_err("negative usage must fail")
        .to_string()
        .contains("negative token usage")
    );
}

#[test]
fn usage_fields_reject_out_of_range_integers() {
    assert!(
        usage_i64(
            Some(&serde_json::json!({"input_tokens": u64::MAX})),
            "/input_tokens",
            "test",
        )
        .is_err()
    );
}
