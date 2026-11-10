use super::*;

#[test]
fn event_larger_than_replay_budget_is_delivered_without_retaining_its_cache() {
    let (events, mut live) = broadcast::channel(1);
    let mut replay = VecDeque::new();
    let mut bytes = 0;
    let frame = ServerFrame::new(ServerMessage::Error {
        code: "large".into(),
        message: "x".repeat(MAX_REPLAY_BYTES / 2).into(),
        fatal: false,
    });
    assert!(publish_frame(&mut replay, &mut bytes, &events, frame, false).unwrap());
    assert!(replay.is_empty());
    assert_eq!(bytes, 0);
    assert!(live.try_recv().is_ok(), "the live event is still delivered");
}

#[test]
fn replay_is_bounded_by_event_and_cached_bytes() {
    let (events, _) = broadcast::channel(1);
    let mut replay = VecDeque::new();
    let mut replay_bytes = 0;
    let large_message: Arc<str> = "x".repeat(MAX_REPLAY_BYTES / 4).into();
    let first = ServerFrame::new(ServerMessage::Error {
        code: "first".into(),
        message: Arc::clone(&large_message),
        fatal: false,
    });
    let second = ServerFrame::new(ServerMessage::Error {
        code: "second".into(),
        message: large_message,
        fatal: false,
    });

    assert!(
        !publish_frame(&mut replay, &mut replay_bytes, &events, first, true)
            .expect("record first frame")
    );
    assert!(
        publish_frame(&mut replay, &mut replay_bytes, &events, second, true)
            .expect("record second frame")
    );

    assert_eq!(replay.len(), 1);
    assert!(replay_bytes <= MAX_REPLAY_BYTES);
}

fn publish_frame(
    replay: &mut VecDeque<ReplayEntry>,
    replay_bytes: &mut usize,
    events: &broadcast::Sender<SharedFrame>,
    frame: ServerFrame,
    suppress_broadcast: bool,
) -> Result<bool> {
    Ok(record_and_publish(
        replay,
        replay_bytes,
        events,
        ReplayEntry::new(frame)?,
        suppress_broadcast,
    ))
}
