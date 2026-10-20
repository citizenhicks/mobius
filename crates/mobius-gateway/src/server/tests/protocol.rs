use super::*;
use crate::wire::PROTOCOL_VERSION;
use mobius::SessionId;

#[test]
fn history_frame_bound_rejects_one_oversized_turn() {
    let frame = ServerFrame::new(ServerMessage::SessionHistory {
        request_id: "history".into(),
        session_id: SessionId::new("session").unwrap(),
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

    assert!(
        crate::wire::encode_bounded_frame(&frame)
            .expect("measure history frame")
            .is_none()
    );
}

#[tokio::test]
async fn rejection_frames_preserve_request_correlation() {
    let (mut writer, reader) = tokio::io::duplex(1024);
    let mut reader = FrameReader::new(reader);
    for message in [
        serde_json::json!({"version":PROTOCOL_VERSION,"type":"open_session","request_id":"request-7","session_id":""}),
        serde_json::json!({"version":PROTOCOL_VERSION,"type":"open_session","request_id":"request-7","session_id":"x".repeat(4_097)}),
        serde_json::json!({"version":PROTOCOL_VERSION,"type":"set_provider_endpoint_credential","request_id":"request-7","instance":"fixture","provider":"responses","base_url":"invalid","api_key":"key"}),
    ] {
        let request: ClientFrame = serde_json::from_value(message).expect("correlated bad input");
        let ClientMessage::InvalidRequest {
            request_id,
            message,
        } = request.message
        else {
            panic!("invalid input must not reach the host");
        };
        write_rejection(
            &mut writer,
            request_id,
            Rejection::new("invalid_request", message),
        )
        .await
        .expect("write rejection");
        let frame = read_frame::<ServerFrame>(&mut reader)
            .await
            .expect("read rejection")
            .expect("frame");
        assert!(matches!(frame.message,
            ServerMessage::Rejected { request_id, .. } if request_id.as_ref() == "request-7"
        ));
    }
}
