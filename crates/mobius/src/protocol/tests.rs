use serde_json::json;

use super::*;

#[test]
fn hosted_executions_are_bounded_once_at_construction() {
    assert!(HostedCallId::new(" ").is_err());
    assert!(HostedCallId::new(&"x".repeat(crate::MAX_IDENTIFIER_BYTES + 1)).is_err());
    let call_id = HostedCallId::new("call").expect("call ID");
    assert!(HostedToolExecution::new(call_id.clone(), "", None, None, []).is_err());
    let oversized = "u".repeat(MAX_HOSTED_SOURCE_BYTES + 1);
    let execution = HostedToolExecution::new(
        call_id,
        "web_search",
        None,
        None,
        std::iter::once(oversized.as_str())
            .chain(std::iter::once(""))
            .chain(std::iter::repeat_n(
                "https://example.com",
                MAX_HOSTED_SOURCES + 1,
            )),
    )
    .expect("execution");
    assert_eq!(execution.sources.len(), MAX_HOSTED_SOURCES);
}

#[test]
fn model_events_keep_typed_correlation_and_web_search_fields() {
    let session_id: SessionId = "session-1".parse().expect("session ID");
    let turn_id = Arc::from("turn-1");
    let model_step_id = Arc::from("step-1");
    let delta = ModelEvent::CommentaryDelta("Checking".into())
        .into_event(
            session_id.share(),
            Arc::clone(&turn_id),
            Arc::clone(&model_step_id),
        )
        .expect("frontend delta");
    let search = ModelEvent::WebSearchCompleted {
        call_id: "search-1".into(),
        action: WebSearchAction::Search {
            queries: vec!["möbius framework".into(), "möbius gateway".into()],
        },
    }
    .into_event(
        session_id.share(),
        Arc::clone(&turn_id),
        Arc::clone(&model_step_id),
    )
    .expect("frontend search");
    let EventMsg::AssistantContentDelta(delta_ids) = &delta else {
        panic!("expected a content delta");
    };
    let EventMsg::WebSearchEnd(search_ids) = &search else {
        panic!("expected a web search result");
    };
    assert_eq!(delta_ids.session_id, session_id);
    assert!(Arc::ptr_eq(&delta_ids.turn_id, &turn_id));
    assert!(Arc::ptr_eq(&delta_ids.model_step_id, &model_step_id));
    assert_eq!(search_ids.session_id, session_id);
    assert!(Arc::ptr_eq(&search_ids.turn_id, &turn_id));
    assert!(Arc::ptr_eq(&search_ids.model_step_id, &model_step_id));

    assert_eq!(
        serde_json::to_value(delta).expect("serialize delta"),
        json!({
            "type": "assistant_content_delta",
            "session_id": "session-1",
            "turn_id": "turn-1",
            "model_step_id": "step-1",
            "delta": "Checking",
            "phase": "commentary"
        })
    );
    assert_eq!(
        serde_json::to_value(search).expect("serialize web search"),
        json!({
            "type": "web_search_end",
            "session_id": "session-1",
            "turn_id": "turn-1",
            "model_step_id": "step-1",
            "call_id": "search-1",
            "action": {
                "type": "search",
                "queries": ["möbius framework", "möbius gateway"]
            }
        })
    );
}

#[test]
fn session_configured_has_a_stable_wire_shape() {
    let event = EventMsg::SessionConfigured(Arc::new(SessionConfiguredEvent {
        session_id: "session-1".parse().expect("session ID"),
        context: Arc::new(SessionContext {
            owner_id: "bot-1".into(),
            tenant_id: Some("tenant-1".into()),
            user_id: Some("user-1".into()),
            user_name: Some("Ada".into()),
            workspace_id: Some("workspace-1".into()),
            workspace_label: Some("Project One".into()),
            origin_label: Some("routine".into()),
        }),
        model: ModelChangedEvent {
            route: "default".into(),
            model: "test-model".into(),
            reasoning_effort: Some("high".into()),
            model_context_window: Some(128_000),
        },
    }));

    assert_eq!(
        serde_json::to_value(event).expect("serialize session event"),
        json!({
            "type": "session_configured",
            "session_id": "session-1",
            "context": {
                "owner_id": "bot-1",
                "tenant_id": "tenant-1",
                "user_id": "user-1",
                "user_name": "Ada",
                "workspace_id": "workspace-1",
                "workspace_label": "Project One",
                "origin_label": "routine"
            },
            "model": {
                "route": "default",
                "model": "test-model",
                "reasoning_effort": "high",
                "model_context_window": 128_000
            }
        })
    );
}

#[test]
fn oversized_serialized_message_is_rejected_at_ingress() {
    let text = "x".repeat(MAX_MESSAGE_BYTES);
    let submission = MessageSubmission {
        author: MessageAuthor::User,
        text: text.clone(),
        attachments: Vec::new(),
        reply: Some(MessageReply {
            target: MessageTarget {
                checkpoint_sequence: 1,
                batch_item_count: std::num::NonZeroUsize::MIN,
            },
            text,
        }),
        requested_delivery: None,
        target_turn_id: None,
    };
    let error = submission
        .validate(crate::backend::session_files::session_file_limits())
        .expect_err("oversized message");
    assert!(error.to_string().contains("serialized size limit"));
}

#[test]
fn message_submission_has_one_typed_payload() {
    let submission = Submission {
        id: "input-1".into(),
        op: Op::Message {
            message: MessageSubmission {
                author: MessageAuthor::User,
                text: "hello".into(),
                attachments: Vec::new(),
                reply: Some(MessageReply {
                    target: MessageTarget {
                        checkpoint_sequence: 7,
                        batch_item_count: std::num::NonZeroUsize::new(2).unwrap(),
                    },
                    text: "earlier".into(),
                }),
                requested_delivery: Some(ActiveMessageDelivery::Queue),
                target_turn_id: Some("turn-1".into()),
            },
        },
    };

    assert_eq!(
        serde_json::to_value(submission).expect("serialize input"),
        json!({
            "id": "input-1",
            "op": {
                "type": "message",
                "message": {
                    "author": {"type": "user"},
                    "text": "hello",
                    "attachments": [],
                    "reply": {
                        "target": {
                            "checkpoint_sequence": 7,
                            "batch_item_count": 2
                        },
                        "text": "earlier"
                    },
                    "requested_delivery": "queue",
                    "target_turn_id": "turn-1"
                }
            }
        })
    );
}

#[test]
fn conversation_events_use_turn_and_text_wire_names() {
    let events = [
        EventMsg::TurnStarted(TurnStartedEvent {
            idle: false,
            turn_id: "turn-1".into(),
            model_context_window: Some(128_000),
        }),
        EventMsg::Message(MessageEvent {
            author: MessageAuthor::User,
            delivery: MessageDelivery::Turn,
            text: "hello".into(),
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        }),
        EventMsg::TurnComplete(TurnCompleteEvent {
            idle: false,
            turn_id: "turn-1".into(),
        }),
    ];

    assert_eq!(
        serde_json::to_value(events).expect("serialize conversation events"),
        json!([
            {
                "type": "turn_started",
                "turn_id": "turn-1",
                "model_context_window": 128_000
            },
            {
                "type": "message",
                "author": {"type": "user"},
                "delivery": "turn",
                "text": "hello",
                "attachments": [],
                "reply": null,
                "message_target": null
            },
            {
                "type": "turn_complete",
                "turn_id": "turn-1"
            }
        ])
    );
}

#[test]
fn token_usage_overflow_does_not_partially_update_the_total() {
    let mut total = TokenUsage {
        input_tokens: 7,
        total_tokens: i64::MAX,
        ..TokenUsage::default()
    };
    let original = total;

    assert!(
        total
            .checked_add(&TokenUsage {
                input_tokens: 1,
                total_tokens: 1,
                ..TokenUsage::default()
            })
            .is_none()
    );
    assert_eq!(total, original);
}
