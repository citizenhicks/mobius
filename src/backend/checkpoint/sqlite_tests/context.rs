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
