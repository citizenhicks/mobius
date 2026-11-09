use sha2::Digest;

use super::bots::gateway_with_bot;
use super::*;
use crate::wire::{HarnessKind, MemoryRecord, WorkspaceInfo};

fn write_page(dir: &std::path::Path, name: &str, frontmatter: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(format!("{name}.md")),
        format!("---\n{frontmatter}---\n{body}\n"),
    )
    .unwrap();
}

#[tokio::test]
async fn memory_pages_list_with_their_project_bot_links_and_edit_by_id() {
    let (_root, gateway, bot) = gateway_with_bot().await;
    let (root, memory) = {
        let state = gateway.state.lock().await;
        (
            state.store.state_dir().join("memory"),
            Arc::clone(&state.memory),
        )
    };
    let owner =
        serde_json::json!({"bot": &*bot.id, "project": {"id": "project-1", "label": "/work/one"}});
    // The on-disk layout is the store's contract with people who edit pages by hand.
    let digest = sha2::Sha256::digest(
        serde_json::to_string(&(&*bot.id, "project-1"))
            .unwrap()
            .as_bytes(),
    );
    let scope = format!(
        "s-{}",
        format!("{digest:x}").chars().take(32).collect::<String>()
    );
    std::fs::create_dir_all(root.join(&scope)).unwrap();
    std::fs::write(root.join(&scope).join("_scope.json"), owner.to_string()).unwrap();
    write_page(
        &root.join(&scope),
        "uses-pnpm",
        "description: Package manager\nbasis: agent_observation\nupdated_at: 5\n",
        "Install with pnpm; see [[metric-units]].",
    );
    write_page(
        &root.join("global"),
        "metric-units",
        &format!("description: Units\nbasis: agent_observation\nupdated_at: 6\nauthor: {owner}\n"),
        "Prefers metric units.",
    );
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
        .find(|memory| &*memory.name == "metric-units")
        .unwrap();
    assert_eq!(
        (
            written.global,
            written.project.as_ref(),
            written.bot_id.as_deref()
        ),
        (true, None, Some(&*bot.id)),
        "a global page keeps its writer but is in no project"
    );
    let global = listed
        .iter()
        .find(|memory| memory.global && memory.bot_id.is_none())
        .unwrap();
    assert_eq!(&*global.text, "the user is in Tallinn");
    let scoped_id = format!("{scope}/uses-pnpm");
    assert_eq!(
        listed.iter().find(|memory| !memory.global),
        Some(&MemoryRecord {
            id: scoped_id.as_str().into(),
            name: "uses-pnpm".into(),
            description: "Package manager".into(),
            links: vec![crate::wire::MemoryLink {
                name: "metric-units".into(),
                target: Some("global/metric-units".into())
            }],
            harness: HarnessKind::Mobius,
            project: Some(WorkspaceInfo {
                id: "project-1".into(),
                path: "/work/one".into(),
            }),
            bot_id: Some(Arc::clone(&bot.id)),
            global: false,
            text: "Install with pnpm; see [[metric-units]].".into(),
            updated_at: 5,
            editable: true,
        })
    );

    let edited = gateway
        .edit_memory(&scoped_id, "Package manager and version", "uses pnpm 10")
        .await
        .unwrap();
    assert!(edited.iter().any(|memory| &*memory.text == "uses pnpm 10"
        && &*memory.description == "Package manager and version"));
    let remaining = gateway.forget_memory(&global.id).await.unwrap();
    assert_eq!(remaining.len(), 2);
    assert_eq!(
        gateway.forget_memory(&global.id).await.unwrap_err().code,
        "invalid_memory"
    );
}
