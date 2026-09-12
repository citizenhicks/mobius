//! Gateway-owned shared conversations and durable, mention-addressed Bot delivery.

mod storage;

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mobius::backend::checkpoint::{EventPageRequest, JournalEvent};
use mobius::protocol::{
    Event, EventMsg, MessageAuthor, MessageDelivery, MessageEvent, MessageSubmission,
    SessionContext,
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

/// Shared conversation metadata; messages live in the gateway chat journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GroupChat {
    pub(crate) id: String,
    pub(crate) sequence: u64,
    pub(crate) first_user_message: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    pub(crate) deleted: bool,
    pub(crate) workspace: PathBuf,
    pub(crate) member_bot_ids: Vec<String>,
    pending: Vec<GroupMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GroupMessage {
    pub(crate) id: String,
    pub(crate) message: MessageSubmission,
    reply_depth: u8,
    recipients: Vec<String>,
}

pub(crate) struct PendingDelivery {
    pub(crate) chat_id: String,
    pub(crate) workspace: PathBuf,
    pub(crate) entry: GroupMessage,
}

pub(crate) enum GroupRunOutcome {
    Succeeded { summary: String },
    Failed { message: String },
}

pub(crate) enum GroupDelivery {
    Changed {
        chat_id: String,
        records: Vec<JournalEvent>,
    },
    RetryPending,
    Pending {
        target_bot_id: String,
    },
    Acknowledged {
        target_bot_id: String,
        message_id: String,
    },
    Rejected {
        target_bot_id: String,
        message_id: String,
    },
    CapacityAvailable {
        target_bot_id: String,
    },
}

#[derive(Clone)]
pub(crate) struct GroupStore {
    connection: Arc<std::sync::Mutex<rusqlite::Connection>>,
    bots: Arc<BotStore>,
    // Serializes chat read/modify/write transactions, never Agent startup or submission.
    gate: Arc<Mutex<()>>,
    deliveries: mpsc::UnboundedSender<GroupDelivery>,
}

pub(crate) struct GroupDeliveryClaim {
    store: GroupStore,
    delivery: PendingDelivery,
    session_id: String,
    target_bot_id: String,
}

impl GroupDeliveryClaim {
    pub(crate) fn delivery(&self) -> &PendingDelivery {
        &self.delivery
    }
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    // The caller holds the gateway session-mutation read guard through acceptance.
    pub(crate) async fn accept<T>(self, acceptance: impl Future<Output = T>) -> Result<Option<T>> {
        let Some(chat) = self.store.load(&self.delivery.chat_id).await? else {
            return Ok(None);
        };
        if !chat.pending.iter().any(|message| {
            message.id == self.delivery.entry.id && message.recipients.contains(&self.target_bot_id)
        }) {
            return Ok(None);
        }
        Ok(Some(acceptance.await))
    }
}

impl GroupStore {
    pub(crate) fn new(
        state_dir: &Path,
        bots: Arc<BotStore>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<GroupDelivery>)> {
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
    ) -> Result<String> {
        if !(2..=MAX_MEMBERS).contains(&member_bot_ids.len())
            || member_bot_ids.iter().collect::<BTreeSet<_>>().len() != member_bot_ids.len()
        {
            return Err(invalid("a group chat requires 2–100 distinct Bots"));
        }
        for id in &member_bot_ids {
            self.bots.bot(id)?;
        }
        let now = chrono::Utc::now().timestamp();
        let chat = GroupChat {
            id: Uuid::new_v4().to_string(),
            sequence: 0,
            first_user_message: None,
            created_at: now,
            updated_at: now,
            deleted: false,
            workspace,
            member_bot_ids,
            pending: Vec::new(),
        };
        validate_chat(&chat)?;
        self.save(&chat, None).await?;
        Ok(chat.id)
    }

    pub(crate) async fn post_user(
        &self,
        chat_id: &str,
        id: String,
        message: MessageSubmission,
    ) -> Result<()> {
        if id.trim().is_empty() || id.len() > 256 {
            return Err(invalid("group message ID must be 1–256 bytes"));
        }
        if !matches!(message.author, MessageAuthor::User) {
            return Err(invalid(
                "only authenticated user messages may be submitted to a group",
            ));
        }
        validate_message(&message)?;
        let _gate = self.gate.lock().await;
        let mut chat = self
            .load(chat_id)
            .await?
            .ok_or_else(|| invalid("unknown group chat"))?;
        if self.contains_message(chat_id, &id).await? {
            return Ok(());
        }
        if chat.first_user_message.is_none() {
            chat.first_user_message = Some(message.text.chars().take(512).collect());
        }
        self.append(&mut chat, id, message, 0).await
    }

    async fn append(
        &self,
        chat: &mut GroupChat,
        id: String,
        mut message: MessageSubmission,
        reply_depth: u8,
    ) -> Result<()> {
        let author_bot_id = match &message.author {
            MessageAuthor::User => None,
            MessageAuthor::Peer { session_id, .. } => {
                participant_chat_id(session_id).and_then(|_| session_id.rsplit(':').next())
            }
        };
        let mentions = mentioned_handles(&message.text);
        let mut recipients = Vec::new();
        if reply_depth <= MAX_REPLY_DEPTH {
            for bot_id in &chat.member_bot_ids {
                if Some(bot_id.as_str()) == author_bot_id {
                    continue;
                }
                if mentions.contains(self.bots.bot(bot_id)?.handle.as_str()) {
                    recipients.push(bot_id.clone());
                }
            }
        }
        if !recipients.is_empty() {
            chat.pending.push(GroupMessage {
                id: id.clone(),
                message: message.clone(),
                reply_depth,
                recipients: recipients.clone(),
            });
            if reply_depth > 0 && pending_capacity_exceeded(&chat.pending)? {
                chat.pending.pop();
                recipients.clear();
                message.text.push_str("\n\nAutomatic replies were not queued because the group delivery queue is full. Mention the members again when capacity is available.");
            }
        }
        validate_pending(chat)?;
        chat.sequence = chat
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("chat sequence exhausted"))?;
        chat.updated_at = chrono::Utc::now().timestamp();
        let event = Event {
            submission_id: Some(id),
            msg: EventMsg::Message(MessageEvent {
                author: message.author,
                delivery: MessageDelivery::Turn,
                text: message.text,
                attachments: message.attachments,
                reply: message.reply,
                message_target: Some(mobius::protocol::MessageTarget {
                    checkpoint_sequence: chat.sequence,
                    batch_item_count: 1,
                }),
            }),
        };
        let record = JournalEvent {
            sequence: chat.sequence,
            recorded_at_ms: chrono::Utc::now().timestamp_millis(),
            event,
            stream_metrics: Vec::new(),
        };
        self.save(chat, Some(&record)).await?;
        let _ = self.deliveries.send(GroupDelivery::Changed {
            chat_id: chat.id.clone(),
            records: vec![record],
        });
        for bot_id in recipients {
            self.notify_pending(&bot_id);
        }
        Ok(())
    }

    pub(crate) async fn pending_recipient_bot_ids(&self) -> Result<Vec<String>> {
        let mut bots = BTreeSet::new();
        for chat in self.chats(false).await? {
            for message in chat.pending {
                bots.extend(message.recipients);
            }
        }
        Ok(bots.into_iter().collect())
    }

    pub(crate) async fn claim_next_delivery(
        &self,
        target_bot_id: &str,
    ) -> Result<Option<GroupDeliveryClaim>> {
        self.bots.bot(target_bot_id)?;
        // ponytail: scan stored queues; index recipients if dispatch latency warrants it.
        let mut pending = Vec::new();
        for chat in self.chats(false).await? {
            if let Some(entry) = chat
                .pending
                .into_iter()
                .find(|message| message.recipients.iter().any(|id| id == target_bot_id))
            {
                pending.push(PendingDelivery {
                    chat_id: chat.id,
                    workspace: chat.workspace,
                    entry,
                });
            }
        }
        // Catalog order is newest first; service the oldest waiting chat first.
        let Some(delivery) = pending.pop() else {
            return Ok(None);
        };
        Ok(Some(GroupDeliveryClaim {
            session_id: participant_session_id(&delivery.chat_id, target_bot_id),
            store: self.clone(),
            delivery,
            target_bot_id: target_bot_id.into(),
        }))
    }

    pub(crate) async fn settle_delivery(
        &self,
        message_id: &str,
        session_id: &str,
        target_bot_id: &str,
        outcome: GroupRunOutcome,
    ) -> Result<bool> {
        let Some(chat_id) = participant_chat_id(session_id) else {
            return Ok(false);
        };
        if participant_session_id(chat_id, target_bot_id) != session_id {
            return Ok(false);
        }
        let _gate = self.gate.lock().await;
        let Some(mut chat) = self.load(chat_id).await? else {
            return Ok(false);
        };
        let Some(source) = chat.pending.iter_mut().find(|entry| {
            entry.id == message_id && entry.recipients.iter().any(|id| id == target_bot_id)
        }) else {
            return Ok(false);
        };
        let reply_depth = source.reply_depth.saturating_add(1);
        source.recipients.retain(|id| id != target_bot_id);
        chat.pending.retain(|entry| !entry.recipients.is_empty());
        let bot = self.bots.bot(target_bot_id)?;
        let text = match outcome {
            GroupRunOutcome::Succeeded { summary } => summary,
            GroupRunOutcome::Failed { message } => {
                format!("Could not finish: {}", message.replace('@', "＠"))
            }
        };
        let id = Uuid::new_v4().to_string();
        let message = MessageSubmission {
            author: MessageAuthor::Peer {
                message_id: id.clone(),
                session_id: session_id.into(),
                handle: bot.handle,
                symbol: None,
            },
            text,
            attachments: Vec::new(),
            reply: None,
            requested_delivery: None,
            target_turn_id: None,
        };
        self.append(&mut chat, id, message, reply_depth).await?;
        self.notify_acknowledged(message_id, target_bot_id);
        Ok(true)
    }

    pub(crate) async fn has_pending_source_sessions(&self, session_ids: &[String]) -> Result<bool> {
        for chat in self.chats(false).await? {
            if chat.pending.iter().any(|entry| matches!(&entry.message.author, MessageAuthor::Peer { session_id, .. } if session_ids.contains(session_id))) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) async fn participant_sessions(&self, chat_ids: &[String]) -> Result<Vec<String>> {
        let mut sessions = Vec::new();
        for chat_id in chat_ids {
            if let Some(chat) = self.load(chat_id).await? {
                sessions.extend(
                    chat.member_bot_ids
                        .iter()
                        .map(|bot_id| participant_session_id(chat_id, bot_id)),
                );
            }
        }
        Ok(sessions)
    }

    pub(crate) async fn remove_bot(&self, bot_id: &str) -> Result<()> {
        let _gate = self.gate.lock().await;
        for mut chat in self.chats(false).await? {
            if !chat.member_bot_ids.iter().any(|id| id == bot_id) {
                continue;
            }
            chat.member_bot_ids.retain(|id| id != bot_id);
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
        let Some(chat_id) = participant_chat_id(session_id) else {
            return Ok(None);
        };
        if participant_session_id(chat_id, bot_id) != session_id {
            return Ok(None);
        }
        let Some(chat) = self.load(chat_id).await? else {
            return Ok(None);
        };
        if !chat.member_bot_ids.iter().any(|id| id == bot_id) {
            return Ok(None);
        }
        let roster = chat
            .member_bot_ids
            .iter()
            .map(|id| {
                self.bots
                    .bot(id)
                    .map(|bot| format!("@{} — {}", bot.handle, bot.name))
            })
            .collect::<Result<Vec<_>>>()?
            .join("\n");
        let page = self
            .message_page(
                chat_id,
                EventPageRequest {
                    before_sequence: None,
                    limit: 100,
                },
            )
            .await?;
        let mut history = Vec::new();
        let mut bytes = 0;
        for journal in page.events {
            let EventMsg::Message(message) = journal.event.msg else {
                continue;
            };
            let author = match message.author {
                MessageAuthor::User => "user".into(),
                MessageAuthor::Peer { handle, .. } => handle,
            };
            let mut value = serde_json::json!({"author": author, "text": message.text, "attachments": message.attachments});
            let mut line = serde_json::to_string(&value)?;
            if bytes + line.len() > CONTEXT_BYTES {
                if !history.is_empty() {
                    break;
                }
                let text = value["text"].as_str().unwrap_or_default();
                value["text"] = format!(
                    "{}… [use search_history to read the full message]",
                    text.chars().take(CONTEXT_BYTES / 8).collect::<String>()
                )
                .into();
                line = serde_json::to_string(&value)?;
            }
            bytes += line.len();
            history.push(line);
        }
        history.reverse();
        Ok(Some(format!(
            "Members:\n{roster}\n\nRecent shared messages (oldest first):\n{}",
            history.join("\n")
        )))
    }

    pub(crate) async fn observe_event(&self, session_id: &str, event: &Event) -> Result<()> {
        let Some(chat_id) = participant_chat_id(session_id) else {
            return Ok(());
        };
        if !matches!(
            event.msg,
            EventMsg::TurnStarted(_)
                | EventMsg::TurnComplete(_)
                | EventMsg::TurnAborted(_)
                | EventMsg::ExecApprovalRequest(_)
        ) && !matches!(&event.msg, EventMsg::Frontend(mobius::protocol::FrontendEvent::Render { block, .. })
                if block.role == mobius::protocol::FrontendBlockRole::Artifact)
        {
            return Ok(());
        }
        let _gate = self.gate.lock().await;
        let Some(chat) = self.load(chat_id).await? else {
            return Ok(());
        };
        let Some(bot_id) = chat
            .member_bot_ids
            .iter()
            .find(|id| participant_session_id(chat_id, id) == session_id)
        else {
            return Ok(());
        };
        let mut event = event.clone();
        if let EventMsg::ExecApprovalRequest(approval) = &mut event.msg {
            self.label_approval(bot_id, approval)?;
        }
        let mut chat = chat;
        chat.sequence = chat
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("chat sequence exhausted"))?;
        let record = JournalEvent {
            sequence: chat.sequence,
            recorded_at_ms: chrono::Utc::now().timestamp_millis(),
            event,
            stream_metrics: Vec::new(),
        };
        self.save(&chat, Some(&record)).await?;
        let _ = self.deliveries.send(GroupDelivery::Changed {
            chat_id: chat_id.into(),
            records: vec![record],
        });
        Ok(())
    }

    pub(crate) fn label_approval(
        &self,
        bot_id: &str,
        approval: &mut mobius::protocol::ExecApprovalRequestEvent,
    ) -> Result<()> {
        approval.reason = format!("@{}: {}", self.bots.bot(bot_id)?.handle, approval.reason);
        Ok(())
    }

    pub(crate) fn notify_pending(&self, target_bot_id: &str) {
        let _ = self.deliveries.send(GroupDelivery::Pending {
            target_bot_id: target_bot_id.into(),
        });
    }
    pub(crate) fn retry_pending(&self) {
        let _ = self.deliveries.send(GroupDelivery::RetryPending);
    }
    pub(crate) fn notify_acknowledged(&self, message_id: &str, target_bot_id: &str) {
        let _ = self.deliveries.send(GroupDelivery::Acknowledged {
            target_bot_id: target_bot_id.into(),
            message_id: message_id.into(),
        });
    }
    pub(crate) fn notify_rejected(&self, message_id: &str, target_bot_id: &str) {
        let _ = self.deliveries.send(GroupDelivery::Rejected {
            target_bot_id: target_bot_id.into(),
            message_id: message_id.into(),
        });
    }
    pub(crate) fn notify_capacity_available(&self, target_bot_id: &str) {
        let _ = self.deliveries.send(GroupDelivery::CapacityAvailable {
            target_bot_id: target_bot_id.into(),
        });
    }
}

impl GroupChat {
    pub(crate) fn context(&self) -> SessionContext {
        SessionContext {
            workspace_id: Some(crate::config::workspace_id(&self.workspace)),
            workspace_label: Some(self.workspace.display().to_string()),
            origin_label: Some("Group chat".into()),
            ..SessionContext::default()
        }
    }
}

fn validate_chat(chat: &GroupChat) -> Result<()> {
    if Uuid::parse_str(&chat.id).is_err()
        || chat.member_bot_ids.len() > MAX_MEMBERS
        || chat.member_bot_ids.iter().collect::<BTreeSet<_>>().len() != chat.member_bot_ids.len()
        || !chat.workspace.is_absolute()
    {
        return Err(invalid("invalid group chat metadata"));
    }
    for id in &chat.member_bot_ids {
        Uuid::parse_str(id).map_err(|_| invalid("invalid group member ID"))?;
    }
    validate_pending(chat)
}

pub(crate) fn participant_session_id(chat_id: &str, bot_id: &str) -> String {
    format!("group:{chat_id}:{bot_id}")
}

pub(crate) fn participant_chat_id(session_id: &str) -> Option<&str> {
    let (chat, bot) = session_id.strip_prefix("group:")?.split_once(':')?;
    Uuid::parse_str(chat).ok()?;
    Uuid::parse_str(bot).ok()?;
    Some(chat)
}

fn validate_pending(chat: &GroupChat) -> Result<()> {
    if pending_capacity_exceeded(&chat.pending)? {
        return Err(invalid(
            "group chat delivery queue is full; wait for a member to finish",
        ));
    }
    let mut ids = BTreeSet::new();
    for entry in &chat.pending {
        if entry.id.is_empty()
            || entry.id.len() > 256
            || !ids.insert(&entry.id)
            || entry.reply_depth > MAX_REPLY_DEPTH
            || entry.recipients.is_empty()
            || entry.recipients.iter().collect::<BTreeSet<_>>().len() != entry.recipients.len()
            || entry
                .recipients
                .iter()
                .any(|id| !chat.member_bot_ids.contains(id))
        {
            return Err(invalid("invalid group delivery"));
        }
        validate_message(&entry.message)?;
    }
    Ok(())
}

fn pending_capacity_exceeded(pending: &[GroupMessage]) -> Result<bool> {
    Ok(pending.len() > MAX_PENDING || serde_json::to_vec(pending)?.len() > MAX_PENDING_BYTES)
}

fn validate_message(message: &MessageSubmission) -> Result<()> {
    message.validate(mobius::backend::session_files::session_file_limits())?;
    Ok(())
}

fn mentioned_handles(text: &str) -> BTreeSet<&str> {
    let is_handle = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_');
    text.match_indices('@')
        .filter_map(|(offset, _)| {
            if offset > 0 && is_handle(text.as_bytes()[offset - 1]) {
                return None;
            }
            let rest = &text[offset + 1..];
            let end = rest.bytes().take_while(|byte| is_handle(*byte)).count();
            (end > 0).then_some(&rest[..end])
        })
        .collect()
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Config(message.into())
}

impl mobius::middleware::bots::BotsBackend for GroupStore {
    fn chat_history<'a>(
        &'a self,
        bot_id: &'a str,
        session_id: &'a str,
        before_sequence: Option<u64>,
    ) -> mobius::BoxFuture<'a, mobius::Result<Option<mobius::backend::checkpoint::TranscriptPage>>>
    {
        Box::pin(async move {
            let Some(chat_id) = participant_chat_id(session_id) else {
                return Ok(None);
            };
            let chat = self
                .load(chat_id)
                .await
                .map_err(|error| mobius::Error::Tool(error.to_string()))?
                .filter(|chat| {
                    participant_session_id(chat_id, bot_id) == session_id
                        && chat.member_bot_ids.iter().any(|id| id == bot_id)
                })
                .ok_or_else(|| {
                    mobius::Error::Tool("shared history is unavailable to this Bot".into())
                })?;
            self.history_page(&chat.id, before_sequence)
                .await
                .map(Some)
                .map_err(|error| mobius::Error::Tool(error.to_string()))
        })
    }
    fn create_routine<'a>(
        &'a self,
        bot_id: &'a str,
        workspace: &'a Path,
        instructions: String,
        schedule: serde_json::Value,
        ends_at: Option<i64>,
    ) -> mobius::BoxFuture<'a, mobius::Result<String>> {
        Box::pin(async move {
            let result = (|| -> Result<String> {
                let routine = self.bots.create_routine(
                    bot_id,
                    workspace,
                    &instructions,
                    serde_json::from_value(schedule)?,
                    ends_at,
                )?;
                Ok(serde_json::to_string(&routine)?)
            })();
            result.map_err(|error| mobius::Error::Tool(error.to_string()))
        })
    }

    fn chat_context<'a>(
        &'a self,
        bot_id: &'a str,
        session_id: &'a str,
    ) -> mobius::BoxFuture<'a, mobius::Result<Option<String>>> {
        Box::pin(async move {
            self.chat_context(bot_id, session_id)
                .await
                .map_err(|error| mobius::Error::Tool(error.to_string()))
        })
    }
}

#[cfg(test)]
mod tests;
