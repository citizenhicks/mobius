use std::path::Path;
use std::sync::Arc;

use mobius::backend::checkpoint::{EventPage, EventPageRequest, JournalEvent};
use rusqlite::{Connection, OptionalExtension as _, params};

use super::{GroupChat, GroupStore, invalid, validate_chat};
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
    PRIMARY KEY(chat_id, sequence),
    UNIQUE(chat_id, message_id)
);
CREATE INDEX chats_recent ON chats(deleted, updated_at DESC);
PRAGMA user_version = 1;
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
        1 => {}
        _ => {
            return Err(invalid(format!(
                "unsupported chat storage schema {version}"
            )));
        }
    }
    Ok(connection)
}

impl GroupStore {
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

    pub(crate) async fn load(&self, id: &str) -> Result<Option<GroupChat>> {
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

    pub(crate) async fn chats(&self, deleted: bool) -> Result<Vec<GroupChat>> {
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

    pub(super) async fn save(&self, chat: &GroupChat, event: Option<&JournalEvent>) -> Result<()> {
        validate_chat(chat)?;
        let chat = chat.clone();
        let state = serde_json::to_string(&chat)?;
        let event = event
            .map(|event| {
                let message_id = matches!(event.event.msg, mobius::protocol::EventMsg::Message(_))
                    .then(|| event.event.submission_id.clone())
                    .flatten();
                Ok::<_, crate::Error>((event.sequence, message_id, serde_json::to_string(event)?))
            })
            .transpose()?;
        self.run(move |connection| {
            let transaction = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            transaction.execute(
                "INSERT INTO chats(id, sequence, updated_at, deleted, state_json) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET sequence=excluded.sequence, updated_at=excluded.updated_at,
                 deleted=excluded.deleted, state_json=excluded.state_json",
                params![chat.id, sql_sequence(chat.sequence)?, chat.updated_at, chat.deleted, state],
            )?;
            if let Some((sequence, message_id, event)) = event {
                transaction.execute("INSERT INTO messages(chat_id, sequence, message_id, event_json) VALUES (?1, ?2, ?3, ?4)",
                    params![chat.id, sql_sequence(sequence)?, message_id, event])?;
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
            ).optional()?.ok_or_else(|| invalid("unknown group chat"))?;
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

    pub(super) async fn history_page(
        &self,
        chat_id: &str,
        before_sequence: Option<u64>,
    ) -> Result<mobius::backend::checkpoint::TranscriptPage> {
        use mobius::backend::checkpoint::{TranscriptBatch, TranscriptPage};
        let chat_id = chat_id.to_owned();
        self.run(move |connection| {
            let mut statement = connection.prepare(
                "SELECT event_json FROM messages WHERE chat_id = ?1 AND message_id IS NOT NULL
                 AND (?2 IS NULL OR sequence < ?2) ORDER BY sequence DESC LIMIT 2",
            )?;
            let mut rows = statement.query(params![
                chat_id,
                before_sequence.map(sql_sequence).transpose()?
            ])?;
            let Some(row) = rows.next()? else {
                return Ok(TranscriptPage::default());
            };
            let journal: JournalEvent = serde_json::from_str(&row.get::<_, String>(0)?)?;
            let mobius::protocol::EventMsg::Message(message) = journal.event.msg else {
                return Err(invalid("invalid shared history message"));
            };
            let (role, text) = match message.author {
                mobius::protocol::MessageAuthor::User => ("user", message.text),
                mobius::protocol::MessageAuthor::Peer { handle, .. } => {
                    ("assistant", format!("@{handle}: {}", message.text))
                }
            };
            let content = serde_json::to_string(
                &serde_json::json!({"text": text, "attachments": message.attachments}),
            )?;
            let next_before_sequence = rows.next()?.map(|_| journal.sequence);
            Ok(TranscriptPage {
                batches: vec![TranscriptBatch {
                    sequence: journal.sequence,
                    created_at: journal.recorded_at_ms / 1000,
                    items: vec![serde_json::json!({"role": role, "content": content})],
                }],
                next_before_sequence,
            })
        })
        .await
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

fn decode(json: &str) -> Result<GroupChat> {
    let chat = serde_json::from_str(json)?;
    validate_chat(&chat)?;
    Ok(chat)
}

fn sql_sequence(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| invalid("chat sequence exceeds storage bounds"))
}
