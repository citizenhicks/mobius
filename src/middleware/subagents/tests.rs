use std::sync::Arc;
use std::time::Duration;

use super::*;

#[tokio::test]
async fn fork_uses_durable_parent_metadata_even_when_the_parent_stopped() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        crate::backend::checkpoint::sqlite::SqliteCheckpoint::new(
            workspace.path().join("checkpoints.sqlite3"),
        )
        .expect("checkpoint store"),
    );
    let mut parent = Checkpoint::empty("parent");
    parent.session_context.owner_id = "test-bot".into();
    parent.metadata.insert(
        "gateway.chat".into(),
        serde_json::json!({"workspace": "/srv/project"}),
    );
    checkpoints
        .save(&parent, &[], None)
        .await
        .expect("save parent");
    let runtime = RuntimeContext {
        children: crate::agent::ChildAgents::default(),
        sender: crate::agent::test_sender(),
        checkpoints: Arc::clone(&checkpoints),
        session_id: parent.session_id.clone(),
        model_route: "test".into(),
        model: "model".into(),
        approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
        session_context: parent.session_context.clone(),
        metadata: parent.metadata.clone(),
        role: crate::agent::AgentRole::Main,
        frontend: Arc::new(|_| Ok(())),
    };
    let scope = AgentScope::new(&runtime, None).expect("agent scope");
    parent
        .metadata
        .insert("latest".into(), serde_json::json!("updated after startup"));
    parent.sequence += 1;
    checkpoints
        .save(&parent, &[], None)
        .await
        .expect("update parent");

    let result = scope
        .fork(
            "child".into(),
            "/root/child".into(),
            "test".into(),
            None,
            ForkTurns::None,
            "turn".into(),
        )
        .await;
    assert!(matches!(result, Err(Error::Stopped(_))));
    let child = checkpoints
        .load("child")
        .await
        .expect("load child")
        .expect("child checkpoint");
    let identity = AgentIdentity::read("child", &child.metadata).expect("child identity");

    assert_eq!(
        child.metadata.get("gateway.chat"),
        parent.metadata.get("gateway.chat")
    );
    assert_eq!(identity.root_session_id, "parent");
    assert_eq!(child.metadata.get("latest"), parent.metadata.get("latest"));
    assert_eq!(identity.agent_path, "/root/child");
    assert_eq!(identity.depth, 1);
    assert_eq!(
        child.metadata.get(SPAWN_CONTEXT_KEY),
        Some(&Value::String("No context".into()))
    );
}

#[test]
fn forked_context_drops_session_owned_attachment_references() {
    let context = vec![serde_json::json!({
        "role": "user",
        "content": [{"type": "input_text", "text": "inspect"}],
        "_mobius_attachments": [{
            "id": "378b8581-e96c-4413-a138-93e74561cb87",
            "name": "photo.png",
            "size": 1,
            "media_type": "image/png"
        }]
    })];

    let context = context.into_iter().map(Arc::new).collect::<Vec<_>>();
    let fork = fork_context(&context, ForkTurns::All, &BTreeSet::new());

    assert!(fork[0].get("_mobius_attachments").is_none());
}

#[tokio::test]
async fn supervised_lifecycle_outlives_a_cancelled_caller() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
    let caller = tokio::spawn(supervise(async move {
        entered_tx.send(()).expect("signal lifecycle start");
        release_rx.await.expect("release lifecycle");
        completed_tx.send(()).expect("signal lifecycle completion");
        Ok(())
    }));

    entered_rx.await.expect("lifecycle started");
    caller.abort();
    assert!(
        caller
            .await
            .expect_err("caller should be cancelled")
            .is_cancelled(),
        "caller should stop before the lifecycle is released"
    );
    release_tx.send(()).expect("release lifecycle");

    tokio::time::timeout(Duration::from_secs(1), completed_rx)
        .await
        .expect("lifecycle continued after caller cancellation")
        .expect("lifecycle completion signal");
}

#[test]
fn fork_selects_shared_history_once_and_excludes_pending_calls() {
    let context = [
        serde_json::json!({"role":"user", "content":"old"}),
        serde_json::json!({"role":"assistant", "content":"prior"}),
        serde_json::json!({"role":"user", "content":"new"}),
        serde_json::json!({"type":"function_call", "call_id":"pending"}),
        serde_json::json!({"type":"function_call", "call_id":"finished"}),
        serde_json::json!({"type":"function_call_output", "call_id":"finished", "output":"ok"}),
    ]
    .map(Arc::new);
    let pending = BTreeSet::from(["pending"]);
    assert!(fork_context(&context, ForkTurns::None, &pending).is_empty());
    let last = fork_context(&context, ForkTurns::Last(1), &pending);
    assert_eq!(last.len(), 3);
    for (forked, index) in last.iter().zip([2, 4, 5]) {
        assert!(Arc::ptr_eq(forked, &context[index]));
    }
    let all = fork_context(&context, ForkTurns::All, &pending);
    assert_eq!(all.len(), 5);
    assert!(Arc::ptr_eq(&all[0], &context[0]));
    assert_eq!(Arc::strong_count(&context[3]), 1);
}
