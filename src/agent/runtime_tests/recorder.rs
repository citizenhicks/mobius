//! Recorder agent runtime tests.

use super::*;

#[test]
fn sender_rejects_oversized_input_before_queueing() {
    let (commands, _receiver) = tokio::sync::mpsc::channel(1);
    let sender = AgentSender { commands };

    assert!(
        sender
            .submit(Op::UserInput {
                text: "x".repeat(MAX_USER_INPUT_BYTES + 1),
                attachments: Vec::new(),
            })
            .is_err()
    );
}

#[test]
fn sender_reports_a_full_live_queue_as_busy() {
    let (commands, _receiver) = tokio::sync::mpsc::channel(1);
    let sender = AgentSender { commands };
    sender
        .submit(Op::UserInput {
            text: "first".into(),
            attachments: Vec::new(),
        })
        .expect("fill queue");

    let error = sender
        .submit(Op::UserInput {
            text: "second".into(),
            attachments: Vec::new(),
        })
        .expect_err("queue should be full");

    assert!(matches!(error, Error::Busy(_)));
}

#[tokio::test]
async fn recorder_persists_an_event_before_delivery() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    checkpoints
        .save(&Checkpoint::empty("session"), &[], None)
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
        .save(&Checkpoint::empty("session"), &[], None)
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
        .save(&Checkpoint::empty("session"), &[], None)
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
async fn recorder_flush_fails_instead_of_blocking_on_a_full_delivery_queue() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    checkpoints
        .save(&Checkpoint::empty("session"), &[], None)
        .await
        .expect("initial checkpoint");
    let (events, _receiver) = EventRecorder::spawn(checkpoints, "session".into());

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
                message: "overflow".into(),
            }),
        },
    )
    .expect("queue overflow event");

    let error = tokio::time::timeout(std::time::Duration::from_secs(1), events.flush())
        .await
        .expect("flush must not block")
        .expect_err("full delivery queue must fail");
    assert_eq!(
        error.to_string(),
        "agent stopped: event delivery queue is full"
    );
}
