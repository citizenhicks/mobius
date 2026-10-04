use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
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

pub(super) struct BotStorage {
    path: PathBuf,
    pub(super) connection: Mutex<Connection>,
    telemetry_notify: OnceLock<Weak<tokio::sync::Notify>>,
}

impl BotStorage {
    pub(super) fn open(path: &Path) -> Result<(Self, bool)> {
        let connection = Connection::open(path).map_err(Error::from)?;
        protect_database_files(path)?;
        connection.busy_timeout(BUSY_TIMEOUT).map_err(Error::from)?;
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(Error::from)?;
        if version == 0
            && connection
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM sqlite_schema LIMIT 1)",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(Error::from)?
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
        let journal_mode: String = connection
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .map_err(Error::from)?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(Error::Config(format!(
                "SQLite could not enable Bot storage WAL mode: {journal_mode}"
            )));
        }
        configure_connection(&connection)?;
        if version == 0 {
            connection.execute_batch(SCHEMA).map_err(Error::from)?;
        }
        // A version-6 database predates telemetry. This optional table is
        // idempotent and does not change the Bot schema version.
        connection.execute(
            "CREATE TABLE IF NOT EXISTS telemetry_cursors (sink_id TEXT PRIMARY KEY, after_rowid INTEGER NOT NULL)",
            [],
        )?;
        protect_database_files(path)?;
        let persisted = connection
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM catalog WHERE id = 1)",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(Error::from)?;
        Ok((
            Self {
                path: path.to_owned(),
                connection: Mutex::new(connection),
                telemetry_notify: OnceLock::new(),
            },
            persisted,
        ))
    }

    pub(super) fn load_catalog(&self) -> Result<Option<String>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        read_catalog(&connection)
    }

    pub(super) fn attach_telemetry_notify(&self, notify: &Arc<tokio::sync::Notify>) {
        let _ = self.telemetry_notify.set(Arc::downgrade(notify));
    }

    /// The catalog with the stamp it was read at, unless nothing committed since `stamp`.
    pub(super) fn catalog_since(
        &self,
        stamp: Option<CatalogStamp>,
    ) -> Result<Option<(CatalogStamp, Option<String>)>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        // data_version moves on commits by other connections, total_changes on our own.
        let current = CatalogStamp {
            data_version: connection
                .query_row("PRAGMA data_version", [], |row| row.get(0))
                .map_err(Error::from)?,
            own_changes: connection.total_changes(),
        };
        if stamp == Some(current) {
            return Ok(None);
        }
        Ok(Some((current, read_catalog(&connection)?)))
    }

    pub(super) fn save_catalog(&self, state_json: &str) -> Result<()> {
        self.transaction(|transaction| save_catalog_row(transaction, state_json))
    }

    #[cfg(test)]
    pub(super) fn save_catalog_and_runs(
        &self,
        state_json: &str,
        runs: &[RoutineRun],
    ) -> Result<()> {
        self.transaction(|transaction| {
            save_catalog_row(transaction, state_json)?;
            for run in runs {
                insert_run(transaction, run)?;
            }
            Ok(())
        })
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
                    )
                    .map_err(Error::from)?;
            } else if let Some(bot_id) = bot_id {
                transaction
                    .execute("DELETE FROM routine_runs WHERE bot_id = ?1", [bot_id])
                    .map_err(Error::from)?;
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
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        let run_id = connection
            .query_row(
                "SELECT run_id FROM routine_commands WHERE command_id = ?1",
                [command_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        run_id
            .map(|id| {
                query_run(&connection, &id)?
                    .ok_or_else(|| Error::Config("routine command receipt has no run".into()))
            })
            .transpose()
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

    pub(super) fn request_run_stop(
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

    pub(super) fn run_cancel_requested(&self, run_id: &str) -> Result<bool> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM routine_stop_requests WHERE run_id=?1)",
            [run_id],
            |row| row.get(0),
        )?)
    }

    pub(super) fn has_running(&self, routine_id: &str) -> Result<bool> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        connection
            .query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM routine_runs
                     WHERE routine_id = ?1 AND status = 'running'
                 )",
                [routine_id],
                |row| row.get::<_, bool>(0),
            )
            .map_err(Error::from)
    }

    pub(super) fn running_routine_count(&self) -> Result<u64> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        connection
            .query_row(
                "SELECT COUNT(*) FROM routine_runs WHERE status='running'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(Error::from)
            .and_then(|count| {
                u64::try_from(count).map_err(|_| Error::Config("invalid routine count".into()))
            })
    }

    pub(super) fn has_running_routines(&self) -> Result<bool> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        connection
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM routine_runs WHERE status = 'running')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(Error::from)
    }

    pub(super) fn validate_run_owners(&self, bot_ids: &BTreeSet<String>) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        let mut statement = connection
            .prepare("SELECT DISTINCT bot_id FROM routine_runs")
            .map_err(Error::from)?;
        let owners = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(Error::from)?;
        for bot_id in owners {
            if !bot_ids.contains(&bot_id?) {
                return Err(Error::Config("persisted routine run has no Bot".into()));
            }
        }
        Ok(())
    }

    pub(super) fn history_routine_candidates(&self, prefix: &str) -> Result<Vec<String>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
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
        Ok(ids)
    }

    pub(super) fn finish_run(
        &self,
        id: &str,
        status: RoutineRunStatus,
        finished_at: i64,
        message: Option<String>,
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
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        let sql = match routine_id {
            Some(_) => {
                "SELECT id, routine_id, bot_id, started_at, finished_at, status,
                            session_id, message
                     FROM routine_runs
                     WHERE routine_id = ?1
                     ORDER BY ordinal DESC"
            }
            None => {
                "SELECT id, routine_id, bot_id, started_at, finished_at, status,
                            session_id, message
                     FROM routine_runs
                     ORDER BY ordinal DESC"
            }
        };
        let mut statement = connection.prepare(sql)?;
        statement
            .query_map(rusqlite::params_from_iter(routine_id), run_from_row)?
            .map(|run| {
                let run = run?;
                validate_run(&run)?;
                Ok(run)
            })
            .collect()
    }

    pub(super) fn session_ids_for_routine(&self, routine_id: &str) -> Result<Vec<String>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        let mut statement = connection
            .prepare(
                "SELECT session_id FROM routine_runs
                 WHERE routine_id = ?1 AND session_id IS NOT NULL",
            )
            .map_err(Error::from)?;
        statement
            .query_map([routine_id], |row| row.get::<_, String>(0))
            .map_err(Error::from)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::from)
    }

    pub(super) fn run(&self, id: &str) -> Result<RoutineRun> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
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
            transaction
                .execute("DELETE FROM routine_runs WHERE id = ?1", [id])
                .map_err(Error::from)?;
            Ok(run)
        })
    }

    pub(super) fn transaction<T>(
        &self,
        operation: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
        protect_database_files(&self.path)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(Error::from)?;
        let result = operation(&transaction)?;
        transaction.commit().map_err(Error::from)?;
        if let Some(notify) = self.telemetry_notify.get().and_then(Weak::upgrade) {
            notify.notify_one();
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
            Ok(if catalog.len() > super::MAX_STATE_BYTES as usize {
                Err(Error::Config("Bot state is too large".into()))
            } else {
                Ok(catalog.to_owned())
            })
        })
        .optional()?
        .transpose()
}

fn configure_connection(connection: &Connection) -> Result<()> {
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(Error::from)?;
    connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(Error::from)
}

fn protect_database_files(path: &Path) -> Result<()> {
    #[cfg(unix)]
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
    transaction
        .execute(
            "INSERT INTO catalog (id, state_json) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET state_json = excluded.state_json",
            [state_json],
        )
        .map_err(Error::from)?;
    Ok(())
}

#[cfg(test)]
fn insert_run(transaction: &rusqlite::Transaction<'_>, run: &RoutineRun) -> Result<()> {
    insert_run_row(transaction, run)?;
    super::events::record_run(transaction, run)
}

fn insert_run_row(transaction: &rusqlite::Transaction<'_>, run: &RoutineRun) -> Result<()> {
    validate_run(run)?;
    transaction
        .execute(
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
        )
        .map_err(Error::from)?;
    Ok(())
}

fn query_run(source: &Connection, id: &str) -> Result<Option<RoutineRun>> {
    source
        .query_row(
            "SELECT id, routine_id, bot_id, started_at, finished_at, status,
                    session_id, message
             FROM routine_runs WHERE id = ?1",
            [id],
            run_from_row,
        )
        .optional()
        .map_err(Error::from)
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
    if run.status == RoutineRunStatus::Running && run.finished_at.is_some() {
        return Err(Error::Config(
            "running routine run has a finish time".into(),
        ));
    }
    if run.status == RoutineRunStatus::Running && run.session_id.is_none() {
        return Err(Error::Config("running routine run has no session".into()));
    }
    if run.status != RoutineRunStatus::Running && run.finished_at.is_none() {
        return Err(Error::Config(
            "completed routine run has no finish time".into(),
        ));
    }
    if let Some(session_id) = &run.session_id {
        super::validate_session_id(session_id)?;
    }
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

    fn completed_run() -> RoutineRun {
        RoutineRun {
            id: uuid::Uuid::new_v4().to_string(),
            routine_id: uuid::Uuid::new_v4().to_string(),
            bot_id: uuid::Uuid::new_v4().to_string(),
            started_at: 1,
            finished_at: Some(1),
            status: RoutineRunStatus::Succeeded,
            session_id: Some(uuid::Uuid::new_v4().to_string()),
            message: None,
        }
    }

    #[test]
    fn catalog_and_run_insert_roll_back_together() {
        let directory = tempfile::tempdir().expect("storage directory");
        let (storage, _) = BotStorage::open(&directory.path().join(STATE_FILE)).expect("storage");
        storage.save_catalog("old").expect("initial catalog");
        let run = completed_run();

        assert!(
            storage
                .save_catalog_and_runs("new", &[run.clone(), run])
                .is_err()
        );
        assert_eq!(
            storage.load_catalog().expect("catalog").as_deref(),
            Some("old")
        );
        assert!(storage.history(None).expect("history").is_empty());
    }

    #[tokio::test]
    async fn committed_bot_changes_nudge_telemetry() {
        let directory = tempfile::tempdir().expect("storage directory");
        let (storage, _) = BotStorage::open(&directory.path().join(STATE_FILE)).expect("storage");
        let notify = Arc::new(tokio::sync::Notify::new());
        storage.attach_telemetry_notify(&notify);
        storage.save_catalog("updated").expect("commit catalog");
        tokio::time::timeout(Duration::from_millis(100), notify.notified())
            .await
            .expect("committed changes nudge telemetry");
    }

    #[test]
    fn existing_version_six_database_gains_telemetry_cursors() {
        let directory = tempfile::tempdir().expect("storage directory");
        let path = directory.path().join(STATE_FILE);
        let (storage, _) = BotStorage::open(&path).expect("initial storage");
        storage.save_catalog("existing").expect("existing catalog");
        drop(storage);
        let connection = Connection::open(&path).expect("old database");
        connection
            .execute("DROP TABLE telemetry_cursors", [])
            .expect("old schema");
        drop(connection);

        let (storage, persisted) = BotStorage::open(&path).expect("reopen version 6");
        assert!(persisted);
        assert_eq!(
            storage.load_catalog().expect("catalog").as_deref(),
            Some("existing")
        );
        let connection = storage.connection.lock().expect("connection");
        let cursors: i64 = connection
            .query_row("SELECT COUNT(*) FROM telemetry_cursors", [], |row| {
                row.get(0)
            })
            .expect("telemetry cursor table");
        assert_eq!(cursors, 0);
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        for version in [SCHEMA_VERSION - 1, SCHEMA_VERSION + 1] {
            let directory = tempfile::tempdir().expect("storage directory");
            let path = directory.path().join(STATE_FILE);
            let connection = Connection::open(&path).expect("database");
            connection
                .pragma_update(None, "user_version", version)
                .expect("schema version");
            drop(connection);

            let error = match BotStorage::open(&path) {
                Ok(_) => panic!("unsupported schema must fail"),
                Err(error) => error,
            };
            assert!(
                error
                    .to_string()
                    .contains("unsupported Bot storage schema version")
            );
        }
    }
}
