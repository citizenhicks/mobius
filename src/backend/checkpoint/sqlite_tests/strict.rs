use super::*;

#[tokio::test]
async fn checkpoint_version_hard_rejects_the_previous_session_context_shape() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    let mut checkpoint = checkpoint("session");
    assert_eq!(checkpoint.version, 12);
    for version in [9, 11] {
        checkpoint.version = version;
        let error = store
            .save(&checkpoint, &[], None)
            .await
            .expect_err("older checkpoint shape must fail");

        assert!(
            error
                .to_string()
                .contains(&format!("unsupported checkpoint version {version}"))
        );
    }
}

#[test]
fn open_hard_rejects_the_previous_session_context_schema() {
    for version in [6, 7] {
        let workspace = tempfile::tempdir().expect("create workspace");
        let path = workspace.path().join("checkpoints.sqlite3");
        drop(SqliteCheckpoint::new(&path).expect("create current database"));
        let connection = Connection::open(&path).expect("open database");
        connection
            .pragma_update(None, "user_version", version)
            .expect("set older schema version");
        drop(connection);

        let error = SqliteCheckpoint::new(path)
            .err()
            .expect("older session-context schema must fail");

        assert_eq!(
            error.to_string(),
            format!(
                "checkpoint error: unsupported SQLite schema version {version}; expected 8 \
                 (start with a fresh database)"
            )
        );
    }
}

#[test]
fn open_rejects_a_nonempty_unversioned_database() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let path = workspace.path().join("checkpoints.sqlite3");
    let connection = Connection::open(&path).expect("create unversioned database");
    connection
        .execute("CREATE TABLE legacy_state (value TEXT)", [])
        .expect("create legacy schema");
    drop(connection);

    let error = SqliteCheckpoint::new(path)
        .err()
        .expect("nonempty unversioned database must fail");

    assert_eq!(
        error.to_string(),
        "checkpoint error: unversioned SQLite database is not empty; expected schema version \
             8 (start with a fresh database)"
    );
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
        .save(&checkpoint, &[], None)
        .await
        .expect("seed checkpoint");
    let (ready_tx, ready_rx) = oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = tokio::spawn({
        let store = Arc::clone(&store);
        async move {
            store
                .run(move |connection| {
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
    let loaded = timeout(Duration::from_secs(1), store.load("session")).await;
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

#[tokio::test]
async fn save_rejects_a_nonadvancing_sequence() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    let mut checkpoint = checkpoint("session");
    checkpoint.sequence = 2;

    store
        .save(&checkpoint, &[], None)
        .await
        .expect("initial save");

    assert!(store.save(&checkpoint, &[], None).await.is_err());
    let mut older = checkpoint.clone();
    older.sequence = 1;
    assert!(store.save(&older, &[], None).await.is_err());
    assert_eq!(
        store.load("session").await.expect("load checkpoint"),
        Some(checkpoint)
    );
}
