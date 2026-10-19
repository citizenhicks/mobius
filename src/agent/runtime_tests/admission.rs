//! Durable message admission and provenance tests.

use super::*;

#[tokio::test]
async fn failed_turn_start_preserves_the_complete_durable_queue() {
    let workspace = tempfile::tempdir().expect("workspace");
    let database = workspace.path().join("checkpoints.sqlite3");
    let checkpoints = Arc::new(SqliteCheckpoint::new(&database).expect("checkpoint store"));
    let mut checkpoint = Checkpoint::empty("failed-start-queue");
    checkpoint.session_context = test_session_context().into();
    checkpoint.model_route = Some("test".into());
    checkpoint.pending_messages = vec![
        queued_user_message("first", "first body", QueuedMessageBoundary::Turn),
        queued_user_message("second", "second body", QueuedMessageBoundary::Queue),
        queued_user_message("third", "third body", QueuedMessageBoundary::Queue),
    ];
    checkpoints
        .save(checkpoint.clone(), Vec::new(), None)
        .await
        .expect("seed queued input");
    rusqlite::Connection::open(&database)
        .expect("open database")
        .execute_batch(
            "CREATE TRIGGER reject_turn_start BEFORE UPDATE ON sessions
             WHEN json_type(NEW.latest_checkpoint_json, '$.active_execution') = 'object'
             BEGIN SELECT RAISE(ABORT, 'injected turn start save failure'); END;",
        )
        .expect("inject turn-start failure");
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([scripted_message("must not run")])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        Arc::<SqliteCheckpoint>::clone(&checkpoints),
        "failed-start-queue",
        "test",
        Arc::<ScriptedModel>::clone(&model),
    ))
    .await
    .expect("resume queued input");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while agent.next_event().await.is_some() {}
    })
    .await
    .expect("failed runner closes its events");
    let saved = checkpoints
        .load("failed-start-queue")
        .await
        .expect("load")
        .expect("checkpoint");
    assert_eq!(saved.pending_messages, checkpoint.pending_messages);
    assert!(saved.active_execution.is_none());
    assert!(model.inputs.lock().expect("model inputs").is_empty());
}
