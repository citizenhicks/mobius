use super::{Client, PendingMessage, PendingSelection};
use mobius::SessionId;
use mobius::protocol::{
    Event, EventMsg, MessageAuthor, MessageDelivery, MessageDeltaEvent, MessageEvent,
};

fn pending(key: &str, text: &str, attachments: &[&str]) -> PendingMessage {
    PendingMessage {
        draft_key: key.into(),
        text: text.into(),
        attachments: attachments.iter().map(|id| (*id).into()).collect(),
        files: Vec::new(),
        reply: None,
    }
}

#[test]
fn only_the_current_session_request_can_change_the_selected_chat() {
    let current = SessionId::new("new-chat").unwrap();
    let old = SessionId::new("old-chat").unwrap();
    let client = Client {
        selection: Some(PendingSelection {
            request_id: "latest".into(),
            session_id: Some(current.share()),
        }),
        ..Default::default()
    };
    assert!(client.accepts_session("latest", &current));
    assert!(!client.accepts_session("old-request", &current));
    assert!(!client.accepts_session("latest", &old));
    assert!(!Client::default().accepts_session("cancelled", &current));
}

#[test]
fn drafts_are_scoped_to_the_gateway_and_clear_only_on_durable_message_confirmation() {
    let mut client = Client {
        endpoint: "wss://one.example".into(),
        ..Default::default()
    };
    let key = client.draft_key("chat");
    client.messages.insert(
        "submission".into(),
        pending(&key, "keep this draft", &["attachment"]),
    );
    let delta = Event {
        submission_id: Some("submission".into()),
        msg: EventMsg::MessageDelta(MessageDeltaEvent {
            text: "partial".into(),
        }),
    };
    assert!(client.attachment_pending(&key, "attachment"));
    assert!(!client.attachment_pending(&key, "newer-attachment"));
    assert!(!client.attachment_pending("another-gateway", "attachment"));
    assert!(client.confirmed_message(&delta).is_none());
    assert!(client.pending_submission("submission"));
    let accepted = Event {
        submission_id: Some("submission".into()),
        msg: EventMsg::Message(MessageEvent {
            author: MessageAuthor::User,
            delivery: MessageDelivery::Turn,
            text: "keep this draft".into(),
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        }),
    };
    let confirmed = client
        .confirmed_message(&accepted)
        .expect("confirmed message");
    assert!(!client.attachment_pending(&key, "attachment"));
    assert_eq!(confirmed.draft_key, key);
    assert_eq!(confirmed.text, "keep this draft");
    client.messages.insert(
        "rejected".into(),
        pending(&key, "retry", &["newer-attachment"]),
    );
    let rejected = Event {
        submission_id: Some("rejected".into()),
        msg: EventMsg::SubmissionRejected(mobius::protocol::SubmissionRejectedEvent {
            message: "retry later".into(),
        }),
    };
    assert!(client.confirmed_message(&rejected).is_none());
    assert!(!client.attachment_pending(&key, "newer-attachment"));
    client.endpoint = "wss://two.example".into();
    assert_ne!(client.draft_key("chat"), key);
    assert!(client.messages.is_empty());
}

#[test]
fn only_certain_failed_sends_release_their_own_draft_and_attachment_protection() {
    let mut client = Client::default();
    let reply = Some((
        "chat".into(),
        mobius::protocol::MessageReply {
            target: mobius::protocol::MessageTarget {
                checkpoint_sequence: 1,
                batch_item_count: std::num::NonZeroUsize::MIN,
            },
            text: "quoted message".into(),
        },
        "Author".into(),
    ));
    for id in ["unsent", "uncertain"] {
        client.messages.insert(
            id.into(),
            PendingMessage {
                reply: reply.clone(),
                ..pending("draft", id, &[id])
            },
        );
    }
    assert!(!client.failed_request("unsent", false));
    assert!(!client.attachment_pending("draft", "unsent"));
    assert!(client.attachment_pending("draft", "uncertain"));
    assert!(!client.failed_request("uncertain", true));
    assert_eq!(client.messages["uncertain"].text, "uncertain");
    assert_eq!(client.messages["uncertain"].reply, reply);
    assert!(client.attachment_pending("draft", "uncertain"));
    client.selection = Some(PendingSelection {
        request_id: "create".into(),
        session_id: None,
    });
    assert!(client.failed_request("create", false));
    assert!(!client.failed_request("old-create", false));
}
