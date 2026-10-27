use super::bots::gateway_with_bot;
use super::*;
use crate::wire::{HarnessKind, MemoryRecord, WorkspaceInfo};

#[tokio::test]
async fn memories_list_with_their_project_bot_and_exposure_and_edit_by_id() {
    let (_root, gateway, bot) = gateway_with_bot().await;
    let (checkpoints, memory) = {
        let state = gateway.state.lock().await;
        (Arc::clone(&state.checkpoints), Arc::clone(&state.memory))
    };
    let scoped = "8f1e5c1a-7f43-4f7e-9a39-0c6c2f6d7f10";
    checkpoints
        .save_state(
            "memory",
            &serde_json::to_string(&(&*bot.id, "project-1")).unwrap(),
            &serde_json::json!({
                "owner": {"bot": &*bot.id, "project": {"id": "project-1", "label": "/work/one"}},
                "memories": [{
                    "id": scoped,
                    "text": "uses pnpm",
                    "basis": {"type": "agent_observation"},
                    "updated_at": 5
                }]
            }),
        )
        .await
        .unwrap();
    let authored = "2b7d0c8e-51a4-4c0e-8d61-6f4f3e0a9b21";
    checkpoints
        .save_state(
            "memory",
            "global",
            &serde_json::json!({
                "owner": null,
                "memories": [{
                    "id": authored,
                    "text": "prefers metric units",
                    "basis": {"type": "agent_observation"},
                    "updated_at": 6,
                    "author": {"bot": &*bot.id, "project": {"id": "project-1", "label": "/work/one"}}
                }]
            }),
        )
        .await
        .unwrap();
    memory
        .manage(&Op::CapabilityCommand {
            capability: mobius::middleware::memory::MANIFEST.id.into(),
            command: "memory".into(),
            arguments: "add".into(),
            input: Some("the user is in Tallinn".into()),
            target: None,
        })
        .await
        .unwrap();

    let listed = gateway.list_memories().await.unwrap();
    let written = listed
        .iter()
        .find(|memory| &*memory.id == authored)
        .unwrap();
    assert_eq!(
        (
            written.global,
            written.project.as_ref(),
            written.bot_id.as_deref()
        ),
        (true, None, Some(&*bot.id)),
        "a global memory keeps its writer but is in no project"
    );
    let global = listed
        .iter()
        .find(|memory| memory.global && memory.bot_id.is_none())
        .unwrap();
    assert_eq!(
        (
            global.project.as_ref(),
            global.bot_id.as_deref(),
            &*global.text
        ),
        (None, None, "the user is in Tallinn")
    );
    assert_eq!(
        listed.iter().find(|memory| !memory.global),
        Some(&MemoryRecord {
            id: scoped.into(),
            harness: HarnessKind::Mobius,
            project: Some(WorkspaceInfo {
                id: "project-1".into(),
                path: "/work/one".into(),
            }),
            bot_id: Some(Arc::clone(&bot.id)),
            global: false,
            text: "uses pnpm".into(),
            updated_at: 5,
            editable: true,
        })
    );

    let edited = gateway.edit_memory(scoped, "uses pnpm 10").await.unwrap();
    assert!(
        edited
            .iter()
            .any(|memory| &*memory.text == "uses pnpm 10" && !memory.global)
    );
    let remaining = gateway.forget_memory(&global.id).await.unwrap();
    assert_eq!(remaining.len(), 2);
    assert_eq!(
        gateway.forget_memory(&global.id).await.unwrap_err().code,
        "invalid_memory"
    );
}
