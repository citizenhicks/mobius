use super::*;
use mobius::protocol::{ToolCallBeginEvent, ToolCallEndEvent};

async fn seed(checkpoints: &Arc<dyn CheckpointStore>, id: &str) {
    let mut checkpoint = Checkpoint::empty(id);
    checkpoint.catalog_visible = false;
    checkpoint.first_user_message = Some("Inspect this private work".into());
    checkpoints.save(&checkpoint, &[], None).await.unwrap();
}

#[tokio::test]
async fn private_conversations_follow_current_retired_routine_and_descendant_ownership() {
    let (root, gateway, bot) = bots::gateway_with_bot().await;
    let (bots, chats, checkpoints, operations) = {
        let state = gateway.state.lock().await;
        (
            state.bots.clone(),
            state.chat_store.clone(),
            state.checkpoints.clone(),
            state.store.runtime_operations.clone(),
        )
    };
    let other = bots
        .create_bot("Other", "Other work", Default::default())
        .unwrap();
    let chat_id = chats
        .create(root.path().into(), vec![bot.id.clone()], None)
        .await
        .unwrap();
    let retired_id = chat_execution_id(&gateway, &chat_id, &bot.id).await;
    seed(&checkpoints, &retired_id).await;
    chats.reassign(&chat_id, &other.id).await.unwrap();
    let other_id = chat_execution_id(&gateway, &chat_id, &other.id).await;
    // A newly assigned execution has its own owner even when it forks an older Bot's work.
    checkpoints
        .fork(&retired_id, 0, &Checkpoint::empty(&other_id))
        .await
        .unwrap();
    let child_id = Uuid::new_v4().to_string();
    checkpoints
        .fork(&retired_id, 0, &Checkpoint::empty(&child_id))
        .await
        .unwrap();
    let foreign_child_id = Uuid::new_v4().to_string();
    checkpoints
        .fork(&other_id, 0, &Checkpoint::empty(&foreign_child_id))
        .await
        .unwrap();
    let current_chat = chats
        .create(root.path().into(), vec![bot.id.clone()], None)
        .await
        .unwrap();
    let current_id = chat_execution_id(&gateway, &current_chat, &bot.id).await;
    seed(&checkpoints, &current_id).await;
    let unmaterialized_chat = chats
        .create(root.path().into(), vec![bot.id.clone()], None)
        .await
        .unwrap();
    let unmaterialized_id = chat_execution_id(&gateway, &unmaterialized_chat, &bot.id).await;
    let deleted_chat = chats
        .create(root.path().into(), vec![bot.id.clone()], None)
        .await
        .unwrap();
    let deleted_id = chat_execution_id(&gateway, &deleted_chat, &bot.id).await;
    checkpoints
        .fork(&retired_id, 0, &Checkpoint::empty(&deleted_id))
        .await
        .unwrap();
    let deleted_child_id = Uuid::new_v4().to_string();
    checkpoints
        .fork(&deleted_id, 0, &Checkpoint::empty(&deleted_child_id))
        .await
        .unwrap();
    chats.mark_deleted(&[deleted_chat]).await.unwrap();
    let routine = bots
        .create_routine(
            &bot.id,
            root.path(),
            "Private routine",
            crate::wire::RoutineSchedule {
                kind: crate::wire::RoutineScheduleKind::Once,
                at: Some(Utc::now().timestamp() + 3600),
                every_seconds: None,
                expression: None,
                time_zone: None,
            },
            None,
        )
        .unwrap();
    let BeginRun::Started(run) = bots.begin_run(&routine.id).unwrap() else {
        panic!("run starts")
    };
    let routine_id = run.session_id().to_owned();
    seed(&checkpoints, &routine_id).await;
    let run = bots
        .finish_run(run, RoutineRunStatus::Succeeded, None)
        .unwrap();
    checkpoints
        .append_event(
            &routine_id,
            1,
            &Event {
                submission_id: Some("routine".into()),
                msg: EventMsg::Warning(mobius::protocol::WarningEvent {
                    message: "routine result".into(),
                }),
            },
        )
        .await
        .unwrap();
    let before = operations.counts();
    let routine_preview = gateway.routine_run_preview(&run.id, None).await.unwrap();
    assert_eq!(routine_preview.run.id, run.id);
    assert_eq!(
        routine_preview.records[0].blocks[0].block.text,
        "routine result"
    );
    let page = gateway.bot_conversations(&bot.id, None).await.unwrap();
    let expected = HashSet::from([
        retired_id.clone(),
        child_id.clone(),
        current_id.clone(),
        routine_id.clone(),
    ]);
    assert_eq!(
        page.conversations
            .iter()
            .map(|item| item.conversation_id.clone())
            .collect::<HashSet<_>>(),
        expected
    );
    assert!(page.next_cursor.is_none());
    assert!(page.conversations.iter().all(|item| item.bot_id == bot.id));
    assert_eq!(
        page.conversations
            .iter()
            .find(|item| item.conversation_id == child_id)
            .unwrap()
            .chat_id
            .as_deref(),
        Some(chat_id.as_str())
    );
    assert!(
        page.conversations
            .iter()
            .find(|item| item.conversation_id == routine_id)
            .unwrap()
            .chat_id
            .is_none()
    );
    let public = gateway.sessions().await.unwrap();
    assert!(
        public
            .iter()
            .all(|item| !expected.contains(&item.session_id))
    );
    for id in [&retired_id, &child_id, &current_id, &routine_id] {
        gateway
            .bot_conversation_history(&bot.id, id, None)
            .await
            .unwrap();
    }
    for id in [
        &other_id,
        &foreign_child_id,
        &unmaterialized_id,
        &deleted_id,
        &deleted_child_id,
        &chat_id,
    ] {
        assert_eq!(
            gateway
                .bot_conversation_history(&bot.id, id, None)
                .await
                .err()
                .unwrap()
                .code,
            "bot_conversation_unavailable"
        );
    }
    bots.delete_run(&run.id).unwrap();
    assert_eq!(
        gateway
            .bot_conversation_history(&bot.id, &routine_id, None)
            .await
            .err()
            .unwrap()
            .code,
        "bot_conversation_unavailable"
    );
    assert_eq!(operations.counts(), before);
    assert!(gateway.state.lock().await.sessions.is_empty());
    gateway.shutdown().await;
}

#[tokio::test]
async fn private_conversation_history_and_files_are_paged_rendered_and_read_only() {
    let (root, gateway, bot) = bots::gateway_with_bot().await;
    let (bots, chats, checkpoints, files, operations) = {
        let state = gateway.state.lock().await;
        (
            state.bots.clone(),
            state.chat_store.clone(),
            state.checkpoints.clone(),
            state.session_files.clone(),
            state.store.runtime_operations.clone(),
        )
    };
    let other = bots
        .create_bot("Other", "Other work", Default::default())
        .unwrap();
    let chat_id = chats
        .create(root.path().into(), vec![bot.id.clone()], None)
        .await
        .unwrap();
    let execution_id = chat_execution_id(&gateway, &chat_id, &bot.id).await;
    seed(&checkpoints, &execution_id).await;
    let child_id = Uuid::new_v4().to_string();
    checkpoints
        .fork(&execution_id, 0, &Checkpoint::empty(&child_id))
        .await
        .unwrap();
    let file = files
        .publish_artifact(
            &child_id,
            "result.txt".into(),
            "text/plain".into(),
            b"private result",
        )
        .await
        .unwrap();
    let events = [
        EventMsg::TurnStarted(mobius::protocol::TurnStartedEvent {
            turn_id: "turn".into(),
            model_context_window: None,
        }),
        EventMsg::ToolCallBegin(ToolCallBeginEvent {
            turn_id: "turn".into(),
            call_id: "call".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path":"README.md"}),
        }),
        EventMsg::ToolCallEnd(ToolCallEndEvent {
            turn_id: "turn".into(),
            call_id: "call".into(),
            name: "read_file".into(),
            output: "private tool output".into(),
            is_error: false,
        }),
        EventMsg::ToolCallEnd(ToolCallEndEvent {
            turn_id: "turn".into(),
            call_id: "patch".into(),
            name: "apply_patch".into(),
            output: "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n".into(),
            is_error: false,
        }),
        EventMsg::ToolLoad(mobius::protocol::ToolLoadEvent {
            turn_id: "turn".into(),
            load_id: "load".into(),
            catalog_revision: "1".into(),
            tools: vec!["read_file".into()],
        }),
        EventMsg::Frontend(FrontendEvent::Render {
            capability: "custom".into(),
            block: EventMsg::Warning(mobius::protocol::WarningEvent {
                message: "custom detail".into(),
            })
            .presentation()
            .unwrap()
            .block,
        }),
        EventMsg::SessionHistory(mobius::protocol::SessionHistoryEvent { events: Vec::new() }),
        EventMsg::SessionResumeRequested(mobius::protocol::SessionResumeRequestedEvent {
            session_id: execution_id.clone(),
            context: Default::default(),
        }),
        EventMsg::Frontend(FrontendEvent::RemoveWidget {
            capability: "test".into(),
            id: "old-widget".into(),
        }),
        EventMsg::TurnComplete(mobius::protocol::TurnCompleteEvent {
            turn_id: "turn".into(),
        }),
    ];
    for (index, msg) in events.into_iter().enumerate() {
        checkpoints
            .append_event(
                &child_id,
                i64::try_from(index).unwrap(),
                &Event {
                    submission_id: Some("submission".into()),
                    msg,
                },
            )
            .await
            .unwrap();
    }
    let latest = std::iter::once(EventMsg::TurnStarted(mobius::protocol::TurnStartedEvent {
        turn_id: "latest".into(),
        model_context_window: None,
    }))
    .chain((0..SESSION_PAGE_SIZE).map(|index| {
        EventMsg::Warning(mobius::protocol::WarningEvent {
            message: format!("notice {index}"),
        })
    }))
    .chain(std::iter::once(EventMsg::TurnComplete(
        mobius::protocol::TurnCompleteEvent {
            turn_id: "latest".into(),
        },
    )));
    for (index, msg) in latest.enumerate() {
        checkpoints
            .append_event(
                &child_id,
                i64::try_from(index + 10).unwrap(),
                &Event {
                    submission_id: Some("latest".into()),
                    msg,
                },
            )
            .await
            .unwrap();
    }
    let before = operations.counts();
    let first = gateway
        .bot_conversation_history(&bot.id, &child_id, None)
        .await
        .unwrap();
    assert_eq!(first.records.len(), SESSION_PAGE_SIZE + 2);
    assert!(
        matches!(&first.records[0].event.msg, EventMsg::TurnStarted(start) if start.turn_id == "latest")
    );
    assert!(
        matches!(&first.records.last().unwrap().event.msg, EventMsg::TurnComplete(end) if end.turn_id == "latest")
    );
    assert_eq!(first.next_before_sequence, Some(first.records[0].sequence));
    let earlier = gateway
        .bot_conversation_history(&bot.id, &child_id, first.next_before_sequence)
        .await
        .unwrap();
    assert_eq!(earlier.records.len(), 7);
    assert!(earlier.records.last().unwrap().sequence < first.records[0].sequence);
    assert!(
        matches!(&earlier.records[0].event.msg, EventMsg::TurnStarted(start) if start.turn_id == "turn")
    );
    assert!(
        matches!(&earlier.records.last().unwrap().event.msg, EventMsg::TurnComplete(end) if end.turn_id == "turn")
    );
    let rendered = &earlier.records[1..earlier.records.len() - 1];
    assert_eq!(
        rendered[0].blocks[0].block.group.as_deref(),
        Some("read:turn")
    );
    assert_eq!(
        rendered[2].blocks[0].block.format,
        mobius::protocol::FrontendBlockFormat::UnifiedDiff
    );
    assert!(rendered[3].blocks[0].block.text.contains("read_file"));
    assert_eq!(rendered[4].blocks[0].capability, "custom");
    assert_eq!(rendered[4].blocks[0].block.text, "custom detail");
    assert!(earlier.next_before_sequence.is_none());
    assert!(rendered[0].blocks[0].block.text.contains("README.md"));
    assert!(
        rendered[1].blocks[0]
            .block
            .text
            .contains("private tool output")
    );
    assert!(
        earlier
            .records
            .iter()
            .all(|record| record.recipient_bot_ids.is_empty())
    );
    let frame = ServerFrame::new(ServerMessage::BotConversationHistory {
        request_id: "history".into(),
        bot_id: bot.id.clone(),
        conversation_id: child_id.clone(),
        records: earlier.records,
        next_before_sequence: None,
    });
    assert_eq!(
        serde_json::from_value::<ServerFrame>(serde_json::to_value(&frame).unwrap()).unwrap(),
        frame
    );
    assert!(
        gateway
            .read_bot_conversation_file(&bot.id, &execution_id, &file.id, 0, 256)
            .await
            .is_err()
    );
    assert_eq!(
        gateway
            .bot_conversation_history(&bot.id, "", None)
            .await
            .err()
            .unwrap()
            .code,
        "invalid_session_id"
    );
    let chunk = gateway
        .read_bot_conversation_file(&bot.id, &child_id, &file.id, 0, 7)
        .await
        .unwrap();
    assert_eq!(chunk.data, b"private");
    let last = gateway
        .read_bot_conversation_file(
            &bot.id,
            &child_id,
            &file.id,
            chunk.next_offset.unwrap(),
            256,
        )
        .await
        .unwrap();
    assert_eq!(last.data, b" result");
    assert!(last.next_offset.is_none());
    assert_eq!(
        gateway
            .read_bot_conversation_file(&other.id, &child_id, &file.id, 0, 256)
            .await
            .unwrap_err()
            .code,
        "bot_conversation_unavailable"
    );
    assert!(
        gateway
            .read_bot_conversation_file(&bot.id, &child_id, "../result", 0, 256)
            .await
            .is_err()
    );
    assert!(
        gateway
            .read_bot_conversation_file(&bot.id, &child_id, &file.id, 0, usize::MAX)
            .await
            .is_err()
    );
    assert_eq!(operations.counts(), before);
    assert!(gateway.state.lock().await.sessions.is_empty());
    checkpoints
        .delete_sessions(std::slice::from_ref(&child_id))
        .await
        .unwrap();
    assert_eq!(
        gateway
            .read_bot_conversation_file(&bot.id, &child_id, &file.id, 0, 256)
            .await
            .unwrap_err()
            .code,
        "bot_conversation_unavailable"
    );
    gateway.shutdown().await;
}

#[tokio::test]
async fn private_conversation_catalog_pages_descendants_without_materializing_agents() {
    let (root, gateway, bot) = bots::gateway_with_bot().await;
    let (chats, checkpoints, operations) = {
        let state = gateway.state.lock().await;
        (
            state.chat_store.clone(),
            state.checkpoints.clone(),
            state.store.runtime_operations.clone(),
        )
    };
    let chat_id = chats
        .create(root.path().into(), vec![bot.id.clone()], None)
        .await
        .unwrap();
    let execution_id = chat_execution_id(&gateway, &chat_id, &bot.id).await;
    seed(&checkpoints, &execution_id).await;
    let mut expected = HashSet::from([execution_id.clone()]);
    for _ in 0..SESSION_PAGE_SIZE + 5 {
        let id = Uuid::new_v4().to_string();
        checkpoints
            .fork(&execution_id, 0, &Checkpoint::empty(&id))
            .await
            .unwrap();
        expected.insert(id);
    }
    let before = operations.counts();
    let first = gateway.bot_conversations(&bot.id, None).await.unwrap();
    assert_eq!(first.conversations.len(), SESSION_PAGE_SIZE);
    let last = gateway
        .bot_conversations(&bot.id, first.next_cursor)
        .await
        .unwrap();
    assert_eq!(last.conversations.len(), 6);
    assert!(last.next_cursor.is_none());
    let actual = first
        .conversations
        .into_iter()
        .chain(last.conversations)
        .map(|item| item.conversation_id)
        .collect::<HashSet<_>>();
    assert_eq!(actual, expected);
    assert_eq!(operations.counts(), before);
    assert!(gateway.state.lock().await.sessions.is_empty());
    gateway.shutdown().await;
}
