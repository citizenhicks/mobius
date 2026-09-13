use super::*;

#[tokio::test]
async fn private_conversation_reads_are_bot_scoped_without_selecting_a_chat() {
    let root = tempfile::tempdir().unwrap();
    let state_dir = root.path().join("state");
    let (server, grant) = configured_test_server(state_dir.clone()).await;
    let bot = server
        .bots
        .create_bot("Owner", "Owner", Default::default())
        .unwrap();
    let other = server
        .bots
        .create_bot("Other", "Other", Default::default())
        .unwrap();
    let (chats, _) = crate::chats::ChatStore::new(&state_dir, server.bots.clone()).unwrap();
    let chat_id = chats
        .create(root.path().into(), vec![bot.id.clone()], None)
        .await
        .unwrap();
    let conversation_id = chats.load(&chat_id).await.unwrap().unwrap().participants[0]
        .session_id
        .clone();
    let (store, _) = ConfigStore::open(state_dir.clone()).unwrap();
    let checkpoints = SqliteCheckpoint::new(store.checkpoints_path()).unwrap();
    checkpoints
        .save(&Checkpoint::empty(&conversation_id), &[], None)
        .await
        .unwrap();
    checkpoints
        .append_event(
            &conversation_id,
            1,
            &Event {
                submission_id: Some("private".into()),
                msg: EventMsg::ToolCallEnd(mobius::protocol::ToolCallEndEvent {
                    turn_id: "turn".into(),
                    call_id: "call".into(),
                    name: "read_file".into(),
                    output: "private file contents".into(),
                    is_error: false,
                }),
            },
        )
        .await
        .unwrap();
    let file = SessionFileStore::new(&state_dir)
        .publish_artifact(
            &conversation_id,
            "result.txt".into(),
            "text/plain".into(),
            b"private file",
        )
        .await
        .unwrap();
    let endpoint = format!("tcp://{}", server.config.listen)
        .parse::<Endpoint>()
        .unwrap();
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    let (connection, _) = GatewayClient::pair(
        &endpoint,
        grant.code,
        "Private history test",
        ClientKind::Ios,
    )
    .await
    .unwrap();
    let (sender, mut events) = connection.into_parts();
    wait_gateway_ready(&mut events).await;
    sender
        .send(ClientMessage::ListBotConversations {
            request_id: "list".into(),
            bot_id: bot.id.clone(),
            cursor: None,
        })
        .await
        .unwrap();
    let ServerMessage::BotConversations { bot_id, page, .. } =
        next_gateway_message(&mut events).await
    else {
        panic!("private list response")
    };
    assert_eq!(bot_id, bot.id);
    assert_eq!(page.conversations.len(), 1);
    assert_eq!(page.conversations[0].conversation_id, conversation_id);
    sender
        .send(ClientMessage::GetBotConversationHistory {
            request_id: "history".into(),
            bot_id: bot.id.clone(),
            conversation_id: conversation_id.clone(),
            before_sequence: None,
        })
        .await
        .unwrap();
    let ServerMessage::BotConversationHistory { records, .. } =
        next_gateway_message(&mut events).await
    else {
        panic!("private history response")
    };
    assert_eq!(records.len(), 1);
    assert!(
        records[0].blocks[0]
            .block
            .text
            .contains("private file contents")
    );
    sender
        .send(ClientMessage::ReadBotConversationFile {
            request_id: "file".into(),
            bot_id: bot.id.clone(),
            conversation_id: conversation_id.clone(),
            file_id: file.id.clone(),
            offset: 0,
            max_bytes: 100,
        })
        .await
        .unwrap();
    let ServerMessage::SessionFileChunk {
        session_id, data, ..
    } = next_gateway_message(&mut events).await
    else {
        panic!("private file response")
    };
    assert_eq!(session_id, conversation_id);
    assert_eq!(data, b"private file");
    sender
        .send(ClientMessage::GetBotConversationHistory {
            request_id: "wrong-owner".into(),
            bot_id: other.id,
            conversation_id: conversation_id.clone(),
            before_sequence: None,
        })
        .await
        .unwrap();
    let ServerMessage::Rejected {
        request_id, code, ..
    } = next_gateway_message(&mut events).await
    else {
        panic!("wrong owner rejected")
    };
    assert_eq!(request_id, "wrong-owner");
    assert_eq!(code, "bot_conversation_unavailable");
    sender
        .send(ClientMessage::ReadSessionFile {
            request_id: "ordinary-file".into(),
            session_id: conversation_id,
            file_id: file.id,
            offset: 0,
            max_bytes: 100,
        })
        .await
        .unwrap();
    let ServerMessage::Rejected { request_id, .. } = next_gateway_message(&mut events).await else {
        panic!("ordinary file route still requires selected Chat")
    };
    assert_eq!(request_id, "ordinary-file");
    sender
        .send(ClientMessage::ListSessions {
            request_id: "public".into(),
        })
        .await
        .unwrap();
    let ServerMessage::Sessions { sessions, .. } = next_gateway_message(&mut events).await else {
        panic!("public list response")
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, chat_id);
    shutdown.send(()).unwrap();
    serving.await.unwrap().unwrap();
}
