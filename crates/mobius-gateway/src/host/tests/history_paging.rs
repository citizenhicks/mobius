use super::*;
use mobius::backend::checkpoint::EventPage;
use mobius::protocol::{
    MessageDelivery, MessageEvent, TurnCompleteEvent, TurnStartedEvent, WarningEvent,
};

fn event(id: &str, msg: EventMsg) -> Event {
    Event {
        submission_id: Some(id.into()),
        msg,
    }
}

fn start(id: &str) -> Event {
    event(
        id,
        EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: id.into(),
            model_context_window: None,
        }),
    )
}

fn complete(id: &str) -> Event {
    event(
        id,
        EventMsg::TurnComplete(TurnCompleteEvent { turn_id: id.into() }),
    )
}

fn input(id: &str) -> Event {
    event(
        id,
        EventMsg::Message(MessageEvent {
            author: MessageAuthor::User,
            delivery: MessageDelivery::Turn,
            text: id.into(),
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        }),
    )
}

fn long_turn(id: &str) -> Vec<Event> {
    std::iter::once(start(id))
        .chain((0..125).map(|index| {
            event(
                id,
                EventMsg::Warning(WarningEvent {
                    message: format!("work {index}"),
                }),
            )
        }))
        .chain(std::iter::once(complete(id)))
        .collect()
}

#[tokio::test]
async fn public_history_and_replay_keep_long_turns_with_their_preceding_input() {
    let (root, gateway, bot) = bots::gateway_with_bot().await;
    group_delivery::pause_deliveries(&gateway).await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let host = gateway
        .create_chat(&workspace, std::slice::from_ref(&bot.id), None)
        .await
        .unwrap();
    let execution_id = chat_execution_id(&gateway, host.session_id(), &bot.id).await;
    let chats = Arc::clone(&gateway.state.lock().await.chat_store);
    for id in ["older", "latest"] {
        let Op::Message { message } = group_management::message(id, id).op else {
            unreachable!()
        };
        chats
            .post_user(
                host.session_id(),
                id.into(),
                message,
                std::slice::from_ref(&bot.id),
            )
            .await
            .unwrap();
        for event in long_turn(id) {
            chats.observe_event(&execution_id, &event).await.unwrap();
        }
    }
    let latest = host.history_page(None).await.unwrap();
    assert_eq!(latest.records.len(), 128);
    assert!(
        matches!(&latest.records[0].event.msg, EventMsg::Message(message) if message.text == "latest")
    );
    assert!(
        matches!(&latest.records[1].event.msg, EventMsg::TurnStarted(started) if started.turn_id == "latest")
    );
    assert!(
        matches!(&latest.records[127].event.msg, EventMsg::TurnComplete(done) if done.turn_id == "latest")
    );
    let older = host
        .history_page(latest.next_before_sequence)
        .await
        .unwrap();
    assert_eq!(older.records.len(), 128);
    assert_eq!(older.next_before_sequence, None);
    assert!(older.records.last().unwrap().sequence < latest.records[0].sequence);
    let snapshot = host.snapshot(None).await.unwrap();
    assert_eq!(
        snapshot.ready.next_before_sequence,
        latest.next_before_sequence
    );
    assert_eq!(
        snapshot
            .replay
            .iter()
            .filter_map(event_sequence)
            .collect::<Vec<_>>(),
        latest
            .records
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>()
    );
    let reconnect = host
        .snapshot(Some(latest.records[40].sequence))
        .await
        .unwrap();
    assert_eq!(
        reconnect
            .replay
            .iter()
            .filter_map(event_sequence)
            .collect::<Vec<_>>(),
        latest.records[41..]
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>()
    );
    let before_first = latest.records[0].sequence - 1;
    assert_eq!(
        host.snapshot(Some(before_first))
            .await
            .unwrap()
            .replay
            .len(),
        128
    );
    assert_eq!(
        host.snapshot(Some(before_first - 1))
            .await
            .err()
            .unwrap()
            .code,
        "replay_unavailable"
    );
    gateway.shutdown().await;
}

async fn public_page(events: Vec<Event>) -> EventPage {
    let events = events
        .into_iter()
        .enumerate()
        .map(|(index, event)| JournalEvent {
            sequence: index as u64 + 1,
            recorded_at_ms: 0,
            event,
            stream_metrics: Vec::new(),
        })
        .collect::<Vec<_>>();
    event_turn_page(None, true, MAX_FRAME_BYTES, |request| {
        let records = events
            .iter()
            .rev()
            .filter(|event| {
                request
                    .before_sequence
                    .is_none_or(|before| event.sequence < before)
            })
            .take(2)
            .cloned()
            .collect::<Vec<_>>();
        let next_before_sequence = records
            .last()
            .filter(|event| event.sequence > 1)
            .map(|event| event.sequence);
        std::future::ready(Ok::<_, Error>(EventPage {
            latest_sequence: events.len() as u64,
            events: records,
            next_before_sequence,
        }))
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn public_pages_support_input_after_start_in_completed_and_active_turns() {
    for completed in [false, true] {
        let mut events = vec![start("older"), input("older"), complete("older")];
        let mut latest = long_turn("latest");
        latest.insert(1, input("latest"));
        if !completed {
            latest.pop();
        }
        let expected = latest.len();
        events.extend(latest);
        let page = public_page(events).await;
        assert_eq!(page.next_before_sequence, Some(4));
        assert_eq!(page.events.len(), expected);
        assert!(
            matches!(&page.events.last().unwrap().event.msg, EventMsg::TurnStarted(start) if start.turn_id == "latest")
        );
    }
}

#[tokio::test]
async fn a_steering_message_stays_with_its_active_turn_and_initial_input() {
    let mut steer = input("steer");
    let EventMsg::Message(message) = &mut steer.msg else {
        unreachable!()
    };
    message.delivery = MessageDelivery::Steer;
    let page = public_page(vec![
        start("older"),
        input("older"),
        complete("older"),
        start("active"),
        input("active"),
        steer,
    ])
    .await;
    assert_eq!(page.next_before_sequence, Some(4));
    assert_eq!(
        page.into_chronological()
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>(),
        vec![4, 5, 6]
    );
}

#[tokio::test]
async fn a_public_input_without_execution_does_not_absorb_an_older_completed_turn() {
    let page = public_page(vec![
        start("older"),
        input("older"),
        complete("older"),
        input("queued"),
    ])
    .await;
    assert_eq!(page.next_before_sequence, Some(4));
    assert_eq!(page.events.len(), 1);
    assert!(
        matches!(&page.events[0].event.msg, EventMsg::Message(message) if message.text == "queued")
    );
}

#[tokio::test]
async fn overlapping_turns_with_different_inputs_keep_both_starts_and_inputs() {
    let page = public_page(vec![
        input("a"),
        start("a"),
        input("b"),
        start("b"),
        complete("a"),
        complete("b"),
    ])
    .await;
    assert_eq!(page.next_before_sequence, None);
    assert_eq!(page.events.len(), 6);
}

#[tokio::test]
async fn private_history_pages_keep_long_turns_and_exact_exclusive_cursors() {
    let (root, gateway, bot) = bots::gateway_with_bot().await;
    group_delivery::pause_deliveries(&gateway).await;
    let (chats, checkpoints) = {
        let state = gateway.state.lock().await;
        (state.chat_store.clone(), state.checkpoints.clone())
    };
    let chat = chats
        .create(root.path().into(), vec![bot.id.clone()], None)
        .await
        .unwrap();
    let execution = chat_execution_id(&gateway, &chat, &bot.id).await;
    checkpoints
        .save(&Checkpoint::empty(&execution), &[], None)
        .await
        .unwrap();
    for id in ["older", "latest"] {
        let mut events = long_turn(id);
        events.insert(1, input(id));
        for event in events {
            checkpoints
                .append_event(&execution, 0, &event)
                .await
                .unwrap();
        }
    }
    let latest = gateway
        .bot_conversation_history(&bot.id, &execution, None)
        .await
        .unwrap();
    let older = gateway
        .bot_conversation_history(&bot.id, &execution, latest.next_before_sequence)
        .await
        .unwrap();
    assert_eq!(latest.records.len(), 128);
    assert_eq!(older.records.len(), 128);
    assert_eq!(
        latest.next_before_sequence,
        Some(latest.records[0].sequence)
    );
    assert_eq!(older.next_before_sequence, None);
    let sequences = older
        .records
        .iter()
        .chain(&latest.records)
        .map(|event| event.sequence)
        .collect::<Vec<_>>();
    assert_eq!(sequences, (1..=256).collect::<Vec<_>>());
    gateway.shutdown().await;
}

#[tokio::test]
async fn overlapping_bot_turns_and_the_shared_input_stay_in_one_page() {
    let events = [
        input("older"),
        start("older"),
        complete("older"),
        input("shared"),
        event("shared", start("bot-a").msg),
        event("shared", start("bot-b").msg),
        event("shared", complete("bot-a").msg),
        event("shared", complete("bot-b").msg),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, event)| JournalEvent {
        sequence: index as u64 + 1,
        recorded_at_ms: 0,
        event,
        stream_metrics: Vec::new(),
    })
    .collect::<Vec<_>>();
    let page = event_turn_page(None, true, MAX_FRAME_BYTES, |request| {
        let events = events
            .iter()
            .rev()
            .filter(|event| {
                request
                    .before_sequence
                    .is_none_or(|before| event.sequence < before)
            })
            .take(2)
            .cloned()
            .collect::<Vec<_>>();
        let next_before_sequence = events
            .last()
            .filter(|event| event.sequence > 1)
            .map(|event| event.sequence);
        std::future::ready(Ok::<_, Error>(EventPage {
            latest_sequence: 8,
            events,
            next_before_sequence,
        }))
    })
    .await
    .unwrap();
    assert_eq!(page.next_before_sequence, Some(4));
    assert_eq!(
        page.into_chronological()
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![4, 5, 6, 7, 8]
    );
}

#[tokio::test]
async fn an_oversized_turn_fails_instead_of_returning_partial_history() {
    let error = event_turn_page(None, false, 1, |_| {
        std::future::ready(Ok::<_, Error>(EventPage {
            latest_sequence: 1,
            events: vec![JournalEvent {
                sequence: 1,
                recorded_at_ms: 0,
                event: complete("turn"),
                stream_metrics: Vec::new(),
            }],
            next_before_sequence: None,
        }))
    })
    .await
    .unwrap_err();
    assert!(error.to_string().contains("history turn exceeds"));
}

#[tokio::test]
async fn lifecycle_events_around_a_turn_do_not_form_their_own_history_page() {
    let notices = || {
        (0..150).map(|_| Event {
            submission_id: None,
            msg: EventMsg::Warning(WarningEvent {
                message: "notice".into(),
            }),
        })
    };
    let events = notices()
        .chain([start("turn"), input("turn"), complete("turn")])
        .chain(notices())
        .enumerate()
        .map(|(index, event)| JournalEvent {
            sequence: index as u64 + 1,
            recorded_at_ms: 0,
            event,
            stream_metrics: Vec::new(),
        })
        .collect::<Vec<_>>();
    for public in [false, true] {
        let page = event_turn_page(None, public, MAX_FRAME_BYTES, |request| {
            let page = events
                .iter()
                .rev()
                .filter(|event| {
                    request
                        .before_sequence
                        .is_none_or(|before| event.sequence < before)
                })
                .take(request.limit)
                .cloned()
                .collect::<Vec<_>>();
            let next_before_sequence = page
                .last()
                .filter(|event| event.sequence > 1)
                .map(|event| event.sequence);
            std::future::ready(Ok::<_, Error>(EventPage {
                latest_sequence: events.len() as u64,
                events: page,
                next_before_sequence,
            }))
        })
        .await
        .unwrap();
        assert_eq!(page.next_before_sequence, None);
        assert_eq!(page.into_chronological(), events);
    }
}
