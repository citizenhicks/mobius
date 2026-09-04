//! Durable SQLite checkpoint storage.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::Connection;
use rusqlite::OpenFlags;
use rusqlite::OptionalExtension;
use rusqlite::Transaction;
use rusqlite::TransactionBehavior;
use rusqlite::{params, params_from_iter};
use serde_json::Value;

use super::CHECKPOINT_VERSION;
use super::Checkpoint;
use super::CheckpointStore;
use super::EventPage;
use super::EventPageRequest;
use super::ExecutionPage;
use super::ExecutionPageRequest;
use super::ExecutionPhase;
use super::ExecutionRecord;
use super::ExecutionStats;
use super::JournalEvent;
use super::SessionCursor;
use super::SessionPage;
use super::SessionPageRequest;
use super::SessionSummary;
use super::TimestampedEvent;
use super::TranscriptBatch;
use super::TranscriptPage;
use super::TranscriptPageRequest;
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::protocol::Event;
use crate::protocol::SessionContext;

#[path = "sqlite_event.rs"]
mod event_journal;

#[cfg(test)]
use self::event_journal::StreamMetricAccumulator;
use self::event_journal::store_event;

const SCHEMA_VERSION: i64 = 9;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA: &str = "
BEGIN IMMEDIATE;
CREATE TABLE IF NOT EXISTS middleware_state (
    scope TEXT NOT NULL,
    key TEXT NOT NULL,
    value_json TEXT NOT NULL,
    PRIMARY KEY (scope, key)
);
CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY,
    parent_session_id TEXT REFERENCES sessions(session_id),
    parent_sequence INTEGER CHECK (parent_sequence IS NULL OR parent_sequence >= 0),
    latest_sequence INTEGER NOT NULL CHECK (latest_sequence >= 0),
    latest_event_sequence INTEGER NOT NULL DEFAULT 0 CHECK (latest_event_sequence >= 0),
    latest_checkpoint_json TEXT NOT NULL,
    session_context_json TEXT NOT NULL,
    execution_stats_json TEXT NOT NULL,
    catalog_visible INTEGER NOT NULL CHECK (catalog_visible IN (0, 1)),
    first_user_message TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK ((parent_session_id IS NULL) = (parent_sequence IS NULL))
);
CREATE TABLE IF NOT EXISTS transcript_delta (
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence >= 0),
    items_json TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (session_id, sequence)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS execution_journal (
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence >= 0),
    record_json TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL CHECK (started_at_ms >= 0),
    PRIMARY KEY (session_id, sequence)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS event_journal (
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    recorded_at_ms INTEGER NOT NULL CHECK (recorded_at_ms >= 0),
    event_kind TEXT NOT NULL,
    model_step_id TEXT,
    stream_phase TEXT,
    delta_bytes INTEGER CHECK (delta_bytes IS NULL OR delta_bytes >= 0),
    event_json TEXT NOT NULL,
    stream_metrics_json TEXT NOT NULL,
    PRIMARY KEY (session_id, sequence)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS sessions_recent_idx
    ON sessions(updated_at DESC, latest_sequence DESC, session_id DESC);
CREATE INDEX IF NOT EXISTS execution_journal_recent_idx
    ON execution_journal(started_at_ms DESC, session_id DESC, sequence DESC);
CREATE INDEX IF NOT EXISTS event_journal_step_idx
    ON event_journal(session_id, model_step_id, event_kind);
PRAGMA user_version = 9;
COMMIT;
";

/// Stores latest checkpoints, transcripts, and middleware state in SQLite.
pub struct SqliteCheckpoint {
    path: PathBuf,
    idle_connection: Arc<Mutex<Option<Connection>>>,
}

impl SqliteCheckpoint {
    /// Opens or creates a durable checkpoint database.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        prepare_path(&path)?;
        let connection = Connection::open(&path)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version == 0
            && connection.query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_schema LIMIT 1)",
                [],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Err(Error::Checkpoint(format!(
                "unversioned SQLite database is not empty; expected schema version \
                 {SCHEMA_VERSION} (start with a fresh database)"
            )));
        }
        if version != 0 && version != SCHEMA_VERSION {
            return Err(Error::Checkpoint(format!(
                "unsupported SQLite schema version {version}; expected {SCHEMA_VERSION} \
                 (start with a fresh database)"
            )));
        }
        let journal_mode: String =
            connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(Error::Checkpoint(format!(
                "SQLite could not enable WAL mode: {journal_mode}"
            )));
        }
        configure_connection(&connection)?;
        if version == 0 {
            connection.execute_batch(SCHEMA)?;
        }
        Ok(Self {
            path,
            idle_connection: Arc::new(Mutex::new(Some(connection))),
        })
    }

    async fn run<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T>
    where
        T: Send + 'static,
    {
        let path = self.path.clone();
        let idle_connection = Arc::clone(&self.idle_connection);
        tokio::task::spawn_blocking(move || {
            let cached = {
                let mut idle = idle_connection.lock().map_err(|_| {
                    Error::Checkpoint("SQLite connection cache lock poisoned".into())
                })?;
                idle.take()
            };
            let mut connection = cached.map_or_else(|| open_existing_connection(&path), Ok)?;
            let result = operation(&mut connection);
            let mut idle = idle_connection
                .lock()
                .map_err(|_| Error::Checkpoint("SQLite connection cache lock poisoned".into()))?;
            // One idle connection stays warm by design; a pool is only justified if
            // connection-open churn is ever measured.
            if idle.is_none() {
                *idle = Some(connection);
            }
            result
        })
        .await
        .map_err(|error| Error::Checkpoint(format!("SQLite worker failed: {error}")))?
    }
}

impl CheckpointStore for SqliteCheckpoint {
    fn load<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<Option<Checkpoint>>> {
        let session_id = session_id.to_string();
        Box::pin(self.run(move |connection| {
            let row = connection
                .query_row(
                    "SELECT latest_sequence, latest_checkpoint_json
                     FROM sessions WHERE session_id = ?1",
                    [&session_id],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            row.map(|(sequence, json)| decode_checkpoint(&session_id, sequence, &json))
                .transpose()
        }))
    }

    fn delete_sessions<'a>(&'a self, session_ids: &'a [String]) -> BoxFuture<'a, Result<bool>> {
        let roots = session_ids.to_vec();
        Box::pin(self.run(move |connection| {
            if roots.is_empty() {
                return Ok(true);
            }
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let placeholders = std::iter::repeat_n("?", roots.len())
                .collect::<Vec<_>>()
                .join(", ");
            let session_tree = format!(
                "WITH RECURSIVE session_tree(session_id) AS (
                     SELECT session_id FROM sessions WHERE session_id IN ({placeholders})
                     UNION
                     SELECT child.session_id
                     FROM sessions AS child
                     JOIN session_tree AS parent
                       ON child.parent_session_id = parent.session_id
                 )"
            );
            let session_ids = {
                let mut statement = transaction.prepare(&format!(
                    "{session_tree} SELECT session_id FROM session_tree"
                ))?;
                statement
                    .query_map(params_from_iter(&roots), |row| row.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            };
            let requested = roots.iter().map(String::as_str).collect::<BTreeSet<_>>();
            let found = session_ids
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            if !requested.is_subset(&found) {
                return Ok(false);
            }
            for id in &session_ids {
                transaction.execute("DELETE FROM middleware_state WHERE scope = ?1", [id])?;
            }
            let deleted = transaction.execute(
                &format!(
                    "{session_tree}
                     DELETE FROM sessions
                     WHERE session_id IN (SELECT session_id FROM session_tree)"
                ),
                params_from_iter(&roots),
            )?;
            if deleted != session_ids.len() {
                return Err(Error::Checkpoint(
                    "session tree changed during deletion".into(),
                ));
            }
            transaction.commit()?;
            Ok(true)
        }))
    }

    fn save<'a>(
        &'a self,
        checkpoint: &'a Checkpoint,
        transcript_delta: &'a [Value],
        execution: Option<&'a ExecutionRecord>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.save_with_events(checkpoint, transcript_delta, execution, &[])
                .await?;
            Ok(())
        })
    }

    fn save_with_events<'a>(
        &'a self,
        checkpoint: &'a Checkpoint,
        transcript_delta: &'a [Value],
        execution: Option<&'a ExecutionRecord>,
        events: &'a [TimestampedEvent],
    ) -> BoxFuture<'a, Result<Vec<JournalEvent>>> {
        let checkpoint = checkpoint.clone();
        let transcript_delta = transcript_delta.to_vec();
        let execution = execution.cloned();
        let events = events.to_vec();
        Box::pin(self.run(move |connection| {
            validate_checkpoint(&checkpoint)?;
            if let Some(execution) = &execution {
                validate_execution(&checkpoint, execution)?;
            }
            let sequence = i64::try_from(checkpoint.sequence).map_err(|_| {
                Error::Checkpoint("checkpoint sequence exceeds SQLite INTEGER".into())
            })?;
            let checkpoint_json = serde_json::to_string(&checkpoint)?;
            let session_context_json = serde_json::to_string(&checkpoint.session_context)?;
            let execution_stats_json = serde_json::to_string(&checkpoint.execution_stats)?;
            let transcript_json = (!transcript_delta.is_empty())
                .then(|| serde_json::to_string(&transcript_delta))
                .transpose()?;
            let execution_json = execution.as_ref().map(serde_json::to_string).transpose()?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            store_checkpoint(
                &transaction,
                &checkpoint,
                sequence,
                SerializedCheckpoint {
                    checkpoint: &checkpoint_json,
                    session_context: &session_context_json,
                    execution_stats: &execution_stats_json,
                    transcript: transcript_json.as_deref(),
                    execution: execution
                        .as_ref()
                        .zip(execution_json.as_deref())
                        .map(|(record, json)| (record.started_at_ms, json)),
                },
            )?;
            let records = events
                .into_iter()
                .map(|event| store_event(&transaction, &checkpoint.session_id, event))
                .collect::<Result<Vec<_>>>()?;
            transaction.commit()?;
            Ok(records)
        }))
    }

    fn append_event<'a>(
        &'a self,
        session_id: &'a str,
        recorded_at_ms: i64,
        event: &'a Event,
    ) -> BoxFuture<'a, Result<JournalEvent>> {
        let session_id = session_id.to_string();
        let event = TimestampedEvent {
            recorded_at_ms,
            event: event.clone(),
        };
        Box::pin(self.run(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let record = store_event(&transaction, &session_id, event)?;
            transaction.commit()?;
            Ok(record)
        }))
    }

    fn event_page<'a>(
        &'a self,
        session_id: &'a str,
        request: EventPageRequest,
    ) -> BoxFuture<'a, Result<EventPage>> {
        let session_id = session_id.to_string();
        Box::pin(async move {
            if request.limit == 0 {
                return Err(Error::Checkpoint(
                    "event journal page limit must be positive".into(),
                ));
            }
            let query_limit = request
                .limit
                .checked_add(1)
                .and_then(|limit| i64::try_from(limit).ok())
                .ok_or_else(|| Error::Checkpoint("event journal page limit is too large".into()))?;
            let before_sequence = request
                .before_sequence
                .map(i64::try_from)
                .transpose()
                .map_err(|_| {
                    Error::Checkpoint("event journal cursor exceeds SQLite INTEGER".into())
                })?;
            self.run(move |connection| {
                let latest_sequence = connection
                    .query_row(
                        "SELECT latest_event_sequence FROM sessions WHERE session_id = ?1",
                        [&session_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?
                    .ok_or_else(|| {
                        Error::Checkpoint("event journal session does not exist".into())
                    })?;
                let latest_sequence = u64::try_from(latest_sequence).map_err(|_| {
                    Error::Checkpoint("event journal sequence became negative".into())
                })?;
                let mut statement = connection.prepare(
                    "SELECT sequence, recorded_at_ms, event_json, stream_metrics_json
                     FROM event_journal
                     WHERE session_id = ?1
                       AND (?2 IS NULL OR sequence < ?2)
                     ORDER BY sequence DESC
                     LIMIT ?3",
                )?;
                let mut rows = statement
                    .query_map(params![session_id, before_sequence, query_limit], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                let has_more = rows.len() > request.limit;
                rows.truncate(request.limit);
                let events = rows
                    .into_iter()
                    .map(|(sequence, recorded_at_ms, json, metrics_json)| {
                        Ok(JournalEvent {
                            sequence: u64::try_from(sequence).map_err(|_| {
                                Error::Checkpoint(
                                    "event journal row has a negative sequence".into(),
                                )
                            })?,
                            recorded_at_ms,
                            event: serde_json::from_str(&json)?,
                            stream_metrics: serde_json::from_str(&metrics_json)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let next_before_sequence = has_more
                    .then(|| events.last().map(|event| event.sequence))
                    .flatten();
                Ok(EventPage {
                    latest_sequence,
                    events,
                    next_before_sequence,
                })
            })
            .await
        })
    }

    fn list_sessions_page(
        &self,
        request: SessionPageRequest,
    ) -> BoxFuture<'_, Result<SessionPage>> {
        Box::pin(async move {
            if request.limit == 0 {
                return Err(Error::Checkpoint(
                    "session page limit must be positive".into(),
                ));
            }
            let query_limit = request
                .limit
                .checked_add(1)
                .and_then(|limit| i64::try_from(limit).ok())
                .ok_or_else(|| Error::Checkpoint("session page limit is too large".into()))?;
            let (cursor_updated_at, cursor_sequence, cursor_session_id) = match request.cursor {
                Some(cursor) => (
                    Some(cursor.updated_at),
                    Some(i64::try_from(cursor.sequence).map_err(|_| {
                        Error::Checkpoint("session cursor sequence exceeds SQLite INTEGER".into())
                    })?),
                    Some(cursor.session_id),
                ),
                None => (None, None, None),
            };
            self.run(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT sessions.session_id, sessions.parent_session_id,
                            sessions.parent_sequence, sessions.latest_sequence,
                            sessions.catalog_visible, sessions.first_user_message,
                            sessions.session_context_json, sessions.execution_stats_json,
                            sessions.created_at, sessions.updated_at
                     FROM sessions
                     WHERE ?1 IS NULL
                        OR (
                            sessions.updated_at,
                            sessions.latest_sequence,
                            sessions.session_id
                        ) < (?1, ?2, ?3)
                     ORDER BY sessions.updated_at DESC, sessions.latest_sequence DESC,
                              sessions.session_id DESC
                     LIMIT ?4",
                )?;
                let mut sessions = statement
                    .query_map(
                        params![
                            cursor_updated_at,
                            cursor_sequence,
                            cursor_session_id,
                            query_limit
                        ],
                        session_row,
                    )?
                    .collect::<std::result::Result<Vec<_>, _>>()?
                    .into_iter()
                    .map(summary_from_row)
                    .collect::<Result<Vec<_>>>()?;
                let has_more = sessions.len() > request.limit;
                sessions.truncate(request.limit);
                let next_cursor = has_more
                    .then(|| sessions.last().map(session_cursor))
                    .flatten();
                Ok(SessionPage {
                    sessions,
                    next_cursor,
                })
            })
            .await
        })
    }

    fn transcript_page<'a>(
        &'a self,
        session_id: &'a str,
        request: TranscriptPageRequest,
    ) -> BoxFuture<'a, Result<TranscriptPage>> {
        let session_id = session_id.to_string();
        Box::pin(async move {
            if request.max_batches == 0 {
                return Err(Error::Checkpoint(
                    "transcript page limit must be positive".into(),
                ));
            }
            let query_limit = request
                .max_batches
                .checked_add(1)
                .and_then(|limit| i64::try_from(limit).ok())
                .ok_or_else(|| Error::Checkpoint("transcript page limit is too large".into()))?;
            let before_sequence = request
                .before_sequence
                .map(i64::try_from)
                .transpose()
                .map_err(|_| {
                    Error::Checkpoint("transcript cursor exceeds SQLite INTEGER".into())
                })?;
            self.run(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT sequence, created_at, items_json
                     FROM transcript_delta
                     WHERE session_id = ?1
                       AND (?2 IS NULL OR sequence < ?2)
                     ORDER BY sequence DESC
                     LIMIT ?3",
                )?;
                let mut batches = statement
                    .query_map(params![session_id, before_sequence, query_limit], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                let has_more = batches.len() > request.max_batches;
                batches.truncate(request.max_batches);
                let batches = batches
                    .into_iter()
                    .map(|(sequence, created_at, json)| {
                        Ok(TranscriptBatch {
                            sequence: u64::try_from(sequence).map_err(|_| {
                                Error::Checkpoint("transcript row has a negative sequence".into())
                            })?,
                            created_at,
                            items: serde_json::from_str(&json)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let next_before_sequence = has_more
                    .then(|| batches.last().map(|batch| batch.sequence))
                    .flatten();
                Ok(TranscriptPage {
                    batches,
                    next_before_sequence,
                })
            })
            .await
        })
    }

    fn execution_page<'a>(
        &'a self,
        session_id: &'a str,
        request: ExecutionPageRequest,
    ) -> BoxFuture<'a, Result<ExecutionPage>> {
        let session_id = session_id.to_string();
        Box::pin(async move {
            if request.limit == 0 {
                return Err(Error::Checkpoint(
                    "execution page limit must be positive".into(),
                ));
            }
            let query_limit = request
                .limit
                .checked_add(1)
                .and_then(|limit| i64::try_from(limit).ok())
                .ok_or_else(|| Error::Checkpoint("execution page limit is too large".into()))?;
            let before_sequence = request
                .before_sequence
                .map(i64::try_from)
                .transpose()
                .map_err(|_| Error::Checkpoint("execution cursor exceeds SQLite INTEGER".into()))?;
            self.run(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT sequence, record_json
                     FROM execution_journal
                     WHERE session_id = ?1
                       AND (?2 IS NULL OR sequence < ?2)
                     ORDER BY sequence DESC
                     LIMIT ?3",
                )?;
                let mut records = statement
                    .query_map(params![session_id, before_sequence, query_limit], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                let has_more = records.len() > request.limit;
                records.truncate(request.limit);
                let next_before_sequence = has_more
                    .then(|| records.last().map(|(sequence, _)| *sequence))
                    .flatten()
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| {
                        Error::Checkpoint("execution row has a negative sequence".into())
                    })?;
                let executions = records
                    .into_iter()
                    .map(|(_, json)| decode_execution(&json))
                    .collect::<Result<Vec<_>>>()?;
                Ok(ExecutionPage {
                    executions,
                    next_before_sequence,
                })
            })
            .await
        })
    }

    fn recent_executions(&self, limit: usize) -> BoxFuture<'_, Result<Vec<ExecutionRecord>>> {
        Box::pin(async move {
            if limit == 0 {
                return Err(Error::Checkpoint(
                    "recent execution limit must be positive".into(),
                ));
            }
            let query_limit = i64::try_from(limit)
                .map_err(|_| Error::Checkpoint("recent execution limit is too large".into()))?;
            self.run(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT record_json
                     FROM execution_journal
                     ORDER BY started_at_ms DESC, session_id DESC, sequence DESC
                     LIMIT ?1",
                )?;
                statement
                    .query_map([query_limit], |row| row.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?
                    .into_iter()
                    .map(|json| decode_execution(&json))
                    .collect()
            })
            .await
        })
    }

    fn fork<'a>(
        &'a self,
        parent_session_id: &'a str,
        parent_sequence: u64,
        checkpoint: &'a Checkpoint,
    ) -> BoxFuture<'a, Result<SessionSummary>> {
        let parent_session_id = parent_session_id.to_string();
        let parent_sequence = i64::try_from(parent_sequence);
        let session_id = checkpoint.session_id.clone();
        let sequence = i64::try_from(checkpoint.sequence);
        let clean = checkpoint.sequence == 0
            && checkpoint.active_execution.is_none()
            && checkpoint.pending_approval.is_none()
            && checkpoint.pending_messages.is_empty();
        let validation = validate_checkpoint(checkpoint);
        let catalog_visible = checkpoint.catalog_visible;
        let checkpoint = checkpoint.clone();
        Box::pin(async move {
            if parent_session_id == session_id {
                return Err(Error::Checkpoint("a session cannot fork itself".into()));
            }
            if !clean {
                return Err(Error::Checkpoint(
                    "a fork must begin at a clean sequence-zero checkpoint".into(),
                ));
            }
            let parent_sequence = parent_sequence
                .map_err(|_| Error::Checkpoint("parent sequence exceeds SQLite INTEGER".into()))?;
            let sequence = sequence.map_err(|_| {
                Error::Checkpoint("checkpoint sequence exceeds SQLite INTEGER".into())
            })?;
            validation?;
            self.run(move |connection| {
                let checkpoint_json = serde_json::to_string(&checkpoint)?;
                let session_context_json = serde_json::to_string(&checkpoint.session_context)?;
                let execution_stats_json = serde_json::to_string(&checkpoint.execution_stats)?;
                let context_json = (!checkpoint.context.is_empty())
                    .then(|| serde_json::to_string(&checkpoint.context))
                    .transpose()?;
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let durable_parent = transaction
                    .query_row(
                        "SELECT latest_sequence FROM sessions WHERE session_id = ?1",
                        [&parent_session_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?
                    .ok_or_else(|| Error::Checkpoint("fork parent does not exist".into()))?;
                if parent_sequence > durable_parent {
                    return Err(Error::Checkpoint(
                        "fork point is newer than the parent checkpoint".into(),
                    ));
                }
                transaction.execute(
                    "INSERT INTO sessions (
                         session_id, parent_session_id, parent_sequence, latest_sequence,
                         latest_checkpoint_json, session_context_json, catalog_visible,
                         first_user_message, execution_stats_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        session_id,
                        parent_session_id,
                        parent_sequence,
                        sequence,
                        checkpoint_json,
                        session_context_json,
                        catalog_visible,
                        checkpoint.first_user_message,
                        execution_stats_json,
                    ],
                )?;
                if let Some(context_json) = context_json {
                    transaction.execute(
                        "INSERT INTO transcript_delta (session_id, sequence, items_json)
                         VALUES (?1, ?2, ?3)",
                        params![session_id, sequence, context_json,],
                    )?;
                }
                let row = transaction.query_row(
                    "SELECT sessions.session_id, sessions.parent_session_id,
                            sessions.parent_sequence, sessions.latest_sequence,
                            sessions.catalog_visible, sessions.first_user_message,
                            sessions.session_context_json, sessions.execution_stats_json,
                            sessions.created_at, sessions.updated_at
                     FROM sessions
                     WHERE sessions.session_id = ?1",
                    [&session_id],
                    session_row,
                )?;
                let summary = summary_from_row(row)?;
                transaction.commit()?;
                Ok(summary)
            })
            .await
        })
    }

    fn load_state<'a>(
        &'a self,
        scope: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        let scope = scope.to_string();
        let key = key.to_string();
        Box::pin(self.run(move |connection| {
            let json = connection
                .query_row(
                    "SELECT value_json FROM middleware_state WHERE scope = ?1 AND key = ?2",
                    params![scope, key],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            Ok(json.as_deref().map(serde_json::from_str).transpose()?)
        }))
    }

    fn save_state<'a>(
        &'a self,
        scope: &'a str,
        key: &'a str,
        value: &'a Value,
    ) -> BoxFuture<'a, Result<()>> {
        let scope = scope.to_string();
        let key = key.to_string();
        let value = value.clone();
        Box::pin(self.run(move |connection| {
            let json = serde_json::to_string(&value)?;
            connection.execute(
                "INSERT INTO middleware_state (scope, key, value_json)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(scope, key) DO UPDATE SET value_json = excluded.value_json",
                params![scope, key, json],
            )?;
            Ok(())
        }))
    }
}

fn open_existing_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    configure_connection(&connection)?;
    Ok(connection)
}

fn configure_connection(connection: &Connection) -> Result<()> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

struct SerializedCheckpoint<'a> {
    checkpoint: &'a str,
    session_context: &'a str,
    execution_stats: &'a str,
    transcript: Option<&'a str>,
    execution: Option<(i64, &'a str)>,
}

fn store_checkpoint(
    transaction: &Transaction<'_>,
    checkpoint: &Checkpoint,
    sequence: i64,
    serialized: SerializedCheckpoint<'_>,
) -> Result<()> {
    let changed = transaction.execute(
        "INSERT INTO sessions (
             session_id, latest_sequence, latest_checkpoint_json, session_context_json,
             execution_stats_json, catalog_visible, first_user_message
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(session_id) DO UPDATE SET
             latest_sequence = excluded.latest_sequence,
             latest_checkpoint_json = excluded.latest_checkpoint_json,
             session_context_json = excluded.session_context_json,
             execution_stats_json = excluded.execution_stats_json,
             catalog_visible = excluded.catalog_visible,
             first_user_message = COALESCE(sessions.first_user_message, excluded.first_user_message),
             updated_at = unixepoch()
         WHERE excluded.latest_sequence > sessions.latest_sequence",
        params![
            checkpoint.session_id,
            sequence,
            serialized.checkpoint,
            serialized.session_context,
            serialized.execution_stats,
            checkpoint.catalog_visible,
            checkpoint.first_user_message,
        ],
    )?;
    if changed == 0 {
        return Err(Error::Checkpoint(
            "checkpoint sequence did not advance".into(),
        ));
    }
    if let Some(transcript_json) = serialized.transcript {
        transaction.execute(
            "INSERT INTO transcript_delta (session_id, sequence, items_json)
             VALUES (?1, ?2, ?3)",
            params![checkpoint.session_id, sequence, transcript_json],
        )?;
    }
    if let Some((started_at_ms, record_json)) = serialized.execution {
        transaction.execute(
            "INSERT INTO execution_journal (
                 session_id, sequence, record_json, started_at_ms
             ) VALUES (?1, ?2, ?3, ?4)",
            params![checkpoint.session_id, sequence, record_json, started_at_ms,],
        )?;
    }
    Ok(())
}

type SessionRow = (
    String,
    Option<String>,
    Option<i64>,
    i64,
    bool,
    Option<String>,
    String,
    String,
    i64,
    i64,
);

fn session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

fn summary_from_row(row: SessionRow) -> Result<SessionSummary> {
    let session_context: SessionContext = serde_json::from_str(&row.6)?;
    validate_session_context(&session_context)?;
    let execution_stats: ExecutionStats = serde_json::from_str(&row.7)?;
    Ok(SessionSummary {
        session_id: row.0,
        session_context,
        parent_session_id: row.1,
        parent_sequence: row
            .2
            .map(u64::try_from)
            .transpose()
            .map_err(|_| Error::Checkpoint("session has a negative parent sequence".into()))?,
        sequence: u64::try_from(row.3)
            .map_err(|_| Error::Checkpoint("session has a negative sequence".into()))?,
        catalog_visible: row.4,
        first_user_message: row.5,
        execution_stats,
        created_at: row.8,
        updated_at: row.9,
    })
}

fn session_cursor(session: &SessionSummary) -> SessionCursor {
    SessionCursor {
        updated_at: session.updated_at,
        sequence: session.sequence,
        session_id: session.session_id.clone(),
    }
}

fn decode_checkpoint(session_id: &str, sequence: i64, json: &str) -> Result<Checkpoint> {
    let checkpoint: Checkpoint = serde_json::from_str(json)?;
    let sequence = u64::try_from(sequence)
        .map_err(|_| Error::Checkpoint("checkpoint row has a negative sequence".into()))?;
    validate_checkpoint(&checkpoint)?;
    if checkpoint.session_id != session_id || checkpoint.sequence != sequence {
        return Err(Error::Checkpoint(
            "checkpoint row does not match its index".into(),
        ));
    }
    Ok(checkpoint)
}

fn decode_execution(json: &str) -> Result<ExecutionRecord> {
    let execution: ExecutionRecord = serde_json::from_str(json)?;
    validate_execution_record(&execution)?;
    Ok(execution)
}

fn validate_checkpoint(checkpoint: &Checkpoint) -> Result<()> {
    if checkpoint.version != CHECKPOINT_VERSION {
        return Err(Error::Checkpoint(format!(
            "unsupported checkpoint version {}",
            checkpoint.version
        )));
    }
    if let Some(active) = &checkpoint.active_execution {
        if active.submission_id.trim().is_empty() || active.turn_id.trim().is_empty() {
            return Err(Error::Checkpoint(
                "active execution identifiers cannot be empty".into(),
            ));
        }
        if active.started_at_ms < 0 {
            return Err(Error::Checkpoint(
                "active execution start time cannot be negative".into(),
            ));
        }
        if active.failed_tool_calls > active.tool_calls {
            return Err(Error::Checkpoint(
                "active execution failed-tool count exceeds tool count".into(),
            ));
        }
        if matches!(&active.phase, ExecutionPhase::Completion { .. })
            && (checkpoint.active_model_step.is_some()
                || !checkpoint.pending_tools.is_empty()
                || checkpoint.pending_approval.is_some())
        {
            return Err(Error::Checkpoint(
                "turn completion conflicts with pending model or tool work".into(),
            ));
        }
    } else if checkpoint.active_model_step.is_some()
        || !checkpoint.pending_tools.is_empty()
        || checkpoint.pending_approval.is_some()
    {
        return Err(Error::Checkpoint(
            "pending model or tool work has no active execution".into(),
        ));
    }
    validate_session_context(&checkpoint.session_context)?;
    if let Some(step) = &checkpoint.active_model_step {
        let execution = checkpoint
            .active_execution
            .as_ref()
            .ok_or_else(|| Error::Checkpoint("active model step has no active execution".into()))?;
        if step.model_step_id.trim().is_empty() {
            return Err(Error::Checkpoint(
                "active model-step identifier cannot be empty".into(),
            ));
        }
        if step.started_at_ms < execution.started_at_ms {
            return Err(Error::Checkpoint(
                "active model step predates its execution".into(),
            ));
        }
    }
    if checkpoint.pending_messages.len() > super::MAX_QUEUED_MESSAGES {
        return Err(Error::Checkpoint(
            "queued messages exceed the durable item limit".into(),
        ));
    }
    let mut pending_ids = BTreeSet::new();
    for message in &checkpoint.pending_messages {
        message
            .validate()
            .map_err(|error| Error::Checkpoint(error.to_string()))?;
        if !pending_ids.insert((message.owner(), message.id())) {
            return Err(Error::Checkpoint(format!(
                "duplicate queued message `{}/{}`",
                message.owner(),
                message.id(),
            )));
        }
    }
    Ok(())
}

fn validate_session_context(context: &SessionContext) -> Result<()> {
    if context.bot_id.trim().is_empty() {
        return Err(Error::Checkpoint("session Bot ID cannot be blank".into()));
    }
    Ok(())
}

fn validate_execution(checkpoint: &Checkpoint, execution: &ExecutionRecord) -> Result<()> {
    if checkpoint.active_execution.is_some() {
        return Err(Error::Checkpoint(
            "a terminal execution requires an idle checkpoint".into(),
        ));
    }
    if execution.session_id != checkpoint.session_id {
        return Err(Error::Checkpoint(
            "execution record does not match its checkpoint".into(),
        ));
    }
    validate_execution_record(execution)
}

fn validate_execution_record(execution: &ExecutionRecord) -> Result<()> {
    if execution.session_id.trim().is_empty()
        || execution.submission_id.trim().is_empty()
        || execution.turn_id.trim().is_empty()
    {
        return Err(Error::Checkpoint(
            "execution record identifiers cannot be empty".into(),
        ));
    }
    if execution.started_at_ms < 0 || execution.finished_at_ms < execution.started_at_ms {
        return Err(Error::Checkpoint(
            "execution record has invalid timestamps".into(),
        ));
    }
    let elapsed_ms = u64::try_from(execution.finished_at_ms - execution.started_at_ms)
        .map_err(|_| Error::Checkpoint("execution elapsed time is unsupported".into()))?;
    if execution.elapsed_ms != elapsed_ms {
        return Err(Error::Checkpoint(
            "execution elapsed time does not match its timestamps".into(),
        ));
    }
    if execution.failed_tool_calls > execution.tool_calls {
        return Err(Error::Checkpoint(
            "execution failed-tool count exceeds tool count".into(),
        ));
    }
    Ok(())
}

fn prepare_path(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;

        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "sqlite_tests.rs"]
mod tests;
