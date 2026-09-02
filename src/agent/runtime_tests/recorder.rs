//! Recorder agent runtime tests.

use super::*;

fn test_checkpoint(session_id: &str) -> Checkpoint {
    let mut checkpoint = Checkpoint::empty(session_id);
    checkpoint.session_context.bot_id = "test-bot".into();
    checkpoint
}

#[test]
fn sender_rejects_oversized_input_before_queueing() {
    let (sender, _inbox) = submission_channel(1);

    assert!(
        sender
            .submit(user_op("x".repeat(MAX_MESSAGE_BYTES + 1)))
            .is_err()
    );
}

#[test]
fn sender_reports_a_full_live_queue_as_busy() {
    let (sender, _inbox) = submission_channel(1);
    sender.submit(user_op("first")).expect("fill queue");

    let error = sender
        .submit(user_op("second"))
        .expect_err("queue should be full");

    assert!(matches!(error, Error::Busy(_)));
}

#[tokio::test]
async fn submission_cutoff_excludes_later_messages() {
    let (sender, mut inbox) = submission_channel(2);
    sender
        .submit(user_op("before"))
        .expect("submit before cutoff");
    let cutoff = inbox.cutoff().expect("capture cutoff");
    sender
        .submit(user_op("after"))
        .expect("submit after cutoff");

    let before = inbox.recv().await.expect("submission before cutoff");

    assert_eq!(inbox.last_sequence, cutoff);
    assert_eq!(before.op, user_op("before"));
    assert_eq!(
        inbox.recv().await.expect("submission after cutoff").op,
        user_op("after")
    );
}

#[tokio::test]
async fn weak_sender_does_not_keep_the_submission_channel_open() {
    let (sender, mut inbox) = submission_channel(1);
    let weak = sender.downgrade();

    drop(sender);

    assert!(weak.upgrade().is_none());
    assert!(inbox.recv().await.is_none());
}

#[tokio::test]
async fn recorder_persists_an_event_before_delivery() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    checkpoints
        .save(&test_checkpoint("session"), &[], None)
        .await
        .expect("initial checkpoint");
    let store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let (events, mut receiver) = EventRecorder::spawn(store, "session".into());
    let event = Event {
        submission_id: Some("submission".into()),
        msg: EventMsg::Warning(WarningEvent {
            message: "durable".into(),
        }),
    };

    send_event(&events, event.clone())
        .await
        .expect("record event");
    let page = checkpoints
        .event_page(
            "session",
            EventPageRequest {
                before_sequence: None,
                limit: 1,
            },
        )
        .await
        .expect("event page");
    let delivered = receiver.recv().await.expect("recorded event");

    assert_eq!(page.events, vec![delivered]);
    assert_eq!(page.events[0].event, event);
}

#[tokio::test]
async fn recorder_stops_without_delivering_when_persistence_fails() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let (events, mut receiver) = EventRecorder::spawn(checkpoints, "missing".into());

    let error = send_event(
        &events,
        Event {
            submission_id: None,
            msg: EventMsg::Warning(WarningEvent {
                message: "durable".into(),
            }),
        },
    )
    .await
    .expect_err("missing session");

    assert!(matches!(error, Error::Checkpoint(_)));
    assert!(receiver.recv().await.is_none());
}

#[tokio::test]
async fn recorder_flush_waits_for_prior_unacknowledged_events() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    checkpoints
        .save(&test_checkpoint("session"), &[], None)
        .await
        .expect("initial checkpoint");
    let store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let (events, mut receiver) = EventRecorder::spawn(store, "session".into());
    let event = Event {
        submission_id: None,
        msg: EventMsg::Warning(WarningEvent {
            message: "queued".into(),
        }),
    };

    try_send_event(&events, event.clone()).expect("queue event");
    events.flush().await.expect("flush event recorder");

    let page = checkpoints
        .event_page(
            "session",
            EventPageRequest {
                before_sequence: None,
                limit: 1,
            },
        )
        .await
        .expect("event page");
    assert_eq!(page.events[0].event, event);
    assert_eq!(
        receiver.try_recv().expect("delivered event"),
        page.events[0]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn recorder_accepts_a_synchronous_provider_burst() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    checkpoints
        .save(&test_checkpoint("session"), &[], None)
        .await
        .expect("initial checkpoint");
    let store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let (events, mut receiver) = EventRecorder::spawn(store, "session".into());
    let event_count = EVENT_QUEUE_CAPACITY + 1;

    for index in 0..event_count {
        try_send_event(
            &events,
            Event {
                submission_id: None,
                msg: EventMsg::Warning(WarningEvent {
                    message: index.to_string(),
                }),
            },
        )
        .expect("queue burst event");
    }
    let drain = tokio::spawn(async move {
        for _ in 0..event_count {
            receiver.recv().await.expect("delivered burst event");
        }
    });

    events.flush().await.expect("flush burst events");
    drain.await.expect("drain task");
    let page = checkpoints
        .event_page(
            "session",
            EventPageRequest {
                before_sequence: None,
                limit: event_count,
            },
        )
        .await
        .expect("event page");

    assert_eq!(page.events.len(), event_count);
}

#[tokio::test]
async fn recorder_flush_reports_a_prior_unacknowledged_failure() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let (events, mut receiver) = EventRecorder::spawn(checkpoints, "missing".into());

    try_send_event(
        &events,
        Event {
            submission_id: None,
            msg: EventMsg::Warning(WarningEvent {
                message: "queued".into(),
            }),
        },
    )
    .expect("queue event");

    let error = events.flush().await.expect_err("flush should fail");
    assert!(matches!(error, Error::Stopped(_)));
    assert!(receiver.recv().await.is_none());
}

#[tokio::test]
async fn recorder_flush_backpressures_until_ordered_delivery_resumes() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    checkpoints
        .save(&test_checkpoint("session"), &[], None)
        .await
        .expect("initial checkpoint");
    let (events, mut receiver) = EventRecorder::spawn(checkpoints, "session".into());

    for index in 0..EVENT_QUEUE_CAPACITY {
        send_event(
            &events,
            Event {
                submission_id: None,
                msg: EventMsg::Warning(WarningEvent {
                    message: index.to_string(),
                }),
            },
        )
        .await
        .expect("fill delivery queue");
    }
    try_send_event(
        &events,
        Event {
            submission_id: None,
            msg: EventMsg::Warning(WarningEvent {
                message: EVENT_QUEUE_CAPACITY.to_string(),
            }),
        },
    )
    .expect("queue overflow event");

    let flush_events = events.clone();
    let mut flush = tokio::spawn(async move { flush_events.flush().await });
    tokio::task::yield_now().await;
    assert!(!flush.is_finished(), "flush must wait for event delivery");

    let mut delivered = vec![receiver.recv().await.expect("first delivered event")];
    tokio::time::timeout(std::time::Duration::from_secs(1), &mut flush)
        .await
        .expect("draining one event should release flush")
        .expect("flush task")
        .expect("flush event recorder");
    for _ in 0..EVENT_QUEUE_CAPACITY {
        delivered.push(receiver.recv().await.expect("ordered delivered event"));
    }

    for (index, recorded) in delivered.iter().enumerate() {
        let EventMsg::Warning(warning) = &recorded.event.msg else {
            panic!("expected warning event");
        };
        assert_eq!(warning.message, index.to_string());
    }
}
