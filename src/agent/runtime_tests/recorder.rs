//! Recorder agent runtime tests.

use super::*;
use crate::agent::recorder::RECORDER_COMMAND_CAPACITY;
use std::time::Duration;
use tokio::sync::Notify;

struct BlockingRetainingModel {
    sink: Arc<Mutex<Option<ModelEventSink>>>,
    started: Arc<Notify>,
    release: Arc<Notify>,
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
    let mut checkpoint = Checkpoint::empty(session_id.parse().expect("session ID"));
    std::sync::Arc::make_mut(&mut checkpoint.session_context).owner_id = "test-bot".into();
    checkpoint
}

#[tokio::test]
async fn recorder_delivers_committed_events_and_statistics_without_publishing_failed_saves() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    checkpoints
        .save(test_checkpoint("session"), Vec::new(), None)
        .await
        .expect("initial checkpoint");
    let store: Arc<dyn CheckpointStore> = Arc::<SqliteCheckpoint>::clone(&checkpoints);
    let (events, mut receiver) = crate::agent::recorder::RecorderIngress::spawn(
        store,
        "session".parse().expect("session ID"),
    );
    let mut stats = events.subscribe_run_stats();
    assert!(
        stats.borrow().is_none(),
        "no lifecycle checkpoint has committed yet"
    );
    let event = Event {
        submission_id: Some("submission".into()),
        msg: EventMsg::Warning(WarningEvent {
            message: "durable".into(),
        }),
    };

    events.record(event.clone()).await.expect("record event");
    let page = checkpoints
        .event_page(
            &"session".parse().expect("session ID"),
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
    assert!(
        stats.borrow().is_none(),
        "ordinary appended events do not publish statistics"
    );
    let mut checkpoint = test_checkpoint("session");
    for count in 1..=2 {
        checkpoint.sequence = count;
        checkpoint.execution_stats.run_count = count;
        events
            .save(
                Arc::new(checkpoint.clone()),
                &[],
                None,
                vec![Event {
                    submission_id: Some("submission".into()),
                    msg: EventMsg::TurnComplete(crate::protocol::TurnCompleteEvent {
                        idle: false,
                        turn_id: "turn".into(),
                    }),
                }],
            )
            .await
            .expect("committed lifecycle checkpoint");
    }
    let first = receiver.recv().await.expect("first receipt");
    let second = receiver.recv().await.expect("second receipt");
    assert_eq!(
        second.sequence,
        first.sequence + 1,
        "statistics add no journal entries"
    );
    assert!(
        receiver.try_recv().is_err(),
        "only the original events were delivered"
    );
    let committed = stats
        .borrow_and_update()
        .clone()
        .expect("coalesced statistics");
    assert_eq!(committed.after_sequence, second.sequence);
    assert_eq!(committed.stats.completed, checkpoint.execution_stats);
    let fresh = events.subscribe_run_stats();
    assert!(
        !fresh.has_changed().unwrap(),
        "new observers start at the latest commit"
    );
    assert_eq!(fresh.borrow().as_ref(), Some(&committed));
    assert!(committed.stats.active.is_none());
    let persisted = checkpoints
        .load(&checkpoint.session_id)
        .await
        .expect("load")
        .expect("checkpoint");
    assert_eq!(persisted.execution_stats, committed.stats.completed);
    let fault = rusqlite::Connection::open(directory.path().join("checkpoints.sqlite3"))
        .expect("fault injector");
    fault.execute_batch("CREATE TRIGGER reject_stats_insert BEFORE INSERT ON sessions BEGIN SELECT RAISE(ABORT, 'injected statistics save failure'); END;
        CREATE TRIGGER reject_stats_update BEFORE UPDATE ON sessions BEGIN SELECT RAISE(ABORT, 'injected statistics save failure'); END;").expect("save fault");
    checkpoint.sequence += 1;
    checkpoint.execution_stats.run_count = 3;
    let failure = events
        .save(
            Arc::new(checkpoint),
            &[],
            None,
            vec![Event {
                submission_id: None,
                msg: EventMsg::TurnComplete(crate::protocol::TurnCompleteEvent {
                    idle: false,
                    turn_id: "failed".into(),
                }),
            }],
        )
        .await
        .expect_err("the atomic write must fail");
    assert!(
        matches!(failure, Error::Sqlite(ref error) if error.to_string().contains("injected statistics save failure")),
        "{failure:?}"
    );
    assert_eq!(stats.borrow().as_ref(), Some(&committed));
    assert!(
        matches!(
            tokio::time::timeout(Duration::from_secs(1), stats.changed()).await,
            Ok(Err(_))
        ),
        "a stopped recorder closes statistics even while its ingress is retained"
    );
    assert!(
        receiver.recv().await.is_none(),
        "the failed recorder closes delivery"
    );
}

#[tokio::test]
async fn recorder_stops_without_delivering_when_persistence_fails() {
    let directory = tempfile::tempdir().expect("checkpoint directory");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let (events, mut receiver) = crate::agent::recorder::RecorderIngress::spawn(
        checkpoints,
        "missing".parse().expect("session ID"),
    );

    let error = events
        .record(Event {
            submission_id: None,
            msg: EventMsg::Warning(WarningEvent {
                message: "durable".into(),
            }),
        })
        .await
        .expect_err("missing session");

    assert!(matches!(error, Error::Checkpoint(_)));
    assert!(receiver.recv().await.is_none());
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
    let mut stats = agent.subscribe_run_stats();
    agent.sender().submit(user_op("hello")).expect("submit");
    started.notified().await;
    let turn_id = loop {
        if let EventMsg::ModelStepStarted(step) = agent.next_event().await.expect("event").msg {
            break Arc::clone(&step.turn_id);
        }
    };
    assert_eq!(
        stats
            .borrow_and_update()
            .as_ref()
            .expect("active statistics")
            .stats
            .active
            .as_ref()
            .expect("active run")
            .model_calls,
        1
    );
    let sink = retained.lock().expect("sink lock").clone().expect("sink");
    sink(ModelEvent::TextDelta("first".into()))
        .await
        .expect("first");
    sink(ModelEvent::TextDelta("pending".into()))
        .await
        .expect("admission");
    assert!(
        !stats.has_changed().expect("observer open"),
        "streamed deltas do not publish statistics"
    );
    agent
        .sender()
        .submit(Op::Interrupt { turn_id })
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
        stats
            .borrow()
            .as_ref()
            .expect("aborted statistics")
            .stats
            .completed
            .aborted_run_count,
        1
    );
    assert!(
        stats
            .borrow()
            .as_ref()
            .expect("aborted statistics")
            .stats
            .active
            .is_none()
    );
    assert_eq!(
        sink(ModelEvent::TextDelta("late".into()))
            .await
            .expect_err("closed sink")
            .to_string(),
        "agent stopped: model event sink closed"
    );
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
    let agent = create_agent(config_with_model(
        workspace.path(),
        checkpoints,
        "fast-model-burst",
        "test",
        Arc::new(BurstModel),
    ))
    .await
    .expect("agent");
    let mut stats = agent.subscribe_run_stats();
    let (sender, mut events) = agent.into_parts();
    sender.submit(user_op("hello")).expect("submit");
    let mut deltas = 0;
    let mut text = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            match events.recv().await.expect("agent event").msg {
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
    assert_eq!(
        stats
            .borrow_and_update()
            .as_ref()
            .expect("completed statistics")
            .stats
            .completed
            .run_count,
        1
    );
    assert!(
        stats
            .borrow()
            .as_ref()
            .expect("completed statistics")
            .stats
            .active
            .is_none()
    );
    drop(sender);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while events.recv().await.is_some() {}
    })
    .await
    .expect("statistics observers do not keep the recorder alive");
    assert!(
        stats.has_changed().is_err(),
        "the statistics writer also closes"
    );
}
