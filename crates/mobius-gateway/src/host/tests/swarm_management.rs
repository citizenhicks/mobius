use mobius::backend::checkpoint::Checkpoint;
use uuid::Uuid;

use super::*;

fn gateway(root: &tempfile::TempDir) -> GatewayHost {
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let cron = Arc::new(CronStore::open(store.state_dir()).expect("cron"));
    GatewayHost::start(store, config, credentials, cron).expect("gateway")
}

async fn save_session(gateway: &GatewayHost, session_id: &str, workspace_id: &str) {
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let mut checkpoint = Checkpoint::empty(session_id);
    checkpoint.session_context.workspace_id = Some(workspace_id.into());
    checkpoints
        .save(&checkpoint, &[], None)
        .await
        .expect("save session");
}

#[tokio::test]
async fn gateway_manages_existing_sessions_and_broadcasts_the_swarm_catalog() {
    let root = tempfile::tempdir().expect("root");
    let gateway = gateway(&root);
    let session_ids = (0..3)
        .map(|_| Uuid::new_v4().to_string())
        .collect::<Vec<_>>();
    for session_id in &session_ids {
        save_session(&gateway, session_id, "workspace-a").await;
    }
    let mut events = gateway.subscribe();

    let created = gateway
        .create_swarm(session_ids[0].clone(), session_ids[..2].to_vec())
        .await
        .expect("create swarm");
    let swarm_id = created[0].id.clone();
    let original_handles = created[0].members.clone();
    assert!(created[0].title.starts_with("Swarm "));
    assert_ne!(original_handles[0].handle, original_handles[1].handle);
    assert!(matches!(
        events.recv().await.expect("swarm broadcast").message,
        ServerMessage::Swarms {
            request_id: None,
            ..
        }
    ));

    let joined = gateway
        .add_swarm_member(&swarm_id, session_ids[2].clone())
        .await
        .expect("add member");
    assert!(
        original_handles
            .iter()
            .all(|member| joined[0].members.contains(member))
    );
    let left = gateway
        .leave_swarm(&swarm_id, &session_ids[2])
        .await
        .expect("leave swarm");
    assert_eq!(left[0].members.len(), 2);
    let disbanded = gateway
        .disband_swarm(&swarm_id)
        .await
        .expect("disband swarm");
    assert!(disbanded.is_empty());

    let missing_session = Uuid::new_v4().to_string();
    let rejection = gateway
        .create_swarm(
            session_ids[0].clone(),
            vec![session_ids[0].clone(), missing_session],
        )
        .await
        .expect_err("members must be existing chats");
    assert_eq!(rejection.code, "unknown_session");

    let rejection = gateway
        .create_swarm(session_ids[0].clone(), vec![session_ids[1].clone()])
        .await
        .expect_err("leader must participate");
    assert_eq!(rejection.code, "invalid_swarm");

    let rejection = gateway
        .create_swarm(session_ids[0].clone(), vec![session_ids[0].clone(); 101])
        .await
        .expect_err("membership must be bounded before catalog lookup");
    assert_eq!(rejection.code, "invalid_swarm");

    let foreign_session_id = Uuid::new_v4().to_string();
    save_session(&gateway, &foreign_session_id, "workspace-b").await;
    let rejection = gateway
        .create_swarm(
            session_ids[0].clone(),
            vec![session_ids[0].clone(), foreign_session_id],
        )
        .await
        .expect_err("members must share one workspace");
    assert_eq!(rejection.code, "invalid_swarm");

    let child_session_id = Uuid::new_v4().to_string();
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let mut child = Checkpoint::empty(&child_session_id);
    child.session_context.workspace_id = Some("workspace-a".into());
    checkpoints
        .fork(&session_ids[0], 0, &child)
        .await
        .expect("fork child session");
    let rejection = gateway
        .create_swarm(
            session_ids[0].clone(),
            vec![session_ids[0].clone(), child_session_id],
        )
        .await
        .expect_err("members must be top-level chats");
    assert_eq!(rejection.code, "invalid_swarm");
}
