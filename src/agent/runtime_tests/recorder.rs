//! Recorder agent runtime tests.

use super::*;
use crate::agent::recorder::{RECORDER_COMMAND_CAPACITY, RECORDER_EVENT_BYTE_BUDGET};
use tokio::sync::Notify;

struct RetainingModel {
    sink: Arc<Mutex<Option<ModelEventSink>>>,
}

struct BlockingRetainingModel {
    sink: Arc<Mutex<Option<ModelEventSink>>>,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl Model for RetainingModel {
    fn respond<'a>(
        &'a self,
        _request: ModelRequest<'a>,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        *self.sink.lock().expect("retained sink lock") = Some(events);
        Box::pin(async { Ok(scripted_message("done")) })
    }
}

impl Model for BlockingRetainingModel {
    fn respond<'a>(
        &'a self,
        _request: ModelRequest<'a>,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        *self.sink.lock().expect("retained sink lock") = Some(events);
        let started = Arc::clone(&self.started);
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            started.notify_one();
            release.notified().await;
            Ok(scripted_message("done"))
        })
    }
}

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

#[tokio::test(flavor = "current_thread")]
async fn recorder_rejects_command_saturation_without_dropping_accepted_events() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    checkpoints
        .save(&test_checkpoint("session"), &[], None)
        .await
        .expect("initial checkpoint");
    let store: Arc<dyn CheckpointStore> = checkpoints;
    let (events, mut receiver) = EventRecorder::spawn(store, "session".into());
    let mut accepted = 0;
    let error = loop {
        match try_send_event(
            &events,
            Event {
                submission_id: None,
                msg: EventMsg::Warning(WarningEvent {
                    message: accepted.to_string(),
                }),
            },
        ) {
            Ok(()) => accepted += 1,
            Err(error) => break error,
        }
    };

    assert_eq!(accepted, RECORDER_COMMAND_CAPACITY);
    assert_eq!(
        error.to_string(),
        "agent stopped: event recorder queue is full"
    );

    let drain = tokio::spawn(async move {
        for _ in 0..accepted {
            receiver.recv().await.expect("accepted event");
        }
    });
    events.flush().await.expect("flush accepted events");
    drain.await.expect("drain accepted events");
}

#[tokio::test(flavor = "current_thread")]
async fn recorder_rejects_event_byte_saturation_without_dropping_accepted_events() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    checkpoints
        .save(&test_checkpoint("session"), &[], None)
        .await
        .expect("initial checkpoint");
    let store: Arc<dyn CheckpointStore> = checkpoints;
    let (events, mut receiver) = EventRecorder::spawn(store, "session".into());
    let message = "x".repeat(RECORDER_EVENT_BYTE_BUDGET / 3);
    let mut accepted = 0;
    let error = loop {
        match try_send_event(
            &events,
            Event {
                submission_id: None,
                msg: EventMsg::Warning(WarningEvent {
                    message: message.clone(),
                }),
            },
        ) {
            Ok(()) => accepted += 1,
            Err(error) => break error,
        }
    };

    assert_eq!(accepted, 2);
    assert_eq!(
        error.to_string(),
        "agent stopped: event recorder queue is full"
    );

    let drain = tokio::spawn(async move {
        for _ in 0..accepted {
            receiver.recv().await.expect("accepted event");
        }
    });
    events.flush().await.expect("flush accepted events");
    drain.await.expect("drain accepted events");
}

#[tokio::test]
async fn retained_model_sink_closes_after_response_completion() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let retained = Arc::new(Mutex::new(None));
    let model = Arc::new(RetainingModel {
        sink: Arc::clone(&retained),
    });
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        checkpoints,
        "retained-sink",
        "test",
        model,
    ))
    .await
    .expect("agent");
    agent.sender().submit(user_op("hello")).expect("submit");
    while !matches!(
        agent.next_event().await.expect("agent event").msg,
        EventMsg::TurnComplete(_)
    ) {}

    let sink = retained
        .lock()
        .expect("retained sink lock")
        .clone()
        .expect("retained model sink");
    let error = sink(ModelEvent::TextDelta("late".into())).expect_err("closed sink");

    assert_eq!(error.to_string(), "agent stopped: model event sink closed");
}

#[tokio::test]
async fn retained_model_sink_closes_when_response_is_cancelled() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let retained = Arc::new(Mutex::new(None));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let model = Arc::new(BlockingRetainingModel {
        sink: Arc::clone(&retained),
        started: Arc::clone(&started),
        release,
    });
    let agent = create_agent(config_with_model(
        workspace.path(),
        checkpoints,
        "cancelled-sink",
        "test",
        model,
    ))
    .await
    .expect("agent");
    agent.sender().submit(user_op("hello")).expect("submit");
    started.notified().await;
    let sink = retained
        .lock()
        .expect("retained sink lock")
        .clone()
        .expect("retained model sink");

    drop(agent);
    let error = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match sink(ModelEvent::TextDelta("late".into())) {
                Ok(()) => tokio::task::yield_now().await,
                Err(error) => break error,
            }
        }
    })
    .await
    .expect("cancelled sink closes");

    assert_eq!(error.to_string(), "agent stopped: model event sink closed");
}

#[tokio::test]
async fn recorder_weak_ingress_does_not_keep_the_recorder_alive() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let (events, _receiver) = EventRecorder::spawn(checkpoints, "session".into());
    let weak = events.downgrade();

    drop(events);

    assert!(weak.upgrade().is_none());
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
