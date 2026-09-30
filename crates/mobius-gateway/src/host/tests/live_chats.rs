use mobius::middleware::sessions::LiveChats;

use super::*;

#[tokio::test]
async fn open_chats_of_one_bot_list_and_message_each_other() {
    let (root, gateway, bot) = super::bots::gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let sender = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let sibling = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let other_bot = gateway
        .state
        .lock()
        .await
        .bots
        .bots()
        .unwrap()
        .into_iter()
        .find(|other| other.id != bot.id)
        .unwrap();
    let foreign = gateway
        .create_session(&workspace, &other_bot.id)
        .await
        .unwrap();
    let hidden = gateway
        .create_session_with_id(
            Some(workspace.as_path()),
            &bot.id,
            Uuid::new_v4().to_string(),
            false,
            "routine",
        )
        .await
        .unwrap();
    let tool_count = async |host: &HostHandle| host.snapshot(None).await.unwrap().ready.tool_count;
    assert_eq!(
        tool_count(&sender).await,
        tool_count(&hidden).await + ["list_chats", "message_chat"].len()
    );
    gateway
        .rename_session(sibling.session_id(), "Backend API")
        .await
        .unwrap();
    let chats =
        crate::host::live_chats::GatewayLiveChats(Arc::downgrade(&gateway.state), gateway.access());

    let listed = chats.list(sender.session_id()).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|chat| (
                chat.session_id.as_str(),
                chat.title.as_deref(),
                chat.running
            ))
            .collect::<Vec<_>>(),
        [(sibling.session_id(), Some("Backend API"), false)]
    );
    assert!(
        chats
            .send(
                sender.session_id(),
                foreign.session_id(),
                "Hi".into(),
                &MessageAuthor::User,
                "foreign"
            )
            .await
            .is_err()
    );

    let target = format!("#{}", &sibling.session_id()[..8]);
    chats
        .send(
            sender.session_id(),
            &target,
            "The API now returns ids.".into(),
            &MessageAuthor::User,
            "peer-message",
        )
        .await
        .unwrap();
    sibling.wait_idle().await;
    let handle = format!("chat #{}", &sender.session_id()[..8]);
    let delivered = sibling.history_page(None).await.unwrap().records;
    assert!(delivered.iter().any(|record| matches!(
        &record.event.msg,
        EventMsg::Message(message) if message.text == "The API now returns ids."
            && matches!(
                &message.author,
                MessageAuthor::Source { source: mobius::protocol::MessageSource::Session { session_id }, handle: sent, .. }
                    if session_id == sender.session_id() && *sent == handle
            )
    )));
    gateway.shutdown().await;
}

#[tokio::test]
async fn peer_delivery_needs_the_current_bot_and_reports_the_agents_rejection() {
    let (root, gateway, bot) = super::bots::gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let target = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let other_bot = gateway
        .state
        .lock()
        .await
        .bots
        .bots()
        .unwrap()
        .into_iter()
        .find(|other| other.id != bot.id)
        .unwrap();
    gateway
        .reassign_session(target.session_id(), &other_bot.id)
        .await
        .unwrap();
    let peer = |target_turn_id: Option<&str>| Submission {
        id: Uuid::new_v4().to_string(),
        op: Op::Message {
            message: MessageSubmission {
                author: MessageAuthor::Source {
                    cause_id: None,
                    ancestry: Vec::new(),
                    message_id: Uuid::new_v4().to_string(),
                    source: mobius::protocol::MessageSource::Session {
                        session_id: "sender".into(),
                    },
                    handle: "chat #sender".into(),
                    symbol: None,
                },
                text: "Coordinate".into(),
                attachments: Vec::new(),
                reply: None,
                requested_delivery: None,
                target_turn_id: target_turn_id.map(Into::into),
            },
        },
    };

    let reassigned = target.deliver_source(peer(None), bot.id).await.unwrap_err();
    assert_eq!(reassigned.message, "the chat now belongs to another Bot");
    let rejected = target
        .deliver_source(peer(Some("stale-turn")), other_bot.id)
        .await
        .unwrap_err();
    assert_eq!(rejected.message, "message targeted a stale turn");
    gateway.shutdown().await;
}
