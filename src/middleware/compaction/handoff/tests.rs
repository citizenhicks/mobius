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

fn shared(input: Vec<Value>) -> Vec<Arc<Value>> {
    input.into_iter().map(Arc::new).collect()
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
        tool_output("reset", &"reset requested".into(), false),
        internal_user_message(REQUEST, ""),
        serde_json::json!({"type":"function_call_output", "call_id":"work", "output":[
            {"type":"input_text", "text":"result that arrived after the checkpoint"},
            {"type":"input_image", "image":{"file":{"id":"screen", "name":"screen.png", "size":100, "media_type":"image/png"}, "width":10, "height":10, "detail":"high"}}
        ]}),
    ];
    let mut input = vec![
        message("previous task", MessageDelivery::Turn),
        internal_user_message(ATTACHMENT_CONTEXT_MARKER, "old upload"),
        active.clone(),
        attachment.clone(),
        call("old-work", "read_file"),
        tool_output(
            "old-work",
            &"old output now recoverable from history".into(),
            false,
        ),
        steer.clone(),
        internal_user_message(NOTES, "stale projection"),
        internal_user_message(NOTES, "old checkpoint"),
    ];
    input.extend(batch.clone());
    input.extend([later_steer.clone(), later_attachment.clone()]);

    let input = shared(input);
    let fresh = retained_input(input.as_slice().into()).expect("fresh context");
    let mut expected = vec![active, attachment, steer];
    expected.extend(batch.into_iter().filter(|item| !is_control(item)));
    expected.extend([later_steer, later_attachment]);

    assert_eq!(fresh, shared(expected));
    assert!(
        fresh
            .iter()
            .all(|retained| input.iter().any(|original| Arc::ptr_eq(retained, original)))
    );
    assert_eq!(
        tool_complete_boundaries(fresh.iter().map(AsRef::as_ref)).last(),
        Some(&fresh.len())
    );
}

#[test]
fn partial_checkpoint_retains_every_unsummarized_parallel_result() {
    let input = shared(vec![
        message("old task", MessageDelivery::Turn),
        serde_json::json!({"role":"assistant", "content":"old answer"}),
        call("a", "read_file"),
        call("b", "read_file"),
        tool_output("a", &"first large result".into(), false),
        tool_output("b", &"second large result".into(), false),
        message("continue", MessageDelivery::Turn),
    ]);
    let retained = retained_input_until(input.as_slice().into(), 2).expect("partial reset");
    assert_eq!(retained, input[2..]);
}
