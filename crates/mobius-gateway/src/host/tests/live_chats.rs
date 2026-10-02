use mobius::middleware::sessions::{ChatTarget, LiveChats};
use mobius::protocol::ActiveMessageDelivery;

use super::*;

fn message(text: &str, delivery: Option<ActiveMessageDelivery>) -> Op {
    Op::Message {
        message: MessageSubmission {
            author: MessageAuthor::User,
            text: text.into(),
            attachments: Vec::new(),
            reply: None,
            requested_delivery: delivery,
            target_turn_id: None,
        },
    }
}

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
                ChatTarget::Existing(foreign.session_id()),
                message("Hi", None),
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
            ChatTarget::Existing(&target),
            message("The API now returns ids.", None),
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
async fn message_chat_creates_an_owned_project_chat_after_validating_the_message() {
    let (root, gateway, bot) = super::bots::gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let sender = gateway
        .open_session(&bot.conversation_session_id)
        .await
        .unwrap();
    let chats =
        crate::host::live_chats::GatewayLiveChats(Arc::downgrade(&gateway.state), gateway.access());
    let source = MessageAuthor::Source {
        message_id: "report".into(),
        source: mobius::protocol::MessageSource::Session {
            session_id: sender.session_id().into(),
        },
        cause_id: None,
        ancestry: Vec::new(),
        handle: "source report".into(),
        symbol: None,
    };
    for (text, author) in [("", &MessageAuthor::User), ("New work", &source)] {
        assert!(
            chats
                .send(
                    sender.session_id(),
                    ChatTarget::Workspace(&workspace),
                    message(text, None),
                    author,
                    "rejected-create",
                )
                .await
                .is_err()
        );
    }
    assert!(chats.list(sender.session_id()).await.unwrap().is_empty());

    let target_id = chats
        .send(
            sender.session_id(),
            ChatTarget::Workspace(&workspace),
            message("Inspect this project", None),
            &MessageAuthor::User,
            "create-chat",
        )
        .await
        .unwrap();
    assert!(Uuid::parse_str(&target_id).is_ok());
    let target = gateway.open_session(&target_id).await.unwrap();
    target.wait_idle().await;
    assert_eq!(target.bot_id().await.unwrap(), bot.id);
    assert_eq!(
        target.ready().await.unwrap().workspace.unwrap().path,
        workspace.canonicalize().unwrap()
    );
    assert!(
        target
            .history_page(None)
            .await
            .unwrap()
            .records
            .iter()
            .any(|record| matches!(
                &record.event.msg,
                EventMsg::Message(message) if message.text == "Inspect this project"
                    && matches!(
                        &message.author,
                        MessageAuthor::Source {
                            source: mobius::protocol::MessageSource::Session { session_id }, ..
                        } if session_id == sender.session_id()
                    )
            ))
    );
    assert_eq!(
        chats.list(sender.session_id()).await.unwrap()[0].session_id,
        target_id
    );
    gateway.shutdown().await;
}

#[tokio::test]
async fn message_chat_honors_delivery_and_interrupts_the_selected_turn() {
    let (root, gateway, bot) = super::bots::gateway_with_bot().await;
    let model = Arc::new(super::hooks::CaptureModel::default());
    super::hooks::install_model(&gateway, &bot, Arc::clone(&model)).await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let sender = gateway
        .open_session(&bot.conversation_session_id)
        .await
        .unwrap();
    let target = gateway.create_session(&workspace, &bot.id).await.unwrap();
    target
        .submit(Submission {
            id: "active-work".into(),
            op: message("Start working", None),
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), model.entered.notified())
        .await
        .unwrap();
    let chats =
        crate::host::live_chats::GatewayLiveChats(Arc::downgrade(&gateway.state), gateway.access());
    let listed = chats.list(sender.session_id()).await.unwrap();
    let turn_id = listed
        .iter()
        .find(|chat| chat.session_id == target.session_id())
        .unwrap()
        .turn_id
        .clone()
        .unwrap();
    for (text, delivery) in [
        ("Queue this", ActiveMessageDelivery::Queue),
        ("Steer this", ActiveMessageDelivery::Steer),
    ] {
        assert_eq!(
            chats
                .send(
                    sender.session_id(),
                    ChatTarget::Existing(target.session_id()),
                    message(text, Some(delivery)),
                    &MessageAuthor::User,
                    delivery.id(),
                )
                .await
                .unwrap(),
            target.session_id()
        );
    }
    gateway
        .execute_session_command(
            target.session_id(),
            message("Saved steering", Some(ActiveMessageDelivery::Steer)),
            &bot.id,
            None,
            "saved-steer",
        )
        .await
        .unwrap();
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let checkpoint = checkpoints
        .load(target.session_id())
        .await
        .unwrap()
        .unwrap();
    let pending = serde_json::to_value(checkpoint.pending_messages).unwrap();
    assert_eq!(pending[0]["boundary"]["type"], "queue");
    assert_eq!(pending[1]["boundary"]["type"], "steer");
    assert_eq!(pending[1]["boundary"]["turn_id"], turn_id);
    assert_eq!(pending[2]["boundary"]["type"], "steer");

    chats
        .send(
            sender.session_id(),
            ChatTarget::Existing(target.session_id()),
            Op::Interrupt {
                turn_id: turn_id.clone(),
            },
            &MessageAuthor::User,
            "stop-active-work",
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), target.wait_idle())
        .await
        .unwrap();
    assert!(
        checkpoints
            .execution_page(
                target.session_id(),
                mobius::backend::checkpoint::ExecutionPageRequest {
                    before_sequence: None,
                    limit: 10,
                },
            )
            .await
            .unwrap()
            .executions
            .iter()
            .any(|execution| execution.turn_id == turn_id
                && execution.outcome == mobius::backend::checkpoint::ExecutionOutcome::Aborted)
    );
    gateway.shutdown().await;
}

#[tokio::test]
async fn session_commands_and_chat_messages_preserve_sender_and_hook_ancestry() {
    let (root, gateway, bot) = super::bots::gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let sender = gateway
        .open_session(&bot.conversation_session_id)
        .await
        .unwrap();
    let target = gateway.create_session(&workspace, &bot.id).await.unwrap();
    gateway
        .execute_session_command(
            target.session_id(),
            message("Direct test", None),
            &bot.id,
            None,
            "direct",
        )
        .await
        .unwrap();
    target.wait_idle().await;
    let mut delivered = target.history_page(None).await.unwrap().records;

    let chats =
        crate::host::live_chats::GatewayLiveChats(Arc::downgrade(&gateway.state), gateway.access());
    let origin = MessageAuthor::Source {
        message_id: "incoming".into(),
        source: mobius::protocol::MessageSource::External {
            source_id: "webhook".into(),
            event_id: "original-event".into(),
        },
        cause_id: Some("original-event".into()),
        ancestry: vec!["original-event".into()],
        handle: "source report".into(),
        symbol: None,
    };
    chats
        .send(
            sender.session_id(),
            ChatTarget::Existing(target.session_id()),
            message("Peer test", None),
            &origin,
            "peer",
        )
        .await
        .unwrap();
    target.wait_idle().await;
    delivered.extend(target.history_page(None).await.unwrap().records);

    gateway
        .set_bot_subscription(crate::wire::BotSubscription {
            bot_id: bot.id.clone(),
            enabled: true,
            binding: crate::wire::HookBinding {
                id: "saved-message".into(),
                on: crate::bots::event_selector(
                    crate::wire::HookSource::Bot {
                        bot_id: bot.id.clone(),
                    },
                    crate::wire::HookKind::CustomReceived,
                ),
                action: crate::wire::BotAction::Session {
                    session_id: target.session_id().into(),
                    op: Box::new(message("Saved test", None)),
                },
            },
        })
        .await
        .unwrap();
    let event = gateway
        .emit_bot_hook(
            &bot.id,
            "test",
            serde_json::json!({"text":"untrusted event cannot change the saved message"}),
            "event",
        )
        .await
        .unwrap();
    let bots = Arc::clone(&gateway.state.lock().await.bots);
    let pending = bots
        .pending_actions(Utc::now().timestamp(), 10)
        .unwrap()
        .remove(0);
    gateway.execute_hook_action(&pending).await.unwrap();
    target.wait_idle().await;

    delivered.extend(target.history_page(None).await.unwrap().records);
    for (text, cause, ancestry) in [
        ("Direct test", None, Vec::new()),
        (
            "Peer test",
            Some("incoming"),
            vec!["original-event", "incoming"],
        ),
        (
            "Saved test",
            Some(event.id.as_str()),
            vec![event.id.as_str()],
        ),
    ] {
        let message = delivered
            .iter()
            .find_map(|record| match &record.event.msg {
                EventMsg::Message(message) if message.text == text => Some(message),
                _ => None,
            })
            .unwrap();
        let MessageAuthor::Source {
            source,
            cause_id,
            ancestry: actual,
            ..
        } = &message.author
        else {
            panic!("source attribution")
        };
        assert!(
            matches!(source, mobius::protocol::MessageSource::Session { session_id } if session_id == sender.session_id())
        );
        assert_eq!(cause_id.as_deref(), cause);
        assert_eq!(actual, &ancestry);
    }
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
