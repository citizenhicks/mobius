use super::*;

#[tokio::test]
async fn event_journal_sequences_and_pages_normalized_events() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    store
        .save(&checkpoint("session"), &[], None)
        .await
        .expect("save session");

    for (recorded_at_ms, message) in [(10, "first"), (20, "second")] {
        store
            .append_event(
                "session",
                recorded_at_ms,
                &Event {
                    submission_id: None,
                    msg: EventMsg::Warning(crate::protocol::WarningEvent {
                        message: message.into(),
                    }),
                },
            )
            .await
            .expect("append event");
    }

    let newest = store
        .event_page(
            "session",
            EventPageRequest {
                before_sequence: None,
                limit: 1,
            },
        )
        .await
        .expect("newest event");
    let older = store
        .event_page(
            "session",
            EventPageRequest {
                before_sequence: newest.next_before_sequence,
                limit: 1,
            },
        )
        .await
        .expect("older event");

    assert_eq!(newest.events[0].sequence, 2);
    assert_eq!(newest.latest_sequence, 2);
    assert_eq!(newest.events[0].recorded_at_ms, 20);
    assert_eq!(newest.next_before_sequence, Some(2));
    assert_eq!(older.events[0].sequence, 1);
    assert_eq!(older.next_before_sequence, None);
}

#[tokio::test]
async fn event_turn_page_keeps_long_turns_whole() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    store
        .save(&checkpoint("session"), &[], None)
        .await
        .expect("save session");

    for event in [
        EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "older".into(),
            model_context_window: None,
        }),
        EventMsg::Warning(crate::protocol::WarningEvent {
            message: "older work".into(),
        }),
    ] {
        store
            .append_event(
                "session",
                0,
                &Event {
                    submission_id: None,
                    msg: event,
                },
            )
            .await
            .expect("append older turn event");
    }
    for index in 0..100 {
        store
            .append_event(
                "session",
                index,
                &Event {
                    submission_id: None,
                    msg: EventMsg::Warning(crate::protocol::WarningEvent {
                        message: format!("older work {index}"),
                    }),
                },
            )
            .await
            .expect("append long older turn");
    }
    store
        .append_event(
            "session",
            100,
            &Event {
                submission_id: None,
                msg: EventMsg::TurnAborted(TurnAbortedEvent {
                    turn_id: "older".into(),
                    reason: "interrupted".into(),
                }),
            },
        )
        .await
        .expect("complete older turn");
    store
        .append_event(
            "session",
            101,
            &Event {
                submission_id: None,
                msg: EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: "latest".into(),
                    model_context_window: None,
                }),
            },
        )
        .await
        .expect("start latest turn");
    store
        .append_event(
            "session",
            102,
            &Event {
                submission_id: None,
                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: "latest".into(),
                }),
            },
        )
        .await
        .expect("complete latest turn");
    store
        .append_event(
            "session",
            103,
            &Event {
                submission_id: None,
                msg: EventMsg::Warning(crate::protocol::WarningEvent {
                    message: "between turns".into(),
                }),
            },
        )
        .await
        .expect("append inter-turn metadata");

    let latest = event_turn_page(&store, "session", None)
        .await
        .expect("load latest turn");
    let older = event_turn_page(&store, "session", latest.next_before_sequence)
        .await
        .expect("load older turn");

    assert!(matches!(
        latest.into_chronological().as_slice(),
        [
            JournalEvent {
                event: Event {
                    msg: EventMsg::TurnStarted(started),
                    ..
                },
                ..
            },
            JournalEvent {
                event: Event {
                    msg: EventMsg::TurnComplete(completed),
                    ..
                },
                ..
            }
        ] if started.turn_id == "latest" && completed.turn_id == "latest"
    ));
    assert_eq!(older.events.len(), 103);
    assert_eq!(older.next_before_sequence, None);
}

#[tokio::test]
async fn transient_controls_advance_sequence_without_entering_history() {
    use crate::protocol::FrontendEvent;
    use crate::protocol::SessionContext;
    use crate::protocol::SessionResumeRequestedEvent;

    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    store
        .save(&checkpoint("session"), &[], None)
        .await
        .expect("save session");
    let events = [
        EventMsg::Warning(crate::protocol::WarningEvent {
            message: "durable".into(),
        }),
        EventMsg::SessionResumeRequested(SessionResumeRequestedEvent {
            session_id: "session".into(),
            context: SessionContext::default(),
        }),
        EventMsg::Frontend(FrontendEvent::Picker {
            title: "Choose".into(),
            options: Vec::new(),
        }),
        EventMsg::Frontend(FrontendEvent::Preview {
            id: "preview".into(),
            title: "Preview".into(),
            subtitle: String::new(),
            page_id: "preview:latest".into(),
            update: crate::protocol::FrontendPreviewUpdate::Replace,
            events: Vec::new(),
            next: None,
        }),
        EventMsg::Frontend(FrontendEvent::Widget {
            capability: "test".into(),
            item: crate::protocol::FrontendWidget {
                id: "status".into(),
                slot: crate::protocol::FrontendSlot::Header,
                text: "Current".into(),
                tone: crate::protocol::FrontendTone::Neutral,
                symbol: None,
                icon_only: false,
                progress: None,
                content: None,
                action: None,
            },
        }),
        EventMsg::Frontend(FrontendEvent::RemoveWidget {
            capability: "test".into(),
            id: "status".into(),
        }),
    ];
    for (index, msg) in events.into_iter().enumerate() {
        store
            .append_event(
                "session",
                i64::try_from(index).expect("timestamp"),
                &Event {
                    submission_id: None,
                    msg,
                },
            )
            .await
            .expect("append event");
    }

    let page = store
        .event_page(
            "session",
            EventPageRequest {
                before_sequence: None,
                limit: 10,
            },
        )
        .await
        .expect("event page");

    assert_eq!(page.latest_sequence, 6);
    assert_eq!(
        page.events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        [1]
    );
}
