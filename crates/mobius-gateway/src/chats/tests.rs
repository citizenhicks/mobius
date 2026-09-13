use super::*;

#[test]
fn previous_chat_schema_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("chats.sqlite3");
    let connection = rusqlite::Connection::open(path).unwrap();
    connection.pragma_update(None, "user_version", 1).unwrap();
    drop(connection);

    let error = storage::open(root.path()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unsupported chat storage schema 1")
    );
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

async fn fixture() -> (
    tempfile::TempDir,
    ChatStore,
    Vec<crate::wire::BotRecord>,
    String,
) {
    let root = tempfile::tempdir().unwrap();
    let bots = Arc::new(BotStore::open(root.path()).unwrap());
    let members = ["Alice", "Bob", "Outsider"]
        .map(|name| {
            bots.create_bot(
                name,
                "Test member",
                crate::wire::AgentComposition::default(),
            )
            .unwrap()
        })
        .to_vec();
    let (chats, _) = ChatStore::new(root.path(), bots).unwrap();
    let id = chats
        .create(
            root.path().to_owned(),
            members[..2].iter().map(|bot| bot.id.clone()).collect(),
            Some(&members[1].id),
        )
        .await
        .unwrap();
    (root, chats, members, id)
}

fn reply(text: &str, recipients: &[String]) -> ChatRunOutcome {
    ChatRunOutcome::Succeeded {
        summary: serde_json::json!({"text": text, "recipient_bot_ids": recipients}).to_string(),
    }
}

#[tokio::test]
async fn recipients_are_explicit_members_and_unaddressed_humans_use_the_primary() {
    let (_root, chats, bots, id) = fixture().await;
    chats
        .post_user(
            &id,
            "human".into(),
            user("@alice this is a literal handle"),
            &[],
        )
        .await
        .unwrap();
    let pending = chats.pending_deliveries(&id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, bots[1].id);
    assert_eq!(pending[0].1.user_message_id, "human");
    let chat = chats.load(&id).await.unwrap().unwrap();
    let bob_session = chat.session_id(&bots[1].id).unwrap();
    assert_ne!(bob_session, id);
    assert_eq!(
        chats
            .chat_for_session(bob_session)
            .await
            .unwrap()
            .unwrap()
            .id,
        id
    );
    assert!(
        !chats
            .settle_delivery("human", bob_session, &bots[0].id, reply("forged", &[]))
            .await
            .unwrap()
    );
    assert!(
        chats
            .settle_delivery(
                "human",
                bob_session,
                &bots[1].id,
                reply("Please review @alice", &[bots[0].id.clone()])
            )
            .await
            .unwrap()
    );
    assert!(
        !chats
            .settle_delivery("human", bob_session, &bots[1].id, reply("duplicate", &[]))
            .await
            .unwrap()
    );
    let next = chats.pending_deliveries(&id).await.unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].0, bots[0].id);
    assert_eq!(next[0].1.user_message_id, "human");
    assert!(
        matches!(&next[0].1.message.author, MessageAuthor::Peer { session_id, handle, .. } if session_id == &id && handle == &bots[1].handle)
    );
    let stored = chats.message_by_id(&id, "human").await.unwrap().unwrap();
    assert_eq!(
        stored.recipients,
        vec![bots[1].id.clone()],
        "accepted recipients survive acknowledgement"
    );
    for recipients in [vec![bots[2].id.clone()], vec![bots[0].id.clone(); 2]] {
        assert!(
            chats
                .post_user(&id, "invalid".into(), user("Invalid"), &recipients)
                .await
                .is_err()
        );
    }
    assert!(!chats.contains_message(&id, "invalid").await.unwrap());
}

#[tokio::test]
async fn stop_survives_restart_and_late_completion_cannot_publish_or_wake_a_member() {
    let (root, chats, bots, id) = fixture().await;
    let other = chats
        .create(root.path().to_owned(), vec![bots[1].id.clone()], None)
        .await
        .unwrap();
    for chat in [&id, &other] {
        chats
            .post_user(chat, "request".into(), user("Work"), &[])
            .await
            .unwrap();
    }
    let stopped = chats.load(&id).await.unwrap().unwrap();
    chats.cancel_pending(&id).await.unwrap();
    let (reopened, _) = ChatStore::new(root.path(), Arc::clone(&chats.bots)).unwrap();
    assert!(reopened.pending_deliveries(&id).await.unwrap().is_empty());
    assert_eq!(reopened.pending_deliveries(&other).await.unwrap().len(), 1);
    assert!(
        !reopened
            .settle_delivery(
                "request",
                stopped.session_id(&bots[1].id).unwrap(),
                &bots[1].id,
                reply("Late", &[bots[0].id.clone()])
            )
            .await
            .unwrap()
    );
    assert_eq!(reopened.load(&id).await.unwrap().unwrap().sequence, 1);
    reopened
        .post_user(&id, "request".into(), user("Retry"), &[])
        .await
        .unwrap();
    assert!(
        reopened.pending_deliveries(&id).await.unwrap().is_empty(),
        "retrying a cancelled accepted message must not revive it"
    );
}

#[tokio::test]
async fn malformed_handoffs_and_depth_limit_never_route_text_mentions() {
    let (_root, chats, bots, id) = fixture().await;
    let chat = chats.load(&id).await.unwrap().unwrap();
    for (index, summary) in [
        "@alice plain text".to_owned(),
        serde_json::json!({"text":"Spoof", "recipient_bot_ids":[bots[2].id]}).to_string(),
        serde_json::json!({"text":"Self", "recipient_bot_ids":[bots[1].id]}).to_string(),
        serde_json::json!({"text":"Extra", "recipient_bot_ids":[], "sender":"Alice"}).to_string(),
    ]
    .into_iter()
    .enumerate()
    {
        let message_id = index.to_string();
        chats
            .post_user(&id, message_id.clone(), user("Check"), &[])
            .await
            .unwrap();
        chats
            .settle_delivery(
                &message_id,
                chat.session_id(&bots[1].id).unwrap(),
                &bots[1].id,
                ChatRunOutcome::Succeeded { summary },
            )
            .await
            .unwrap();
        assert!(chats.pending_deliveries(&id).await.unwrap().is_empty());
    }
    chats
        .post_user(&id, "chain".into(), user("Discuss"), &[])
        .await
        .unwrap();
    for depth in 0..=MAX_REPLY_DEPTH {
        let (sender, message) = chats.pending_deliveries(&id).await.unwrap().remove(0);
        let recipient = if sender == bots[0].id {
            &bots[1].id
        } else {
            &bots[0].id
        };
        chats
            .settle_delivery(
                &message.id,
                chat.session_id(&sender).unwrap(),
                &sender,
                reply("Next", std::slice::from_ref(recipient)),
            )
            .await
            .unwrap();
        assert_eq!(
            chats.pending_deliveries(&id).await.unwrap().is_empty(),
            depth == MAX_REPLY_DEPTH
        );
    }
}

#[tokio::test]
async fn inline_events_keep_public_targets_and_durable_bot_identity_without_duplicate_finals() {
    let (root, chats, bots, _id) = fixture().await;
    let id = chats
        .create(root.path().to_owned(), vec![bots[0].id.clone()], None)
        .await
        .unwrap();
    let chat = chats.load(&id).await.unwrap().unwrap();
    let session = chat.session_id(&bots[0].id).unwrap();
    chats
        .post_user(&id, "human".into(), user("Explain"), &[])
        .await
        .unwrap();
    let event = Event {
        submission_id: Some("human".into()),
        msg: EventMsg::AssistantMessage(mobius::protocol::AssistantMessageEvent {
            session_id: session.into(),
            turn_id: "turn".into(),
            model_step_id: "step".into(),
            content: vec![mobius::protocol::ModelStepContent {
                output_index: 0,
                part_index: 0,
                annotations: Vec::new(),
                phase: ModelStepContentPhase::FinalAnswer,
                text: "Answer".into(),
            }],
            message_target: Some(MessageTarget {
                checkpoint_sequence: 999,
                batch_item_count: 3,
            }),
        }),
    };
    chats.observe_event(session, &event).await.unwrap();
    assert!(
        chats
            .settle_delivery(
                "human",
                session,
                &bots[0].id,
                ChatRunOutcome::Succeeded {
                    summary: "Answer".into()
                }
            )
            .await
            .unwrap()
    );
    let records = chats
        .event_page(
            &id,
            EventPageRequest {
                before_sequence: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(records.events.len(), 2);
    let EventMsg::AssistantMessage(message) = &records.events[0].event.msg else {
        panic!("inline answer missing")
    };
    assert_eq!(message.session_id, id);
    assert_eq!(
        message.message_target,
        Some(MessageTarget {
            checkpoint_sequence: 2,
            batch_item_count: 1
        })
    );
    assert_eq!(
        records.events[0].event.submission_id.as_deref(),
        Some("human")
    );
    let metadata = chats.message_by_sequence(&id, 2).await.unwrap().unwrap();
    assert!(
        matches!(metadata.message.author, MessageAuthor::Peer { handle, .. } if handle == bots[0].handle)
    );
    chats.reassign(&id, &bots[1].id).await.unwrap();
    let reassigned = chats.load(&id).await.unwrap().unwrap();
    assert_eq!(
        reassigned.primary_bot_id.as_deref(),
        Some(bots[1].id.as_str())
    );
    assert_eq!(reassigned.retired_participants[0].session_id, session);
    assert!(chats.chat_for_session(session).await.unwrap().is_none());
    chats.observe_event(session, &event).await.unwrap();
    assert_eq!(chats.load(&id).await.unwrap().unwrap().sequence, 2);
}

#[tokio::test]
async fn failed_single_bot_delivery_is_visible_and_not_retried() {
    let (root, chats, bots, _id) = fixture().await;
    let id = chats
        .create(root.path().to_owned(), vec![bots[0].id.clone()], None)
        .await
        .unwrap();
    chats
        .post_user(&id, "human".into(), user("Work"), &[])
        .await
        .unwrap();
    let chat = chats.load(&id).await.unwrap().unwrap();

    assert!(
        chats
            .settle_delivery(
                "human",
                chat.session_id(&bots[0].id).unwrap(),
                &bots[0].id,
                ChatRunOutcome::Failed {
                    message: "submission rejected".into()
                }
            )
            .await
            .unwrap()
    );
    assert!(chats.pending_deliveries(&id).await.unwrap().is_empty());
    let page = chats
        .message_page(
            &id,
            EventPageRequest {
                before_sequence: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.events.len(), 2);
    let EventMsg::Message(message) = &page.events[0].event.msg else {
        panic!("missing failure message")
    };
    assert_eq!(message.text, "Could not finish: submission rejected");
}

#[tokio::test]
async fn steering_acknowledges_only_its_delivery_and_keeps_the_original_turn_pending() {
    let (_root, chats, bots, id) = fixture().await;
    for message in ["initial", "steer"] {
        chats
            .post_user(&id, message.into(), user(message), &[])
            .await
            .unwrap();
    }
    let chat = chats.load(&id).await.unwrap().unwrap();
    chats
        .observe_event(
            chat.session_id(&bots[1].id).unwrap(),
            &Event {
                submission_id: Some("steer".into()),
                msg: EventMsg::Message(MessageEvent {
                    author: MessageAuthor::User,
                    delivery: MessageDelivery::Steer,
                    text: "New priority".into(),
                    attachments: Vec::new(),
                    reply: None,
                    message_target: None,
                }),
            },
        )
        .await
        .unwrap();
    let pending = chats.pending_deliveries(&id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].1.id, "initial");
    assert_eq!(chats.load(&id).await.unwrap().unwrap().sequence, 2);
}

#[tokio::test]
async fn bounded_context_retains_original_request_and_ignores_busy_lifecycle_history() {
    let (_root, chats, bots, id) = fixture().await;
    let chat = chats.load(&id).await.unwrap().unwrap();
    let session = chat.session_id(&bots[1].id).unwrap();
    let original = format!("{} THE ORIGINAL REQUEST", "x".repeat(600));
    chats
        .post_user(&id, "original".into(), user(&original), &[])
        .await
        .unwrap();
    for index in 0..130 {
        chats
            .observe_event(
                session,
                &Event {
                    submission_id: Some("original".into()),
                    msg: EventMsg::TurnStarted(mobius::protocol::TurnStartedEvent {
                        turn_id: index.to_string(),
                        model_context_window: None,
                    }),
                },
            )
            .await
            .unwrap();
    }
    let context = chats
        .chat_context(&bots[1].id, session)
        .await
        .unwrap()
        .unwrap();
    assert!(
        context.contains(&original),
        "catalog preview must not become the original request"
    );
    assert!(context.len() < CONTEXT_BYTES + 256);
    assert!(
        chats
            .chat_context(&bots[0].id, session)
            .await
            .unwrap()
            .is_none()
    );
    let mut reply_message = user("Reply");
    reply_message.reply = Some(mobius::protocol::MessageReply {
        target: MessageTarget {
            checkpoint_sequence: 2,
            batch_item_count: 1,
        },
        text: "not a message".into(),
    });
    assert!(
        chats
            .post_user(&id, "invalid-target".into(), reply_message, &[])
            .await
            .is_err()
    );
    assert!(!chats.contains_message(&id, "invalid-target").await.unwrap());
}

#[tokio::test]
async fn queue_limit_rejects_humans_atomically_but_still_publishes_completed_work() {
    let (_root, chats, bots, id) = fixture().await;
    let both = bots[..2]
        .iter()
        .map(|bot| bot.id.clone())
        .collect::<Vec<_>>();
    for index in 0..MAX_PENDING {
        chats
            .post_user(&id, index.to_string(), user("Work"), &both)
            .await
            .unwrap();
    }
    assert!(
        chats
            .post_user(&id, "overfull".into(), user("No capacity"), &[])
            .await
            .is_err()
    );
    assert!(!chats.contains_message(&id, "overfull").await.unwrap());
    let chat = chats.load(&id).await.unwrap().unwrap();
    chats
        .settle_delivery(
            "0",
            chat.session_id(&bots[1].id).unwrap(),
            &bots[1].id,
            reply("Finished", &[bots[0].id.clone()]),
        )
        .await
        .unwrap();
    let latest = chats
        .message_page(
            &id,
            EventPageRequest {
                before_sequence: None,
                limit: 1,
            },
        )
        .await
        .unwrap();
    let EventMsg::Message(message) = &latest.events[0].event.msg else {
        panic!("missing final")
    };
    assert!(message.text.starts_with("Finished"));
    assert!(message.text.contains("queue is full"));
    assert_eq!(
        chats.pending_deliveries(&id).await.unwrap().len(),
        MAX_PENDING * 2 - 1
    );
}
