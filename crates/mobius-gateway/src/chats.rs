//! Gateway-owned conversations, participants, routing, and durable delivery.

pub(crate) mod context;
mod fork;
pub(crate) mod history;
mod storage;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mobius::backend::checkpoint::{EventPageRequest, JournalEvent};
use mobius::protocol::{
    Event, EventMsg, MessageAuthor, MessageDelivery, MessageEvent, MessageSubmission,
    MessageTarget, ModelStepContentPhase, SessionContext,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use crate::bots::BotStore;
use crate::{Error, Result};

const MAX_MEMBERS: usize = 100;
const MAX_PENDING: usize = 256;
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;
const MAX_REPLY_DEPTH: u8 = 3;
const CONTEXT_BYTES: usize = 32_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Chat {
    pub(crate) id: String,
    pub(crate) sequence: u64,
    pub(crate) first_user_message: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    pub(crate) deleted: bool,
    pub(crate) workspace: PathBuf,
    pub(crate) participants: Vec<ChatParticipant>,
    pub(crate) retired_participants: Vec<ChatParticipant>,
    pub(crate) primary_bot_id: Option<String>,
    pending: Vec<ChatMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChatParticipant {
    pub(crate) bot_id: String,
    pub(crate) session_id: String,
    pub(crate) published_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChatMessage {
    pub(crate) id: String,
    pub(crate) message: MessageSubmission,
    pub(crate) user_message_id: String,
    pub(crate) reply_depth: u8,
    pub(crate) recipients: Vec<String>,
}

pub(crate) enum ChatRunOutcome {
    Succeeded { summary: String },
    Failed { message: String },
}

pub(crate) enum ChatDelivery {
    Changed {
        chat_id: String,
        records: Vec<JournalEvent>,
    },
    RetryPending,
    Pending {
        chat_id: String,
    },
}

#[derive(Clone)]
pub(crate) struct ChatStore {
    connection: Arc<std::sync::Mutex<rusqlite::Connection>>,
    bots: Arc<BotStore>,
    // Serializes durable chat mutations, never model execution or startup.
    gate: Arc<Mutex<()>>,
    deliveries: mpsc::UnboundedSender<ChatDelivery>,
}

impl ChatStore {
    pub(crate) fn new(
        state_dir: &Path,
        bots: Arc<BotStore>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<ChatDelivery>)> {
        let connection = storage::open(state_dir)?;
        let (deliveries, receiver) = mpsc::unbounded_channel();
        Ok((
            Self {
                connection: Arc::new(std::sync::Mutex::new(connection)),
                bots,
                gate: Arc::new(Mutex::new(())),
                deliveries,
            },
            receiver,
        ))
    }

    pub(crate) async fn create(
        &self,
        workspace: PathBuf,
        member_bot_ids: Vec<String>,
        primary: Option<&str>,
    ) -> Result<String> {
        if !(1..=MAX_MEMBERS).contains(&member_bot_ids.len())
            || member_bot_ids.iter().collect::<BTreeSet<_>>().len() != member_bot_ids.len()
        {
            return Err(invalid("a chat requires 1–100 distinct Bots"));
        }
        for id in &member_bot_ids {
            self.bots.bot(id)?;
        }
        let primary_bot_id = primary
            .map(str::to_owned)
            .unwrap_or_else(|| member_bot_ids[0].clone());
        if !member_bot_ids.contains(&primary_bot_id) {
            return Err(invalid("the primary Bot must be a chat member"));
        }
        let now = chrono::Utc::now().timestamp();
        let chat = Chat {
            id: Uuid::new_v4().to_string(),
            sequence: 0,
            first_user_message: None,
            created_at: now,
            updated_at: now,
            deleted: false,
            workspace,
            participants: member_bot_ids
                .into_iter()
                .map(|bot_id| ChatParticipant {
                    bot_id,
                    session_id: Uuid::new_v4().to_string(),
                    published_sequence: 0,
                })
                .collect(),
            primary_bot_id: Some(primary_bot_id),
            retired_participants: Vec::new(),
            pending: Vec::new(),
        };
        self.save(&chat, None).await?;
        Ok(chat.id)
    }

    pub(crate) async fn post_user(
        &self,
        chat_id: &str,
        id: String,
        message: MessageSubmission,
        recipients: &[String],
    ) -> Result<()> {
        if !matches!(message.author, MessageAuthor::User) {
            return Err(invalid("only authenticated user messages may be submitted"));
        }
        self.post_message(chat_id, id, message, recipients).await
    }

    /// Trusted gateway input, including voice; external callers use `post_user`.
    pub(crate) async fn post_message(
        &self,
        chat_id: &str,
        id: String,
        mut message: MessageSubmission,
        recipients: &[String],
    ) -> Result<()> {
        if id.trim().is_empty() || id.len() > 256 {
            return Err(invalid("chat message ID must be 1–256 bytes"));
        }
        validate_message(&message)?;
        let _gate = self.gate.lock().await;
        let mut chat = self
            .load(chat_id)
            .await?
            .ok_or_else(|| invalid("unknown chat"))?;
        if self.contains_message(chat_id, &id).await? {
            return Ok(());
        }
        if let Some(reply) = &message.reply {
            self.validate_target(chat_id, &reply.target).await?;
        }
        let recipients = if recipients.is_empty() {
            chat.primary_bot_id.iter().cloned().collect()
        } else {
            recipients.to_vec()
        };
        validate_recipients(&chat, &recipients, None)?;
        match &mut message.author {
            MessageAuthor::User if chat.first_user_message.is_none() => {
                chat.first_user_message = Some(message.text.chars().take(512).collect());
            }
            MessageAuthor::Peer { session_id, .. } => *session_id = chat.id.clone(),
            _ => {}
        }
        let entry = ChatMessage {
            user_message_id: id.clone(),
            id,
            message,
            reply_depth: 0,
            recipients,
        };
        self.append(&mut chat, entry).await
    }

    async fn append(&self, chat: &mut Chat, mut entry: ChatMessage) -> Result<()> {
        if !entry.recipients.is_empty() {
            chat.pending.push(entry.clone());
            if entry.reply_depth > 0 && pending_capacity_exceeded(&chat.pending)? {
                chat.pending.pop();
                entry.recipients.clear();
                entry.message.text.push_str("\n\nAutomatic replies were not queued because the chat delivery queue is full.");
            }
        }
        validate_pending(chat)?;
        let sequence = next_sequence(chat)?;
        let event = Event {
            submission_id: Some(entry.id.clone()),
            msg: EventMsg::Message(MessageEvent {
                author: entry.message.author.clone(),
                delivery: MessageDelivery::Turn,
                text: entry.message.text.clone(),
                attachments: entry.message.attachments.clone(),
                reply: entry.message.reply.clone(),
                message_target: Some(MessageTarget {
                    checkpoint_sequence: sequence,
                    batch_item_count: 1,
                }),
            }),
        };
        let record = journal(sequence, event);
        self.persist(chat, &[(record.clone(), Some(entry))]).await?;
        self.changed(&chat.id, vec![record]);
        self.notify_pending(&chat.id);
        Ok(())
    }

    pub(crate) async fn pending_chat_ids(&self) -> Result<Vec<String>> {
        Ok(self
            .chats(false)
            .await?
            .into_iter()
            .filter(|chat| !chat.pending.is_empty())
            .map(|chat| chat.id)
            .collect())
    }

    pub(crate) async fn pending_deliveries(
        &self,
        chat_id: &str,
    ) -> Result<Vec<(String, ChatMessage)>> {
        Ok(self
            .load(chat_id)
            .await?
            .into_iter()
            .flat_map(|chat| chat.pending)
            .flat_map(|entry| {
                entry
                    .recipients
                    .clone()
                    .into_iter()
                    .map(move |bot_id| (bot_id, entry.clone()))
            })
            .collect())
    }

    pub(crate) async fn cancel_pending(&self, chat_id: &str) -> Result<()> {
        let _gate = self.gate.lock().await;
        if let Some(mut chat) = self.load(chat_id).await? {
            chat.pending.clear();
            self.save(&chat, None).await?;
        }
        Ok(())
    }

    pub(crate) async fn settle_delivery(
        &self,
        message_id: &str,
        session_id: &str,
        target_bot_id: &str,
        outcome: ChatRunOutcome,
    ) -> Result<bool> {
        let _gate = self.gate.lock().await;
        let Some(mut chat) = self.chat_for_session(session_id).await? else {
            return Ok(false);
        };
        if chat.session_id(target_bot_id) != Some(session_id) {
            return Ok(false);
        }
        let Some(source) = chat
            .pending
            .iter()
            .find(|entry| {
                entry.id == message_id && entry.recipients.iter().any(|id| id == target_bot_id)
            })
            .cloned()
        else {
            return Ok(false);
        };
        acknowledge(&mut chat, message_id, target_bot_id);
        if chat.participants.len() == 1 && matches!(&outcome, ChatRunOutcome::Succeeded { .. }) {
            self.save(&chat, None).await?;
            self.notify_pending(&chat.id);
            return Ok(true);
        }
        let reply_depth = source.reply_depth.saturating_add(1);
        let (mut text, mut recipients) = match outcome {
            ChatRunOutcome::Succeeded { summary } => match serde_json::from_str::<BotReply>(&summary) {
                Ok(reply) if !reply.text.trim().is_empty() && validate_recipients(&chat, &reply.recipient_bot_ids, Some(target_bot_id)).is_ok() => (reply.text, reply.recipient_bot_ids),
                _ => ("Could not publish the Bot reply: the response did not contain a valid message and recipient list.".into(), Vec::new()),
            },
            ChatRunOutcome::Failed { message } => (format!("Could not finish: {message}"), Vec::new()),
        };
        if reply_depth > MAX_REPLY_DEPTH && !recipients.is_empty() {
            recipients.clear();
            text.push_str("\n\nAutomatic replies paused at the reply limit. Send another message to continue.");
        }
        let bot = self.bots.bot(target_bot_id)?;
        let id = Uuid::new_v4().to_string();
        let message = MessageSubmission {
            author: MessageAuthor::Peer {
                message_id: id.clone(),
                session_id: chat.id.clone(),
                handle: bot.handle,
                symbol: None,
            },
            text,
            attachments: Vec::new(),
            reply: None,
            requested_delivery: None,
            target_turn_id: None,
        };
        self.append(
            &mut chat,
            ChatMessage {
                id,
                message,
                user_message_id: source.user_message_id,
                reply_depth,
                recipients,
            },
        )
        .await?;
        Ok(true)
    }

    pub(crate) async fn has_pending_source_sessions(&self, session_ids: &[String]) -> Result<bool> {
        Ok(self.chats(false).await?.iter().any(|chat| {
            !chat.pending.is_empty()
                && (session_ids.contains(&chat.id)
                    || chat
                        .participants
                        .iter()
                        .any(|p| session_ids.contains(&p.session_id)))
        }))
    }

    pub(crate) async fn reassign(&self, chat_id: &str, bot_id: &str) -> Result<()> {
        self.bots.bot(bot_id)?;
        let _gate = self.gate.lock().await;
        let mut chat = self
            .load(chat_id)
            .await?
            .ok_or_else(|| invalid("unknown chat"))?;
        if chat.participants.len() != 1 || !chat.pending.is_empty() {
            return Err(invalid("only an idle chat with one Bot can be reassigned"));
        }
        if chat.contains_bot(bot_id) {
            return Ok(());
        }
        chat.retired_participants.append(&mut chat.participants);
        chat.participants.push(ChatParticipant {
            bot_id: bot_id.into(),
            session_id: Uuid::new_v4().to_string(),
            published_sequence: 0,
        });
        chat.primary_bot_id = Some(bot_id.into());
        self.save(&chat, None).await
    }

    pub(crate) async fn remove_bot(&self, bot_id: &str) -> Result<()> {
        let _gate = self.gate.lock().await;
        for mut chat in self.chats(false).await? {
            if !chat.execution_participants().any(|p| p.bot_id == bot_id) {
                continue;
            }
            chat.participants.retain(|p| p.bot_id != bot_id);
            chat.retired_participants.retain(|p| p.bot_id != bot_id);
            if chat.primary_bot_id.as_deref() == Some(bot_id) {
                chat.primary_bot_id = chat.participants.first().map(|p| p.bot_id.clone());
            }
            for entry in &mut chat.pending {
                entry.recipients.retain(|id| id != bot_id);
            }
            chat.pending.retain(|entry| !entry.recipients.is_empty());
            self.save(&chat, None).await?;
        }
        Ok(())
    }

    pub(crate) async fn chat_context(
        &self,
        bot_id: &str,
        session_id: &str,
    ) -> Result<Option<String>> {
        let Some(chat) = self.chat_for_session(session_id).await? else {
            return Ok(None);
        };
        if chat.session_id(bot_id) != Some(session_id) {
            return Ok(None);
        }
        let mut original = Vec::new();
        let mut seen = BTreeSet::new();
        for pending in &chat.pending {
            if pending.recipients.iter().any(|id| id == bot_id)
                && seen.insert(&pending.user_message_id)
                && let Some(source) = self
                    .message_by_id(&chat.id, &pending.user_message_id)
                    .await?
            {
                original.push(serde_json::json!({"message_id": source.id, "author": source.message.author, "text": bounded_text(&source.message.text, CONTEXT_BYTES / 8)}));
            }
            if original.len() == 4 {
                break;
            }
        }
        let page = self
            .message_page(
                &chat.id,
                EventPageRequest {
                    before_sequence: None,
                    limit: 100,
                },
            )
            .await?;
        let mut history = Vec::new();
        let mut bytes = serde_json::to_vec(&original)?.len();
        for record in page.events {
            let metadata = self.message_by_sequence(&chat.id, record.sequence).await?;
            let (author, text, attachments) = match record.event.msg {
                EventMsg::Message(message) => (
                    serde_json::to_value(message.author)?,
                    message.text,
                    message.attachments,
                ),
                EventMsg::AssistantMessage(message) => (
                    metadata
                        .map(|entry| serde_json::to_value(entry.message.author))
                        .transpose()?
                        .unwrap_or(serde_json::Value::Null),
                    message
                        .content
                        .into_iter()
                        .filter(|part| part.phase != ModelStepContentPhase::Reasoning)
                        .map(|part| part.text)
                        .collect(),
                    Vec::new(),
                ),
                _ => continue,
            };
            let value = serde_json::json!({"sequence": record.sequence, "author": author, "text": bounded_text(&text, CONTEXT_BYTES / 4), "attachments": attachments});
            let line = serde_json::to_string(&value)?;
            if bytes + line.len() > CONTEXT_BYTES {
                break;
            }
            bytes += line.len();
            history.push(line);
        }
        history.reverse();
        Ok(Some(format!(
            "Original requests for pending work (new human messages may revise these):\n{}\n\nRecent shared messages (oldest first):\n{}",
            serde_json::to_string(&original)?,
            history.join("\n")
        )))
    }

    pub(crate) async fn observe_record(
        &self,
        session_id: &str,
        source: &JournalEvent,
        artifacts: &[Event],
    ) -> Result<()> {
        let _gate = self.gate.lock().await;
        let Some(mut chat) = self.chat_for_session(session_id).await? else {
            return Ok(());
        };
        let participant = chat
            .participants
            .iter()
            .position(|p| p.session_id == session_id)
            .expect("resolved participant");
        if source.sequence <= chat.participants[participant].published_sequence {
            return Ok(());
        }
        let bot_id = chat.participants[participant].bot_id.clone();
        let mut records = Vec::new();
        for event in std::iter::once(&source.event).chain(artifacts) {
            if let Some(record) = self.project_event(&mut chat, &bot_id, event)? {
                records.push(record);
            }
        }
        chat.participants[participant].published_sequence = source.sequence;
        self.persist(&chat, &records).await?;
        if !records.is_empty() {
            self.changed(
                &chat.id,
                records.into_iter().map(|(record, _)| record).collect(),
            );
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn observe_event(&self, session_id: &str, event: &Event) -> Result<()> {
        let Some(chat) = self.chat_for_session(session_id).await? else {
            return Ok(());
        };
        let participant = chat
            .participants
            .iter()
            .find(|p| p.session_id == session_id)
            .unwrap();
        let sequence = participant
            .published_sequence
            .checked_add(1)
            .ok_or_else(|| invalid("execution journal sequence exhausted"))?;
        self.observe_record(session_id, &journal(sequence, event.clone()), &[])
            .await
    }

    fn project_event(
        &self,
        chat: &mut Chat,
        bot_id: &str,
        event: &Event,
    ) -> Result<Option<(JournalEvent, Option<ChatMessage>)>> {
        if let EventMsg::Message(message) = &event.msg {
            if message.delivery == MessageDelivery::Steer
                && let Some(id) = &event.submission_id
            {
                acknowledge(chat, id, bot_id);
            }
            // Chat input is already durable. Execution-local peer messages stay private.
            return Ok(None);
        }
        if event.submission_id.as_ref().is_some_and(|id| {
            !chat.pending.iter().any(|entry| {
                &entry.id == id && entry.recipients.iter().any(|recipient| recipient == bot_id)
            })
        }) && !matches!(
            event.msg,
            EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
        ) {
            return Ok(None);
        }
        let single = chat.participants.len() == 1;
        let publish = if single {
            !matches!(
                event.msg,
                EventMsg::SessionConfigured(_)
                    | EventMsg::SessionHistory(_)
                    | EventMsg::SessionResumeRequested(_)
            )
        } else {
            matches!(
                event.msg,
                EventMsg::TurnStarted(_)
                    | EventMsg::TurnComplete(_)
                    | EventMsg::TurnAborted(_)
                    | EventMsg::ExecApprovalRequest(_)
            ) || matches!(&event.msg, EventMsg::Frontend(mobius::protocol::FrontendEvent::Render { block, .. }) if block.role == mobius::protocol::FrontendBlockRole::Artifact)
        };
        if !publish {
            return Ok(None);
        }
        let mut event = event.clone();
        let sequence = next_sequence(chat)?;
        match &mut event.msg {
            EventMsg::AssistantMessage(message) => {
                message.session_id = chat.id.clone();
                message.message_target = Some(MessageTarget {
                    checkpoint_sequence: sequence,
                    batch_item_count: 1,
                });
            }
            EventMsg::AssistantContentDelta(message) => message.session_id = chat.id.clone(),
            EventMsg::ModelStepStarted(step) => step.session_id = chat.id.clone(),
            EventMsg::ModelStepCompleted(step) => step.session_id = chat.id.clone(),
            EventMsg::WebSearchBegin(search) => search.session_id = chat.id.clone(),
            EventMsg::WebSearchEnd(search) => search.session_id = chat.id.clone(),
            EventMsg::ExecApprovalRequest(approval) => self.label_approval(bot_id, approval)?,
            _ => {}
        }
        let metadata = if let EventMsg::AssistantMessage(message) = &event.msg {
            let id = Uuid::new_v4().to_string();
            Some(ChatMessage {
                id: id.clone(),
                user_message_id: event.submission_id.clone().unwrap_or_else(|| id.clone()),
                reply_depth: 0,
                recipients: Vec::new(),
                message: MessageSubmission {
                    author: MessageAuthor::Peer {
                        message_id: id,
                        session_id: chat.id.clone(),
                        handle: self.bots.bot(bot_id)?.handle,
                        symbol: None,
                    },
                    text: message
                        .content
                        .iter()
                        .filter(|part| part.phase != ModelStepContentPhase::Reasoning)
                        .map(|part| part.text.as_str())
                        .collect(),
                    attachments: Vec::new(),
                    reply: None,
                    requested_delivery: None,
                    target_turn_id: None,
                },
            })
        } else {
            None
        };
        Ok(Some((journal(sequence, event), metadata)))
    }

    pub(crate) fn label_approval(
        &self,
        bot_id: &str,
        approval: &mut mobius::protocol::ExecApprovalRequestEvent,
    ) -> Result<()> {
        approval.reason = format!("@{}: {}", self.bots.bot(bot_id)?.handle, approval.reason);
        Ok(())
    }

    pub(crate) fn notify_ready_changed(&self, chat_id: &str) {
        self.changed(chat_id, Vec::new());
    }

    fn changed(&self, chat_id: &str, records: Vec<JournalEvent>) {
        let _ = self.deliveries.send(ChatDelivery::Changed {
            chat_id: chat_id.into(),
            records,
        });
    }
    pub(crate) fn notify_pending(&self, chat_id: &str) {
        let _ = self.deliveries.send(ChatDelivery::Pending {
            chat_id: chat_id.into(),
        });
    }
    pub(crate) fn retry_pending(&self) {
        let _ = self.deliveries.send(ChatDelivery::RetryPending);
    }
}

impl Chat {
    pub(crate) fn execution_participants(&self) -> impl Iterator<Item = &ChatParticipant> {
        self.participants.iter().chain(&self.retired_participants)
    }
    pub(crate) fn member_bot_ids(&self) -> Vec<String> {
        self.participants.iter().map(|p| p.bot_id.clone()).collect()
    }
    pub(crate) fn contains_bot(&self, bot_id: &str) -> bool {
        self.participants.iter().any(|p| p.bot_id == bot_id)
    }
    pub(crate) fn session_id(&self, bot_id: &str) -> Option<&str> {
        self.participants
            .iter()
            .find(|p| p.bot_id == bot_id)
            .map(|p| p.session_id.as_str())
    }
    pub(crate) fn context(&self) -> SessionContext {
        SessionContext {
            workspace_id: Some(crate::config::workspace_id(&self.workspace)),
            workspace_label: Some(self.workspace.display().to_string()),
            ..SessionContext::default()
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BotReply {
    pub(crate) text: String,
    pub(crate) recipient_bot_ids: Vec<String>,
}

fn acknowledge(chat: &mut Chat, message_id: &str, bot_id: &str) {
    for entry in chat
        .pending
        .iter_mut()
        .filter(|entry| entry.id == message_id)
    {
        entry.recipients.retain(|id| id != bot_id);
    }
    chat.pending.retain(|entry| !entry.recipients.is_empty());
}
fn next_sequence(chat: &mut Chat) -> Result<u64> {
    chat.sequence = chat
        .sequence
        .checked_add(1)
        .ok_or_else(|| invalid("chat sequence exhausted"))?;
    chat.updated_at = chrono::Utc::now().timestamp();
    Ok(chat.sequence)
}
fn journal(sequence: u64, event: Event) -> JournalEvent {
    JournalEvent {
        sequence,
        recorded_at_ms: chrono::Utc::now().timestamp_millis(),
        event,
        stream_metrics: Vec::new(),
    }
}
fn bounded_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        text.into()
    } else {
        format!(
            "{}… [use search_history for the full message]",
            &text[..text.floor_char_boundary(max_bytes)]
        )
    }
}
fn validate_chat(chat: &Chat) -> Result<()> {
    if Uuid::parse_str(&chat.id).is_err()
        || chat.participants.len() > MAX_MEMBERS
        || chat
            .participants
            .iter()
            .map(|p| &p.bot_id)
            .collect::<BTreeSet<_>>()
            .len()
            != chat.participants.len()
        || chat
            .execution_participants()
            .map(|p| &p.session_id)
            .collect::<BTreeSet<_>>()
            .len()
            != chat.participants.len() + chat.retired_participants.len()
        || !chat.workspace.is_absolute()
        || chat
            .primary_bot_id
            .as_ref()
            .is_some_and(|id| !chat.contains_bot(id))
        || chat.primary_bot_id.is_none() != chat.participants.is_empty()
    {
        return Err(invalid("invalid chat metadata"));
    }
    for p in chat.execution_participants() {
        Uuid::parse_str(&p.bot_id).map_err(|_| invalid("invalid participant Bot ID"))?;
        Uuid::parse_str(&p.session_id).map_err(|_| invalid("invalid participant execution ID"))?;
        if p.session_id == chat.id {
            return Err(invalid("a participant execution cannot be its public chat"));
        }
    }
    validate_pending(chat)
}
fn validate_recipients(chat: &Chat, recipients: &[String], sender: Option<&str>) -> Result<()> {
    if recipients.len() > MAX_MEMBERS
        || recipients.iter().collect::<BTreeSet<_>>().len() != recipients.len()
        || recipients
            .iter()
            .any(|id| !chat.contains_bot(id) || sender == Some(id.as_str()))
    {
        return Err(invalid(
            "recipients must be distinct current chat members other than the sender",
        ));
    }
    Ok(())
}
fn validate_pending(chat: &Chat) -> Result<()> {
    if pending_capacity_exceeded(&chat.pending)? {
        return Err(invalid(
            "chat delivery queue is full; wait for a member to finish",
        ));
    }
    let mut ids = BTreeSet::new();
    for entry in &chat.pending {
        if entry.id.is_empty()
            || entry.id.len() > 256
            || !ids.insert(&entry.id)
            || entry.user_message_id.is_empty()
            || entry.reply_depth > MAX_REPLY_DEPTH
            || entry.recipients.is_empty()
        {
            return Err(invalid("invalid chat delivery"));
        }
        validate_recipients(chat, &entry.recipients, None)?;
        validate_message(&entry.message)?;
    }
    Ok(())
}
fn pending_capacity_exceeded(pending: &[ChatMessage]) -> Result<bool> {
    Ok(pending.len() > MAX_PENDING || serde_json::to_vec(pending)?.len() > MAX_PENDING_BYTES)
}
fn validate_message(message: &MessageSubmission) -> Result<()> {
    message.validate(mobius::backend::session_files::session_file_limits())?;
    Ok(())
}
fn invalid(message: impl Into<String>) -> Error {
    Error::Config(message.into())
}

#[cfg(test)]
mod tests;
