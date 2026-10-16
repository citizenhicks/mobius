use mobius::middleware::sessions::{ChatTarget, LiveChats, PeerCommand};
use mobius::protocol::ActiveMessageDelivery;

use super::*;

fn peer_message<'a>(target: ChatTarget<'a>, text: &str) -> PeerCommand<'a> {
    PeerCommand::Message {
        target,
        text: text.into(),
        delivery: ActiveMessageDelivery::Steer,
    }
}

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
            peer_message(ChatTarget::Existing(target.session_id()), "Peer test"),
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
