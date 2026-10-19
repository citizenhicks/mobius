use super::*;

#[test]
fn history_frame_bound_rejects_one_oversized_turn() {
    let frame = ServerFrame::new(ServerMessage::SessionHistory {
        request_id: "history".into(),
        session_id: "session".into(),
        records: vec![crate::wire::RecordedEvent {
            sequence: 1,
            recorded_at_ms: 1,
            event: Event {
                submission_id: None,
                msg: EventMsg::Warning(mobius::protocol::WarningEvent {
                    message: "x".repeat(MAX_FRAME_BYTES).into(),
                }),
            },
            stream_metrics: Vec::new(),
            blocks: Vec::new(),
            preview: None,
        }],
        next_before_sequence: None,
    });

    assert!(!encoded_frame_fits(&frame).expect("measure history frame"));
}

#[tokio::test]
async fn rejection_frames_preserve_request_correlation() {
    let (mut writer, reader) = tokio::io::duplex(1024);
    let mut reader = FrameReader::new(reader);
    write_rejection(
        &mut writer,
        "request-7",
        Rejection::new("agent_busy", "busy"),
    )
    .await
    .expect("write rejection");

    let frame = read_frame::<ServerFrame>(&mut reader)
        .await
        .expect("read rejection")
        .expect("frame");

    assert!(matches!(
        frame.message,
        ServerMessage::Rejected { request_id, .. } if request_id.as_ref() == "request-7"
    ));
}
