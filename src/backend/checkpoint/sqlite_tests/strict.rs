use super::*;

#[test]
fn open_hard_rejects_previous_schema_generations() {
    for version in [6, 7, 8, 9, 10, 11, 12, 13] {
        let workspace = tempfile::tempdir().expect("create workspace");
        let path = workspace.path().join("checkpoints.sqlite3");
        drop(SqliteCheckpoint::new(&path).expect("create current database"));
        let connection = Connection::open(&path).expect("open database");
        connection
            .pragma_update(None, "user_version", version)
            .expect("set older schema version");
        drop(connection);

        let error =
            SqliteCheckpoint::new(path).expect_err("older session-context schema must fail");

        assert_eq!(
            error.to_string(),
            format!(
                "checkpoint error: unsupported SQLite schema version {version}; expected 14 \
                 (start with a fresh database)"
            )
        );
    }
}

#[tokio::test]
async fn load_completes_while_another_connection_holds_a_write_transaction() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("open checkpoint database"),
    );
    let checkpoint = checkpoint("session");
    store
        .save(checkpoint.clone(), Vec::new(), None)
        .await
        .expect("seed checkpoint");
    let (ready_tx, ready_rx) = oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = tokio::spawn({
        let store = Arc::clone(&store);
        async move {
            store
                .run(move |connection, _cache| {
                    let transaction =
                        connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    ready_tx.send(()).expect("signal held transaction");
                    release_rx.recv().expect("release held transaction");
                    transaction.commit()?;
                    Ok(())
                })
                .await
        }
    });

    ready_rx.await.expect("wait for held transaction");
    let loaded = timeout(Duration::from_secs(1), store.load(&session("session"))).await;
    release_tx.send(()).expect("release held transaction");
    writer
        .await
        .expect("join held transaction")
        .expect("commit held transaction");

    assert_eq!(
        loaded
            .expect("reader blocked behind writer")
            .expect("load checkpoint"),
        Some(checkpoint)
    );
}
