//! Recorder agent runtime tests.

use super::*;
use crate::agent::recorder::RECORDER_COMMAND_CAPACITY;
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
    let mut checkpoint = Checkpoint::empty(session_id);
    checkpoint.session_context.owner_id = "test-bot".into();
    checkpoint
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
