use super::*;

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
        .run(|connection, _cache| {
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
            .save(&next, &[json!({"role": "assistant"}).into()], Some(&record))
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
