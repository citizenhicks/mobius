use super::*;

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
