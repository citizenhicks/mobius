use mobius::SessionId;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::wire::{RoutineRun, RoutineRunStatus};
use crate::{Error, Result};

pub(super) const STATE_FILE: &str = "bots.sqlite3";

const SCHEMA_VERSION: i64 = 6;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const SCHEMA: &str = "
BEGIN IMMEDIATE;
CREATE TABLE IF NOT EXISTS catalog (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    state_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS routine_runs (
    ordinal INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    routine_id TEXT NOT NULL,
    bot_id TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    status TEXT NOT NULL CHECK (
        status IN ('running', 'succeeded', 'failed', 'skipped', 'cancelled')
    ),
    session_id TEXT,
    message TEXT,
    CHECK ((status = 'running') = (finished_at IS NULL)),
    CHECK (status != 'running' OR session_id IS NOT NULL)
);
CREATE INDEX IF NOT EXISTS routine_runs_routine_recent
    ON routine_runs(routine_id, ordinal DESC);
CREATE INDEX IF NOT EXISTS routine_runs_bot_recent
    ON routine_runs(bot_id, ordinal DESC);
CREATE INDEX IF NOT EXISTS routine_runs_active
    ON routine_runs(routine_id) WHERE status = 'running';
CREATE TABLE IF NOT EXISTS hook_bindings (
    bot_id TEXT NOT NULL,
    id TEXT NOT NULL,
    routine_id TEXT,
    selector_json TEXT NOT NULL,
    action_json TEXT NOT NULL,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    after_sequence INTEGER NOT NULL CHECK (after_sequence >= 0),
    starts_at INTEGER NOT NULL,
    PRIMARY KEY (bot_id, id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS hook_events (
    id TEXT PRIMARY KEY,
    bot_id TEXT NOT NULL,
    source_json TEXT NOT NULL,
    occurred_at INTEGER NOT NULL,
    event_json TEXT NOT NULL
    , published INTEGER NOT NULL DEFAULT 0 CHECK (published IN (0, 1))
);
CREATE TABLE IF NOT EXISTS hook_outbox (
    id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL REFERENCES hook_events(id) ON DELETE CASCADE,
    binding_id TEXT NOT NULL,
    bot_id TEXT NOT NULL,
    action_json TEXT NOT NULL,
    retry_at INTEGER,
    accepted INTEGER NOT NULL DEFAULT 0 CHECK (accepted IN (0, 1)),
    last_error TEXT
);
CREATE INDEX IF NOT EXISTS hook_outbox_due ON hook_outbox(retry_at) WHERE accepted = 0;
CREATE TABLE IF NOT EXISTS routine_commands (
    command_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES routine_runs(id) ON DELETE CASCADE,
    cause_json TEXT
);
CREATE TABLE IF NOT EXISTS routine_stop_requests (
    run_id TEXT PRIMARY KEY REFERENCES routine_runs(id) ON DELETE CASCADE,
    command_id TEXT NOT NULL UNIQUE,
    cause_json TEXT
);
CREATE TABLE IF NOT EXISTS bot_session_cursors (
    session_id TEXT PRIMARY KEY,
    bot_id TEXT NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence >= 0)
);
PRAGMA user_version = 6;
COMMIT;
";

// Optional collector metadata remains available for every schema-six database.
const OPTIONAL_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS telemetry_cursors (
    sink_id TEXT PRIMARY KEY,
    after_rowid INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS hook_journal_sequences (
    event_id TEXT PRIMARY KEY REFERENCES hook_events(id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence > 0)
);
";

pub(crate) struct BotStorage {
    pub(super) connection: Mutex<Connection>,
    telemetry_notify: OnceLock<(Weak<tokio::sync::Notify>, Weak<crate::host::WorkActivity>)>,
}

impl BotStorage {
    pub(super) fn open(path: &Path) -> Result<(Self, bool)> {
        let connection = Connection::open(path)?;
        protect_database_files(path)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version == 0
            && connection.query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_schema LIMIT 1)",
                [],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Err(Error::Config(
                "Bot storage is unversioned and not empty; remove that database".into(),
            ));
        }
        if version != 0 && version != SCHEMA_VERSION {
            return Err(Error::Config(format!(
                "unsupported Bot storage schema version {version}; expected {SCHEMA_VERSION}"
            )));
        }
        let journal_mode: String =
            connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(Error::Config(format!(
                "SQLite could not enable Bot storage WAL mode: {journal_mode}"
            )));
        }
        configure_connection(&connection)?;
        if version == 0 {
            connection.execute_batch(SCHEMA)?;
        }
        connection.execute_batch(OPTIONAL_SCHEMA)?;
        // SQLite creates the WAL and shared-memory files with the database file's mode.
        protect_database_files(path)?;
        let persisted = connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM catalog WHERE id = 1)",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        Ok((
            Self {
                connection: Mutex::new(connection),
                telemetry_notify: OnceLock::new(),
            },
            persisted,
        ))
    }

    pub(super) fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(crate::poisoned("Bot storage"))
    }

    pub(super) fn load_catalog(&self) -> Result<Option<String>> {
        read_catalog(&*self.lock()?)
    }

    pub(crate) fn attach_telemetry_notify(
        &self,
        notify: &Arc<tokio::sync::Notify>,
        activity: &Arc<crate::host::WorkActivity>,
    ) {
        if self
            .telemetry_notify
            .set((Arc::downgrade(notify), Arc::downgrade(activity)))
            .is_err()
        {
            tracing::debug!("Bot telemetry notification is already attached");
        }
    }

    /// The catalog with the stamp it was read at, unless nothing committed since `stamp`.
    pub(super) fn catalog_since(
        &self,
        stamp: Option<CatalogStamp>,
    ) -> Result<Option<(CatalogStamp, Option<String>)>> {
        let connection = self.lock()?;
        // data_version moves on commits by other connections, total_changes on our own.
        let current = CatalogStamp {
            data_version: connection.query_row("PRAGMA data_version", [], |row| row.get(0))?,
            own_changes: connection.total_changes(),
        };
        if stamp == Some(current) {
            return Ok(None);
        }
        let catalog = read_catalog(&connection)?;
        drop(connection);
        Ok(Some((current, catalog)))
    }

    pub(super) fn save_catalog(&self, state_json: &str) -> Result<()> {
        self.transaction(|transaction| save_catalog_row(transaction, state_json))
    }

    pub(super) fn delete_runs_and_save_catalog(
        &self,
        state_json: &str,
        routine_id: Option<&str>,
        bot_id: Option<&str>,
        event: Option<&crate::wire::HookEvent>,
        accepted_action_id: Option<&str>,
    ) -> Result<()> {
        debug_assert!(routine_id.is_some() ^ bot_id.is_some());
        self.transaction(|transaction| {
            super::events::accept_action(transaction, accepted_action_id)?;
            if let Some(routine_id) = routine_id {
                transaction
                    .execute(
                        "DELETE FROM routine_runs WHERE routine_id = ?1",
                        [routine_id],
                    )?;
            } else if let Some(bot_id) = bot_id {
                transaction
                    .execute("DELETE FROM routine_runs WHERE bot_id = ?1", [bot_id])?;
            }
            if let Some(routine_id) = routine_id {
                let source = serde_json::to_string(&crate::wire::HookSource::Routine {
                    routine_id: routine_id.into(),
                })?;
                transaction.execute(
                    "DELETE FROM hook_bindings WHERE routine_id = ?1",
                    [routine_id],
                )?;
                transaction.execute("DELETE FROM hook_outbox WHERE accepted=0 AND event_id IN(SELECT id FROM hook_events WHERE source_json=?1)", [&source])?;
            } else if let Some(bot_id) = bot_id {
                for table in [
                    "hook_bindings",
                    "hook_events",
                    "bot_session_cursors",
                ] {
                    transaction
                        .execute(&format!("DELETE FROM {table} WHERE bot_id = ?1"), [bot_id])?;
                }
            }
            save_catalog_row(transaction, state_json)?;
            if let Some(event) = event {super::events::record_event(transaction,event,None)?;}
            Ok(())
        })
    }

    pub(super) fn command_run(&self, command_id: &str) -> Result<Option<RoutineRun>> {
        let connection = self.lock()?;
        let run_id = connection
            .query_row(
                "SELECT run_id FROM routine_commands WHERE command_id = ?1",
                [command_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let run = run_id
            .map(|id| {
                query_run(&connection, &id)?
                    .ok_or_else(|| Error::Config("routine command receipt has no run".into()))
            })
            .transpose();
        drop(connection);
        run
    }

    pub(super) fn insert_run_with_cause(
        &self,
        run: &RoutineRun,
        command_id: &str,
        cause: Option<&crate::wire::HookEvent>,
    ) -> Result<()> {
        self.transaction(|transaction| {
            insert_run_row(transaction, run)?;
            transaction.execute(
                "INSERT INTO routine_commands(command_id,run_id,cause_json) VALUES(?1,?2,?3)",
                params![
                    command_id,
                    run.id,
                    cause.map(serde_json::to_string).transpose()?
                ],
            )?;
            if cause.is_some() {
                super::events::accept_action(transaction, Some(command_id))?;
            }
            super::events::record_run(transaction, run)
        })
    }

    pub(crate) fn request_run_stop(
        &self,
        run_id: &str,
        command_id: &str,
        cause: Option<&crate::wire::HookEvent>,
    ) -> Result<()> {
        self.transaction(|transaction| {
            if let Some(previous) = transaction.query_row("SELECT run_id FROM routine_stop_requests WHERE command_id = ?1", [command_id], |row| row.get::<_,String>(0)).optional()? {
                if previous != run_id {return Err(Error::Config("stop command identity targets another run".into()));}
                return Ok(());
            }
            let run = query_run(transaction, run_id)?.ok_or_else(||Error::Config("unknown routine run".into()))?;
            if run.status != RoutineRunStatus::Running {return Err(Error::Config("only an active invocation can be stopped".into()));}
            transaction.execute("INSERT INTO routine_stop_requests(run_id,command_id,cause_json) VALUES(?1,?2,?3) ON CONFLICT(run_id) DO NOTHING",params![run_id,command_id,cause.map(serde_json::to_string).transpose()?])?;
            Ok(())
        })
    }

    pub(crate) fn run_cancel_requested(&self, run_id: &str) -> Result<bool> {
        let connection = self.lock()?;
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM routine_stop_requests WHERE run_id=?1)",
            [run_id],
            |row| row.get(0),
        )?)
    }

    pub(super) fn has_running(&self, routine_id: &str) -> Result<bool> {
        let connection = self.lock()?;
        Ok(connection.query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM routine_runs
                 WHERE routine_id = ?1 AND status = 'running'
             )",
            [routine_id],
            |row| row.get::<_, bool>(0),
        )?)
    }

    pub(crate) fn running_routine_count(&self) -> Result<u64> {
        let connection = self.lock()?;
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM routine_runs WHERE status='running'",
            [],
            |row| row.get(0),
        )?;
        drop(connection);
        u64::try_from(count)
            .map_err(|error| Error::Config(format!("invalid routine count: {error}")))
    }

    pub(crate) fn has_running_routines(&self) -> Result<bool> {
        let connection = self.lock()?;
        Ok(connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM routine_runs WHERE status = 'running')",
            [],
            |row| row.get::<_, bool>(0),
        )?)
    }

    pub(super) fn validate_run_owners(&self, bot_ids: &BTreeSet<&str>) -> Result<()> {
        let connection = self.lock()?;
        let mut statement = connection.prepare("SELECT DISTINCT bot_id FROM routine_runs")?;
        let owners = statement.query_map([], |row| row.get::<_, String>(0))?;
        let first_failure = owners
            .map(|bot_id| bot_id.map(|bot_id| bot_ids.contains(bot_id.as_str())))
            .find(|owned| !matches!(owned, Ok(true)));
        drop(statement);
        drop(connection);
        match first_failure {
            None => Ok(()),
            Some(Ok(_)) => Err(Error::Config("persisted routine run has no Bot".into())),
            Some(Err(error)) => Err(error.into()),
        }
    }

    pub(super) fn history_routine_candidates(&self, prefix: &str) -> Result<Vec<String>> {
        let connection = self.lock()?;
        // Two distinct IDs suffice to reject an ambiguous prefix. Seek the index
        // and stop at the first nonmatch rather than scanning unrelated history.
        let mut statement = connection.prepare(
            "SELECT DISTINCT routine_id FROM routine_runs
             WHERE routine_id >= ?1 ORDER BY routine_id LIMIT 2",
        )?;
        let mut rows = statement.query([prefix])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            if !id.starts_with(prefix) {
                break;
            }
            let exact = id == prefix;
            ids.push(id);
            if exact {
                break;
            }
        }
        drop(rows);
        drop(statement);
        drop(connection);
        Ok(ids)
    }

    pub(crate) fn finish_run(
        &self,
        id: &str,
        status: RoutineRunStatus,
        finished_at: i64,
        message: Option<Arc<str>>,
    ) -> Result<RoutineRun> {
        self.transaction(|transaction| {
            let mut run = query_run(transaction, id)?
                .ok_or_else(|| Error::Config(format!("unknown routine run `{id}`")))?;
            if status == RoutineRunStatus::Running {
                return Err(Error::Config("cannot finish a run with running status".into()));
            }
            if run.status != RoutineRunStatus::Running {
                if run.status == status && run.message == message {
                    return Ok(run);
                }
                return Err(Error::Config("routine run already has a different terminal result".into()));
            }
            run.status = status;
            run.finished_at = Some(finished_at);
            run.message = message;
            validate_run(&run)?;
            transaction.execute(
                "UPDATE routine_runs SET finished_at = ?1, status = ?2, message = ?3 WHERE id = ?4 AND status = 'running'",
                params![finished_at, status_text(status), run.message, id],
            )?;
            super::events::record_run(transaction, &run)?;
            Ok(run)
        })
    }

    pub(super) fn history(&self, routine_id: Option<&str>) -> Result<Vec<RoutineRun>> {
        let connection = self.lock()?;
        let filter = if routine_id.is_some() {
            "WHERE routine_id = ?1"
        } else {
            ""
        };
        let mut statement =
            connection.prepare(&format!("{RUN_SELECT} {filter} ORDER BY ordinal DESC"))?;
        let runs = statement
            .query_map(rusqlite::params_from_iter(routine_id), run_from_row)?
            .map(|run| {
                let run = run?;
                validate_run(&run)?;
                Ok(run)
            })
            .collect();
        drop(statement);
        drop(connection);
        runs
    }

    pub(super) fn session_ids_for_routine(&self, routine_id: &str) -> Result<Vec<SessionId>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT session_id FROM routine_runs
                 WHERE routine_id = ?1 AND session_id IS NOT NULL",
        )?;
        let session_ids: rusqlite::Result<Vec<SessionId>> = statement
            .query_map([routine_id], |row| row.get(0))?
            .collect();
        drop(statement);
        drop(connection);
        Ok(session_ids?)
    }

    pub(super) fn run(&self, id: &str) -> Result<RoutineRun> {
        let connection = self.lock()?;
        query_run(&connection, id)?
            .ok_or_else(|| Error::Config(format!("unknown routine run `{id}`")))
            .and_then(|run| validate_run(&run).map(|()| run))
    }

    pub(super) fn delete_run(&self, id: &str) -> Result<RoutineRun> {
        self.transaction(|transaction| {
            let run = query_run(transaction, id)?
                .ok_or_else(|| Error::Config(format!("unknown routine run `{id}`")))?;
            if run.status == RoutineRunStatus::Running {
                return Err(Error::Config(format!(
                    "routine run {id} is currently running"
                )));
            }
            transaction.execute("DELETE FROM routine_runs WHERE id = ?1", [id])?;
            Ok(run)
        })
    }

    pub(super) fn transaction<T>(
        &self,
        operation: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = operation(&transaction)?;
        transaction.commit()?;
        drop(connection);
        if let Some((notify, activity)) = self.telemetry_notify.get() {
            if let Some(activity) = activity.upgrade() {
                activity.wake();
            }
            if let Some(notify) = notify.upgrade() {
                notify.notify_one();
            }
        }
        Ok(result)
    }
}

/// When the Bot database was last read: every commit changes one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CatalogStamp {
    data_version: i64,
    own_changes: u64,
}

fn read_catalog(connection: &Connection) -> Result<Option<String>> {
    connection
        .query_row("SELECT state_json FROM catalog WHERE id = 1", [], |row| {
            let catalog = row.get_ref(0)?.as_str()?;
            Ok(
                if u64::try_from(catalog.len()).unwrap_or(u64::MAX) > super::MAX_STATE_BYTES {
                    Err(Error::Config("Bot state is too large".into()))
                } else {
                    Ok(catalog.to_owned())
                },
            )
        })
        .optional()?
        .transpose()
}

fn configure_connection(connection: &Connection) -> Result<()> {
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

fn protect_database_files(path: &Path) -> Result<()> {
    {
        if let Some(parent) = path.parent() {
            fs::set_permissions(parent, mobius::owner_only::dir())?;
        }
        for candidate in [
            path.to_owned(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            if candidate.exists() {
                fs::set_permissions(candidate, mobius::owner_only::file())?;
            }
        }
    }
    Ok(())
}

pub(super) fn save_catalog_row(
    transaction: &rusqlite::Transaction<'_>,
    state_json: &str,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO catalog (id, state_json) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET state_json = excluded.state_json",
        [state_json],
    )?;
    Ok(())
}

fn insert_run_row(transaction: &rusqlite::Transaction<'_>, run: &RoutineRun) -> Result<()> {
    transaction.execute(
        "INSERT INTO routine_runs (
                 id, routine_id, bot_id, started_at, finished_at, status, session_id, message
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            run.id,
            run.routine_id,
            run.bot_id,
            run.started_at,
            run.finished_at,
            status_text(run.status),
            run.session_id,
            run.message,
        ],
    )?;
    Ok(())
}

const RUN_SELECT: &str = "SELECT id, routine_id, bot_id, started_at, finished_at, status,
    session_id, message FROM routine_runs";

fn query_run(source: &Connection, id: &str) -> Result<Option<RoutineRun>> {
    Ok(source
        .query_row(&format!("{RUN_SELECT} WHERE id = ?1"), [id], run_from_row)
        .optional()?)
}

fn run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RoutineRun> {
    let status: String = row.get(5)?;
    let status = match status.as_str() {
        "running" => RoutineRunStatus::Running,
        "succeeded" => RoutineRunStatus::Succeeded,
        "failed" => RoutineRunStatus::Failed,
        "skipped" => RoutineRunStatus::Skipped,
        "cancelled" => RoutineRunStatus::Cancelled,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(RoutineRun {
        id: row.get(0)?,
        routine_id: row.get(1)?,
        bot_id: row.get(2)?,
        started_at: row.get(3)?,
        finished_at: row.get(4)?,
        status,
        session_id: row.get(6)?,
        message: row.get(7)?,
    })
}

fn validate_run(run: &RoutineRun) -> Result<()> {
    if uuid::Uuid::parse_str(&run.id).is_err() {
        return Err(Error::Config("invalid persisted routine run ID".into()));
    }
    if run.routine_id.is_empty() || run.bot_id.is_empty() {
        return Err(Error::Config(
            "persisted routine run has an empty owner ID".into(),
        ));
    }
    // The table's CHECK constraints already tie status, finish time, and session.
    Ok(())
}

pub(super) const fn status_text(status: RoutineRunStatus) -> &'static str {
    match status {
        RoutineRunStatus::Running => "running",
        RoutineRunStatus::Succeeded => "succeeded",
        RoutineRunStatus::Failed => "failed",
        RoutineRunStatus::Skipped => "skipped",
        RoutineRunStatus::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_metadata_tables_are_available_for_existing_schema_six() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("bots.sqlite3");
        let (store, _) = BotStorage::open(&path).unwrap();
        store
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE telemetry_cursors; DROP TABLE hook_journal_sequences;")
            .unwrap();
        drop(store);
        let (store, _) = BotStorage::open(&path).unwrap();
        store
            .lock()
            .unwrap()
            .execute_batch("SELECT * FROM telemetry_cursors; SELECT * FROM hook_journal_sequences;")
            .expect("schema-six stores must retain the optional metadata tables");
    }
}
