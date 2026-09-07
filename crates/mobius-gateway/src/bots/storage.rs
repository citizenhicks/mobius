use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::wire::{RoutineRun, RoutineRunStatus};
use crate::{Error, Result};

pub(super) const STATE_FILE: &str = "bots.sqlite3";

const SCHEMA_VERSION: i64 = 1;
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
        status IN ('running', 'succeeded', 'failed', 'skipped')
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
PRAGMA user_version = 1;
COMMIT;
";

pub(super) struct BotStorage {
    path: PathBuf,
    connection: Mutex<Connection>,
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
            },
            persisted,
        ))
    }

    pub(super) fn load_catalog(&self) -> Result<Option<String>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| Error::Config("Bot storage lock is poisoned".into()))?;
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

    pub(super) fn save_catalog(&self, state_json: &str) -> Result<()> {
        self.transaction(|transaction| save_catalog_row(transaction, state_json))
    }

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
    ) -> Result<()> {
        debug_assert!(routine_id.is_some() ^ bot_id.is_some());
        self.transaction(|transaction| {
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
            save_catalog_row(transaction, state_json)
        })
    }

    pub(super) fn insert_run(&self, run: &RoutineRun) -> Result<()> {
        self.transaction(|transaction| insert_run(transaction, run))
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
            let changed = transaction
                .execute(
                    "UPDATE routine_runs
                     SET finished_at = ?1, status = ?2, message = ?3
                     WHERE id = ?4",
                    params![finished_at, status_text(status), message, id],
                )
                .map_err(Error::from)?;
            if changed == 0 {
                return Err(Error::Config(format!("unknown routine run `{id}`")));
            }
            query_run(transaction, id)?
                .ok_or_else(|| Error::Config(format!("unknown routine run `{id}`")))
                .and_then(|run| validate_run(&run).map(|()| run))
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

    pub(super) fn recover_interrupted_runs(&self) -> Result<bool> {
        let now = chrono::Utc::now().timestamp();
        self.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE routine_runs
                     SET status = 'failed', finished_at = ?1,
                         message = 'the gateway stopped before this run completed'
                     WHERE status = 'running'",
                    [now],
                )
                .map_err(Error::from)?;
            Ok(changed != 0)
        })
    }

    fn transaction<T>(
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
        Ok(result)
    }
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
        use std::os::unix::fs::PermissionsExt as _;

        if let Some(parent) = path.parent() {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        for candidate in [
            path.to_owned(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            if candidate.exists() {
                fs::set_permissions(candidate, fs::Permissions::from_mode(0o600))?;
            }
        }
    }
    Ok(())
}

fn save_catalog_row(transaction: &rusqlite::Transaction<'_>, state_json: &str) -> Result<()> {
    transaction
        .execute(
            "INSERT INTO catalog (id, state_json) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET state_json = excluded.state_json",
            [state_json],
        )
        .map_err(Error::from)?;
    Ok(())
}

fn insert_run(transaction: &rusqlite::Transaction<'_>, run: &RoutineRun) -> Result<()> {
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

const fn status_text(status: RoutineRunStatus) -> &'static str {
    match status {
        RoutineRunStatus::Running => "running",
        RoutineRunStatus::Succeeded => "succeeded",
        RoutineRunStatus::Failed => "failed",
        RoutineRunStatus::Skipped => "skipped",
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

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let directory = tempfile::tempdir().expect("storage directory");
        let path = directory.path().join(STATE_FILE);
        let connection = Connection::open(&path).expect("database");
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
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
