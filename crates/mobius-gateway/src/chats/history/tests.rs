use mobius::backend::checkpoint::{Checkpoint, sqlite::SqliteCheckpoint};
use mobius::backend::model::{tool_output, user_message};
use mobius::protocol::{
    AssistantMessageEvent, Event, MessageSubmission, ModelStepContent, ModelStepContentPhase,
    ToolCallEndEvent,
};

use super::*;

async fn fixture(
    member_count: usize,
) -> (
    tempfile::TempDir,
    History,
    Vec<crate::wire::BotRecord>,
    String,
) {
    let root = tempfile::tempdir().expect("state");
    let bots = Arc::new(BotStore::open(root.path()).expect("Bots"));
    let members = ["Alice", "Bob", "Outsider"]
        .map(|name| {
            bots.create_bot(name, "Test Bot", crate::wire::AgentComposition::default())
                .expect("Bot")
        })
        .to_vec();
    let (chats, _) = ChatStore::new(root.path(), Arc::clone(&bots)).expect("Chats");
    let chats = Arc::new(chats);
    let chat_id = chats
        .create(
            root.path().to_owned(),
            members[..member_count]
                .iter()
                .map(|bot| bot.id.clone())
                .collect(),
            None,
        )
        .await
        .expect("Chat");
    let chat = chats.load(&chat_id).await.expect("load").expect("Chat");
    let history = History {
        chats,
        bots,
        checkpoints: Arc::new(
            SqliteCheckpoint::new(root.path().join("history.sqlite3")).expect("checkpoints"),
        ),
        session_id: chat.session_id(&members[0].id).expect("participant").into(),
        bot_id: members[0].id.clone(),
    };
    (root, history, members, chat_id)
}

fn user(text: impl Into<String>) -> MessageSubmission {
    MessageSubmission {
        author: MessageAuthor::User,
        text: text.into(),
        attachments: Vec::new(),
        reply: None,
        requested_delivery: None,
        target_turn_id: None,
    }
}

async fn save_history(
    checkpoints: &dyn CheckpointStore,
    session_id: &str,
    items: Vec<Value>,
) -> Checkpoint {
    let mut checkpoint = Checkpoint::empty(session_id);
    checkpoint.sequence = 1;
    checkpoint.context.clone_from(&items);
    checkpoints
        .save(&checkpoint, &items, None)
        .await
        .expect("save history");
    checkpoint
}

fn search_args(query: &str, scope: HistoryScope, cursor: Option<String>) -> SearchHistoryArgs {
    SearchHistoryArgs {
        query: query.into(),
        scope,
        cursor,
    }
}

async fn search(history: &History, query: &str, scope: HistoryScope) -> Value {
    serde_json::from_str(
        &history
            .search(search_args(query, scope, None))
            .await
            .expect("search"),
    )
    .expect("search page")
}

fn read_args(reference: HistoryReference) -> ReadHistoryArgs {
    ReadHistoryArgs {
        reference,
        offset: 0,
        max_chars: MAX_HISTORY_READ_CHARS,
    }
}

#[tokio::test]
async fn execution_history_recovers_offloaded_tool_output_beyond_the_first_page() {
    let (_root, history, _bots, _chat) = fixture(2).await;
    let output = format!("{}\nneedle exact result\n", "🦀".repeat(9_000));
    let mut checkpoint = save_history(history.checkpoints.as_ref(), &history.session_id, vec![
        user_message("Investigate"),
        serde_json::json!({"type":"function_call", "call_id":"call-1", "name":"read_file", "arguments":"{\"path\":\"earlier.rs\"}"}),
        tool_output("call-1", &output, false),
    ]).await;
    checkpoint.context[2]["output"] =
        serde_json::json!([{"type":"input_text", "text":"[offloaded]"}]);
    for sequence in 2..=70 {
        checkpoint.sequence = sequence;
        history
            .checkpoints
            .save(
                &checkpoint,
                &[serde_json::json!({"role":"assistant", "content":"newer unrelated work"})],
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
                .search(search_args("needle", HistoryScope::Execution, cursor))
                .await
                .expect("search"),
        )
        .expect("page");
        pages += 1;
        if let Some(hit) = page["hits"].as_array().expect("hits").first() {
            break hit.clone();
        }
        assert!(pages < 10, "bounded history scans must make progress");
        cursor = Some(
            page["next_cursor"]
                .as_str()
                .expect("older history cursor")
                .into(),
        );
    };
    assert!(pages > 1);
    assert_eq!(hit["kind"], "tool_result");
    assert!(
        hit["excerpt"]
            .as_str()
            .expect("excerpt")
            .contains("needle exact result")
    );
    let reference: HistoryReference =
        serde_json::from_value(hit["reference"].clone()).expect("reference");
    assert_eq!(
        reference,
        HistoryReference::Execution {
            session_id: history.session_id.clone(),
            target: MessageTarget {
                checkpoint_sequence: 1,
                batch_item_count: 3
            }
        }
    );
    let mut restored = String::new();
    let mut offset = 0;
    loop {
        let page: Value = serde_json::from_str(
            &history
                .read(ReadHistoryArgs {
                    reference: reference.clone(),
                    offset,
                    max_chars: 4_000,
                })
                .await
                .expect("read"),
        )
        .expect("page");
        restored.push_str(page["text"].as_str().expect("text"));
        let Some(next) = page["next_offset"].as_u64() else {
            break;
        };
        offset = usize::try_from(next).expect("offset");
    }
    assert_eq!(restored, output);
    let calls: Value = serde_json::from_str(
        &history
            .read(read_args(HistoryReference::Execution {
                session_id: history.session_id.clone(),
                target: MessageTarget {
                    checkpoint_sequence: 1,
                    batch_item_count: 2,
                },
            }))
            .await
            .expect("read call"),
    )
    .expect("call");
    assert_eq!(calls["text"], "read_file\n{\"path\":\"earlier.rs\"}");
}

#[tokio::test]
async fn public_history_and_catalog_require_membership_and_never_expose_peer_executions() {
    let (root, history, bots, current_id) = fixture(2).await;
    let prior_id = history
        .chats
        .create(root.path().to_owned(), vec![bots[0].id.clone()], None)
        .await
        .expect("prior");
    let foreign_id = history
        .chats
        .create(
            root.path().to_owned(),
            vec![bots[1].id.clone(), bots[2].id.clone()],
            None,
        )
        .await
        .expect("foreign");
    for id in [&current_id, &prior_id, &foreign_id] {
        history
            .chats
            .post_user(
                id,
                format!("message-{id}"),
                user(format!("needle public {id}")),
                &[],
            )
            .await
            .expect("post");
    }
    let current = history
        .chats
        .load(&current_id)
        .await
        .expect("load")
        .expect("current");
    let peer_execution = current.session_id(&bots[1].id).expect("peer");
    save_history(
        history.checkpoints.as_ref(),
        &history.session_id,
        vec![user_message("needle own private")],
    )
    .await;
    save_history(
        history.checkpoints.as_ref(),
        peer_execution,
        vec![user_message("needle peer private")],
    )
    .await;
    for (scope, expected) in [
        (HistoryScope::Current, &current_id),
        (HistoryScope::OtherChats, &prior_id),
    ] {
        let page = search(&history, "needle", scope).await;
        let hits = page["hits"].as_array().expect("hits");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["reference"]["source"], "chat");
        assert_eq!(hits[0]["reference"]["chat_id"], *expected);
        assert!(!page.to_string().contains("private"));
    }
    let private = search(&history, "needle", HistoryScope::Execution).await;
    assert_eq!(private["hits"].as_array().expect("hits").len(), 1);
    assert!(
        private["hits"][0]["excerpt"]
            .as_str()
            .expect("excerpt")
            .contains("own private")
    );
    assert!(!private.to_string().contains("peer private"));
    let current_hits = search(&history, "needle", HistoryScope::Current).await;
    let reference: HistoryReference =
        serde_json::from_value(current_hits["hits"][0]["reference"].clone())
            .expect("public reference");
    assert!(
        history
            .read(read_args(reference.clone()))
            .await
            .expect("public read")
            .contains("needle public")
    );
    assert!(
        history
            .read(read_args(HistoryReference::Execution {
                session_id: peer_execution.into(),
                target: MessageTarget {
                    checkpoint_sequence: 1,
                    batch_item_count: 1
                }
            }))
            .await
            .is_err()
    );
    assert!(
        history
            .read(read_args(HistoryReference::Chat {
                chat_id: foreign_id.clone(),
                sequence: 1
            }))
            .await
            .is_err()
    );
    assert!(
        history
            .read(read_args(HistoryReference::Chat {
                chat_id: peer_execution.into(),
                sequence: 1
            }))
            .await
            .is_err()
    );
    let mut forged = history
        .cursor(search_args("needle", HistoryScope::OtherChats, None))
        .await
        .expect("cursor");
    forged.chat_id = Some(foreign_id);
    assert!(
        history
            .search(search_args(
                "needle",
                HistoryScope::OtherChats,
                Some(serde_json::to_string(&forged).expect("cursor"))
            ))
            .await
            .is_err()
    );
    let older_id = history
        .chats
        .create(root.path().to_owned(), vec![bots[0].id.clone()], None)
        .await
        .expect("older Chat");
    let mut catalog_cursor = String::new();
    let mut listed = Vec::new();
    loop {
        let catalog = history.resume(&catalog_cursor, 1).await.expect("catalog");
        let FrontendEvent::Picker { options, .. } = &catalog.events[0] else {
            panic!("catalog picker")
        };
        let mut next_cursor = None;
        for option in options {
            match &option.op {
                Op::ResumeSession { session_id } => listed.push(session_id.clone()),
                Op::CapabilityCommand { arguments, .. } => next_cursor = Some(arguments.clone()),
                _ => panic!("unexpected catalog action"),
            }
        }
        assert!(
            listed.len() <= 2,
            "catalog pages must advance without duplicates"
        );
        let Some(next) = next_cursor else { break };
        catalog_cursor = next;
    }
    assert_eq!(
        listed
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([prior_id, older_id])
    );
    history
        .chats
        .remove_bot(&history.bot_id)
        .await
        .expect("revoke membership");
    assert!(history.read(read_args(reference)).await.is_err());
    assert!(
        history
            .search(search_args("needle", HistoryScope::Current, None))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn public_history_follows_inline_tool_visibility() {
    for member_count in [1, 2] {
        let (_root, history, bots, chat_id) = fixture(member_count).await;
        history
            .chats
            .post_user(&chat_id, "work".into(), user("Inspect the files"), &[])
            .await
            .expect("post work");
        history
            .chats
            .observe_event(
                &history.session_id,
                &Event {
                    submission_id: Some("work".into()),
                    msg: EventMsg::ToolCallEnd(ToolCallEndEvent {
                        turn_id: "turn".into(),
                        call_id: "call".into(),
                        name: "read_file".into(),
                        output: "needle observed file contents".into(),
                        is_error: false,
                    }),
                },
            )
            .await
            .expect("observe tool result");
        let page = search(&history, "needle", HistoryScope::Current).await;
        let hits = page["hits"].as_array().expect("hits");
        assert_eq!(hits.len(), usize::from(member_count == 1));
        if member_count == 1 {
            let reference =
                serde_json::from_value(hits[0]["reference"].clone()).expect("reference");
            let read: Value = serde_json::from_str(
                &history
                    .read(read_args(reference))
                    .await
                    .expect("read published tool result"),
            )
            .expect("result");
            assert_eq!(read["text"], "needle observed file contents");
        }
        history
            .chats
            .observe_event(
                &history.session_id,
                &Event {
                    submission_id: Some("work".into()),
                    msg: EventMsg::AssistantMessage(AssistantMessageEvent {
                        session_id: history.session_id.clone(),
                        turn_id: "turn".into(),
                        model_step_id: "step".into(),
                        message_target: None,
                        content: [
                            (ModelStepContentPhase::Reasoning, "hidden analysis"),
                            (ModelStepContentPhase::Commentary, "commentary evidence\n"),
                            (ModelStepContentPhase::FinalAnswer, "final evidence"),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (phase, text))| ModelStepContent {
                            output_index: index,
                            part_index: 0,
                            phase,
                            text: text.into(),
                            annotations: Vec::new(),
                        })
                        .collect(),
                    }),
                },
            )
            .await
            .expect("observe assistant message");
        let page = search(&history, "commentary", HistoryScope::Current).await;
        let hits = page["hits"].as_array().expect("hits");
        assert_eq!(hits.len(), usize::from(member_count == 1));
        if member_count == 1 {
            let reference =
                serde_json::from_value(hits[0]["reference"].clone()).expect("reference");
            let read: Value = serde_json::from_str(
                &history
                    .read(read_args(reference))
                    .await
                    .expect("published assistant"),
            )
            .expect("read");
            let message: Value = serde_json::from_str(read["text"].as_str().expect("text"))
                .expect("authored message");
            assert_eq!(message["author"]["handle"], bots[0].handle);
            assert_eq!(message["text"], "commentary evidence\nfinal evidence");
            assert!(!read.to_string().contains("hidden analysis"));
            assert!(!read.to_string().contains(&history.session_id));
        }
    }
}

#[tokio::test]
async fn execution_history_continues_within_large_items_and_rejects_invalid_references() {
    let (_root, history, _bots, _chat) = fixture(1).await;
    let text = format!("{}needle after the scan limit", "padding ".repeat(10_000));
    save_history(
        history.checkpoints.as_ref(),
        &history.session_id,
        vec![
            user_message(&text),
            serde_json::json!({"type":"reasoning", "encrypted_content":"needle hidden reasoning"}),
        ],
    )
    .await;
    let first = search(&history, "needle", HistoryScope::Execution).await;
    assert!(first["hits"].as_array().expect("hits").is_empty());
    assert!(first["scanned_chars"].as_u64().expect("scan bound") <= 64_000);
    let cursor = Some(first["next_cursor"].as_str().expect("cursor").into());
    let second: Value = serde_json::from_str(
        &history
            .search(search_args("needle", HistoryScope::Execution, cursor))
            .await
            .expect("continued search"),
    )
    .expect("page");
    assert!(
        second["hits"][0]["excerpt"]
            .as_str()
            .expect("excerpt")
            .contains("needle after the scan limit")
    );
    assert!(!second.to_string().contains("hidden reasoning"));
    let reference = HistoryReference::Execution {
        session_id: history.session_id.clone(),
        target: MessageTarget {
            checkpoint_sequence: 1,
            batch_item_count: 1,
        },
    };
    for (offset, max_chars) in [(0, 0), (0, MAX_HISTORY_READ_CHARS + 1), (usize::MAX, 10)] {
        assert!(
            history
                .read(ReadHistoryArgs {
                    reference: reference.clone(),
                    offset,
                    max_chars
                })
                .await
                .is_err()
        );
    }
    for batch_item_count in [0, 2, 3] {
        assert!(
            history
                .read(read_args(HistoryReference::Execution {
                    session_id: history.session_id.clone(),
                    target: MessageTarget {
                        checkpoint_sequence: 1,
                        batch_item_count
                    }
                }))
                .await
                .is_err()
        );
    }
    for query in [String::new(), "a".repeat(MAX_HISTORY_QUERY_BYTES + 1)] {
        assert!(
            history
                .search(search_args(&query, HistoryScope::Execution, None))
                .await
                .is_err()
        );
    }
    assert!(
        history
            .search(search_args(
                "needle",
                HistoryScope::Execution,
                Some("x".repeat(MAX_HISTORY_CURSOR_BYTES + 1))
            ))
            .await
            .is_err()
    );
    let mut forged = history
        .cursor(search_args("needle", HistoryScope::Execution, None))
        .await
        .expect("cursor");
    forged.chat_id = Some("other-execution".into());
    assert!(
        history
            .search(search_args(
                "needle",
                HistoryScope::Execution,
                Some(serde_json::to_string(&forged).expect("cursor"))
            ))
            .await
            .is_err()
    );
}
