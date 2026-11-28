use super::*;

#[tokio::test]
async fn execution_insert_failure_rolls_back_checkpoint_and_transcript() {
    let session_id = session("session");
    let (_directory, store) = SqliteCheckpoint::temporary();
    let original = checkpoint("session");
    store
        .save(original.clone(), Vec::new(), None)
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
            .save(
                next.clone(),
                [json!({"role": "assistant"}).into()].to_vec(),
                Some(record.clone())
            )
            .await
            .is_err()
    );
    assert_eq!(
        store.load(&session_id).await.expect("load session"),
        Some(original)
    );
    assert!(
        store
            .transcript_page(
                &session_id,
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
async fn transcript_cursor_includes_the_largest_sequence_and_excludes_the_boundary() {
    let (_directory, store) = SqliteCheckpoint::temporary();
    let id = session("session");
    store
        .save(checkpoint("session"), Vec::new(), None)
        .await
        .unwrap();
    store.run(|db, _| {
        db.execute("INSERT INTO transcript_delta(session_id, sequence, items_json) VALUES ('session', ?1, '[]'), ('session', 0, '[]')", [i64::MAX])?;
        Ok(())
    }).await.unwrap();
    let page = async |before_sequence| {
        store
            .transcript_page(
                &id,
                TranscriptPageRequest {
                    before_sequence,
                    max_batches: 1,
                },
            )
            .await
    };
    let first = page(None).await.unwrap();
    assert_eq!(first.next_before_sequence, Some(i64::MAX as u64));
    let last = page(first.next_before_sequence).await.unwrap();
    assert_eq!(last.batches[0].sequence, 0);
    assert_eq!(last.next_before_sequence, None);
    assert!(page(Some(0)).await.unwrap().batches.is_empty());
    assert!(page(Some(u64::MAX)).await.is_err());
}
