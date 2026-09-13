use super::*;
use mobius::protocol::{
    AssistantMessageEvent, MAX_MESSAGE_BYTES, MessageReply, MessageTarget, ModelStepContent,
    ModelStepContentPhase,
};

fn assistant(
    id: &str,
    submission: &str,
    step: &str,
    text: &str,
    phase: ModelStepContentPhase,
) -> Event {
    Event {
        submission_id: Some(submission.into()),
        msg: EventMsg::AssistantMessage(AssistantMessageEvent {
            session_id: id.into(),
            turn_id: "turn".into(),
            model_step_id: step.into(),
            content: vec![ModelStepContent {
                output_index: 0,
                part_index: 0,
                phase,
                text: text.into(),
                annotations: Vec::new(),
            }],
            message_target: None,
        }),
    }
}

#[test]
fn reply_context_never_invalidates_an_accepted_message() {
    let original = "x".repeat(MAX_MESSAGE_BYTES);
    let submission = chat_message_submission(ChatMessage {
        id: "message".into(),
        message: MessageSubmission {
            author: MessageAuthor::User,
            text: original.clone(),
            attachments: Vec::new(),
            reply: Some(MessageReply {
                target: MessageTarget {
                    checkpoint_sequence: 1,
                    batch_item_count: 1,
                },
                text: "quoted".into(),
            }),
            requested_delivery: None,
            target_turn_id: None,
        },
        user_message_id: "message".into(),
        reply_depth: 0,
        recipients: Vec::new(),
    });
    let Op::Message { message } = submission.op else {
        panic!("message submission")
    };
    assert_eq!(message.text, original);
    assert!(message.reply.is_none());
}

#[tokio::test]
async fn private_final_replies_require_historical_publication_and_preserve_literal_json() {
    let (_root, gateway, workspace, host, bots) = group_management::gateway_with_group().await;
    group_delivery::pause_deliveries(&gateway).await;
    let (chats, checkpoints, operations) = {
        let state = gateway.state.lock().await;
        (
            state.chat_store.clone(),
            state.checkpoints.clone(),
            state.store.runtime_operations.clone(),
        )
    };
    let chat_id = host.session_id();
    let private_id = chat_execution_id(&gateway, chat_id, &bots[0].id).await;
    checkpoints
        .save(&Checkpoint::empty(&private_id), &[], None)
        .await
        .unwrap();
    let literal_json = r#"{"text":"literal JSON answer","recipient_bot_ids":[]}"#;
    let published =
        serde_json::json!({"text":"Visible reply", "recipient_bot_ids":[bots[1].id]}).to_string();
    let nested = serde_json::json!({"text":literal_json, "recipient_bot_ids":[]}).to_string();
    for (submission, encoded) in [("source", &published), ("nested", &nested)] {
        let Op::Message { message } = group_management::message(submission, "Request").op else {
            unreachable!()
        };
        chats
            .post_user(chat_id, submission.into(), message, &[])
            .await
            .unwrap();
        assert!(
            chats
                .settle_delivery(
                    submission,
                    &private_id,
                    &bots[0].id,
                    crate::chats::ChatRunOutcome::Succeeded {
                        summary: encoded.clone()
                    }
                )
                .await
                .unwrap()
        );
    }
    let unpublished =
        serde_json::json!({"text":"Never published", "recipient_bot_ids":[]}).to_string();
    let malformed =
        serde_json::json!({"text":"Visible reply", "recipient_bot_ids":[bots[1].id], "extra":true})
            .to_string();
    let cases = [
        (
            "source",
            published.as_str(),
            ModelStepContentPhase::FinalAnswer,
            "Visible reply",
        ),
        (
            "nested",
            nested.as_str(),
            ModelStepContentPhase::FinalAnswer,
            literal_json,
        ),
        (
            "source",
            unpublished.as_str(),
            ModelStepContentPhase::FinalAnswer,
            unpublished.as_str(),
        ),
        (
            "source",
            malformed.as_str(),
            ModelStepContentPhase::FinalAnswer,
            malformed.as_str(),
        ),
        (
            "source",
            published.as_str(),
            ModelStepContentPhase::Commentary,
            published.as_str(),
        ),
    ];
    for (index, (submission, text, phase, _)) in cases.iter().enumerate() {
        checkpoints
            .append_event(
                &private_id,
                i64::try_from(index).unwrap() + 1,
                &assistant(&private_id, submission, &index.to_string(), text, *phase),
            )
            .await
            .unwrap();
    }
    // Historical publication stays authoritative after the original recipient leaves.
    chats.remove_bot(&bots[1].id).await.unwrap();
    assert_eq!(
        chats
            .load(chat_id)
            .await
            .unwrap()
            .unwrap()
            .participants
            .len(),
        1
    );
    let before = operations.counts();
    let page = gateway
        .bot_conversation_history(&bots[0].id, &private_id, None)
        .await
        .unwrap();
    assert_eq!(page.records.len(), cases.len());
    for (record, (_, _, _, expected)) in page.records.iter().zip(cases) {
        let EventMsg::AssistantMessage(message) = &record.event.msg else {
            panic!("assistant message")
        };
        assert_eq!(message.content[0].text, expected);
    }
    assert_eq!(page.records[0].recipient_bot_ids, [bots[1].id.clone()]);
    assert!(
        page.records[1..]
            .iter()
            .all(|record| record.recipient_bot_ids.is_empty())
    );
    assert_eq!(
        operations.counts(),
        before,
        "history must not materialize execution"
    );
    let original = checkpoints
        .event_page(
            &private_id,
            EventPageRequest {
                before_sequence: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(original.events.len(), cases.len());
    let EventMsg::AssistantMessage(message) = &original.events.last().unwrap().event.msg else {
        panic!("assistant message")
    };
    assert_eq!(
        message.content[0].text, published,
        "projection does not rewrite durable events"
    );

    let child_id = Uuid::new_v4().to_string();
    checkpoints
        .fork(&private_id, 0, &Checkpoint::empty(&child_id))
        .await
        .unwrap();
    checkpoints
        .append_event(
            &child_id,
            1,
            &assistant(
                &child_id,
                "source",
                "child",
                &published,
                ModelStepContentPhase::FinalAnswer,
            ),
        )
        .await
        .unwrap();
    let solo_chat = chats
        .create(workspace, vec![bots[0].id.clone()], None)
        .await
        .unwrap();
    let solo_id = chat_execution_id(&gateway, &solo_chat, &bots[0].id).await;
    checkpoints
        .save(&Checkpoint::empty(&solo_id), &[], None)
        .await
        .unwrap();
    checkpoints
        .append_event(
            &solo_id,
            1,
            &assistant(
                &solo_id,
                "source",
                "solo",
                &published,
                ModelStepContentPhase::FinalAnswer,
            ),
        )
        .await
        .unwrap();
    for id in [child_id, solo_id] {
        let page = gateway
            .bot_conversation_history(&bots[0].id, &id, None)
            .await
            .unwrap();
        let EventMsg::AssistantMessage(message) = &page.records.last().unwrap().event.msg else {
            panic!("assistant message")
        };
        assert_eq!(
            message.content[0].text, published,
            "unrelated JSON answers stay literal"
        );
        assert!(page.records.last().unwrap().recipient_bot_ids.is_empty());
    }
}
