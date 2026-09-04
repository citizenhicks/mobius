//! Agent input-boundary validation tests.

use crate::agent::validate_submission;
use crate::middleware::session_files::session_file_limits;
use crate::protocol::{
    MAX_MESSAGE_BYTES, MessageAuthor, MessageReply, MessageSubmission, MessageTarget, Op,
    SessionFileReference, Submission,
};

fn attachment(index: usize, size: u64) -> SessionFileReference {
    SessionFileReference {
        id: format!("00000000-0000-0000-0000-{index:012x}"),
        name: format!("{index}.bin"),
        size,
        media_type: "application/octet-stream".into(),
    }
}

fn submission(attachments: Vec<SessionFileReference>) -> Submission {
    Submission {
        id: "submission".into(),
        op: Op::Message {
            message: MessageSubmission {
                author: MessageAuthor::User,
                text: String::new(),
                attachments,
                reply: None,
                requested_delivery: None,
                target_turn_id: None,
            },
        },
    }
}

#[test]
fn advertised_attachment_reference_limit_is_enforced() {
    let limits = session_file_limits();
    let attachments = (1..=limits.max_attachment_references + 1)
        .map(|index| attachment(index, 1))
        .collect();

    let error = validate_submission(&submission(attachments)).expect_err("too many references");

    assert!(
        error
            .to_string()
            .contains(&limits.max_attachment_references.to_string())
    );
}

#[test]
fn advertised_file_size_limit_is_enforced() {
    let limits = session_file_limits();

    let error = validate_submission(&submission(vec![attachment(1, limits.max_file_bytes + 1)]))
        .expect_err("oversized attachment");

    assert!(
        error
            .to_string()
            .contains(&limits.max_file_bytes.to_string())
    );
}

#[test]
fn advertised_session_byte_limit_is_enforced() {
    let limits = session_file_limits();
    let attachment_count = limits.max_session_bytes / limits.max_file_bytes + 1;
    let attachments = (1..=attachment_count)
        .map(|index| {
            attachment(
                usize::try_from(index).expect("attachment index"),
                limits.max_file_bytes,
            )
        })
        .collect();

    let error = validate_submission(&submission(attachments)).expect_err("session quota");

    assert!(
        error
            .to_string()
            .contains(&limits.max_session_bytes.to_string())
    );
}

#[test]
fn quoted_message_size_limit_is_enforced() {
    let input = Submission {
        id: "submission".into(),
        op: Op::Message {
            message: MessageSubmission {
                author: MessageAuthor::User,
                text: "reply".into(),
                attachments: Vec::new(),
                reply: Some(MessageReply {
                    target: MessageTarget {
                        checkpoint_sequence: 1,
                        batch_item_count: 1,
                    },
                    text: "x".repeat(MAX_MESSAGE_BYTES + 1),
                }),
                requested_delivery: None,
                target_turn_id: None,
            },
        },
    };

    let error = validate_submission(&input).expect_err("oversized quote");

    assert!(error.to_string().contains("quoted message"));
}

#[test]
fn empty_quoted_message_is_rejected() {
    let mut input = submission(Vec::new());
    let Op::Message { message } = &mut input.op else {
        unreachable!("message submission")
    };
    message.text = "reply".into();
    message.reply = Some(MessageReply {
        target: MessageTarget {
            checkpoint_sequence: 1,
            batch_item_count: 1,
        },
        text: String::new(),
    });

    let error = validate_submission(&input).expect_err("empty quote");

    assert!(error.to_string().contains("quoted message cannot be empty"));
}
