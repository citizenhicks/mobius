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
    checkpoint.session_context.owner_id = "test-bot".into();
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
    assert_eq!(before.submission.op, user_op("before"));
    assert_eq!(
        inbox
            .recv()
            .await
            .expect("submission after cutoff")
            .submission
            .op,
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
    let (events, mut receiver) =
        crate::agent::recorder::RecorderIngress::spawn(store, "session".into());
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
    let (events, mut receiver) =
        crate::agent::recorder::RecorderIngress::spawn(checkpoints, "missing".into());

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
    let (events, mut receiver) =
        crate::agent::recorder::RecorderIngress::spawn(store, "session".into());
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
    let (events, mut receiver) =
        crate::agent::recorder::RecorderIngress::spawn(store, "session".into());
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
    let (events, mut receiver) =
        crate::agent::recorder::RecorderIngress::spawn(store, "session".into());
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
    let (events, mut receiver) =
        crate::agent::recorder::RecorderIngress::spawn(store, "session".into());
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
    let error = sink(ModelEvent::TextDelta("late".into()))
        .await
        .expect_err("closed sink");

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
            match sink(ModelEvent::TextDelta("late".into())).await {
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
async fn interrupted_response_delivers_accepted_text_before_aborting_and_rejects_late_text() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3")).expect("store"),
    );
    let retained = Arc::new(Mutex::new(None));
    let started = Arc::new(Notify::new());
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        checkpoints,
        "interrupted-stream",
        "test",
        Arc::new(BlockingRetainingModel {
            sink: Arc::clone(&retained),
            started: Arc::clone(&started),
            release: Arc::new(Notify::new()),
        }),
    ))
    .await
    .expect("agent");
    agent.sender().submit(user_op("hello")).expect("submit");
    started.notified().await;
    let turn_id = loop {
        if let EventMsg::ModelStepStarted(step) = agent.next_event().await.expect("event").msg {
            break step.turn_id;
        }
    };
    let sink = retained.lock().expect("sink lock").clone().expect("sink");
    sink(ModelEvent::TextDelta("first".into()))
        .await
        .expect("first");
    sink(ModelEvent::TextDelta("pending".into()))
        .await
        .expect("admission");
    agent
        .sender()
        .submit(Op::Interrupt {
            turn_id: turn_id.to_string(),
        })
        .expect("interrupt");
    let text = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut text = String::new();
        loop {
            match agent.next_event().await.expect("event").msg {
                EventMsg::AssistantContentDelta(delta) => text.push_str(&delta.delta),
                EventMsg::TurnAborted(_) => return text,
                _ => {}
            }
        }
    })
    .await
    .expect("aborted turn");
    assert_eq!(text, "firstpending");
    assert_eq!(
        sink(ModelEvent::TextDelta("late".into()))
            .await
            .expect_err("closed sink")
            .to_string(),
        "agent stopped: model event sink closed"
    );
}

#[tokio::test]
async fn recorder_weak_ingress_does_not_keep_the_recorder_alive() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let (events, _receiver) =
        crate::agent::recorder::RecorderIngress::spawn(checkpoints, "session".into());
    let weak = Arc::downgrade(&events);

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
    let (events, mut receiver) =
        crate::agent::recorder::RecorderIngress::spawn(checkpoints, "missing".into());

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
    let (events, mut receiver) =
        crate::agent::recorder::RecorderIngress::spawn(checkpoints, "session".into());

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

#[tokio::test]
async fn recorder_save_shares_the_checkpoint_without_copying_its_payload() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let mut checkpoint = test_checkpoint("session");
    std::sync::Arc::make_mut(&mut checkpoint.context).push(std::sync::Arc::new(
        serde_json::json!({"role": "user", "content": "retained context"}),
    ));
    let copies = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    checkpoint.clone_count.0 = Some(Arc::clone(&copies));
    let (recorder, _receiver) =
        crate::agent::recorder::RecorderIngress::spawn(checkpoints.clone(), "session".into());

    let checkpoint = Arc::new(checkpoint);
    recorder
        .save(Arc::clone(&checkpoint), &[], None, Vec::new())
        .await
        .expect("save snapshot");

    assert_eq!(copies.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&checkpoint), 1);
    assert_eq!(
        checkpoints.load("session").await.expect("load").as_ref(),
        Some(checkpoint.as_ref())
    );
}

#[test]
fn explicit_checkpoint_mutation_copies_only_when_a_snapshot_is_retained() {
    let copies = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut checkpoint = test_checkpoint("session");
    checkpoint.clone_count.0 = Some(Arc::clone(&copies));
    let mut live = super::super::LiveCheckpoint(Arc::new(checkpoint));
    live.make_mut().sequence = 1;
    assert_eq!(copies.load(std::sync::atomic::Ordering::Relaxed), 0);

    let snapshot = Arc::clone(&live.0);
    live.make_mut().sequence = 2;
    assert_eq!(snapshot.sequence, 1);
    assert_eq!(live.sequence, 2);
    assert_eq!(copies.load(std::sync::atomic::Ordering::Relaxed), 1);
    live.make_mut().sequence = 3;
    assert_eq!(copies.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn fast_model_burst_coalesces_without_losing_text_before_completion() {
    struct BurstModel;
    const DELTAS: usize = RECORDER_COMMAND_CAPACITY * 4;
    impl Model for BurstModel {
        fn respond<'a>(
            &'a self,
            _request: ModelRequest<'a>,
            events: ModelEventSink,
        ) -> BoxFuture<'a, Result<ModelOutput>> {
            Box::pin(async move {
                for _ in 0..DELTAS {
                    events(ModelEvent::TextDelta("x".into())).await?;
                }
                Ok(scripted_message(&"x".repeat(DELTAS)))
            })
        }
    }
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        checkpoints,
        "fast-model-burst",
        "test",
        Arc::new(BurstModel),
    ))
    .await
    .expect("agent");
    agent.sender().submit(user_op("hello")).expect("submit");
    let mut deltas = 0;
    let mut text = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            match agent.next_event().await.expect("agent event").msg {
                EventMsg::AssistantContentDelta(delta) => {
                    deltas += 1;
                    text.push_str(&delta.delta);
                }
                EventMsg::TurnComplete(_) => break,
                EventMsg::Error(error) => panic!("burst aborted: {}", error.message),
                _ => {}
            }
        }
    })
    .await
    .expect("burst completes");
    assert!(deltas < DELTAS);
    assert_eq!(text, "x".repeat(DELTAS));
}

fn stream_delta(step: &str, phase: ModelStepContentPhase, text: &str) -> Event {
    Event {
        submission_id: Some("submission".into()),
        msg: EventMsg::AssistantContentDelta(crate::protocol::AssistantContentDeltaEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: step.into(),
            phase,
            delta: text.into(),
        }),
    }
}

#[tokio::test]
async fn recorder_batches_text_and_flushes_before_phase_step_and_event_boundaries() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3")).expect("store"),
    );
    checkpoints
        .save(&test_checkpoint("session"), &[], None)
        .await
        .expect("session");
    let (recorder, mut receiver) =
        crate::agent::recorder::RecorderIngress::spawn(checkpoints, "session".into());
    let final_answer = ModelStepContentPhase::FinalAnswer;
    recorder
        .record(stream_delta("one", final_answer, "first"))
        .await
        .expect("first");
    assert_eq!(
        receiver.recv().await.expect("immediate first").event,
        stream_delta("one", final_answer, "first")
    );
    recorder
        .record(stream_delta("one", final_answer, "second"))
        .await
        .expect("buffer");
    recorder
        .record(stream_delta("one", final_answer, "third"))
        .await
        .expect("merge");
    assert!(receiver.try_recv().is_err());
    recorder
        .record(stream_delta(
            "one",
            ModelStepContentPhase::Reasoning,
            "reason",
        ))
        .await
        .expect("phase barrier");
    recorder
        .record(stream_delta("two", final_answer, "next"))
        .await
        .expect("step barrier");
    recorder
        .record(stream_delta("two", final_answer, "last"))
        .await
        .expect("buffer");
    recorder
        .record(Event {
            submission_id: None,
            msg: EventMsg::Warning(WarningEvent {
                message: "barrier".into(),
            }),
        })
        .await
        .expect("event barrier");
    let mut texts = Vec::new();
    for _ in 0..4 {
        let EventMsg::AssistantContentDelta(delta) =
            receiver.recv().await.expect("ordered delta").event.msg
        else {
            panic!("delta must precede warning")
        };
        texts.push(delta.delta);
    }
    assert_eq!(texts, ["secondthird", "reason", "next", "last"]);
    assert!(matches!(
        receiver.recv().await.expect("warning").event.msg,
        EventMsg::Warning(_)
    ));
}

#[tokio::test]
async fn recorder_flushes_at_the_stream_byte_boundary_without_splitting_provider_deltas() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3")).expect("store"),
    );
    checkpoints
        .save(&test_checkpoint("session"), &[], None)
        .await
        .expect("session");
    let (recorder, mut receiver) =
        crate::agent::recorder::RecorderIngress::spawn(checkpoints, "session".into());
    let phase = ModelStepContentPhase::FinalAnswer;
    let capped = "界".repeat(16 * 1024 / 3);
    let oversized = "x".repeat(16 * 1024 + 1);
    for text in ["first", capped.as_str(), "next", oversized.as_str()] {
        recorder
            .record(stream_delta("one", phase, text))
            .await
            .expect("admit delta");
    }
    recorder.flush().await.expect("durable barrier");
    for text in ["first", capped.as_str(), "next", oversized.as_str()] {
        assert_eq!(
            receiver.recv().await.expect("ordered delta").event,
            stream_delta("one", phase, text)
        );
    }
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn recorder_flushes_a_quiet_stream_and_reports_buffered_persistence_failure() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3")).expect("store"),
    );
    checkpoints
        .save(&test_checkpoint("session"), &[], None)
        .await
        .expect("session");
    let store: Arc<dyn CheckpointStore> = checkpoints.clone();
    let (recorder, mut receiver) =
        crate::agent::recorder::RecorderIngress::spawn(store, "session".into());
    let phase = ModelStepContentPhase::FinalAnswer;
    recorder
        .record(stream_delta("one", phase, "first"))
        .await
        .expect("first");
    receiver.recv().await.expect("first delivered");
    recorder
        .record(stream_delta("one", phase, "quiet"))
        .await
        .expect("buffer");
    let delivered = tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
        .await
        .expect("timer flush")
        .expect("delta");
    assert_eq!(delivered.event, stream_delta("one", phase, "quiet"));
    checkpoints
        .delete_sessions(&["session".into()])
        .await
        .expect("remove session");
    recorder
        .record(stream_delta("one", phase, "cannot persist"))
        .await
        .expect("bounded admission");
    assert!(recorder.flush().await.is_err());
    assert!(receiver.recv().await.is_none());
}

#[test]
fn execution_completion_moves_fields_and_restores_them_on_overflow() {
    use crate::backend::checkpoint::{ActiveExecution, ExecutionPhase};
    let mut checkpoint = Checkpoint::empty("completion-ownership");
    checkpoint.active_execution = Some(ActiveExecution {
        submission_id: "submission".into(),
        author: crate::protocol::MessageAuthor::User,
        turn_id: "turn".into(),
        started_at_ms: i64::MIN,
        model_calls: 2,
        tool_calls: 3,
        failed_tool_calls: 1,
        usage: TokenUsage::default(),
        next_model_step: 4,
        stop_hook_active: true,
        phase: ExecutionPhase::Completion {
            last_assistant_message: Some("done".into()),
        },
    });
    let before = checkpoint.active_execution.clone();
    let submission_ptr = before.as_ref().expect("active").submission_id.as_ptr();
    let original_ptr = checkpoint
        .active_execution
        .as_ref()
        .expect("active")
        .submission_id
        .as_ptr();
    assert_ne!(submission_ptr, original_ptr);
    checkpoint.execution_stats.run_count = u64::MAX;
    let stats = checkpoint.execution_stats.clone();
    assert!(
        checkpoint
            .finish_execution(ExecutionOutcome::Completed, i64::MAX)
            .is_err()
    );
    assert_eq!(checkpoint.active_execution, before);
    assert_eq!(checkpoint.execution_stats, stats);
    assert_eq!(
        checkpoint
            .active_execution
            .as_ref()
            .expect("restored")
            .submission_id
            .as_ptr(),
        original_ptr
    );
    checkpoint.execution_stats.run_count = 0;
    let record = checkpoint
        .finish_execution(ExecutionOutcome::Completed, i64::MAX)
        .expect("completion");
    assert!(checkpoint.active_execution.is_none());
    assert_eq!(record.submission_id.as_ptr(), original_ptr);
    assert_eq!(record.elapsed_ms, u64::MAX);
    assert_eq!(checkpoint.execution_stats.run_count, 1);
}
