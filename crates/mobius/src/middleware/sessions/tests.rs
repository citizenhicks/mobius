use super::*;

async fn save_history(
    checkpoints: &dyn crate::backend::checkpoint::CheckpointStore,
    session_id: &str,
    owner_id: &str,
    items: Vec<Value>,
) -> Checkpoint {
    let mut checkpoint = Checkpoint::empty(session_id.parse().expect("session ID"));
    checkpoint.sequence = 1;
    std::sync::Arc::make_mut(&mut checkpoint.session_context).owner_id = owner_id.into();
    let items = items.into_iter().map(Arc::new).collect::<Vec<_>>();
    checkpoint.context = Arc::new(items);
    checkpoints
        .save(checkpoint.clone(), checkpoint.context.to_vec(), None)
        .await
        .expect("save history");
    checkpoint
}

fn history_store(path: &std::path::Path) -> History {
    History {
        checkpoints: Arc::new(
            crate::backend::checkpoint::sqlite::SqliteCheckpoint::new(path).expect("store"),
        ),
        session_id: "current".parse().expect("session ID"),
        owner_id: "researcher".into(),
        pages: None,
    }
}

fn search_args(query: &str, scope: HistoryScope, cursor: Option<String>) -> SearchHistoryArgs {
    SearchHistoryArgs {
        query: query.into(),
        scope,
        cursor,
    }
}

#[tokio::test]
async fn history_recovers_offloaded_tool_output_beyond_the_first_page() {
    let state = tempfile::tempdir().expect("state");
    let history = history_store(&state.path().join("history.sqlite3"));
    let output = format!("{}\nneedle exact result\n", "🦀".repeat(9_000));
    let mut checkpoint = save_history(history.checkpoints.as_ref(), "current", "researcher", vec![
            crate::backend::model::user_message("Investigate"),
            serde_json::json!({"type":"function_call", "call_id":"call-1", "name":"read_file", "arguments":"{\"path\":\"earlier.rs\"}"}),
            crate::backend::model::tool_output("call-1", &output.as_str().into(), false),
        ]).await;
    Arc::make_mut(&mut Arc::make_mut(&mut checkpoint.context)[2])["output"] =
        serde_json::json!([{"type":"input_text", "text":"[offloaded]"}]);
    checkpoint.context_epoch += 1;
    for sequence in 2..=70 {
        checkpoint.sequence = sequence;
        history
            .checkpoints
            .save(
                checkpoint.clone(),
                [
                    serde_json::json!({"role":"assistant", "content":"newer unrelated work"})
                        .into(),
                ]
                .to_vec(),
                None,
            )
            .await
            .expect("save later batch");
    }
    let mut cursor = None;
    let mut pages = 0;
    let hit = loop {
        let page: Value = serde_json::from_str(
            &history
                .search(search_args("needle", HistoryScope::ThisChat, cursor))
                .await
                .expect("search"),
        )
        .expect("search page");
        pages += 1;
        if let Some(hit) = page["hits"].as_array().expect("hits").first() {
            break hit.clone();
        }
        assert!(pages < 10, "history scan must make progress");
        cursor = Some(
            page["next_cursor"]
                .as_str()
                .expect("older history cursor")
                .into(),
        );
    };
    assert!(pages > 1);
    assert_eq!(hit["session_id"], "current");
    assert_eq!(hit["kind"], "tool_result");
    assert!(
        hit["excerpt"]
            .as_str()
            .expect("excerpt")
            .contains("needle exact result")
    );
    let target: MessageTarget = serde_json::from_value(hit["target"].clone()).expect("target");
    assert_eq!(
        target,
        MessageTarget {
            checkpoint_sequence: 1,
            batch_item_count: std::num::NonZeroUsize::new(3).unwrap()
        }
    );
    let mut restored = String::new();
    let mut offset = 0;
    loop {
        let page: Value = serde_json::from_str(
            &history
                .read(ReadHistoryArgs {
                    page: None,
                    global: false,
                    session_id: None,
                    target: Some(target),
                    offset,
                    max_chars: 4_000,
                })
                .await
                .expect("read history"),
        )
        .expect("read page");
        restored.push_str(page["text"].as_str().expect("text"));
        let Some(next) = page["next_offset"].as_u64() else {
            break;
        };
        offset = usize::try_from(next).expect("offset");
    }
    assert_eq!(restored, output);
    let calls: Value = serde_json::from_str(
        &history
            .read(ReadHistoryArgs {
                page: None,
                global: false,
                session_id: None,
                target: Some(MessageTarget {
                    checkpoint_sequence: 1,
                    batch_item_count: std::num::NonZeroUsize::new(2).unwrap(),
                }),
                offset: 0,
                max_chars: 4_000,
            })
            .await
            .expect("read call"),
    )
    .expect("call");
    assert_eq!(calls["text"], "read_file\n{\"path\":\"earlier.rs\"}");
}

#[tokio::test]
async fn history_cursor_continues_inside_a_large_message() {
    let state = tempfile::tempdir().expect("state");
    let history = history_store(&state.path().join("history.sqlite3"));
    save_history(
        history.checkpoints.as_ref(),
        "current",
        "researcher",
        vec![crate::backend::model::user_message(&format!(
            "{}needle after the scan limit",
            "padding ".repeat(10_000)
        ))],
    )
    .await;
    let first: Value = serde_json::from_str(
        &history
            .search(search_args("needle", HistoryScope::ThisChat, None))
            .await
            .expect("first page"),
    )
    .expect("first page json");
    assert!(first["hits"].as_array().expect("hits").is_empty());
    assert!(first["scanned_chars"].as_u64().expect("scan bound") <= 64_000);
    let second: Value = serde_json::from_str(
        &history
            .search(search_args(
                "needle",
                HistoryScope::ThisChat,
                Some(first["next_cursor"].as_str().expect("cursor").into()),
            ))
            .await
            .expect("second page"),
    )
    .expect("second page json");
    assert!(
        second["hits"][0]["excerpt"]
            .as_str()
            .expect("late match")
            .contains("needle after the scan limit")
    );
}

#[tokio::test]
async fn search_memory_ranks_pages_above_every_chat_hit_and_reads_a_page() {
    let state = tempfile::tempdir().expect("state");
    let mut history = history_store(&state.path().join("history.sqlite3"));
    let memory = super::super::memory::Memory::new(
        Arc::clone(&history.checkpoints),
        state.path().join("memory"),
    );
    memory
        .manage(&Op::CapabilityCommand {
            capability: super::super::memory::MANIFEST.id.into(),
            command: "memory".into(),
            arguments: "add".into(),
            input: Some("Deploy needle: run make ship".into()),
            target: None,
        })
        .await
        .expect("page");
    history.pages = Some((
        memory.pages(),
        Arc::new(MemoryOwner {
            bot: "researcher".into(),
            project: None,
        }),
    ));
    save_history(
        history.checkpoints.as_ref(),
        "current",
        "researcher",
        vec![crate::backend::model::user_message("needle in this chat")],
    )
    .await;
    let result: Value = serde_json::from_str(
        &history
            .search(search_args("needle", HistoryScope::All, None))
            .await
            .expect("search"),
    )
    .expect("result");
    assert_eq!(result["pages"][0]["page"], "deploy-needle-run-make-ship");
    assert_eq!(result["hits"][0]["session_id"], "current");
    let page: Value = serde_json::from_str(
        &history
            .read(ReadHistoryArgs {
                page: Some("deploy-needle-run-make-ship".into()),
                global: true,
                session_id: None,
                target: None,
                offset: 0,
                max_chars: 100,
            })
            .await
            .expect("read page"),
    )
    .expect("page");
    assert_eq!(page["body"], "Deploy needle: run make ship");
}

#[tokio::test]
async fn history_requires_explicit_other_chat_scope_and_rejects_foreign_owner_reads() {
    let state = tempfile::tempdir().expect("state");
    let history = history_store(&state.path().join("history.sqlite3"));
    let prior = "prior".repeat(140);
    for (session, owner) in [
        ("current", "researcher"),
        (prior.as_str(), "researcher"),
        ("private", "writer"),
    ] {
        save_history(
            history.checkpoints.as_ref(),
            session,
            owner,
            vec![crate::backend::model::user_message(&format!(
                "needle in {session}"
            ))],
        )
        .await;
    }
    for (scope, expected) in [
        (HistoryScope::ThisChat, "current"),
        (HistoryScope::OtherChats, prior.as_str()),
    ] {
        let result: Value = serde_json::from_str(
            &history
                .search(search_args("needle", scope, None))
                .await
                .expect("search"),
        )
        .expect("result");
        let hits = result["hits"].as_array().expect("hits");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["session_id"], expected);
    }
    let target = MessageTarget {
        checkpoint_sequence: 1,
        batch_item_count: std::num::NonZeroUsize::MIN,
    };
    assert!(
        history
            .read(ReadHistoryArgs {
                page: None,
                global: false,
                session_id: Some(prior.parse().expect("session ID")),
                target: Some(target),
                offset: 0,
                max_chars: 100
            })
            .await
            .is_ok()
    );
    assert!(
        history
            .read(ReadHistoryArgs {
                page: None,
                global: false,
                session_id: Some("private".parse().expect("session ID")),
                target: Some(target),
                offset: 0,
                max_chars: 100
            })
            .await
            .is_err()
    );
    let mut forged = history
        .cursor(search_args("needle", HistoryScope::OtherChats, None))
        .expect("cursor");
    forged.session_id = Some("private".parse().expect("session ID"));
    assert!(
        history
            .search(search_args(
                "needle",
                HistoryScope::OtherChats,
                Some(serde_json::to_string(&forged).expect("cursor json"))
            ))
            .await
            .is_err()
    );
    forged.session_id = Some(prior.parse().expect("session ID"));
    assert!(
        history
            .search(search_args(
                "needle",
                HistoryScope::ThisChat,
                Some(serde_json::to_string(&forged).expect("cursor json"))
            ))
            .await
            .is_err()
    );
}

#[test]
fn fork_prefix_rejects_a_message_with_an_open_tool_call() {
    let items = vec![
        (
            MessageTarget {
                checkpoint_sequence: 1,
                batch_item_count: std::num::NonZeroUsize::MIN,
            },
            serde_json::json!({"type": "function_call", "call_id": "call-1", "name": "read"}),
        ),
        (
            MessageTarget {
                checkpoint_sequence: 1,
                batch_item_count: std::num::NonZeroUsize::new(2).unwrap(),
            },
            serde_json::json!({"role": "assistant", "content": "Working"}),
        ),
    ];
    let target = items[1].0;

    let error = fork_prefix(items, &target, &"session".parse().expect("session ID"))
        .expect_err("unsafe fork must fail");

    assert_eq!(
        error.to_string(),
        "checkpoint error: fork target is not a safe durable message boundary"
    );
}

#[test]
fn resume_excludes_only_current_and_explicitly_hidden_chats() {
    let summary = |session_id: &str, catalog_visible: bool| SessionSummary {
        session_id: session_id.parse().expect("session ID"),
        session_context: Default::default(),
        parent_session_id: None,
        parent_sequence: None,
        sequence: 0,
        catalog_visible,
        first_user_message: None,
        execution_stats: Default::default(),
        created_at: 0,
        updated_at: 0,
    };
    let options = resume_page_options(
        SessionPage {
            sessions: vec![
                summary("current", true),
                summary("hidden", false),
                summary("visible", true),
            ],
            next_cursor: None,
        },
        &"current".parse().expect("session ID"),
    )
    .expect("resume options");
    let session_ids = options
        .into_iter()
        .map(|option| match option.op {
            Op::ResumeSession { session_id } => session_id,
            operation => panic!("expected resume operation, got {operation:?}"),
        })
        .collect::<Vec<_>>();

    assert_eq!(session_ids, ["visible".parse().expect("session ID")]);
}

#[test]
fn manual_fork_keeps_context_workspace_and_metadata_but_clears_origin() {
    let mut parent = Checkpoint::empty("parent".parse().expect("session ID"));
    parent.context = std::sync::Arc::new(
        vec![serde_json::json!({
            "role": "user",
            "content": "Hello",
            "_mobius_attachments": [{
                "id": "378b8581-e96c-4413-a138-93e74561cb87",
                "name": "photo.png",
                "size": 1,
                "media_type": "image/png"
            }]
        })]
        .into_iter()
        .map(std::sync::Arc::new)
        .collect(),
    );
    parent.first_user_message = Some("Hello".into());
    parent.metadata.insert(
        "gateway.chat".into(),
        serde_json::json!({"workspace": "/srv/project"}),
    );
    parent.session_context = crate::protocol::SessionContext {
        owner_id: "bot-1".into(),
        workspace_id: Some("workspace-1".into()),
        workspace_label: Some("Project One".into()),
        origin_label: Some("routine".into()),
        ..crate::protocol::SessionContext::default()
    }
    .into();

    let fork = manual_fork_checkpoint(
        &parent,
        parent
            .context
            .iter()
            .map(|item| item.as_ref().to_owned())
            .collect(),
    );

    assert!(fork.context[0].get("_mobius_attachments").is_none());
    assert_eq!(fork.first_user_message, parent.first_user_message);
    assert_eq!(fork.metadata, parent.metadata);
    assert_eq!(
        *fork.session_context,
        crate::protocol::SessionContext {
            owner_id: "bot-1".into(),
            workspace_id: Some("workspace-1".into()),
            workspace_label: Some("Project One".into()),
            ..crate::protocol::SessionContext::default()
        }
    );
}
