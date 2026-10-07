use super::*;

fn with_context(session: &str, items: &[Value]) -> Checkpoint {
    let mut state = checkpoint(session);
    state.context = Arc::new(items.iter().cloned().map(Arc::new).collect());
    state
}

#[tokio::test]
async fn context_rows_append_without_rewriting_previous_items() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("context.sqlite3");
    let store = SqliteCheckpoint::new(&path).unwrap();
    let mut state = with_context(
        "session",
        &[json!({"text":"first"}), json!({"text":"second"})],
    );
    store.save(&state, &[], None).await.unwrap();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE context_writes (operation TEXT NOT NULL);
         CREATE TRIGGER context_insert AFTER INSERT ON context_items BEGIN
             INSERT INTO context_writes VALUES ('insert'); END;
         CREATE TRIGGER context_update AFTER UPDATE ON context_items BEGIN
             INSERT INTO context_writes VALUES ('update'); END;
         CREATE TRIGGER context_delete AFTER DELETE ON context_items BEGIN
             INSERT INTO context_writes VALUES ('delete'); END;",
        )
        .unwrap();
    let json: String = connection
        .query_row("SELECT latest_checkpoint_json FROM sessions", [], |r| {
            r.get(0)
        })
        .unwrap();
    let header: Value = serde_json::from_str(&json).unwrap();
    assert!(header.get("context").is_none());
    assert!(
        serde_json::to_value(&state)
            .unwrap()
            .get("context")
            .is_some()
    );
    assert_eq!(Arc::strong_count(&state.context), 1);
    assert_eq!(Arc::strong_count(&state.context[0]), 1);
    state.sequence += 1;
    state.catalog_visible = false;
    store.save(&state, &[], None).await.unwrap();
    let changes: i64 = connection
        .query_row("SELECT count(*) FROM context_writes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(changes, 0);
    state.sequence += 1;
    Arc::make_mut(&mut state.context).push(Arc::new(json!({"text":"third"})));
    store.save(&state, &[], None).await.unwrap();
    let inserted: i64 = connection
        .query_row(
            "SELECT count(*) FROM context_writes WHERE operation='insert'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(inserted, 1);
    assert_eq!(store.load("session").await.unwrap(), Some(state.clone()));
    state.sequence += 1;
    state.context_epoch += 1;
    state.context = Arc::new(vec![Arc::new(json!({"summary":"all three"}))]);
    store.save(&state, &[], None).await.unwrap();
    let removed: i64 = connection
        .query_row(
            "SELECT count(*) FROM context_writes WHERE operation='delete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(removed, 3);
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM context_items", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(store);
    assert_eq!(
        SqliteCheckpoint::new(&path)
            .unwrap()
            .load("session")
            .await
            .unwrap(),
        Some(state)
    );
}

#[tokio::test]
async fn same_epoch_accepts_equal_new_arcs_but_rejects_changed_or_truncated_prefixes() {
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteCheckpoint::new(directory.path().join("context.sqlite3")).unwrap();
    let mut state = with_context(
        "session",
        &[json!({"text":"first"}), json!({"text":"second"})],
    );
    store.save(&state, &[], None).await.unwrap();
    state.sequence += 1;
    state.context = Arc::new(
        state
            .context
            .iter()
            .map(|item| Arc::new((**item).clone()))
            .collect(),
    );
    store
        .save(&state, &[], None)
        .await
        .expect("equal values do not require identical allocations");
    let durable = state.clone();
    state.sequence += 1;
    Arc::make_mut(&mut Arc::make_mut(&mut state.context)[0])["text"] = json!("edited");
    assert!(
        store
            .save(&state, &[], None)
            .await
            .unwrap_err()
            .to_string()
            .contains("prefix changed")
    );
    state.context = Arc::new(Vec::new());
    assert!(
        store
            .save(&state, &[], None)
            .await
            .unwrap_err()
            .to_string()
            .contains("prefix was truncated")
    );
    assert_eq!(store.load("session").await.unwrap(), Some(durable));
}

#[tokio::test]
async fn context_validation_survives_other_sessions_and_fresh_store_handles() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("context.sqlite3");
    let store = SqliteCheckpoint::new(&path).unwrap();
    let mut one = with_context("one", &[json!("one")]);
    let two = with_context("two", &[json!("two")]);
    store.save(&one, &[], None).await.unwrap();
    store.save(&two, &[], None).await.unwrap();
    one.sequence += 1;
    store.save(&one, &[], None).await.unwrap();
    let other = SqliteCheckpoint::new(&path).unwrap();
    one.sequence += 1;
    Arc::make_mut(&mut one.context).push(Arc::new(json!("another")));
    other.save(&one, &[], None).await.unwrap();
    one.sequence += 1;
    // The first connection's sequence hint is stale after the independent writer.
    store.save(&one, &[], None).await.unwrap();
    one.sequence += 1;
    one.context = Arc::new(vec![Arc::new(json!("changed")), Arc::new(json!("another"))]);
    assert!(
        other
            .save(&one, &[], None)
            .await
            .unwrap_err()
            .to_string()
            .contains("prefix changed")
    );
    assert_eq!(store.load("two").await.unwrap(), Some(two));
}

#[tokio::test]
async fn failed_event_rolls_back_context_header_receipt_and_transcript_together() {
    use crate::backend::checkpoint::{QueuedMessage, QueuedMessageBoundary};
    use crate::protocol::{MessageAuthor, MessageDelivery, MessageEvent, WarningEvent};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("context.sqlite3");
    let store = SqliteCheckpoint::new(&path).unwrap();
    let original = with_context("session", &[json!("old context")]);
    store.save(&original, &[], None).await.unwrap();
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_event BEFORE INSERT ON event_journal BEGIN SELECT RAISE(ABORT, 'injected event failure'); END;").unwrap();
    let mut next = original.clone();
    next.sequence += 1;
    next.context_epoch += 1;
    next.context = Arc::new(vec![Arc::new(json!("rewritten context"))]);
    next.pending_messages.push(
        QueuedMessage::new(
            "messages",
            "new-input",
            QueuedMessageBoundary::Turn,
            MessageEvent {
                author: MessageAuthor::User,
                delivery: MessageDelivery::Turn,
                text: "queued".into(),
                attachments: Vec::new(),
                reply: None,
                message_target: None,
            },
        )
        .unwrap(),
    );
    let result = store
        .save_with_events(
            Arc::new(next.clone()),
            vec![Arc::new(json!("transcript addition"))],
            None,
            vec![TimestampedEvent {
                recorded_at_ms: 1,
                event: Event {
                    submission_id: None,
                    msg: EventMsg::Warning(WarningEvent {
                        message: "fail".into(),
                    }),
                },
            }],
        )
        .await;
    assert!(result.is_err());
    assert_eq!(store.load("session").await.unwrap(), Some(original));
    assert!(
        !store
            .message_accepted("session", "new-input")
            .await
            .unwrap()
    );
    for table in ["transcript_delta", "event_journal"] {
        assert_eq!(
            connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    connection
        .execute_batch("DROP TRIGGER reject_event")
        .unwrap();
    store
        .save(&next, &[], None)
        .await
        .expect("failed transaction did not advance hint or durable context");
    assert_eq!(store.load("session").await.unwrap(), Some(next));
}

#[tokio::test]
async fn fork_persists_its_context_and_deletion_cascades_context_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("context.sqlite3");
    let store = SqliteCheckpoint::new(&path).unwrap();
    let parent = with_context("parent", &[json!("parent")]);
    store.save(&parent, &[], None).await.unwrap();
    let child = with_context(
        "child",
        &[json!("selected prefix"), json!("fork instructions")],
    );
    store.fork("parent", 0, &child).await.unwrap();
    assert_eq!(store.load("child").await.unwrap(), Some(child.clone()));
    let transcript = store
        .transcript_page(
            "child",
            TranscriptPageRequest {
                before_sequence: None,
                max_batches: 10,
            },
        )
        .await
        .unwrap();
    assert!(
        transcript.batches[0]
            .items
            .iter()
            .eq(child.context.iter().map(AsRef::as_ref))
    );
    assert!(store.delete_sessions(&["parent".into()]).await.unwrap());
    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM context_items", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn loading_rejects_missing_misindexed_and_retired_context_rows() {
    for corruption in [
        "DELETE FROM context_items WHERE item_index=0",
        "UPDATE context_items SET item_index=4 WHERE item_index=0",
        "UPDATE context_items SET epoch=1 WHERE item_index=0",
        "UPDATE sessions SET context_count=0",
        "UPDATE sessions SET context_epoch=1",
        "UPDATE sessions SET latest_checkpoint_json=json_set(latest_checkpoint_json,'$.context',json('[]'))",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("context.sqlite3");
        let store = SqliteCheckpoint::new(&path).unwrap();
        store
            .save(&with_context("session", &[json!("stored")]), &[], None)
            .await
            .unwrap();
        Connection::open(&path)
            .unwrap()
            .execute_batch(corruption)
            .unwrap();
        assert!(
            store.load("session").await.is_err(),
            "accepted {corruption}"
        );
    }
}

#[tokio::test]
async fn partial_context_append_failure_leaves_the_committed_prefix_and_hint_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("context.sqlite3");
    let store = SqliteCheckpoint::new(&path).unwrap();
    let mut state = with_context("session", &[json!("original")]);
    store.save(&state, &[], None).await.unwrap();
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_second_append BEFORE INSERT ON context_items WHEN NEW.item_index=2 BEGIN SELECT RAISE(ABORT, 'injected partial append failure'); END;").unwrap();
    state.sequence += 1;
    Arc::make_mut(&mut state.context).extend([
        Arc::new(json!("first append")),
        Arc::new(json!("second append")),
    ]);
    assert!(store.save(&state, &[], None).await.is_err());
    assert_eq!(
        connection
            .query_row("SELECT latest_sequence FROM sessions", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM context_items", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    connection
        .execute_batch("DROP TRIGGER reject_second_append")
        .unwrap();
    store
        .save(&state, &[], None)
        .await
        .expect("retry against unchanged cached/durable prefix");
    assert_eq!(store.load("session").await.unwrap(), Some(state));
}

#[tokio::test]
async fn cancelled_save_caller_does_not_interrupt_the_started_atomic_worker() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("context.sqlite3");
    let store = Arc::new(SqliteCheckpoint::new(&path).unwrap());
    let mut state = with_context("session", &[json!("original")]);
    store.save(&state, &[], None).await.unwrap();
    state.sequence += 1;
    Arc::make_mut(&mut state.context).push(Arc::new(json!("durable after cancellation")));
    let blocker = Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let writing = {
        let store = Arc::clone(&store);
        let state = state.clone();
        tokio::spawn(async move { store.save(&state, &[], None).await })
    };
    timeout(Duration::from_secs(2), async {
        loop {
            let worker_started = store.idle_connection.lock().unwrap().is_none();
            if worker_started {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("blocking worker takes cached connection");
    writing.abort();
    assert!(writing.await.unwrap_err().is_cancelled());
    blocker.execute_batch("COMMIT").unwrap();
    timeout(Duration::from_secs(2), async {
        loop {
            let sequence: i64 = blocker
                .query_row("SELECT latest_sequence FROM sessions", [], |r| r.get(0))
                .unwrap();
            if sequence == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("started worker completes its transaction");
    assert_eq!(store.load("session").await.unwrap(), Some(state));
}

#[tokio::test]
async fn externally_recreated_session_invalidates_matching_identity_and_sequence_hints() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("context.sqlite3");
    let first = SqliteCheckpoint::new(&path).unwrap();
    let second = SqliteCheckpoint::new(&path).unwrap();
    let mut old = with_context("session", &[json!("old identity")]);
    first.save(&old, &[], None).await.unwrap();
    second.delete_sessions(&["session".into()]).await.unwrap();
    let replacement = with_context("session", &[json!("replacement identity")]);
    second.save(&replacement, &[], None).await.unwrap();
    old.sequence += 1;
    Arc::make_mut(&mut old.context).push(Arc::new(json!("stale append")));
    assert!(
        first
            .save(&old, &[], None)
            .await
            .unwrap_err()
            .to_string()
            .contains("prefix changed")
    );
    assert_eq!(first.load("session").await.unwrap(), Some(replacement));
}

#[tokio::test]
async fn alternating_live_sessions_never_read_cached_context_payloads() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    let directory = tempfile::tempdir().unwrap();
    let store = SqliteCheckpoint::new(directory.path().join("context.sqlite3")).unwrap();
    let mut one = with_context("one", &[json!({"text": "a".repeat(16_384)})]);
    let mut two = with_context("two", &[json!({"text": "b".repeat(16_384)})]);
    store.save(&one, &[], None).await.unwrap();
    store.save(&two, &[], None).await.unwrap();
    store
        .run(|connection, _| {
            connection.authorizer(Some(|context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Read {
                        table_name: "context_items",
                        column_name: "item_json"
                    }
                ) {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }))?;
            Ok(())
        })
        .await
        .unwrap();
    for _ in 0..4 {
        for state in [&mut one, &mut two] {
            state.sequence += 1;
            Arc::make_mut(&mut state.context).push(Arc::new(json!("new item")));
            store
                .save(state, &[], None)
                .await
                .expect("warm save must not read prior payloads");
        }
    }
    assert_eq!(Arc::strong_count(&one.context[0]), 1);
    drop(one);
    two.sequence += 1;
    store.save(&two, &[], None).await.unwrap();
    store
        .run(|_, contexts| {
            assert!(
                !contexts.contains_key("one"),
                "closed context must release its weak hint"
            );
            assert!(contexts.contains_key("two"));
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn cold_context_validation_uses_one_ordered_payload_query() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let directory = tempfile::tempdir().unwrap();
    let store = SqliteCheckpoint::new(directory.path().join("context.sqlite3")).unwrap();
    let mut state = with_context(
        "session",
        &(0..64).map(|i| json!({"index":i})).collect::<Vec<_>>(),
    );
    store.save(&state, &[], None).await.unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&reads);
    store
        .run(move |connection, _| {
            connection.authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Read {
                        table_name: "context_items",
                        column_name: "item_json"
                    }
                ) {
                    observed.fetch_add(1, Ordering::Relaxed);
                }
                Authorization::Allow
            }))?;
            Ok(())
        })
        .await
        .unwrap();
    state.sequence += 1;
    state.context = Arc::new(
        state
            .context
            .iter()
            .map(|item| Arc::new((**item).clone()))
            .collect(),
    );
    store.save(&state, &[], None).await.unwrap();
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    store
        .run(|connection, _| {
            let statement = connection.prepare_cached(context_store::CONTEXT_PREFIX_SQL)?;
            assert_eq!(
                statement.get_status(rusqlite::StatementStatus::Run),
                1,
                "one execution must validate the complete prefix"
            );
            Ok(())
        })
        .await
        .unwrap();
    // A changed value is still rejected; hints never replace content validation on a miss.
    state.sequence += 1;
    Arc::make_mut(&mut Arc::make_mut(&mut state.context)[63])["index"] = json!("changed");
    assert!(
        store
            .save(&state, &[], None)
            .await
            .unwrap_err()
            .to_string()
            .contains("prefix changed")
    );
}

#[tokio::test]
async fn compaction_and_deletion_reclaim_free_database_pages() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("context.sqlite3");
    let store = SqliteCheckpoint::new(&path).unwrap();
    store
        .run(|connection, _| {
            assert_eq!(
                connection.query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0))?,
                2
            );
            assert_eq!(
                connection.query_row("PRAGMA journal_size_limit", [], |r| r.get::<_, i64>(0))?,
                67_108_864
            );
            assert_eq!(
                connection.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))?,
                2
            );
            Ok(())
        })
        .await
        .unwrap();
    let mut state = with_context(
        "session",
        &(0..32)
            .map(|_| json!({"text":"x".repeat(16_384)}))
            .collect::<Vec<_>>(),
    );
    store.save(&state, &[], None).await.unwrap();
    let observer = Connection::open(&path).unwrap();
    observer
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    let before = std::fs::metadata(&path).unwrap().len();
    state.sequence += 1;
    state.context_epoch += 1;
    state.context = Arc::new(vec![Arc::new(json!({"summary":"compacted"}))]);
    store.save(&state, &[], None).await.unwrap();
    observer
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() < before);
    assert_eq!(
        observer
            .query_row("PRAGMA freelist_count", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(store.load("session").await.unwrap(), Some(state));
    assert!(store.delete_sessions(&["session".into()]).await.unwrap());
    assert_eq!(
        observer
            .query_row("PRAGMA freelist_count", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}
