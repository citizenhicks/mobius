use super::group_delivery::pause_deliveries;
use super::group_management::{gateway_with_group, message};
use super::*;
use std::time::Duration;

#[tokio::test]
async fn settled_message_recipients_survive_live_projection_replay_and_older_history() {
    let (_root, gateway, _workspace, chat, bots) = gateway_with_group().await;
    pause_deliveries(&gateway).await;
    let store = Arc::clone(&gateway.state.lock().await.chat_store);
    let mut events = chat.subscribe();
    let both = vec![bots[1].id.clone(), bots[0].id.clone()];
    chat.submit(message("primary", "Use the primary Bot"))
        .await
        .unwrap();
    chat.submit_to(message("mentioned", "Ask both Bots"), both.clone())
        .await
        .unwrap();
    let recipients = store.pending_deliveries(chat.session_id()).await.unwrap();
    for (bot_id, entry) in recipients {
        let execution = chat_execution_id(&gateway, chat.session_id(), &bot_id).await;
        store
            .settle_delivery(
                &entry.id,
                &execution,
                &bot_id,
                ChatRunOutcome::Failed {
                    message: "finished fixture".into(),
                },
            )
            .await
            .unwrap();
    }
    assert!(
        store
            .pending_deliveries(chat.session_id())
            .await
            .unwrap()
            .is_empty()
    );
    let records = store
        .event_page(
            chat.session_id(),
            EventPageRequest {
                before_sequence: None,
                limit: 50,
            },
        )
        .await
        .unwrap()
        .into_chronological();
    chat.send(HostCommand::Publish { records }).await.unwrap();
    let live = tokio::time::timeout(Duration::from_secs(5), async {
        let mut live = Vec::new();
        while live.len() < 2 {
            if let ServerMessage::AgentEvent { record, .. } = events.recv().await.unwrap().message
                && matches!(&record.event.msg, EventMsg::Message(message) if message.author == MessageAuthor::User)
            {
                live.push(record);
            }
        }
        live
    }).await.unwrap();
    assert_eq!(live[0].recipient_bot_ids, vec![bots[0].id.clone()]);
    assert_eq!(live[1].recipient_bot_ids, both);

    for index in 0..26 {
        let id = format!("later-{index}");
        chat.submit_to(message(&id, "Later message"), vec![bots[1].id.clone()])
            .await
            .unwrap();
        let execution = chat_execution_id(&gateway, chat.session_id(), &bots[1].id).await;
        store
            .settle_delivery(
                &id,
                &execution,
                &bots[1].id,
                ChatRunOutcome::Failed {
                    message: "finished fixture".into(),
                },
            )
            .await
            .unwrap();
    }
    assert!(
        store
            .pending_deliveries(chat.session_id())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(chat.stop_if_idle().await);
    let reopened = gateway.open_session(chat.session_id()).await.unwrap();
    let snapshot = reopened.snapshot(None).await.unwrap();
    let replay = snapshot
        .replay
        .into_iter()
        .filter_map(|frame| match frame.message {
            ServerMessage::AgentEvent { record, .. } => Some(record),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        replay
            .iter()
            .any(|record| record.event.submission_id.as_deref() == Some("later-25"))
    );
    for record in replay.iter().filter(|record| matches!(&record.event.msg, EventMsg::Message(message) if message.author == MessageAuthor::User)) {
        assert_eq!(record.recipient_bot_ids, vec![bots[1].id.clone()]);
    }
    let mut cursor = snapshot.ready.next_before_sequence;
    let mut older_user = Vec::new();
    while let Some(before) = cursor {
        let older = reopened.history_page(Some(before)).await.unwrap();
        assert!(older.next_before_sequence.is_none_or(|next| next < before));
        cursor = older.next_before_sequence;
        older_user.extend(older.records.into_iter().filter(|record| {
            matches!(&record.event.msg, EventMsg::Message(message) if message.author == MessageAuthor::User)
        }));
    }
    older_user.sort_by_key(|record| record.sequence);
    assert_eq!(older_user.len(), 27);
    assert_eq!(
        older_user[0].event.submission_id.as_deref(),
        Some("primary")
    );
    assert_eq!(older_user[0].recipient_bot_ids, vec![bots[0].id.clone()]);
    assert_eq!(
        older_user[1].event.submission_id.as_deref(),
        Some("mentioned")
    );
    assert_eq!(older_user[1].recipient_bot_ids, both);
    gateway.shutdown().await;
}
