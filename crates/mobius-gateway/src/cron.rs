//! Gateway-owned scheduled task persistence, matching, and overlap locks.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read as _, Write as _};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::Mutex;

use chrono::{TimeZone as _, Timelike as _, Utc};
use chrono_tz::Tz;
use croner::Cron;
use mobius::protocol::MAX_USER_INPUT_BYTES;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::wire::{
    CronRun, CronRunStatus, CronSchedule, CronScheduleKind, CronTask as CronTaskRecord,
};
use crate::{Error, Result};

const STATE_VERSION: u32 = 3;
const STATE_FILE: &str = "cron.json";
const STATE_LOCK_FILE: &str = "cron-state.lock";
const TASKS_DIR: &str = "tasks";
const MAX_STATE_BYTES: u64 = 1024 * 1024;
const MAX_RUNS: usize = 256;

/// Gateway-wide persistent scheduled-task state.
pub(crate) struct CronStore {
    state_dir: PathBuf,
    tasks_dir: PathBuf,
    path: PathBuf,
    state: Mutex<CronState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredCronTask {
    pub(crate) id: String,
    pub(crate) session_id: String,
    pub(crate) task: PathBuf,
    pub(crate) schedule: CronSchedule,
    pub(crate) ends_at: Option<i64>,
    pub(crate) enabled: bool,
    pub(crate) next_run_at: Option<i64>,
    pub(crate) last_scheduled_minute: Option<i64>,
}

impl StoredCronTask {
    fn reset_next_run(&mut self, now: i64) -> Result<()> {
        self.last_scheduled_minute = None;
        self.next_run_at = match self.schedule.kind {
            CronScheduleKind::Once => self.schedule.at,
            CronScheduleKind::Interval => Some(
                now.checked_add(
                    i64::try_from(self.schedule.every_seconds.ok_or_else(|| {
                        Error::Config("interval schedule is missing its interval".into())
                    })?)
                    .map_err(|_| Error::Config("interval schedule is too large".into()))?,
                )
                .ok_or_else(|| Error::Config("interval schedule overflows its timestamp".into()))?,
            ),
            CronScheduleKind::Cron => None,
        };
        Ok(())
    }

    fn advance_interval(&mut self, now: i64) -> Result<()> {
        let every =
            i64::try_from(self.schedule.every_seconds.ok_or_else(|| {
                Error::Config("interval schedule is missing its interval".into())
            })?)
            .map_err(|_| Error::Config("interval schedule is too large".into()))?;
        let next = self
            .next_run_at
            .ok_or_else(|| Error::Config("interval schedule has no next run".into()))?;
        let missed = (now.saturating_sub(next) / every).saturating_add(1);
        self.next_run_at = Some(
            next.checked_add(every.saturating_mul(missed))
                .ok_or_else(|| Error::Config("interval schedule overflows its timestamp".into()))?,
        );
        Ok(())
    }

    fn is_finished(&self, now: i64) -> bool {
        match self.schedule.kind {
            CronScheduleKind::Once => {
                self.next_run_at.is_none()
                    || self
                        .ends_at
                        .is_some_and(|ends_at| self.next_run_at.is_some_and(|next| next > ends_at))
            }
            CronScheduleKind::Interval => self
                .ends_at
                .is_some_and(|ends_at| self.next_run_at.is_none_or(|next| next > ends_at)),
            CronScheduleKind::Cron => self
                .ends_at
                .is_some_and(|ends_at| ends_at.div_euclid(60) < now.div_euclid(60)),
        }
    }

    fn next_run_at(&self, now: i64) -> Option<i64> {
        if self.is_finished(now) || !self.enabled {
            return None;
        }
        if self.schedule.kind != CronScheduleKind::Cron {
            return self.next_run_at;
        }
        let expression = self.schedule.expression.as_deref()?;
        let schedule = Cron::from_str(expression).ok()?;
        let time_zone = self.schedule.time_zone.as_deref()?.parse::<Tz>().ok()?;
        let now = Utc
            .timestamp_opt(now, 0)
            .single()?
            .with_timezone(&time_zone);
        let next = schedule.find_next_occurrence(&now, false).ok()?.timestamp();
        self.ends_at
            .map_or(Some(next), |ends_at| (next <= ends_at).then_some(next))
    }
}

/// Result of reserving one task invocation.
pub(crate) enum BeginRun {
    Started(ActiveCronRun),
    Skipped,
}

/// A durable running invocation whose file lock is held until completion.
pub(crate) struct ActiveCronRun {
    run_id: String,
    _lock: File,
}

impl Drop for ActiveCronRun {
    fn drop(&mut self) {
        let _ = self._lock.unlock();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CronState {
    version: u32,
    tasks: Vec<StoredCronTask>,
    runs: Vec<CronRun>,
}

impl Default for CronState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            tasks: Vec::new(),
            runs: Vec::new(),
        }
    }
}

impl CronStore {
    /// Opens or creates owner-only cron state.
    pub(crate) fn open(state_dir: &Path) -> Result<Self> {
        let state_dir = std::fs::canonicalize(state_dir)?;
        let tasks_dir = private_tasks_dir(&state_dir)?;
        let path = state_dir.join(STATE_FILE);
        let mut state = match File::open(&path) {
            Ok(mut file) => {
                #[cfg(unix)]
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                let mut contents = Vec::new();
                std::io::Read::by_ref(&mut file)
                    .take(MAX_STATE_BYTES + 1)
                    .read_to_end(&mut contents)?;
                if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_STATE_BYTES {
                    return Err(Error::Config("cron state is too large".into()));
                }
                serde_json::from_slice(&contents)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => CronState::default(),
            Err(error) => return Err(error.into()),
        };
        validate_state(&state, &tasks_dir)?;
        let recovered = recover_interrupted_runs(&mut state);
        let store = Self {
            state_dir,
            tasks_dir,
            path,
            state: Mutex::new(state),
        };
        if recovered || !store.path.exists() {
            let state = store.lock_state()?;
            store.save(&state)?;
        }
        Ok(store)
    }

    /// Writes and registers one user-created task in the private gateway task directory.
    pub(crate) fn create(
        &self,
        source_session_id: &str,
        task: &str,
        schedule: CronSchedule,
        ends_at: Option<i64>,
    ) -> Result<StoredCronTask> {
        validate_session_id(source_session_id)?;
        let task = task.trim();
        if task.is_empty() {
            return Err(Error::Config("scheduled task cannot be empty".into()));
        }
        if task.len() > MAX_USER_INPUT_BYTES {
            return Err(Error::Config(format!(
                "scheduled task exceeds the {MAX_USER_INPUT_BYTES}-byte input limit"
            )));
        }
        validate_schedule(&schedule, ends_at)?;
        let path = self
            .tasks_dir
            .join(format!("{}.md", Uuid::new_v4().as_hyphenated()));
        write_private_task(&self.tasks_dir, &path, task.as_bytes())?;
        let result = self.update(|state| {
            let now = Utc::now().timestamp();
            let mut task = StoredCronTask {
                id: Uuid::new_v4().to_string(),
                session_id: source_session_id.into(),
                task: path.clone(),
                schedule,
                ends_at,
                enabled: true,
                next_run_at: Some(now),
                last_scheduled_minute: None,
            };
            task.reset_next_run(now)?;
            state.tasks.push(task.clone());
            Ok(task)
        });
        match result {
            Ok(task) => Ok(task),
            Err(error) => match std::fs::remove_file(&path) {
                Ok(()) => Err(error),
                Err(rollback) => Err(Error::Config(format!(
                    "{error}; removing the unregistered task failed: {rollback}"
                ))),
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn add_for_test(
        &self,
        source_session_id: &str,
        task: &str,
        schedule: CronSchedule,
        ends_at: Option<i64>,
    ) -> Result<StoredCronTask> {
        self.create(source_session_id, task, schedule, ends_at)
    }

    /// Lists all scheduled tasks in creation order.
    pub(crate) fn list(&self) -> Result<Vec<StoredCronTask>> {
        Ok(self.lock_state()?.tasks.clone())
    }

    /// Lists all scheduled tasks without exposing their storage paths.
    pub(crate) fn records(&self, now: i64) -> Result<Vec<CronTaskRecord>> {
        self.list()?
            .into_iter()
            .map(|stored| {
                let finished = stored.is_finished(now);
                let next_run_at = stored.next_run_at(now);
                let (_, task) = self.task_input(&stored.id)?;
                Ok(CronTaskRecord {
                    id: stored.id,
                    source_session_id: stored.session_id,
                    task,
                    schedule: stored.schedule.clone(),
                    ends_at: stored.ends_at,
                    enabled: stored.enabled,
                    finished,
                    next_run_at,
                })
            })
            .collect()
    }

    pub(crate) fn record(&self, id: &str, now: i64) -> Result<CronTaskRecord> {
        self.records(now)?
            .into_iter()
            .find(|task| task.id == id)
            .ok_or_else(|| Error::Config(format!("unknown cron task `{id}`")))
    }

    pub(crate) fn has_active_tasks(&self, now: i64) -> Result<bool> {
        Ok(self
            .lock_state()?
            .tasks
            .iter()
            .any(|task| task.enabled && !task.is_finished(now)))
    }

    /// Replaces one task, accepting an unambiguous ID prefix.
    pub(crate) fn reschedule(
        &self,
        id: &str,
        source_session_id: &str,
        task_input: &str,
        schedule: CronSchedule,
        ends_at: Option<i64>,
        enabled: bool,
    ) -> Result<StoredCronTask> {
        validate_session_id(source_session_id)?;
        validate_task_input(task_input)?;
        validate_schedule(&schedule, ends_at)?;
        let existing = self.task(id)?;
        let (_, previous_input) = self.task_input(&existing.id)?;
        let Some(_lock) = self.try_task_lock(&existing.id)? else {
            return Err(Error::Config(format!(
                "cron task {} is currently running",
                existing.id
            )));
        };
        rewrite_private_task(&self.tasks_dir, &existing.task, task_input.as_bytes())?;
        let result = self.update(|state| {
            let index = resolve_task(&state.tasks, &existing.id)?;
            let stored = &mut state.tasks[index];
            stored.session_id = source_session_id.into();
            stored.schedule = schedule;
            stored.ends_at = ends_at;
            stored.enabled = enabled;
            stored.reset_next_run(Utc::now().timestamp())?;
            Ok(state.tasks[index].clone())
        });
        match result {
            Ok(task) => Ok(task),
            Err(error) => match rewrite_private_task(
                &self.tasks_dir,
                &existing.task,
                previous_input.as_bytes(),
            ) {
                Ok(()) => Err(error),
                Err(rollback) => Err(Error::Config(format!(
                    "{error}; restoring the previous task input failed: {rollback}"
                ))),
            },
        }
    }

    /// Deletes one idle task, accepting an unambiguous ID prefix.
    pub(crate) fn delete(&self, id: &str) -> Result<StoredCronTask> {
        let task = self.task(id)?;
        let Some(_lock) = self.try_task_lock(&task.id)? else {
            return Err(Error::Config(format!(
                "cron task {} is currently running",
                task.id
            )));
        };
        let deleted = self.update(|state| {
            let index = resolve_task(&state.tasks, &task.id)?;
            let deleted = state.tasks.remove(index);
            state.runs.retain(|run| run.task_id != deleted.id);
            Ok(deleted)
        })?;
        match std::fs::remove_file(&deleted.task) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Error::Config(format!(
                    "cron task {} was deleted, but its task file could not be removed: {error}",
                    deleted.id
                )));
            }
        }
        Ok(deleted)
    }

    /// Permanently removes one idle session's schedules and run history.
    pub(crate) fn delete_session(&self, source_session_id: &str) -> Result<()> {
        let (tasks, locks) = self.lock_session_tasks(source_session_id)?;
        for task in &tasks {
            remove_if_present(&task.task)?;
        }
        self.update(|state| {
            state
                .tasks
                .retain(|task| task.session_id != source_session_id);
            state
                .runs
                .retain(|run| run.source_session_id != source_session_id);
            Ok(())
        })?;
        drop(locks);
        Ok(())
    }

    /// Resolves one task by full ID or unambiguous prefix.
    pub(crate) fn task(&self, id: &str) -> Result<StoredCronTask> {
        let state = self.lock_state()?;
        Ok(state.tasks[resolve_task(&state.tasks, id)?].clone())
    }

    /// Reads a task after rechecking its path and input-size boundary.
    pub(crate) fn task_input(&self, id: &str) -> Result<(StoredCronTask, String)> {
        let task = self.stored_task(id)?;
        let path = std::fs::canonicalize(&task.task)?;
        if !path.is_file() || path.parent() != Some(self.tasks_dir.as_path()) {
            return Err(Error::Config(
                "cron task must remain inside the private gateway task directory".into(),
            ));
        }
        let mut file = File::open(&path)?;
        let opened = file.metadata()?;
        let verified = std::fs::canonicalize(&task.task)?;
        let current = std::fs::metadata(&verified)?;
        if verified != path || !same_file(&opened, &current) {
            return Err(Error::Config(
                "cron task changed while it was being opened".into(),
            ));
        }
        let limit = u64::try_from(MAX_USER_INPUT_BYTES).unwrap_or(u64::MAX);
        let mut bytes = Vec::new();
        std::io::Read::by_ref(&mut file)
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_USER_INPUT_BYTES {
            return Err(Error::Config(format!(
                "cron task exceeds the {MAX_USER_INPUT_BYTES}-byte input limit"
            )));
        }
        let input = String::from_utf8(bytes)
            .map_err(|_| Error::Config("cron task is not valid UTF-8".into()))?;
        if input.trim().is_empty() {
            return Err(Error::Config("cron task is empty".into()));
        }
        Ok((task, input))
    }

    /// Reserves due tasks and records their invocations atomically.
    pub(crate) fn take_due(&self, now: i64) -> Result<Vec<(String, ActiveCronRun)>> {
        let minute = now.div_euclid(60);
        self.update(|state| {
            let mut due = Vec::new();
            for index in 0..state.tasks.len() {
                let task = &state.tasks[index];
                if !task.enabled || task.is_finished(now) {
                    continue;
                }
                let should_run = match task.schedule.kind {
                    CronScheduleKind::Once | CronScheduleKind::Interval => {
                        task.next_run_at.is_some_and(|next| next <= now)
                    }
                    CronScheduleKind::Cron => {
                        if task.last_scheduled_minute == Some(minute) {
                            false
                        } else {
                            let expression =
                                task.schedule.expression.as_deref().ok_or_else(|| {
                                    Error::Config("cron schedule is missing its expression".into())
                                })?;
                            let schedule = Cron::from_str(expression).map_err(|error| {
                                Error::Config(format!("invalid persisted cron schedule: {error}"))
                            })?;
                            let time_zone = task
                                .schedule
                                .time_zone
                                .as_deref()
                                .ok_or_else(|| {
                                    Error::Config("cron schedule is missing its time zone".into())
                                })?
                                .parse::<Tz>()
                                .map_err(|error| {
                                    Error::Config(format!(
                                        "invalid persisted cron time zone: {error}"
                                    ))
                                })?;
                            let local_time = Utc
                                .timestamp_opt(now, 0)
                                .single()
                                .and_then(|time| time.with_timezone(&time_zone).with_second(0))
                                .ok_or_else(|| {
                                    Error::Config(
                                        "cron timestamp is outside the supported range".into(),
                                    )
                                })?;
                            schedule.is_time_matching(&local_time).map_err(|error| {
                                Error::Config(format!("invalid persisted cron schedule: {error}"))
                            })?
                        }
                    }
                };
                if !should_run {
                    continue;
                }
                let task = state.tasks[index].clone();
                {
                    let stored = &mut state.tasks[index];
                    stored.last_scheduled_minute = Some(minute);
                    match stored.schedule.kind {
                        CronScheduleKind::Once => stored.next_run_at = None,
                        CronScheduleKind::Interval => stored.advance_interval(now)?,
                        CronScheduleKind::Cron => {}
                    }
                }
                let Some(lock) = self.try_task_lock(&task.id)? else {
                    append_run(
                        state,
                        new_run(
                            &task,
                            CronRunStatus::Skipped,
                            Some("the previous invocation is still running".into()),
                        ),
                    )?;
                    continue;
                };
                let run = new_run(&task, CronRunStatus::Running, None);
                append_run(state, run.clone())?;
                due.push((
                    task.id,
                    ActiveCronRun {
                        run_id: run.id,
                        _lock: lock,
                    },
                ));
            }
            Ok(due)
        })
    }

    /// Starts an overlap-locked invocation or records an overlap skip.
    pub(crate) fn begin_run(&self, id: &str) -> Result<BeginRun> {
        let task = self.stored_task(id)?;
        let Some(lock) = self.try_task_lock(&task.id)? else {
            self.record_terminal_run(
                &task,
                CronRunStatus::Skipped,
                Some("the previous invocation is still running".into()),
            )?;
            return Ok(BeginRun::Skipped);
        };
        let run = new_run(&task, CronRunStatus::Running, None);
        self.update(|state| {
            append_run(state, run.clone())?;
            Ok(())
        })?;
        Ok(BeginRun::Started(ActiveCronRun {
            run_id: run.id,
            _lock: lock,
        }))
    }

    /// Associates the newly-created execution session with a running invocation.
    pub(crate) fn attach_execution_session(
        &self,
        run: &ActiveCronRun,
        execution_session_id: &str,
    ) -> Result<()> {
        validate_session_id(execution_session_id)?;
        self.update(|state| {
            let stored = find_run_mut(state, &run.run_id)?;
            stored.session_id = Some(execution_session_id.into());
            Ok(())
        })
    }

    /// Completes a running invocation and releases its overlap lock.
    pub(crate) fn finish_run(
        &self,
        run: ActiveCronRun,
        status: CronRunStatus,
        message: Option<String>,
    ) -> Result<CronRun> {
        if status == CronRunStatus::Running {
            return Err(Error::Config(
                "a completed cron run cannot remain running".into(),
            ));
        }
        self.update(|state| {
            let stored = find_run_mut(state, &run.run_id)?;
            stored.finished_at = Some(Utc::now().timestamp());
            stored.status = status;
            stored.message = message;
            Ok(stored.clone())
        })
    }

    /// Returns newest-first run history for one source session.
    pub(crate) fn history(&self, id: Option<&str>) -> Result<Vec<CronRun>> {
        let state = self.lock_state()?;
        let task_id = id.map(|id| resolve_history_task(&state, id)).transpose()?;
        Ok(state
            .runs
            .iter()
            .rev()
            .filter(|run| task_id.as_ref().is_none_or(|id| &run.task_id == id))
            .cloned()
            .collect())
    }

    pub(crate) fn run(&self, id: &str) -> Result<CronRun> {
        self.lock_state()?
            .runs
            .iter()
            .find(|run| run.id == id)
            .cloned()
            .ok_or_else(|| Error::Config(format!("unknown cron run `{id}`")))
    }

    fn stored_task(&self, id: &str) -> Result<StoredCronTask> {
        self.lock_state()?
            .tasks
            .iter()
            .find(|task| task.id == id)
            .cloned()
            .ok_or_else(|| Error::Config(format!("unknown cron task `{id}`")))
    }

    fn try_task_lock(&self, id: &str) -> Result<Option<File>> {
        let file = open_private_lock(self.state_dir.join(format!("cron-{id}.lock")))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    fn lock_session_tasks(
        &self,
        source_session_id: &str,
    ) -> Result<(Vec<StoredCronTask>, Vec<File>)> {
        let tasks = self
            .list()?
            .into_iter()
            .filter(|task| task.session_id == source_session_id)
            .collect::<Vec<_>>();
        let mut locks = Vec::with_capacity(tasks.len());
        for task in &tasks {
            let Some(lock) = self.try_task_lock(&task.id)? else {
                return Err(Error::Config(format!(
                    "cron task {} is currently running",
                    task.id
                )));
            };
            locks.push(lock);
        }
        Ok((tasks, locks))
    }

    fn record_terminal_run(
        &self,
        task: &StoredCronTask,
        status: CronRunStatus,
        message: Option<String>,
    ) -> Result<CronRun> {
        let run = new_run(task, status, message);
        self.update(|state| {
            append_run(state, run.clone())?;
            Ok(run)
        })
    }

    fn update<T>(&self, mutate: impl FnOnce(&mut CronState) -> Result<T>) -> Result<T> {
        let _file_lock = open_private_lock(self.state_dir.join(STATE_LOCK_FILE))?;
        _file_lock.lock()?;
        let mut state = self.lock_state()?;
        let mut next = state.clone();
        let result = mutate(&mut next)?;
        validate_state(&next, &self.tasks_dir)?;
        self.save(&next)?;
        *state = next;
        Ok(result)
    }

    fn save(&self, state: &CronState) -> Result<()> {
        let contents = serde_json::to_vec_pretty(state)?;
        if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_STATE_BYTES {
            return Err(Error::Config("cron state is too large".into()));
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.state_dir)?;
        #[cfg(unix)]
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(&contents)?;
        file.as_file().sync_all()?;
        file.persist(&self.path).map_err(|error| error.error)?;
        Ok(())
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, CronState>> {
        self.state
            .lock()
            .map_err(|_| Error::Config("cron state lock is poisoned".into()))
    }
}

fn validate_session_id(session_id: &str) -> Result<()> {
    if session_id.trim().is_empty() {
        return Err(Error::Config("cron session ID cannot be empty".into()));
    }
    Ok(())
}

fn validate_task_id_prefix(id: &str) -> Result<()> {
    if id.is_empty() || id.chars().any(char::is_whitespace) {
        return Err(Error::Config("cron task ID cannot be empty".into()));
    }
    Ok(())
}

fn validate_task_input(task: &str) -> Result<()> {
    let task = task.trim();
    if task.is_empty() {
        return Err(Error::Config("scheduled task cannot be empty".into()));
    }
    if task.len() > MAX_USER_INPUT_BYTES {
        return Err(Error::Config(format!(
            "scheduled task exceeds the {MAX_USER_INPUT_BYTES}-byte input limit"
        )));
    }
    Ok(())
}

fn validate_schedule(schedule: &CronSchedule, ends_at: Option<i64>) -> Result<()> {
    let populated = [
        schedule.at.is_some(),
        schedule.every_seconds.is_some(),
        schedule.expression.is_some(),
    ]
    .into_iter()
    .filter(|populated| *populated)
    .count();
    match schedule.kind {
        CronScheduleKind::Once
            if populated == 1 && schedule.at.is_some() && schedule.time_zone.is_none() =>
        {
            if ends_at.is_some_and(|ends_at| schedule.at.is_some_and(|at| at > ends_at)) {
                return Err(Error::Config(
                    "a once schedule cannot end before it runs".into(),
                ));
            }
        }
        CronScheduleKind::Interval
            if populated == 1
                && schedule.every_seconds.is_some()
                && schedule.time_zone.is_none() =>
        {
            if schedule.every_seconds.unwrap_or_default() < 60 {
                return Err(Error::Config("interval must be at least 60 seconds".into()));
            }
        }
        CronScheduleKind::Cron
            if populated == 1 && schedule.expression.is_some() && schedule.time_zone.is_some() =>
        {
            let time_zone = schedule.time_zone.as_deref().unwrap_or_default();
            time_zone
                .parse::<Tz>()
                .map_err(|error| Error::Config(format!("invalid cron time zone: {error}")))?;
            let expression = schedule.expression.as_deref().unwrap_or_default();
            let fields = expression.split_ascii_whitespace().collect::<Vec<_>>();
            if fields.len() != 5
                || fields.iter().any(|field| {
                    field.is_empty()
                        || !field.chars().all(|character| {
                            character.is_ascii_alphanumeric()
                                || matches!(character, '*' | '/' | ',' | '-')
                        })
                })
            {
                return Err(Error::Config(
                    "schedule must be a five-field cron expression".into(),
                ));
            }
            Cron::from_str(expression)
                .map_err(|error| Error::Config(format!("invalid cron schedule: {error}")))?;
        }
        _ => {
            return Err(Error::Config(
                "schedule fields do not match the selected schedule kind".into(),
            ));
        }
    }
    if ends_at.is_some_and(|ends_at| ends_at <= 0) {
        return Err(Error::Config("schedule end time must be positive".into()));
    }
    Ok(())
}

fn validate_state(state: &CronState, tasks_dir: &Path) -> Result<()> {
    if state.version != STATE_VERSION {
        return Err(Error::Config(format!(
            "unsupported cron state version {}",
            state.version
        )));
    }
    if state.runs.len() > MAX_RUNS {
        return Err(Error::Config("cron run history is too large".into()));
    }
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for task in &state.tasks {
        let parsed = Uuid::parse_str(&task.id)
            .map_err(|_| Error::Config("invalid persisted cron task ID".into()))?;
        if parsed.to_string() != task.id || !ids.insert(task.id.as_str()) {
            return Err(Error::Config("duplicate persisted cron task ID".into()));
        }
        validate_session_id(&task.session_id)?;
        if !task.task.is_absolute()
            || task.task.parent() != Some(tasks_dir)
            || !paths.insert(task.task.as_path())
        {
            return Err(Error::Config(
                "persisted cron task path is outside the private gateway task directory".into(),
            ));
        }
        validate_schedule(&task.schedule, task.ends_at)?;
        if task.next_run_at.is_some_and(|next| next <= 0) {
            return Err(Error::Config("invalid persisted cron next run".into()));
        }
    }
    let mut run_ids = BTreeSet::new();
    for run in &state.runs {
        if Uuid::parse_str(&run.id).is_err() || !run_ids.insert(run.id.as_str()) {
            return Err(Error::Config("invalid persisted cron run ID".into()));
        }
        if run.task_id.is_empty() {
            return Err(Error::Config("persisted cron run has no task ID".into()));
        }
        validate_session_id(&run.source_session_id)?;
        if let Some(session_id) = &run.session_id {
            validate_session_id(session_id)?;
        }
    }
    Ok(())
}

fn recover_interrupted_runs(state: &mut CronState) -> bool {
    let now = Utc::now().timestamp();
    let mut changed = false;
    for run in &mut state.runs {
        if run.status == CronRunStatus::Running {
            run.status = CronRunStatus::Failed;
            run.finished_at = Some(now);
            run.message = Some("the gateway stopped before this run completed".into());
            changed = true;
        }
    }
    changed
}

fn resolve_task(tasks: &[StoredCronTask], id: &str) -> Result<usize> {
    validate_task_id_prefix(id)?;
    if let Some(index) = tasks.iter().position(|task| task.id == id) {
        return Ok(index);
    }
    let mut matches = tasks
        .iter()
        .enumerate()
        .filter(|(_, task)| task.id.starts_with(id));
    let (index, _) = matches
        .next()
        .ok_or_else(|| Error::Config(format!("unknown cron task `{id}`")))?;
    if matches.next().is_some() {
        return Err(Error::Config(format!(
            "cron task ID prefix `{id}` is ambiguous"
        )));
    }
    Ok(index)
}

fn resolve_history_task(state: &CronState, id: &str) -> Result<String> {
    validate_task_id_prefix(id)?;
    let mut ids = state
        .tasks
        .iter()
        .map(|task| task.id.as_str())
        .chain(state.runs.iter().map(|run| run.task_id.as_str()))
        .filter(|task_id| task_id.starts_with(id))
        .collect::<BTreeSet<_>>();
    if ids.contains(id) {
        return Ok(id.into());
    }
    let resolved = ids
        .pop_first()
        .ok_or_else(|| Error::Config(format!("unknown cron task `{id}`")))?;
    if !ids.is_empty() {
        return Err(Error::Config(format!(
            "cron task ID prefix `{id}` is ambiguous"
        )));
    }
    Ok(resolved.into())
}

fn new_run(task: &StoredCronTask, status: CronRunStatus, message: Option<String>) -> CronRun {
    let now = Utc::now().timestamp();
    CronRun {
        id: Uuid::new_v4().to_string(),
        task_id: task.id.clone(),
        source_session_id: task.session_id.clone(),
        started_at: now,
        finished_at: (status != CronRunStatus::Running).then_some(now),
        status,
        session_id: None,
        message,
    }
}

fn append_run(state: &mut CronState, run: CronRun) -> Result<()> {
    if state.runs.len() == MAX_RUNS {
        let index = state
            .runs
            .iter()
            .position(|run| run.status != CronRunStatus::Running)
            .ok_or_else(|| Error::Config("cron run history is full of active runs".into()))?;
        state.runs.remove(index);
    }
    state.runs.push(run);
    Ok(())
}

fn find_run_mut<'a>(state: &'a mut CronState, id: &str) -> Result<&'a mut CronRun> {
    state
        .runs
        .iter_mut()
        .find(|run| run.id == id)
        .ok_or_else(|| Error::Config(format!("unknown cron run `{id}`")))
}

fn open_private_lock(path: PathBuf) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path)?;
    #[cfg(unix)]
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn private_tasks_dir(state_dir: &Path) -> Result<PathBuf> {
    let path = state_dir.join(TASKS_DIR);
    std::fs::create_dir_all(&path)?;
    let path = std::fs::canonicalize(path)?;
    if path.parent() != Some(state_dir) || !path.is_dir() {
        return Err(Error::Config(
            "gateway task directory must be a real directory inside gateway state".into(),
        ));
    }
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn write_private_task(directory: &Path, path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    #[cfg(unix)]
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(contents)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    Ok(())
}

fn rewrite_private_task(directory: &Path, path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    #[cfg(unix)]
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(contents)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(unix)]
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests;
