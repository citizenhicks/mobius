//! Forks published conversation records without copying execution state.

use std::collections::{BTreeMap, BTreeSet};

use mobius::backend::checkpoint::{EventPageRequest, JournalEvent};
use mobius::backend::session_files::SessionFileStore;
use mobius::protocol::{
    EventMsg, FrontendBlockState, FrontendEvent, MessageAuthor, MessageTarget, SessionFileOrigin,
    SessionFileReference,
};
use uuid::Uuid;

use super::{ChatMessage, ChatStore, invalid};
use crate::Result;

impl ChatStore {
    pub(crate) async fn fork_chat(
        &self,
        chat_id: &str,
        target: &MessageTarget,
        files: &SessionFileStore,
    ) -> Result<String> {
        let _gate = self.gate.lock().await;
        let mut fork = self
            .load(chat_id)
            .await?
            .ok_or_else(|| invalid("unknown chat"))?;
        self.validate_target(chat_id, target).await?;
        let mut records = self.fork_records(chat_id, target).await?;
        fork.id = Uuid::new_v4().to_string();
        fork.sequence = target.checkpoint_sequence;
        fork.created_at = chrono::Utc::now().timestamp();
        fork.updated_at = fork.created_at;
        fork.pending.clear();
        fork.retired_participants.clear();
        for participant in &mut fork.participants {
            self.bots.bot(&participant.bot_id)?;
            participant.session_id = Uuid::new_v4().to_string();
            participant.published_sequence = 0;
        }
        fork.first_user_message = records.iter().find_map(|(record, _)| {
            let EventMsg::Message(message) = &record.event.msg else {
                return None;
            };
            matches!(message.author, MessageAuthor::User)
                .then(|| message.text.chars().take(512).collect())
        });
        for (record, _) in &mut records {
            match &mut record.event.msg {
                EventMsg::AssistantMessage(message) => message.session_id.clone_from(&fork.id),
                EventMsg::ModelStepCompleted(step) => step.session_id.clone_from(&fork.id),
                EventMsg::WebSearchEnd(search) => search.session_id.clone_from(&fork.id),
                _ => {}
            }
        }
        let result = async {
            grant_fork_files(files, chat_id, &fork.id, &records).await?;
            self.persist(&fork, &records).await
        }
        .await;
        if let Err(error) = result {
            return match files.delete_session(&fork.id).await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(invalid(format!(
                    "{error}; failed to clean up fork files: {cleanup}"
                ))),
            };
        }
        self.changed(
            &fork.id,
            records.into_iter().map(|(record, _)| record).collect(),
        );
        Ok(fork.id)
    }

    async fn fork_records(
        &self,
        chat_id: &str,
        target: &MessageTarget,
    ) -> Result<Vec<(JournalEvent, Option<ChatMessage>)>> {
        // ponytail: forks materialize one public prefix; stream it if large chats make memory material.
        let mut before_sequence = Some(
            target
                .checkpoint_sequence
                .checked_add(1)
                .ok_or_else(|| invalid("fork target sequence is too large"))?,
        );
        let mut events = Vec::new();
        loop {
            let page = self
                .event_page(
                    chat_id,
                    EventPageRequest {
                        before_sequence,
                        limit: 256,
                    },
                )
                .await?;
            events.extend(page.events);
            before_sequence = page.next_before_sequence;
            if before_sequence.is_none() {
                break;
            }
        }
        events.reverse();
        let completed_calls = events
            .iter()
            .filter_map(|record| match &record.event.msg {
                EventMsg::ToolCallEnd(call) => Some((call.turn_id.clone(), call.call_id.clone())),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let mut records = Vec::new();
        for record in events {
            let keep = match &record.event.msg {
                EventMsg::Message(_)
                | EventMsg::AssistantMessage(_)
                | EventMsg::ToolCallEnd(_)
                | EventMsg::ModelStepCompleted(_)
                | EventMsg::WebSearchEnd(_)
                | EventMsg::Error(_)
                | EventMsg::Warning(_) => true,
                EventMsg::ToolCallBegin(call) => {
                    completed_calls.contains(&(call.turn_id.clone(), call.call_id.clone()))
                }
                EventMsg::Frontend(FrontendEvent::Render { block, .. }) => {
                    block.state == FrontendBlockState::Complete
                }
                _ => false,
            };
            if !keep {
                continue;
            }
            let message = if matches!(
                record.event.msg,
                EventMsg::Message(_) | EventMsg::AssistantMessage(_)
            ) {
                Some(
                    self.message_by_sequence(chat_id, record.sequence)
                        .await?
                        .ok_or_else(|| invalid("published message has no author metadata"))?,
                )
            } else {
                None
            };
            records.push((record, message));
        }
        Ok(records)
    }
}

async fn grant_fork_files(
    files: &SessionFileStore,
    source: &str,
    target: &str,
    records: &[(JournalEvent, Option<ChatMessage>)],
) -> Result<()> {
    let mut references = BTreeMap::<&str, &SessionFileReference>::new();
    for (record, _) in records {
        for file in event_files(&record.event.msg) {
            if let Some(previous) = references.insert(&file.id, file)
                && previous != file
            {
                return Err(invalid(
                    "published file references have conflicting metadata",
                ));
            }
        }
    }
    if references.is_empty() {
        return Ok(());
    }
    let origins = files
        .list_files(source)
        .await?
        .into_iter()
        .map(|record| (record.file.id, record.origin))
        .collect::<BTreeMap<_, _>>();
    for (id, file) in references {
        match origins.get(id) {
            Some(SessionFileOrigin::User) => {
                files.grant_upload(source, target, file).await?;
            }
            Some(SessionFileOrigin::Agent) => {
                files.share_artifact(source, target, file).await?;
            }
            None => {
                // Owned observations are absent from the public upload/artifact catalog.
                files.grant_file(source, target, file).await?;
            }
        }
    }
    Ok(())
}

fn event_files(event: &EventMsg) -> Vec<&SessionFileReference> {
    match event {
        EventMsg::Message(message) => message.attachments.iter().collect(),
        EventMsg::ToolCallEnd(result) => result.output.files().collect(),
        EventMsg::Frontend(FrontendEvent::Render { block, .. }) => {
            block.files.iter().chain(block.content.files()).collect()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mobius::protocol::{
        AssistantMessageEvent, Event, FrontendBlock, FrontendBlockFormat, FrontendBlockRole,
        FrontendBlockUpdate, FrontendTone, MessageSubmission, ModelStepContent,
        ModelStepContentPhase, TurnStartedEvent,
    };

    use super::*;
    use crate::bots::BotStore;
    use crate::chats::ChatRunOutcome;

    fn user(text: &str) -> MessageSubmission {
        MessageSubmission {
            author: MessageAuthor::User,
            text: text.into(),
            attachments: Vec::new(),
            reply: None,
            requested_delivery: None,
            target_turn_id: None,
        }
    }

    #[tokio::test]
    async fn fork_retains_public_prefix_files_and_handoff_history_without_dispatching_work() {
        let root = tempfile::tempdir().unwrap();
        let bots = Arc::new(BotStore::open(root.path()).unwrap());
        let alice = bots
            .create_bot("Alice", "Member", Default::default())
            .unwrap();
        let bob = bots
            .create_bot("Bob", "Member", Default::default())
            .unwrap();
        let (chats, _deliveries) = ChatStore::new(root.path(), bots).unwrap();
        let source_id = chats
            .create(
                root.path().to_owned(),
                vec![alice.id.clone(), bob.id.clone()],
                Some(&alice.id),
            )
            .await
            .unwrap();
        let source = chats.load(&source_id).await.unwrap().unwrap();
        let alice_session = source.session_id(&alice.id).unwrap();
        let files = SessionFileStore::new(root.path());
        let mut upload = files
            .begin_upload(&source_id, "input.txt".into(), 5, "text/plain".into())
            .await
            .unwrap();
        upload.append(0, b"input").await.unwrap();
        let upload = upload.finish().await.unwrap();
        let mut request = user("Review the input");
        request.attachments.push(upload.clone());
        chats
            .post_user(
                &source_id,
                "request".into(),
                request,
                std::slice::from_ref(&alice.id),
            )
            .await
            .unwrap();
        chats
            .observe_event(
                alice_session,
                &Event {
                    submission_id: Some("request".into()),
                    msg: EventMsg::TurnStarted(TurnStartedEvent {
                        turn_id: "turn".into(),
                        model_context_window: None,
                    }),
                },
            )
            .await
            .unwrap();
        let artifact = files
            .publish_artifact(
                &source_id,
                "review.txt".into(),
                "text/plain".into(),
                b"review",
            )
            .await
            .unwrap();
        chats
            .observe_event(
                alice_session,
                &Event {
                    submission_id: Some("request".into()),
                    msg: EventMsg::Frontend(FrontendEvent::Render {
                        capability: "artifacts".into(),
                        block: FrontendBlock {
                            id: None,
                            group: None,
                            update: FrontendBlockUpdate::Replace,
                            state: FrontendBlockState::Complete,
                            role: FrontendBlockRole::Artifact,
                            title: "Review".into(),
                            text: String::new(),
                            symbol: None,
                            files: vec![artifact.clone()],
                            content: Default::default(),
                            format: FrontendBlockFormat::PlainText,
                            tone: FrontendTone::Neutral,
                        },
                    }),
                },
            )
            .await
            .unwrap();
        chats
            .settle_delivery(
                "request",
                alice_session,
                &alice.id,
                ChatRunOutcome::Succeeded {
                    summary: serde_json::json!({"text": "Bob, check the review", "recipient_bot_ids": [bob.id]}).to_string(),
                },
            )
            .await
            .unwrap();
        let target = MessageTarget {
            checkpoint_sequence: chats.load(&source_id).await.unwrap().unwrap().sequence,
            batch_item_count: 1,
        };
        let original = chats
            .message_by_sequence(&source_id, target.checkpoint_sequence)
            .await
            .unwrap()
            .unwrap();
        chats
            .post_user(
                &source_id,
                "later".into(),
                user("Outside the fork"),
                std::slice::from_ref(&bob.id),
            )
            .await
            .unwrap();
        let fork_id = chats.fork_chat(&source_id, &target, &files).await.unwrap();
        let fork = chats.load(&fork_id).await.unwrap().unwrap();
        assert_ne!(fork.id, source_id);
        assert_eq!(fork.workspace, source.workspace);
        assert_eq!(fork.member_bot_ids(), source.member_bot_ids());
        assert_eq!(fork.primary_bot_id, source.primary_bot_id);
        assert_eq!(fork.sequence, target.checkpoint_sequence);
        assert!(fork.pending.is_empty());
        assert!(fork.retired_participants.is_empty());
        for participant in &fork.participants {
            assert_ne!(
                Some(participant.session_id.as_str()),
                source.session_id(&participant.bot_id)
            );
        }
        let copied = chats
            .message_by_id(&fork_id, &original.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(copied.user_message_id, "request");
        assert_eq!(copied.message, original.message);
        assert_eq!(copied.recipients, std::slice::from_ref(&bob.id));
        assert!(
            chats
                .message_by_id(&fork_id, "later")
                .await
                .unwrap()
                .is_none()
        );
        let history = chats
            .event_page(
                &fork_id,
                EventPageRequest {
                    before_sequence: None,
                    limit: 100,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            history
                .events
                .iter()
                .rev()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            [1, 3, 4]
        );
        files.delete_session(&source_id).await.unwrap();
        files.verify_upload(&fork_id, &upload).await.unwrap();
        assert_eq!(files.read_file(&fork_id, &upload).await.unwrap(), b"input");
        assert_eq!(
            files.list_artifacts(&fork_id).await.unwrap().as_slice(),
            std::slice::from_ref(&artifact)
        );
        assert_eq!(
            files.read_file(&fork_id, &artifact).await.unwrap(),
            b"review"
        );
    }

    #[tokio::test]
    async fn fork_keeps_inline_authorship_after_reassignment_without_retired_executions() {
        let root = tempfile::tempdir().unwrap();
        let bots = Arc::new(BotStore::open(root.path()).unwrap());
        let alice = bots
            .create_bot("Alice", "Member", Default::default())
            .unwrap();
        let bob = bots
            .create_bot("Bob", "Member", Default::default())
            .unwrap();
        let (chats, _deliveries) = ChatStore::new(root.path(), bots).unwrap();
        let source_id = chats
            .create(root.path().to_owned(), vec![alice.id.clone()], None)
            .await
            .unwrap();
        let source = chats.load(&source_id).await.unwrap().unwrap();
        let session_id = source.session_id(&alice.id).unwrap();
        chats
            .post_user(
                &source_id,
                "request".into(),
                user("Review"),
                std::slice::from_ref(&alice.id),
            )
            .await
            .unwrap();
        chats
            .observe_event(
                session_id,
                &Event {
                    submission_id: Some("request".into()),
                    msg: EventMsg::AssistantMessage(AssistantMessageEvent {
                        session_id: session_id.into(),
                        turn_id: "turn".into(),
                        model_step_id: "step".into(),
                        content: vec![ModelStepContent {
                            output_index: 0,
                            part_index: 0,
                            phase: ModelStepContentPhase::FinalAnswer,
                            text: "Alice's review".into(),
                            annotations: Vec::new(),
                        }],
                        message_target: None,
                    }),
                },
            )
            .await
            .unwrap();
        let target = MessageTarget {
            checkpoint_sequence: 2,
            batch_item_count: 1,
        };
        let original = chats
            .message_by_sequence(&source_id, 2)
            .await
            .unwrap()
            .unwrap();
        chats
            .settle_delivery(
                "request",
                session_id,
                &alice.id,
                ChatRunOutcome::Succeeded {
                    summary: "Alice's review".into(),
                },
            )
            .await
            .unwrap();
        chats.reassign(&source_id, &bob.id).await.unwrap();
        let files = SessionFileStore::new(root.path());
        let fork_id = chats.fork_chat(&source_id, &target, &files).await.unwrap();
        let fork = chats.load(&fork_id).await.unwrap().unwrap();
        assert_eq!(fork.member_bot_ids(), [bob.id]);
        assert!(fork.retired_participants.is_empty());
        assert_eq!(
            chats
                .message_by_sequence(&fork_id, 2)
                .await
                .unwrap()
                .unwrap()
                .message
                .author,
            original.message.author
        );
        let history = chats
            .event_page(
                &fork_id,
                EventPageRequest {
                    before_sequence: None,
                    limit: 1,
                },
            )
            .await
            .unwrap();
        let EventMsg::AssistantMessage(message) = &history.events[0].event.msg else {
            panic!("expected copied inline answer")
        };
        assert_eq!(message.session_id, fork_id);
        assert_eq!(message.message_target, Some(target));
    }

    #[tokio::test]
    async fn invalid_targets_or_missing_files_do_not_publish_a_fork() {
        let root = tempfile::tempdir().unwrap();
        let bots = Arc::new(BotStore::open(root.path()).unwrap());
        let alice = bots
            .create_bot("Alice", "Member", Default::default())
            .unwrap();
        let (chats, _deliveries) = ChatStore::new(root.path(), bots).unwrap();
        let source_id = chats
            .create(root.path().to_owned(), vec![alice.id.clone()], None)
            .await
            .unwrap();
        let mut request = user("Missing file");
        request.attachments.push(SessionFileReference {
            id: Uuid::new_v4().to_string(),
            name: "missing.txt".into(),
            size: 1,
            media_type: "text/plain".into(),
        });
        chats
            .post_user(
                &source_id,
                "request".into(),
                request,
                std::slice::from_ref(&alice.id),
            )
            .await
            .unwrap();
        let files = SessionFileStore::new(root.path());
        for target in [
            MessageTarget {
                checkpoint_sequence: 1,
                batch_item_count: 2,
            },
            MessageTarget {
                checkpoint_sequence: 9,
                batch_item_count: 1,
            },
            MessageTarget {
                checkpoint_sequence: 1,
                batch_item_count: 1,
            },
        ] {
            assert!(chats.fork_chat(&source_id, &target, &files).await.is_err());
            let all = chats.chats(false).await.unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].id, source_id);
        }
    }
}
