use super::super::*;
use super::support::model_request;
use crate::backend::model::ToolDefinition;

#[test]
fn continuation_ignores_searchable_inventory_and_resets_on_catalog_change() {
    let known = vec![
        serde_json::json!({"role": "user", "content": "one"}),
        serde_json::json!({"type":"function_call_output", "call_id":"screenshot-1", "output":[{"type":"input_text","text":"before"},{"type":"input_image","media_type":"image/png","data":"immutable-encoded-pixels","detail":"high"}]}),
    ];
    let mut state = SocketState {
        continuation: Some(Continuation {
            response_id: "resp-1".into(),
            known_items: known.len(),
            fingerprint: fingerprint(known.iter()).expect("fingerprint"),
            envelope_fingerprint: envelope_fingerprint("test-model", &model_request(), None, &[])
                .expect("envelope fingerprint"),
        }),
        ..SocketState::default()
    };
    let mut continued = known.clone();
    continued.push(serde_json::json!({"type": "function_call_output"}));
    let envelope = envelope_fingerprint("test-model", &model_request(), None, &[])
        .expect("envelope fingerprint");
    let (response, input) =
        continuation_input(&mut state, (&continued).into(), envelope).expect("continue");
    assert_eq!(response, Some("resp-1"));
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
    assert_eq!(response, Some("resp-1"));
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
