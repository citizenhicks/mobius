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
            symbol: None,
            duration_ms: None,
            started_at_ms: None,
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

#[tokio::test]
async fn large_tool_output_is_stored_once_and_restored_for_both_event_readers() {
    use crate::protocol::ToolCallEndEvent;

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
            Arc::new(state),
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
                "session",
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
        store.events_after("session", 0, 10).await.unwrap()[0].event,
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
                "session",
                EventPageRequest {
                    before_sequence: None,
                    limit: 10
                }
            )
            .await
            .is_err()
    );
    assert!(store.events_after("session", 0, 10).await.is_err());
}

#[tokio::test]
async fn unmatched_tool_output_stays_inline_in_current_storage_format() {
    use crate::protocol::ToolCallEndEvent;

    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("unmatched-tool.sqlite3");
    let store = SqliteCheckpoint::new(&path).unwrap();
    let output = "custom output".repeat(1024);
    let event = Event {
        submission_id: None,
        msg: EventMsg::ToolCallEnd(ToolCallEndEvent {
            turn_id: "turn".into(),
            call_id: "call".into(),
            name: "custom".into(),
            output: output.as_str().into(),
            is_error: false,
        }),
    };
    // Same identity with different content must never substitute the transcript payload.
    let item = Arc::new(crate::backend::model::tool_output(
        "call",
        &"different".into(),
        false,
    ));
    store
        .save_with_events(
            Arc::new(checkpoint("session")),
            vec![item],
            None,
            vec![TimestampedEvent {
                recorded_at_ms: 1,
                event: event.clone(),
            }],
        )
        .await
        .unwrap();
    let connection = Connection::open(&path).unwrap();
    let stored: String = connection
        .query_row("SELECT event_json FROM event_journal", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored).unwrap()["storage"],
        "inline"
    );
    assert_eq!(
        store.events_after("session", 0, 10).await.unwrap()[0].event,
        event
    );
    store.append_event("session", 2, &event).await.unwrap();
    assert_eq!(
        store.events_after("session", 1, 10).await.unwrap()[0].event,
        event
    );
}

#[tokio::test]
async fn stored_inline_message_compacts_only_its_own_submission_deltas() {
    use crate::protocol::{MessageAuthor, MessageDelivery, MessageDeltaEvent, MessageEvent};

    let workspace = tempfile::tempdir().unwrap();
    let store = SqliteCheckpoint::new(workspace.path().join("message-deltas.sqlite3")).unwrap();
    store.save(&checkpoint("session"), &[], None).await.unwrap();
    for submission in ["first", "second"] {
        store
            .append_event(
                "session",
                1,
                &Event {
                    submission_id: Some(submission.into()),
                    msg: EventMsg::MessageDelta(MessageDeltaEvent {
                        text: "partial".into(),
                    }),
                },
            )
            .await
            .unwrap();
    }
    store
        .append_event(
            "session",
            2,
            &Event {
                submission_id: Some("first".into()),
                msg: EventMsg::Message(MessageEvent {
                    author: MessageAuthor::User,
                    delivery: MessageDelivery::Turn,
                    text: "complete".into(),
                    attachments: Vec::new(),
                    reply: None,
                    message_target: None,
                }),
            },
        )
        .await
        .unwrap();
    let events = store.events_after("session", 0, 10).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event.submission_id.as_deref(), Some("second"));
    assert!(matches!(events[0].event.msg, EventMsg::MessageDelta(_)));
    assert_eq!(events[1].event.submission_id.as_deref(), Some("first"));
    assert!(matches!(events[1].event.msg, EventMsg::Message(_)));
}

/// Explicit offline bridge: the public Python migrator feeds the actual Rust readers.
#[tokio::test]
#[ignore = "manual migration bridge requires Python 3.11+; uses only disposable state"]
async fn public_migration_restores_historical_tool_events_through_rust_readers() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use crate::protocol::ToolCallEndEvent;

    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/upgrade-portable-compaction.py");
    let python =
        std::env::var_os("MOBIUS_MIGRATION_TEST_PYTHON").unwrap_or_else(|| "python3".into());
    let output = "historical 🦀 output\n".repeat(1024);
    let event = Event {
        submission_id: Some("submission".into()),
        msg: EventMsg::ToolCallEnd(ToolCallEndEvent {
            turn_id: "turn".into(),
            call_id: "call".into(),
            name: "read_file".into(),
            output: output.as_str().into(),
            is_error: false,
        }),
    };
    let item = crate::backend::model::tool_output("call", &output.as_str().into(), false);
    for schema in [11, 12] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoints.sqlite3");
        let payload = serde_json::to_vec(&json!({
            "checkpoint": checkpoint("session"), "event": event, "item": item,
        }))
        .unwrap();
        let mut generator = Command::new(&python)
            .arg("-c")
            .arg(
                r#"import json, pathlib, runpy, sqlite3, sys
from contextlib import closing
m = runpy.run_path(sys.argv[1])
payload = json.load(sys.stdin)
with closing(sqlite3.connect(sys.argv[2])) as db, db:
    m['fixture_schema'](db)
    state = payload['checkpoint']
    m['fixture_checkpoint'](db, state)
    if sys.argv[3] == '12':
        db.execute('ALTER TABLE sessions ADD COLUMN context_epoch INTEGER NOT NULL DEFAULT 0')
        db.execute('ALTER TABLE sessions ADD COLUMN context_count INTEGER NOT NULL DEFAULT 0')
        db.execute(m['CONTEXT_TABLE'])
        del state['context']
        db.execute('UPDATE sessions SET latest_checkpoint_json=?', (json.dumps(state),))
        db.execute('PRAGMA user_version=12')
    db.execute('INSERT INTO transcript_delta(session_id,sequence,items_json) VALUES (?,0,?)',
               ('session', json.dumps([payload['item']])))
    db.execute('INSERT INTO event_journal(session_id,sequence,recorded_at_ms,event_kind,event_json,stream_metrics_json) VALUES (?,1,1,?,?,?)',
               ('session', 'tool_call_end', json.dumps(payload['event']), '[]'))
    db.execute('UPDATE sessions SET latest_event_sequence=1')
"#,
            )
            .arg(&script)
            .arg(&path)
            .arg(schema.to_string())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        generator.stdin.take().unwrap().write_all(&payload).unwrap();
        assert!(generator.wait().unwrap().success());
        let migrated = Command::new(&python)
            .arg(&script)
            .args(["--database"])
            .arg(&path)
            .args(["--apply", "--confirm-stopped", "--backup-dir"])
            .arg(directory.path().join("backups"))
            .output()
            .unwrap();
        assert!(
            migrated.status.success(),
            "migration failed: {:?}",
            migrated.stderr
        );
        let report: serde_json::Value = serde_json::from_slice(&migrated.stdout).unwrap();
        assert_eq!(report["tool_outputs_deduplicated"], 1);
        let store = SqliteCheckpoint::new(&path).unwrap();
        assert!(store.load("session").await.unwrap().is_some());
        assert_eq!(
            store.events_after("session", 0, 10).await.unwrap()[0].event,
            event
        );
        assert_eq!(
            store
                .event_page(
                    "session",
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
    }
}
