use std::path::Path;
use std::sync::Arc;

use mobius::backend::checkpoint::{EventPage, EventPageRequest, JournalEvent};
use rusqlite::{Connection, OptionalExtension as _, params};

use super::{Chat, ChatMessage, ChatStore, invalid, validate_chat};
use crate::Result;

const SCHEMA: &str = "
BEGIN IMMEDIATE;
CREATE TABLE chats (
    id TEXT PRIMARY KEY,
    sequence INTEGER NOT NULL CHECK (sequence >= 0),
    updated_at INTEGER NOT NULL,
    deleted INTEGER NOT NULL CHECK (deleted IN (0, 1)),
    state_json TEXT NOT NULL
);
CREATE TABLE messages (
    chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence >= 0),
    message_id TEXT,
    event_json TEXT NOT NULL,
    message_json TEXT,
    PRIMARY KEY(chat_id, sequence),
    UNIQUE(chat_id, message_id)
);
CREATE INDEX chats_recent ON chats(deleted, updated_at DESC);
PRAGMA user_version = 2;
COMMIT;
";

pub(super) fn open(state_dir: &Path) -> Result<Connection> {
    let path = state_dir.join("chats.sqlite3");
    let connection = Connection::open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    let mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(invalid("could not enable chat journal WAL"));
    }
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    match version {
        0 => {
            let populated: bool =
                connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema)", [], |row| {
                    row.get(0)
                })?;
            if populated {
                return Err(invalid("chat storage is unversioned and not empty"));
            }
            connection.execute_batch(SCHEMA)?;
        }
        2 => {}
        _ => {
            return Err(invalid(format!(
                "unsupported chat storage schema {version}"
            )));
        }
    }
    Ok(connection)
}

impl ChatStore {
    pub(crate) async fn published_reply(
        &self,
        session_id: &str,
        submission_id: &str,
        text: &str,
    ) -> Result<Option<super::BotReply>> {
        let Ok(reply) = serde_json::from_str::<super::BotReply>(text) else {
            return Ok(None);
        };
        let session_id = session_id.to_owned();
        let submission_id = submission_id.to_owned();
        self.run(move |connection| {
            let published: bool = connection.query_row(
                "SELECT EXISTS (
                    SELECT 1 FROM chats
                    JOIN messages AS source ON source.chat_id = chats.id
                    JOIN messages AS published ON published.chat_id = chats.id
                    WHERE chats.deleted = 0 AND source.message_id = ?2
                      AND (EXISTS (SELECT 1 FROM json_each(chats.state_json, '$.participants')
                                   WHERE json_extract(value, '$.session_id') = ?1)
                           OR EXISTS (SELECT 1 FROM json_each(chats.state_json, '$.retired_participants')
                                      WHERE json_extract(value, '$.session_id') = ?1))
                      AND json_extract(published.event_json, '$.event.msg.type') = 'message'
                      AND json_extract(published.message_json, '$.message.author.type') = 'peer'
                      AND json_extract(published.message_json, '$.user_message_id') =
                          json_extract(source.message_json, '$.user_message_id')
                      AND json_extract(published.message_json, '$.message.text') = ?3
                      AND json(json_extract(published.message_json, '$.recipients')) = json(?4)
                )",
                params![session_id, submission_id, reply.text, serde_json::to_string(&reply.recipient_bot_ids)?],
                |row| row.get(0),
            )?;
            Ok(published.then_some(reply))
        }).await
    }

    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let connection = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| invalid("chat storage lock is poisoned"))?;
            operation(&mut connection)
        })
        .await
        .map_err(|error| invalid(format!("chat storage task failed: {error}")))?
    }

    pub(crate) async fn load(&self, id: &str) -> Result<Option<Chat>> {
        let id = id.to_owned();
        self.run(move |connection| {
            let state: Option<String> = connection
                .query_row(
                    "SELECT state_json FROM chats WHERE id = ?1 AND deleted = 0",
                    [&id],
                    |row| row.get(0),
                )
                .optional()?;
            state.map(|json| decode(&json)).transpose()
        })
        .await
    }

    pub(crate) async fn chats(&self, deleted: bool) -> Result<Vec<Chat>> {
        self.run(move |connection| {
            let mut statement = connection.prepare(
                "SELECT state_json FROM chats WHERE deleted = ?1 ORDER BY updated_at DESC, id",
            )?;
            statement
                .query_map([deleted], |row| row.get::<_, String>(0))?
                .map(|json| decode(&json?))
                .collect()
        })
        .await
    }

    pub(super) async fn catalog_page(
        &self,
        bot_id: &str,
        before: Option<(i64, u64, String)>,
        limit: usize,
    ) -> Result<Vec<Chat>> {
        let bot_id = bot_id.to_owned();
        self.run(move |connection| {
            let (updated_at, sequence, id) = match before {
                Some((updated_at, sequence, id)) => (Some(updated_at), Some(sql_sequence(sequence)?), Some(id)),
                None => (None, None, None),
            };
            let limit = i64::try_from(limit.clamp(1, 1_002)).map_err(|_| invalid("invalid catalog page size"))?;
            let mut statement = connection.prepare(
                "SELECT state_json FROM chats WHERE deleted = 0
                 AND EXISTS (SELECT 1 FROM json_each(state_json, '$.participants') WHERE json_extract(value, '$.bot_id') = ?1)
                 AND (?2 IS NULL OR updated_at < ?2 OR (updated_at = ?2 AND (sequence < ?3 OR (sequence = ?3 AND id > ?4))))
                 ORDER BY updated_at DESC, sequence DESC, id LIMIT ?5",
            )?;
            statement.query_map(params![bot_id, updated_at, sequence, id, limit], |row| row.get::<_, String>(0))?
                .map(|json| decode(&json?)).collect()
        }).await
    }

    pub(super) async fn contains_message(&self, id: &str, message_id: &str) -> Result<bool> {
        let id = id.to_owned();
        let message_id = message_id.to_owned();
        self.run(move |connection| {
            Ok(connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE chat_id = ?1 AND message_id = ?2)",
                params![id, message_id],
                |row| row.get(0),
            )?)
        })
        .await
    }

    pub(crate) async fn chat_for_session(&self, session_id: &str) -> Result<Option<Chat>> {
        let session_id = session_id.to_owned();
        self.run(move |connection| {
            let state: Option<String> = connection.query_row(
                "SELECT state_json FROM chats WHERE deleted = 0 AND EXISTS (SELECT 1 FROM json_each(state_json, '$.participants') WHERE json_extract(value, '$.session_id') = ?1)",
                [session_id], |row| row.get(0),
            ).optional()?;
            state.map(|json| decode(&json)).transpose()
        }).await
    }

    pub(super) async fn message_by_id(
        &self,
        chat_id: &str,
        message_id: &str,
    ) -> Result<Option<ChatMessage>> {
        let chat_id = chat_id.to_owned();
        let message_id = message_id.to_owned();
        self.run(move |connection| {
            let json: Option<String> = connection.query_row(
                "SELECT message_json FROM messages WHERE chat_id = ?1 AND message_id = ?2 AND message_json IS NOT NULL",
                params![chat_id, message_id], |row| row.get(0),
            ).optional()?;
            json.map(|json| serde_json::from_str(&json).map_err(Into::into)).transpose()
        }).await
    }

    pub(crate) async fn message_by_sequence(
        &self,
        chat_id: &str,
        sequence: u64,
    ) -> Result<Option<ChatMessage>> {
        let chat_id = chat_id.to_owned();
        self.run(move |connection| {
            let json: Option<String> = connection.query_row(
                "SELECT message_json FROM messages WHERE chat_id = ?1 AND sequence = ?2 AND message_json IS NOT NULL",
                params![chat_id, sql_sequence(sequence)?], |row| row.get(0),
            ).optional()?;
            json.map(|json| serde_json::from_str(&json).map_err(Into::into)).transpose()
        }).await
    }

    pub(super) async fn validate_target(
        &self,
        chat_id: &str,
        target: &mobius::protocol::MessageTarget,
    ) -> Result<()> {
        if target.batch_item_count != 1
            || self
                .message_by_sequence(chat_id, target.checkpoint_sequence)
                .await?
                .is_none()
        {
            return Err(invalid("reply target is not a published chat message"));
        }
        Ok(())
    }

    pub(super) async fn save(&self, chat: &Chat, event: Option<&JournalEvent>) -> Result<()> {
        let events = event
            .map(|event| (event.clone(), None))
            .into_iter()
            .collect::<Vec<_>>();
        self.persist(chat, &events).await
    }

    pub(super) async fn persist(
        &self,
        chat: &Chat,
        events: &[(JournalEvent, Option<ChatMessage>)],
    ) -> Result<()> {
        validate_chat(chat)?;
        let chat = chat.clone();
        let state = serde_json::to_string(&chat)?;
        let events = events
            .iter()
            .map(|(event, message)| {
                Ok::<_, crate::Error>((
                    event.sequence,
                    message.as_ref().map(|m| m.id.clone()),
                    serde_json::to_string(event)?,
                    message.as_ref().map(serde_json::to_string).transpose()?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            transaction.execute(
                "INSERT INTO chats(id, sequence, updated_at, deleted, state_json) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET sequence=excluded.sequence, updated_at=excluded.updated_at,
                 deleted=excluded.deleted, state_json=excluded.state_json",
                params![chat.id, sql_sequence(chat.sequence)?, chat.updated_at, chat.deleted, state],
            )?;
            for (sequence, message_id, event, message_json) in events {
                transaction.execute("INSERT INTO messages(chat_id, sequence, message_id, event_json, message_json) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![chat.id, sql_sequence(sequence)?, message_id, event, message_json])?;
            }
            transaction.commit()?;
            Ok(())
        }).await
    }

    pub(crate) async fn event_page(
        &self,
        chat_id: &str,
        request: EventPageRequest,
    ) -> Result<EventPage> {
        self.page(chat_id, request, false).await
    }

    pub(super) async fn message_page(
        &self,
        chat_id: &str,
        request: EventPageRequest,
    ) -> Result<EventPage> {
        self.page(chat_id, request, true).await
    }

    async fn page(
        &self,
        chat_id: &str,
        request: EventPageRequest,
        messages_only: bool,
    ) -> Result<EventPage> {
        let chat_id = chat_id.to_owned();
        self.run(move |connection| {
            let latest_sequence: i64 = connection.query_row(
                "SELECT sequence FROM chats WHERE id = ?1 AND deleted = 0", [&chat_id], |row| row.get(0),
            ).optional()?.ok_or_else(|| invalid("unknown chat"))?;
            let latest_sequence = u64::try_from(latest_sequence).map_err(|_| invalid("invalid chat sequence"))?;
            let limit = request.limit.clamp(1, 256);
            let mut statement = connection.prepare(
                "SELECT event_json FROM messages WHERE chat_id = ?1 AND (?2 IS NULL OR sequence < ?2)
                 AND (?4 = 0 OR message_id IS NOT NULL) ORDER BY sequence DESC LIMIT ?3"
            )?;
            let mut rows = statement.query(params![chat_id, request.before_sequence.map(sql_sequence).transpose()?, i64::try_from(limit + 1).map_err(|_| invalid("invalid page size"))?, messages_only])?;
            let mut events: Vec<JournalEvent> = Vec::new();
            let mut bytes = 0;
            let mut next_before_sequence = None;
            while let Some(row) = rows.next()? {
                let json: String = row.get(0)?;
                if events.len() == limit || (!events.is_empty() && bytes + json.len() > 8 * 1024 * 1024) {
                    next_before_sequence = events.last().map(|event| event.sequence);
                    break;
                }
                bytes += json.len();
                events.push(serde_json::from_str(&json)?);
            }
            Ok(EventPage { latest_sequence, events, next_before_sequence })
        }).await
    }

    pub(crate) async fn mark_deleted(&self, ids: &[String]) -> Result<()> {
        let _gate = self.gate.lock().await;
        for id in ids {
            if let Some(mut chat) = self.load(id).await? {
                chat.deleted = true;
                self.save(&chat, None).await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn finish_deletion(&self, ids: &[String]) -> Result<()> {
        let ids = ids.to_vec();
        self.run(move |connection| {
            let transaction = connection.transaction()?;
            for id in ids {
                transaction.execute("DELETE FROM chats WHERE id = ?1 AND deleted = 1", [&id])?;
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    }
}

fn decode(json: &str) -> Result<Chat> {
    let chat = serde_json::from_str(json)?;
    validate_chat(&chat)?;
    Ok(chat)
}

fn sql_sequence(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| invalid("chat sequence exceeds storage bounds"))
}
