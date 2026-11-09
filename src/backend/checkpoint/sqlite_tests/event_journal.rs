use super::*;

#[tokio::test]
async fn turn_history_keeps_idle_promotion_together_across_storage_pages() {
    use crate::backend::checkpoint::{TURN_PAGE_BATCH_SIZE, event_turn_page};
    use crate::protocol::{TurnCompleteEvent, TurnStartedEvent, WarningEvent};

    let directory = tempfile::tempdir().expect("directory");
    let store = SqliteCheckpoint::new(directory.path().join("promoted-turn.sqlite3"))
        .expect("checkpoint store");
    let session_id = session("session");
    let start = |id: &str, idle| {
        EventMsg::TurnStarted(TurnStartedEvent {
            idle,
            turn_id: id.into(),
            model_context_window: None,
        })
    };
    let complete = |id: &str| {
        EventMsg::TurnComplete(TurnCompleteEvent {
            idle: false,
            turn_id: id.into(),
        })
    };
    let mut events = vec![
        start("prior", false),
        EventMsg::Warning(WarningEvent {
            message: "prior work".into(),
        }),
        complete("prior"),
        start("idle", true),
    ];
    events.extend((0..TURN_PAGE_BATCH_SIZE).map(|_| {
        EventMsg::Warning(WarningEvent {
            message: "idle preparation".into(),
        })
    }));
    events.extend([start("idle", false), complete("idle")]);
    let recorded = store
        .save_with_events(
            ValidatedCheckpoint::new(Arc::new(checkpoint("session"))).unwrap(),
            Vec::new(),
            None,
            events
                .into_iter()
                .map(|msg| TimestampedEvent {
                    recorded_at_ms: 1,
                    event: Event {
                        submission_id: None,
                        msg,
                    },
                })
                .collect(),
        )
        .await
        .expect("save events");
    let page = event_turn_page(&store, &session_id, None)
        .await
        .expect("promoted turn");
    assert_eq!(
        page.latest_sequence,
        recorded.last().expect("last event").sequence
    );
    assert_eq!(
        page.events,
        recorded[3..].iter().rev().cloned().collect::<Vec<_>>()
    );
    assert_eq!(page.next_before_sequence, Some(recorded[3].sequence));
    let prior = event_turn_page(&store, &session_id, page.next_before_sequence)
        .await
        .expect("prior turn");
    assert_eq!(
        prior.events,
        recorded[..3].iter().rev().cloned().collect::<Vec<_>>()
    );
    assert_eq!(prior.next_before_sequence, None);
}

#[tokio::test]
async fn large_tool_output_is_stored_once_and_restored_for_both_event_readers() {
    use crate::protocol::ToolCallEndEvent;

    let session_id = session("session");
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("tool-output.sqlite3");
    let store = SqliteCheckpoint::new(&path).unwrap();
    let state = checkpoint("session");
    let output = "🦀 tool output\n".repeat(1024);
    let event = Event {
        submission_id: Some("submission".into()),
        msg: EventMsg::ToolCallEnd(ToolCallEndEvent {
            turn_id: "turn".into(),
            call_id: "call".into(),
            name: "bash".into(),
            output: output.as_str().into(),
            is_error: false,
        }),
    };
    let item = Arc::new(crate::backend::model::tool_output(
        "call",
        &output.as_str().into(),
        false,
    ));
    let live = store
        .save_with_events(
            ValidatedCheckpoint::new(Arc::new(state)).unwrap(),
            vec![item],
            None,
            vec![TimestampedEvent {
                recorded_at_ms: 1,
                event: event.clone(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(live[0].event, event);
    let connection = Connection::open(&path).unwrap();
    let stored: String = connection
        .query_row("SELECT event_json FROM event_journal", [], |row| row.get(0))
        .unwrap();
    assert!(stored.len() < 512);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored).unwrap()["storage"],
        "tool_output"
    );
    drop(store);
    let store = SqliteCheckpoint::new(&path).unwrap();
    assert_eq!(
        store
            .event_page(
                &session_id,
                EventPageRequest {
                    before_sequence: None,
                    limit: 10
                }
            )
            .await
            .unwrap()
            .events[0]
            .event,
        event
    );
    assert_eq!(
        store.events_after(&session_id, 0, 10).await.unwrap()[0].event,
        event
    );
    connection
        .execute(
            "DELETE FROM transcript_delta WHERE session_id = 'session'",
            [],
        )
        .unwrap();
    assert!(
        store
            .event_page(
                &session_id,
                EventPageRequest {
                    before_sequence: None,
                    limit: 10
                }
            )
            .await
            .is_err()
    );
    assert!(store.events_after(&session_id, 0, 10).await.is_err());
}

#[tokio::test]
async fn transient_notices_are_delivered_but_not_kept() {
    use crate::protocol::{FrontendBlock, FrontendEvent, TRANSIENT_BLOCK_ID_PREFIX};

    let session_id = session("session");
    let workspace = tempfile::tempdir().unwrap();
    let store = SqliteCheckpoint::new(workspace.path().join("notice.sqlite3")).unwrap();
    let event = Event {
        submission_id: None,
        msg: EventMsg::Frontend(FrontendEvent::Render {
            capability: "extensions".into(),
            block: FrontendBlock {
                id: Some(format!("{TRANSIENT_BLOCK_ID_PREFIX}hook-notice-0")),
                title: "PONYTAIL:ULTRA".into(),
                ..Default::default()
            },
        }),
    };
    let live = store
        .save_with_events(
            ValidatedCheckpoint::new(Arc::new(checkpoint("session"))).unwrap(),
            Vec::new(),
            None,
            vec![TimestampedEvent {
                recorded_at_ms: 1,
                event: event.clone(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(live[0].event, event);
    assert!(
        store
            .events_after(&session_id, 0, 10)
            .await
            .unwrap()
            .is_empty()
    );
}
