#[test]
fn orphaned_call_cannot_be_completed_by_a_synthetic_id() {
    for kind in ["function_call", "tool_search_call"] {
        let rows = [
            serde_json::json!({"role":"user","content":"hello"}),
            serde_json::json!({"type":kind}),
            serde_json::json!({"type":"function_call_output","call_id":"missing-1"}),
        ];
        assert_eq!(super::tool_complete_boundaries(&rows), [1]);
    }
}

#[test]
fn stripped_attachment_only_messages_keep_a_neutral_fork_placeholder() {
    use crate::backend::model::internal_user_message;
    use crate::protocol::{
        MessageAuthor, MessageDelivery, MessageTarget, SessionFileReference, replay_events,
    };

    let typed_user_message = |text: &str, attachments| {
        crate::backend::model::message_input(&MessageEvent {
            author: MessageAuthor::User,
            delivery: MessageDelivery::Turn,
            text: text.into(),
            attachments,
            reply: None,
            message_target: None,
        })
        .expect("typed user message")
    };
    let items = vec![
        typed_user_message(
            "",
            vec![SessionFileReference {
                id: "3d46beff-7e84-46ea-859a-e66b4614a79b".into(),
                name: "photo.png".into(),
                size: 4,
                media_type: "image/png".into(),
            }],
        ),
        internal_user_message(ATTACHMENT_CONTEXT_MARKER, "private blob context"),
    ];

    let mut items = items.into_iter().map(Arc::new).collect();
    strip_attachment_references(&mut items);
    assert_eq!(items.len(), 1);
    let context = [(
        MessageTarget {
            checkpoint_sequence: 1,
            batch_item_count: std::num::NonZeroUsize::MIN,
        },
        items.remove(0),
    )];
    let replayed = replay_events(&context, &"fork".parse().expect("session ID"));

    assert!(matches!(
        replayed.as_slice(),
        [EventMsg::Message(message)]
            if message.text == FORKED_ATTACHMENT_PLACEHOLDER
                && message.attachments.is_empty()
    ));
}

use super::*;
use crate::protocol::{MessageAuthor, MessageDelivery, SessionFileReference};

fn user_message(text: &str) -> Value {
    typed_user_message(text, Vec::new())
}

fn typed_user_message(text: &str, attachments: Vec<SessionFileReference>) -> Value {
    let event = MessageEvent {
        author: MessageAuthor::User,
        delivery: MessageDelivery::Turn,
        text: text.into(),
        attachments,
        reply: None,
        message_target: None,
    };
    serde_json::json!({
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
        MESSAGE_METADATA_FIELD: event,
    })
}

fn internal_user_message(kind: &str, text: &str) -> Value {
    serde_json::json!({
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
        INTERNAL_MESSAGE_FIELD: kind,
    })
}

#[test]
fn replay_emits_repeated_compaction_markers_in_transcript_order() {
    let history = vec![
        user_message("first"),
        internal_user_message(CONTEXT_COMPACTED_MARKER, ""),
        serde_json::json!({"role": "assistant", "content": "answer"}),
        internal_user_message(CONTEXT_COMPACTED_MARKER, ""),
        user_message("second"),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, item)| {
        (
            MessageTarget {
                checkpoint_sequence: index as u64 + 1,
                batch_item_count: std::num::NonZeroUsize::MIN,
            },
            item,
        )
    })
    .collect::<Vec<_>>();

    let replayed = events(&history, &"session".parse().expect("session ID"));

    assert!(matches!(
        replayed.as_slice(),
        [
            EventMsg::Message(first),
            EventMsg::ContextCompacted,
            EventMsg::AssistantMessage(answer),
            EventMsg::ContextCompacted,
            EventMsg::Message(second),
        ] if first.text == "first"
            && answer.content[0].text == "answer"
            && second.text == "second"
    ));
}

#[test]
fn replay_keeps_tool_identity_stable_across_durable_batches() {
    let history = vec![
        (
            MessageTarget {
                checkpoint_sequence: 7,
                batch_item_count: std::num::NonZeroUsize::MIN,
            },
            serde_json::json!({
                "type": "function_call",
                "call_id": "call-1",
                "name": "read_file",
                "arguments": "{}"
            }),
        ),
        (
            MessageTarget {
                checkpoint_sequence: 9,
                batch_item_count: std::num::NonZeroUsize::MIN,
            },
            serde_json::json!({
                "type": "function_call_output",
                "call_id": "call-1",
                "output": [{"type": "input_text", "text": "done", (PROMPT_CACHE_BREAKPOINT_FIELD): true},
                    {"type":"input_image", "image":{"file":{"id":"screen","name":"screen.png","media_type":"image/png","size":100},"width":10,"height":10,"detail":"high"}, (PROMPT_CACHE_BREAKPOINT_FIELD): true}]
            }),
        ),
    ];

    let replayed = events(&history, &"session".parse().expect("session ID"));

    assert!(matches!(
        replayed.as_slice(),
        [EventMsg::ToolCallBegin(begin), EventMsg::ToolCallEnd(end)]
            if &*begin.turn_id == "history-7-1" && end.turn_id == begin.turn_id
                && end.output.0.len() == 2
                && matches!(&end.output.0[1], crate::protocol::ContentPart::Image { image } if image.file.id == "screen")
    ));
}
