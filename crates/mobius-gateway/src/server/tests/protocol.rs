use super::*;
use mobius::SessionId;

#[test]
fn history_frame_bound_rejects_one_oversized_turn() {
    let frame = ServerFrame::new(ServerMessage::SessionHistory {
        request_id: "history".into(),
        session_id: SessionId::new("session").unwrap(),
        page: crate::wire::TranscriptPage {
            items: vec![Arc::new(crate::wire::TranscriptItem {
                id: "notice:1:0".into(),
                turn_id: None,
                recorded_at_ms: 1,
                state: crate::wire::TranscriptItemState::Complete,
                terminal: false,
                starts_turn: false,
                resumed: false,
                inherited: false,
                target: None,
                content: crate::wire::TranscriptItemContent::Notice {
                    tone: mobius::protocol::FrontendTone::Warning,
                    text: "x".repeat(MAX_FRAME_BYTES).into(),
                },
            })],
            rows: Vec::new(),
        },
        next: None,
    });

    assert!(
        crate::wire::encode_bounded_frame(&frame)
            .expect("measure history frame")
            .is_none()
    );
}
