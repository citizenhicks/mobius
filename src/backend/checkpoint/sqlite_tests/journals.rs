use super::*;

#[tokio::test]
async fn transcript_page_bounds_batches_and_continues_backward() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    let mut checkpoint = checkpoint("session");
    store
        .save(&checkpoint, &[], None)
        .await
        .expect("save session");
    for sequence in 1..=3 {
        checkpoint.sequence = sequence;
        let item = json!({"sequence": sequence});
        store
            .save(&checkpoint, std::slice::from_ref(&item), None)
            .await
            .expect("append transcript");
    }

    let first = store
        .transcript_page(
            "session",
            TranscriptPageRequest {
                before_sequence: None,
                max_batches: 2,
            },
        )
        .await
        .expect("load first page");
    let second = store
        .transcript_page(
            "session",
            TranscriptPageRequest {
                before_sequence: first.next_before_sequence,
                max_batches: 2,
            },
        )
        .await
        .expect("load second page");

    assert_eq!(
        (
            first
                .batches
                .iter()
                .map(|batch| batch.sequence)
                .collect::<Vec<_>>(),
            first.next_before_sequence,
            second
                .batches
                .iter()
                .map(|batch| batch.sequence)
                .collect::<Vec<_>>(),
            second.next_before_sequence,
        ),
        (vec![3, 2], Some(2), vec![1], None)
    );
    assert!(first.batches.iter().all(|batch| batch.created_at > 0));
}

#[tokio::test]
async fn execution_journal_pages_records_and_updates_catalog_stats() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    let mut checkpoint = checkpoint("session");
    store
        .save(&checkpoint, &[], None)
        .await
        .expect("save session");
    for turn in 1..=3 {
        let record = execution("session", turn);
        checkpoint.sequence = turn;
        checkpoint
            .execution_stats
            .checked_record(&record)
            .expect("record execution stats");
        store
            .save(&checkpoint, &[], Some(&record))
            .await
            .expect("save execution");
    }

    let first = store
        .execution_page(
            "session",
            ExecutionPageRequest {
                before_sequence: None,
                limit: 2,
            },
        )
        .await
        .expect("first execution page");
    let second = store
        .execution_page(
            "session",
            ExecutionPageRequest {
                before_sequence: first.next_before_sequence,
                limit: 2,
            },
        )
        .await
        .expect("second execution page");
    let catalog = store
        .list_sessions_page(SessionPageRequest {
            cursor: None,
            limit: 1,
        })
        .await
        .expect("session catalog");
    let recent = store.recent_executions(2).await.expect("recent executions");

    assert_eq!(
        (
            first
                .executions
                .iter()
                .map(|record| record.turn_id.as_str())
                .collect::<Vec<_>>(),
            first.next_before_sequence,
            second
                .executions
                .iter()
                .map(|record| record.turn_id.as_str())
                .collect::<Vec<_>>(),
            catalog.sessions[0].execution_stats.run_count,
            recent
                .iter()
                .map(|record| record.turn_id.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            vec!["turn-3", "turn-2"],
            Some(2),
            vec!["turn-1"],
            3,
            vec!["turn-3", "turn-2"],
        )
    );
}

#[tokio::test]
async fn execution_insert_failure_rolls_back_checkpoint_and_transcript() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    let original = checkpoint("session");
    store
        .save(&original, &[], None)
        .await
        .expect("save session");
    store
        .run(|connection| {
            connection.execute_batch(
                "CREATE TRIGGER reject_execution
                     BEFORE INSERT ON execution_journal
                     BEGIN
                         SELECT RAISE(ABORT, 'forced execution failure');
                     END;",
            )?;
            Ok(())
        })
        .await
        .expect("install failure trigger");
    let record = execution("session", 1);
    let mut next = original.clone();
    next.sequence = 1;
    next.execution_stats
        .checked_record(&record)
        .expect("record execution stats");

    assert!(
        store
            .save(&next, &[json!({"role": "assistant"})], Some(&record))
            .await
            .is_err()
    );
    assert_eq!(
        store.load("session").await.expect("load session"),
        Some(original)
    );
    assert!(
        store
            .transcript_page(
                "session",
                TranscriptPageRequest {
                    before_sequence: None,
                    max_batches: 1,
                },
            )
            .await
            .expect("load transcript")
            .batches
            .is_empty()
    );
}

#[tokio::test]
async fn event_insert_failure_rolls_back_checkpoint_and_event_batch() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    let original = checkpoint("session");
    store
        .save(&original, &[], None)
        .await
        .expect("save session");
    let mut next = original.clone();
    next.sequence = 1;
    let warning = |recorded_at_ms, message: &str| TimestampedEvent {
        recorded_at_ms,
        event: Event {
            submission_id: None,
            msg: EventMsg::Warning(crate::protocol::WarningEvent {
                message: message.into(),
            }),
        },
    };

    let error = store
        .save_with_events(
            &next,
            &[json!({"role": "assistant"})],
            None,
            &[warning(10, "first"), warning(-1, "invalid")],
        )
        .await
        .expect_err("invalid event must roll back the transaction");
    let saved = store.load("session").await.expect("load session");
    let events = store
        .event_page(
            "session",
            EventPageRequest {
                before_sequence: None,
                limit: 1,
            },
        )
        .await
        .expect("load event page");

    assert!(matches!(error, Error::Checkpoint(_)));
    assert_eq!(
        (saved, events.latest_sequence, events.events),
        (Some(original), 0, Vec::new())
    );
}
