use super::*;
use crate::backend::model::{message_input, tool_output};
use crate::protocol::{ATTACHMENT_CONTEXT_MARKER, MessageAuthor, MessageEvent};

fn message(text: &str, delivery: MessageDelivery) -> Value {
    message_input(&MessageEvent {
        author: MessageAuthor::User,
        delivery,
        text: text.into(),
        attachments: Vec::new(),
        reply: None,
        message_target: None,
    })
    .expect("typed user input")
}

fn call(call_id: &str, name: &str) -> Value {
    serde_json::json!({
        "type": "function_call", "call_id": call_id, "name": name, "arguments": "{}"
    })
}

#[test]
fn handoff_limit_counts_utf8_bytes_and_reports_overage() {
    for notes in ["a".repeat(21_000), "🦀".repeat(5_250)] {
        validate_notes(&notes).expect("21,000 UTF-8 bytes fit");
        let error = validate_notes(&(notes + &"a".repeat(50))).expect_err("over the limit");
        assert_eq!(
            error.to_string(),
            "tool error: handoff notes contain 21050 UTF-8 bytes; maximum 21000. Remove at least 50 bytes. Checkpoint unchanged."
        );
    }
    assert_eq!(
        validate_notes(" \n\t")
            .expect_err("blank notes")
            .to_string(),
        "tool error: handoff notes must contain non-whitespace text. Checkpoint unchanged."
    );
}

#[test]
fn fresh_input_preserves_the_active_request_steers_attachments_and_mixed_batch() {
    let active = message("finish the task", MessageDelivery::Turn);
    let attachment = internal_user_message(ATTACHMENT_CONTEXT_MARKER, "active upload");
    let steer = message("preserve this correction", MessageDelivery::Steer);
    let later_steer = message("and verify twice", MessageDelivery::Steer);
    let later_attachment = internal_user_message(ATTACHMENT_CONTEXT_MARKER, "newer upload");
    let batch = vec![
        call("work", "read_file"),
        call("reset", "new_context"),
        tool_output("reset", "reset requested", false),
        internal_user_message(REQUEST, ""),
        tool_output("work", "result that arrived after the checkpoint", false),
    ];
    let mut input = vec![
        message("previous task", MessageDelivery::Turn),
        internal_user_message(ATTACHMENT_CONTEXT_MARKER, "old upload"),
        active.clone(),
        attachment.clone(),
        call("old-work", "read_file"),
        tool_output("old-work", "old output now recoverable from history", false),
        steer.clone(),
        internal_user_message(NOTES, "stale projection"),
        internal_user_message(WARNING, "handoff soon"),
    ];
    input.extend(batch.clone());
    input.extend([later_steer.clone(), later_attachment.clone()]);

    let fresh = fresh_input(&input, "reset").expect("fresh context");
    let mut expected = vec![active, attachment, steer];
    expected.extend(batch.into_iter().filter(|item| !is_control(item)));
    expected.extend([later_steer, later_attachment]);

    assert_eq!(fresh, expected);
    assert_eq!(tool_complete_boundaries(&fresh).last(), Some(&fresh.len()));
}

#[test]
fn fresh_input_uses_the_latest_batch_when_tool_call_ids_repeat() {
    let active = message("current task", MessageDelivery::Turn);
    let current_call = call("reset", "new_context");
    let current_result = tool_output("reset", "current reset", false);
    let input = vec![
        message("previous task", MessageDelivery::Turn),
        call("reset", "new_context"),
        tool_output("reset", "previous reset", false),
        serde_json::json!({"role": "assistant", "content": "old work"}),
        active.clone(),
        serde_json::json!({"role": "assistant", "content": "intervening work"}),
        current_call.clone(),
        current_result.clone(),
        internal_user_message(REQUEST, ""),
    ];

    assert_eq!(
        fresh_input(&input, "reset").expect("latest request"),
        vec![active, current_call, current_result]
    );
}

#[test]
fn fresh_input_rejects_an_incomplete_mixed_tool_batch() {
    let input = vec![
        message("current task", MessageDelivery::Turn),
        call("pending", "read_file"),
        call("reset", "new_context"),
        tool_output("reset", "reset requested", false),
        internal_user_message(REQUEST, ""),
    ];

    assert!(fresh_input(&input, "reset").is_err());
}
