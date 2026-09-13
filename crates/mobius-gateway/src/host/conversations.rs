use super::*;
use mobius::backend::checkpoint::SessionCursor;
use mobius::middleware::tools::{ToolHeading, Tools, render_tool_event};

use crate::wire::{BotConversation, BotConversationPage};

impl GatewayHost {
    pub(crate) async fn bot_conversations(
        &self,
        bot_id: &str,
        mut cursor: Option<SessionCursor>,
    ) -> std::result::Result<BotConversationPage, Rejection> {
        if let Some(cursor) = &cursor {
            validate_session_id(&cursor.session_id).map_err(|_| invalid_session_id())?;
        }
        let _access = self.begin_mutation().await?;
        let state = self.state.lock().await;
        state.bots.bot(bot_id).map_err(invalid_bot)?;
        let mut owners = conversation_roots(&state).await?;
        loop {
            let page = state
                .checkpoints
                .list_sessions_page(SessionPageRequest {
                    cursor,
                    limit: SESSION_PAGE_SIZE,
                })
                .await
                .map_err(internal)?;
            let mut conversations = Vec::new();
            for session in page.sessions {
                let Some(owner) =
                    conversation_owner(&state.checkpoints, &mut owners, &session).await?
                else {
                    continue;
                };
                if owner.bot_id == bot_id {
                    let activity = state
                        .activities
                        .lock()
                        .await
                        .activities
                        .get(&session.session_id)
                        .cloned()
                        .unwrap_or_default();
                    conversations.push(BotConversation {
                        conversation_id: session.session_id,
                        bot_id: bot_id.into(),
                        chat_id: owner.chat_id,
                        session_context: session.session_context,
                        sequence: session.sequence,
                        first_user_message: session.first_user_message,
                        execution_stats: session.execution_stats,
                        activity,
                        created_at: session.created_at,
                        updated_at: session.updated_at,
                    });
                }
            }
            if !conversations.is_empty() || page.next_cursor.is_none() {
                return Ok(BotConversationPage {
                    conversations,
                    next_cursor: page.next_cursor,
                });
            }
            cursor = page.next_cursor;
        }
    }

    pub(crate) async fn bot_conversation_history(
        &self,
        bot_id: &str,
        conversation_id: &str,
        before_sequence: Option<u64>,
    ) -> std::result::Result<SessionHistoryPage, Rejection> {
        let _access = self.begin_mutation().await?;
        let state = self.state.lock().await;
        require_conversation(&state, bot_id, conversation_id).await?;
        let page = event_turn_page(before_sequence, false, MAX_FRAME_BYTES, |request| {
            state.checkpoints.event_page(conversation_id, request)
        })
        .await
        .map_err(internal)?;
        let next_before_sequence = page.next_before_sequence;
        let tools = Tools::coding(state.session_files.clone());
        let mut records = Vec::new();
        for journal in page.into_chronological() {
            if let Some(record) =
                conversation_record(&tools, &state.chat_store, conversation_id, journal).await?
            {
                records.push(record);
            }
        }
        Ok(SessionHistoryPage {
            records,
            next_before_sequence,
        })
    }

    pub(crate) async fn read_bot_conversation_file(
        &self,
        bot_id: &str,
        conversation_id: &str,
        file_id: &str,
        offset: u64,
        max_bytes: usize,
    ) -> std::result::Result<mobius::backend::session_files::SessionFileChunk, Rejection> {
        let _access = self.begin_mutation().await?;
        let state = self.state.lock().await;
        require_conversation(&state, bot_id, conversation_id).await?;
        state
            .session_files
            .read_chunk(conversation_id, file_id, offset, max_bytes)
            .await
            .map_err(|error| Rejection {
                code: "session_file_rejected",
                message: error.to_string(),
                fatal: false,
            })
    }
}

async fn require_conversation(
    state: &GatewayState,
    bot_id: &str,
    conversation_id: &str,
) -> std::result::Result<(), Rejection> {
    validate_session_id(conversation_id).map_err(|_| invalid_session_id())?;
    state.bots.bot(bot_id).map_err(invalid_bot)?;
    let mut owners = conversation_roots(state).await?;
    if let Some(summary) = state
        .checkpoints
        .session_summary(conversation_id)
        .await
        .map_err(internal)?
        && conversation_owner(&state.checkpoints, &mut owners, &summary)
            .await?
            .is_some_and(|owner| owner.bot_id == bot_id)
    {
        return Ok(());
    }
    Err(Rejection {
        code: "bot_conversation_unavailable",
        message: "this private conversation is no longer available for this Bot".into(),
        fatal: false,
    })
}

#[derive(Clone)]
struct ConversationOwner {
    bot_id: String,
    chat_id: Option<String>,
}

async fn conversation_roots(
    state: &GatewayState,
) -> std::result::Result<HashMap<String, Option<ConversationOwner>>, Rejection> {
    let mut owners = HashMap::new();
    for chat in state.chat_store.chats(false).await.map_err(internal)? {
        for participant in chat.execution_participants() {
            owners.insert(
                participant.session_id.clone(),
                Some(ConversationOwner {
                    bot_id: participant.bot_id.clone(),
                    chat_id: Some(chat.id.clone()),
                }),
            );
        }
    }
    for chat in state.chat_store.chats(true).await.map_err(internal)? {
        for participant in chat.execution_participants() {
            owners.insert(participant.session_id.clone(), None);
        }
    }
    for run in state.bots.history(None).map_err(internal)? {
        if let Some(session_id) = run.session_id {
            owners.insert(
                session_id,
                Some(ConversationOwner {
                    bot_id: run.bot_id,
                    chat_id: None,
                }),
            );
        }
    }
    Ok(owners)
}

async fn conversation_owner(
    checkpoints: &Arc<dyn CheckpointStore>,
    owners: &mut HashMap<String, Option<ConversationOwner>>,
    summary: &SessionSummary,
) -> std::result::Result<Option<ConversationOwner>, Rejection> {
    let mut current = summary.clone();
    let mut visited = HashSet::new();
    let owner = loop {
        if let Some(owner) = owners.get(&current.session_id) {
            break owner.clone();
        }
        if !visited.insert(current.session_id.clone()) {
            return Err(internal("private conversation ancestry contains a cycle"));
        }
        let Some(parent_id) = current.parent_session_id else {
            break None;
        };
        let Some(parent) = checkpoints
            .session_summary(&parent_id)
            .await
            .map_err(internal)?
        else {
            break None;
        };
        current = parent;
    };
    for id in visited {
        owners.insert(id, owner.clone());
    }
    Ok(owner)
}

async fn conversation_record(
    tools: &Tools,
    chats: &crate::chats::ChatStore,
    conversation_id: &str,
    journal: JournalEvent,
) -> std::result::Result<Option<RecordedEvent>, Rejection> {
    if !replayable_event(&journal.event.msg)
        || matches!(
            &journal.event.msg,
            EventMsg::SessionHistory(_) | EventMsg::SessionConfigured(_)
        )
    {
        return Ok(None);
    }
    let mut record = chat::record(journal);
    if let EventMsg::AssistantMessage(message) = &mut record.event.msg
        && let Some(submission_id) = &record.event.submission_id
    {
        for part in &mut message.content {
            if part.phase == mobius::protocol::ModelStepContentPhase::FinalAnswer
                && let Some(reply) = chats
                    .published_reply(conversation_id, submission_id, &part.text)
                    .await
                    .map_err(internal)?
            {
                part.text = reply.text;
                part.annotations.clear();
                record.recipient_bot_ids = reply.recipient_bot_ids;
            }
        }
    }
    let block = tools
        .render(&record.event.msg, conversation_id)
        .or_else(|| {
            render_tool_event(
                &record.event.msg,
                |_| true,
                |name, arguments| ToolHeading {
                    title: name.into(),
                    detail: if arguments.is_null() {
                        String::new()
                    } else {
                        arguments.to_string()
                    },
                },
            )
        });
    if let Some(block) = block {
        record.blocks.push(RenderedBlock {
            capability: "tools".into(),
            block,
        });
    }
    Ok(Some(record))
}
