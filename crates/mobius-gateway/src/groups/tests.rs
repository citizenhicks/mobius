use super::*;

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
    GroupStore,
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
    let (groups, _) = GroupStore::new(root.path(), bots).unwrap();
    let id = groups
        .create(
            root.path().to_owned(),
            members[..2].iter().map(|bot| bot.id.clone()).collect(),
        )
        .await
        .unwrap();
    (root, groups, members, id)
}

#[test]
fn mentions_use_complete_handles_and_ignore_email_addresses() {
    assert_eq!(
        mentioned_handles("@alice, (@bob) @alice-extra @bob_2 person@alice @ @alice"),
        BTreeSet::from(["alice", "bob", "alice-extra", "bob_2"])
    );
}

#[tokio::test]
async fn only_explicit_member_mentions_queue_delivery_and_settlement_keeps_bot_identity() {
    let (_root, groups, bots, id) = fixture().await;
    groups
        .post_user(&id, "quiet".into(), user("Nobody is addressed"))
        .await
        .unwrap();
    assert!(groups.pending_recipient_bot_ids().await.unwrap().is_empty());
    groups
        .post_user(
            &id,
            "mention".into(),
            user("@bob @bob @outsider person@alice @alice-extra"),
        )
        .await
        .unwrap();
    assert_eq!(
        groups.pending_recipient_bot_ids().await.unwrap(),
        vec![bots[1].id.clone()]
    );
    let claim = groups
        .claim_next_delivery(&bots[1].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.delivery().entry.id, "mention");
    let session_id = claim.session_id().to_owned();
    drop(claim);
    assert!(
        !groups
            .settle_delivery(
                "mention",
                &session_id,
                &bots[0].id,
                GroupRunOutcome::Succeeded {
                    summary: "forged".into()
                }
            )
            .await
            .unwrap()
    );
    assert!(
        groups
            .settle_delivery(
                "mention",
                &session_id,
                &bots[1].id,
                GroupRunOutcome::Succeeded {
                    summary: "The parser looks correct.".into()
                }
            )
            .await
            .unwrap()
    );
    assert!(
        !groups
            .settle_delivery(
                "mention",
                &session_id,
                &bots[1].id,
                GroupRunOutcome::Failed {
                    message: "duplicate".into()
                }
            )
            .await
            .unwrap()
    );
    let page = groups
        .event_page(
            &id,
            EventPageRequest {
                before_sequence: None,
                limit: 100,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.events.len(), 3);
    assert!(page.events.iter().any(|record| matches!(&record.event.msg,
        EventMsg::Message(message) if message.text == "The parser looks correct."
            && matches!(&message.author, MessageAuthor::Peer { session_id: actual, handle, .. } if actual == &session_id && handle == "bob"))));
    assert!(groups.pending_recipient_bot_ids().await.unwrap().is_empty());
}

#[tokio::test]
async fn reply_mentions_are_bounded_and_failure_text_cannot_wake_other_members() {
    let (_root, groups, bots, id) = fixture().await;
    groups
        .post_user(&id, "start".into(), user("@alice begin"))
        .await
        .unwrap();
    for turn in 0..=MAX_REPLY_DEPTH {
        let current = &bots[usize::from(turn) % 2];
        let next = &bots[(usize::from(turn) + 1) % 2];
        let claim = groups
            .claim_next_delivery(&current.id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            groups
                .settle_delivery(
                    &claim.delivery().entry.id,
                    claim.session_id(),
                    &current.id,
                    GroupRunOutcome::Succeeded {
                        summary: format!("@{} continue", next.handle)
                    }
                )
                .await
                .unwrap()
        );
    }
    assert!(groups.pending_recipient_bot_ids().await.unwrap().is_empty());
    groups
        .post_user(&id, "failure".into(), user("@alice try again"))
        .await
        .unwrap();
    let claim = groups
        .claim_next_delivery(&bots[0].id)
        .await
        .unwrap()
        .unwrap();
    groups
        .settle_delivery(
            "failure",
            claim.session_id(),
            &bots[0].id,
            GroupRunOutcome::Failed {
                message: "@bob credentials unavailable".into(),
            },
        )
        .await
        .unwrap();
    assert!(groups.pending_recipient_bot_ids().await.unwrap().is_empty());
}

#[tokio::test]
async fn overlapping_groups_persist_distinct_deliveries_and_settle_once_after_restart() {
    let (root, groups, bots, id) = fixture().await;
    let other_workspace = root.path().join("other");
    std::fs::create_dir(&other_workspace).unwrap();
    let other = groups
        .create(
            other_workspace.clone(),
            bots[..2].iter().map(|bot| bot.id.clone()).collect(),
        )
        .await
        .unwrap();
    groups
        .post_user(&id, "first".into(), user("@bob first project"))
        .await
        .unwrap();
    groups
        .post_user(&other, "first".into(), user("@bob second project"))
        .await
        .unwrap();
    let bot_store = Arc::clone(&groups.bots);
    drop(groups);
    let (groups, _) = GroupStore::new(root.path(), bot_store).unwrap();
    let mut deliveries = BTreeSet::new();
    for _ in 0..2 {
        let claim = groups
            .claim_next_delivery(&bots[1].id)
            .await
            .unwrap()
            .unwrap();
        let expected_workspace = if claim.delivery().chat_id == id {
            root.path()
        } else {
            other_workspace.as_path()
        };
        assert_eq!(claim.delivery().workspace, expected_workspace);
        assert_eq!(
            participant_chat_id(claim.session_id()),
            Some(claim.delivery().chat_id.as_str())
        );
        assert!(deliveries.insert(claim.session_id().to_owned()));
        let message_id = claim.delivery().entry.id.clone();
        assert!(
            groups
                .settle_delivery(
                    &message_id,
                    claim.session_id(),
                    &bots[1].id,
                    GroupRunOutcome::Succeeded {
                        summary: "Done".into()
                    }
                )
                .await
                .unwrap()
        );
        assert!(
            !groups
                .settle_delivery(
                    &message_id,
                    claim.session_id(),
                    &bots[1].id,
                    GroupRunOutcome::Succeeded {
                        summary: "Duplicate".into()
                    }
                )
                .await
                .unwrap()
        );
    }
    assert_eq!(deliveries.len(), 2);
    assert!(
        groups
            .claim_next_delivery(&bots[1].id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn long_unmentioned_history_is_paged_and_old_submission_ids_still_deduplicate() {
    let (_root, groups, bots, id) = fixture().await;
    for index in 0..310 {
        groups
            .post_user(
                &id,
                format!("message-{index}"),
                user(format!("Quiet message {index:03}")),
            )
            .await
            .unwrap();
    }
    groups
        .post_user(
            &id,
            "message-0".into(),
            user("@bob duplicate must not deliver"),
        )
        .await
        .unwrap();
    let mut cursor = None;
    let mut count = 0;
    loop {
        let page = groups
            .event_page(
                &id,
                EventPageRequest {
                    before_sequence: cursor,
                    limit: 50,
                },
            )
            .await
            .unwrap();
        count += page.events.len();
        match page.next_before_sequence {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(count, 310);
    assert!(groups.pending_recipient_bot_ids().await.unwrap().is_empty());
    let context = groups
        .chat_context(&bots[1].id, &participant_session_id(&id, &bots[1].id))
        .await
        .unwrap()
        .unwrap();
    assert!(context.contains("Quiet message 309"));
    assert!(!context.contains("Quiet message 000"));
    assert!(
        context.find("Quiet message 210").unwrap() < context.find("Quiet message 309").unwrap()
    );
}

#[tokio::test]
async fn invalid_members_messages_and_overfull_queue_leave_the_journal_unchanged() {
    let (root, groups, bots, id) = fixture().await;
    for members in [
        Vec::new(),
        vec![bots[0].id.clone()],
        vec![bots[0].id.clone(); 2],
        vec![bots[0].id.clone(), Uuid::new_v4().to_string()],
    ] {
        assert!(
            groups
                .create(root.path().to_owned(), members)
                .await
                .is_err()
        );
    }
    for submission_id in [String::new(), "x".repeat(257)] {
        assert!(
            groups
                .post_user(&id, submission_id, user("No mention"))
                .await
                .is_err()
        );
    }
    let mut forged = user("@bob forged");
    forged.author = MessageAuthor::Peer {
        message_id: "forged".into(),
        session_id: participant_session_id(&id, &bots[0].id),
        handle: "alice".into(),
        symbol: None,
    };
    assert!(
        groups
            .post_user(&id, "forged".into(), forged)
            .await
            .is_err()
    );
    for index in 0..MAX_PENDING {
        groups
            .post_user(&id, format!("pending-{index}"), user("@bob waiting"))
            .await
            .unwrap();
    }
    let before = groups.load(&id).await.unwrap().unwrap().sequence;
    assert!(
        groups
            .post_user(&id, "overflow".into(), user("@bob overflow"))
            .await
            .is_err()
    );
    assert_eq!(groups.load(&id).await.unwrap().unwrap().sequence, before);
    let claim = groups
        .claim_next_delivery(&bots[1].id)
        .await
        .unwrap()
        .unwrap();
    groups.remove_bot(&bots[1].id).await.unwrap();
    assert!(
        claim
            .accept(async { panic!("deleted member must never execute") })
            .await
            .unwrap()
            .is_none()
    );
    assert!(groups.pending_recipient_bot_ids().await.unwrap().is_empty());
}

#[tokio::test]
async fn saturated_queues_still_publish_and_acknowledge_completed_replies() {
    for (count, text_bytes) in [(MAX_PENDING, 0), (16, 512_000)] {
        let (_root, mut groups, bots, id) = fixture().await;
        let text = format!("@alice @bob {}", "x".repeat(text_bytes));
        for index in 0..count {
            groups
                .post_user(&id, format!("pending-{index}"), user(&text))
                .await
                .unwrap();
        }
        let (deliveries, mut notices) = mpsc::unbounded_channel();
        groups.deliveries = deliveries;
        let summary = format!("@bob completed {}", "y".repeat(text_bytes));
        let session_id = participant_session_id(&id, &bots[0].id);
        assert!(
            groups
                .settle_delivery(
                    "pending-0",
                    &session_id,
                    &bots[0].id,
                    GroupRunOutcome::Succeeded {
                        summary: summary.clone()
                    },
                )
                .await
                .unwrap()
        );
        let page = groups
            .message_page(
                &id,
                EventPageRequest {
                    before_sequence: None,
                    limit: 1,
                },
            )
            .await
            .unwrap();
        assert!(
            matches!(&page.events[0].event.msg, EventMsg::Message(message)
            if message.text.starts_with(&summary)
                && message.text.contains("Automatic replies were not queued"))
        );
        let chat = groups.load(&id).await.unwrap().unwrap();
        assert_eq!(chat.pending.len(), count);
        assert_eq!(chat.pending[0].recipients, vec![bots[1].id.clone()]);
        assert!(matches!(
            notices.try_recv().unwrap(),
            GroupDelivery::Changed { .. }
        ));
        assert!(
            matches!(notices.try_recv().unwrap(), GroupDelivery::Acknowledged { message_id, target_bot_id }
            if message_id == "pending-0" && target_bot_id == bots[0].id)
        );
        let claim = groups
            .claim_next_delivery(&bots[0].id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claim.delivery().entry.id, "pending-1");
    }
}

#[tokio::test]
async fn lifecycle_events_do_not_displace_shared_messages_from_context() {
    let (_root, groups, bots, id) = fixture().await;
    groups
        .post_user(
            &id,
            "message".into(),
            user("Shared conversation stays readable"),
        )
        .await
        .unwrap();
    let session_id = participant_session_id(&id, &bots[0].id);
    for index in 0..101 {
        groups
            .observe_event(
                &session_id,
                &Event {
                    submission_id: Some("message".into()),
                    msg: EventMsg::TurnStarted(mobius::protocol::TurnStartedEvent {
                        turn_id: format!("turn-{index}"),
                        model_context_window: None,
                    }),
                },
            )
            .await
            .unwrap();
    }
    assert!(
        groups
            .chat_context(&bots[0].id, &session_id)
            .await
            .unwrap()
            .unwrap()
            .contains("Shared conversation stays readable")
    );
}

#[tokio::test]
async fn delivery_acceptance_does_not_block_the_events_needed_to_accept_it() {
    let (_root, groups, bots, id) = fixture().await;
    groups
        .post_user(&id, "message".into(), user("@alice start"))
        .await
        .unwrap();
    let claim = groups
        .claim_next_delivery(&bots[0].id)
        .await
        .unwrap()
        .unwrap();
    let session_id = claim.session_id().to_owned();
    let event = Event {
        submission_id: Some("message".into()),
        msg: EventMsg::TurnStarted(mobius::protocol::TurnStartedEvent {
            turn_id: "turn".into(),
            model_context_window: None,
        }),
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        claim.accept(groups.observe_event(&session_id, &event)),
    )
    .await
    .expect("an actor must be able to publish an event before accepting a command")
    .unwrap()
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn shared_history_reaches_older_messages_and_enforces_membership() {
    use mobius::middleware::bots::BotsBackend as _;

    let (_root, groups, bots, id) = fixture().await;
    let session_id = participant_session_id(&id, &bots[0].id);
    for index in 0..150 {
        groups
            .post_user(
                &id,
                format!("history-{index}"),
                user(format!("Shared message {index}")),
            )
            .await
            .unwrap();
    }
    let context = groups
        .chat_context(&bots[0].id, &session_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!context.contains("Shared message 0\""));
    let mut cursor = None;
    let mut messages = Vec::new();
    loop {
        let page = groups
            .chat_history(&bots[0].id, &session_id, cursor)
            .await
            .unwrap()
            .unwrap();
        messages.extend(page.batches);
        let Some(next) = page.next_before_sequence else {
            break;
        };
        cursor = Some(next);
    }
    assert_eq!(messages.len(), 150);
    let oldest = messages.last().unwrap();
    let content: serde_json::Value =
        serde_json::from_str(oldest.items[0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(content["text"], "Shared message 0");
    assert!(
        groups
            .chat_history(&bots[1].id, &session_id, None)
            .await
            .is_err()
    );
    assert!(
        groups
            .chat_history(&bots[2].id, &participant_session_id(&id, &bots[2].id), None)
            .await
            .is_err()
    );
    assert!(
        groups
            .chat_history(&bots[0].id, "ordinary-session", None)
            .await
            .unwrap()
            .is_none()
    );
    groups.remove_bot(&bots[0].id).await.unwrap();
    assert!(
        groups
            .chat_history(&bots[0].id, &session_id, None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn oversized_latest_message_is_truncated_in_context_and_retained_in_history() {
    use mobius::middleware::bots::BotsBackend as _;

    let (_root, groups, bots, id) = fixture().await;
    let text = format!(
        "Latest message: {} exact ending",
        "🦀".repeat(CONTEXT_BYTES)
    );
    groups
        .post_user(&id, "large-message".into(), user(text.clone()))
        .await
        .unwrap();
    let session_id = participant_session_id(&id, &bots[0].id);
    let context = groups
        .chat_context(&bots[0].id, &session_id)
        .await
        .unwrap()
        .unwrap();
    assert!(context.contains("Latest message: 🦀"));
    assert!(context.contains("use search_history to read the full message"));
    assert!(!context.contains("exact ending"));
    assert!(context.len() < CONTEXT_BYTES);
    let page = groups
        .chat_history(&bots[0].id, &session_id, None)
        .await
        .unwrap()
        .unwrap();
    let content: serde_json::Value =
        serde_json::from_str(page.batches[0].items[0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(content["text"], text);
}
