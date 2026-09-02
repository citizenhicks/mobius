//! Gateway-owned Bot profiles, routines, run history, and schedule matching.

pub(crate) mod swarm;

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read as _, Write as _};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr as _;
use std::sync::Mutex;

use chrono::{TimeZone as _, Timelike as _, Utc};
use chrono_tz::Tz;
use croner::Cron;
use mobius::protocol::MAX_MESSAGE_BYTES;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::validate_agent_composition;
use crate::wire::{
    AgentComposition, BotRecord, ProviderTint, Routine, RoutineRun, RoutineRunStatus,
    RoutineSchedule, RoutineScheduleKind, VersionedAgentConfig,
};
use crate::{Error, Result};

const STATE_VERSION: u32 = 3;
const STATE_FILE: &str = "bots.json";
const STATE_LOCK_FILE: &str = "bots-state.lock";
const ROUTINES_DIR: &str = "routines";
const ROUTINE_SUBMISSION_PREFIX: &str =
    "# Routine\n\nThe instructions below relate to a routine task.";
const MAX_ROUTINE_INSTRUCTIONS_BYTES: usize =
    MAX_MESSAGE_BYTES - ROUTINE_SUBMISSION_PREFIX.len() - 2;
const MAX_STATE_BYTES: u64 = 1024 * 1024;
const MAX_HANDLE_BYTES: usize = 64;
const MAX_NAME_BYTES: usize = 128;
const MAX_DESCRIPTION_BYTES: usize = 2 * 1024;
pub(crate) const MOBIUS_HANDLE: &str = "mobius";
const USER_HANDLE: &str = "user";
const MOBIUS_NAME: &str = "Mobius";
pub(crate) const MOBIUS_DESCRIPTION: &str = "You are möbius, a concise coding agent. Inspect the real code path before editing, make the smallest focused change, and preserve unrelated work.";
const BOT_TINTS: [ProviderTint; 7] = [
    ProviderTint::Blue,
    ProviderTint::Teal,
    ProviderTint::Green,
    ProviderTint::Yellow,
    ProviderTint::Orange,
    ProviderTint::Red,
    ProviderTint::Purple,
];

/// Gateway-wide persistent Bot profiles, routines, and run history.
pub(crate) struct BotStore {
    state_dir: PathBuf,
    routines_dir: PathBuf,
    path: PathBuf,
    state: Mutex<BotState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredRoutine {
    pub(crate) id: String,
    pub(crate) bot_id: String,
    pub(crate) workspace: PathBuf,
    pub(crate) instructions: PathBuf,
    pub(crate) schedule: RoutineSchedule,
    pub(crate) ends_at: Option<i64>,
    pub(crate) enabled: bool,
    pub(crate) next_run_at: Option<i64>,
    pub(crate) last_matched_minute: Option<i64>,
}

impl StoredRoutine {
    fn reset_next_run(&mut self, now: i64) -> Result<()> {
        self.last_matched_minute = None;
        self.next_run_at = match self.schedule.kind {
            RoutineScheduleKind::Once => self.schedule.at,
            RoutineScheduleKind::Interval => Some(
                now.checked_add(
                    i64::try_from(self.schedule.every_seconds.ok_or_else(|| {
                        Error::Config("interval schedule is missing its interval".into())
                    })?)
                    .map_err(|_| Error::Config("interval schedule is too large".into()))?,
                )
                .ok_or_else(|| Error::Config("interval schedule overflows its timestamp".into()))?,
            ),
            RoutineScheduleKind::Cron => None,
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
            RoutineScheduleKind::Once => {
                self.next_run_at.is_none()
                    || self
                        .ends_at
                        .is_some_and(|ends_at| self.next_run_at.is_some_and(|next| next > ends_at))
            }
            RoutineScheduleKind::Interval => self
                .ends_at
                .is_some_and(|ends_at| self.next_run_at.is_none_or(|next| next > ends_at)),
            RoutineScheduleKind::Cron => self
                .ends_at
                .is_some_and(|ends_at| ends_at.div_euclid(60) < now.div_euclid(60)),
        }
    }

    fn next_run_at(&self, now: i64) -> Option<i64> {
        if self.is_finished(now) || !self.enabled {
            return None;
        }
        if self.schedule.kind != RoutineScheduleKind::Cron {
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
    Started(ActiveRoutineRun),
    Skipped,
}

/// A durable running invocation whose file lock is held until completion.
pub(crate) struct ActiveRoutineRun {
    run_id: String,
    session_id: String,
    _lock: File,
}

/// Validated Bot deletion whose routine locks stay held through gateway cleanup.
#[derive(Debug)]
pub(crate) struct BotDeletion {
    bot_id: String,
    expected_revision: u64,
    routine_ids: BTreeSet<String>,
    instructions: BTreeSet<PathBuf>,
    state_lock: Option<File>,
    _routine_locks: Vec<File>,
}

impl BotDeletion {
    pub(crate) fn release_state_lock(&mut self) {
        drop(self.state_lock.take());
    }
}

/// Validated routine deletion whose lock stays held through gateway cleanup.
#[derive(Debug)]
pub(crate) struct RoutineDeletion {
    routine_id: String,
    session_ids: BTreeSet<String>,
    instructions: PathBuf,
    _state_lock: File,
    _lock: File,
}

/// Durable forward-recovery record for a cross-owner Bot cascade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingBotDeletion {
    pub(crate) bot_id: String,
    pub(crate) expected_revision: u64,
    pub(crate) session_roots: Vec<String>,
    pub(crate) session_ids: Vec<String>,
    pub(crate) swarm_id: Option<String>,
    pub(crate) disbanded_swarm: bool,
    instruction_paths: Vec<PathBuf>,
}

impl RoutineDeletion {
    pub(crate) fn session_ids(&self) -> &BTreeSet<String> {
        &self.session_ids
    }
}

impl ActiveRoutineRun {
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl Drop for ActiveRoutineRun {
    fn drop(&mut self) {
        let _ = self._lock.unlock();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BotState {
    version: u32,
    bots: Vec<BotRecord>,
    routines: Vec<StoredRoutine>,
    runs: Vec<RoutineRun>,
    pending_bot_deletion: Option<PendingBotDeletion>,
}

impl Default for BotState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            bots: Vec::new(),
            routines: Vec::new(),
            runs: Vec::new(),
            pending_bot_deletion: None,
        }
    }
}

impl BotStore {
    /// Opens or creates owner-only Bot state.
    pub(crate) fn open(state_dir: &Path) -> Result<Self> {
        let state_dir = std::fs::canonicalize(state_dir)?;
        let routines_dir = private_routines_dir(&state_dir)?;
        let path = state_dir.join(STATE_FILE);
        let (mut state, persisted) = match File::open(&path) {
            Ok(mut file) => {
                #[cfg(unix)]
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                let mut contents = Vec::new();
                std::io::Read::by_ref(&mut file)
                    .take(MAX_STATE_BYTES + 1)
                    .read_to_end(&mut contents)?;
                if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_STATE_BYTES {
                    return Err(Error::Config("Bot state is too large".into()));
                }
                (serde_json::from_slice(&contents)?, true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (BotState::default(), false)
            }
            Err(error) => return Err(error.into()),
        };
        validate_state(&state, &routines_dir)?;
        if persisted && !state.bots.iter().any(|bot| bot.handle == MOBIUS_HANDLE) {
            return Err(Error::Config(
                "persisted Bot state has no built-in @mobius Bot".into(),
            ));
        }
        let recovered = recover_interrupted_runs(&mut state);
        let store = Self {
            state_dir,
            routines_dir,
            path,
            state: Mutex::new(state),
        };
        if recovered {
            let state = store.lock_state()?;
            store.save(&state)?;
        }
        Ok(store)
    }

    /// Creates the ordinary built-in Bot only before Bot state has ever existed.
    pub(crate) fn seed_default(
        &self,
        defaults: &VersionedAgentConfig,
    ) -> Result<Option<BotRecord>> {
        if self.path.exists() {
            return Ok(None);
        }
        let config = defaults.config.clone();
        validate_agent_composition(&config)?;
        self.update(|state| {
            if !state.bots.is_empty() {
                return Err(Error::Config(
                    "missing Bot state cannot contain in-memory Bots".into(),
                ));
            }
            let bot = BotRecord {
                id: Uuid::new_v4().to_string(),
                handle: MOBIUS_HANDLE.into(),
                name: MOBIUS_NAME.into(),
                description: MOBIUS_DESCRIPTION.into(),
                tint: ProviderTint::default(),
                config: VersionedAgentConfig {
                    revision: 1,
                    config,
                },
            };
            state.bots.push(bot.clone());
            Ok(Some(bot))
        })
    }

    pub(crate) fn create_bot(
        &self,
        name: &str,
        description: &str,
        config: AgentComposition,
    ) -> Result<BotRecord> {
        let name = validate_name(name)?;
        let description = validate_description(description)?;
        validate_agent_composition(&config)?;
        self.update(|state| {
            let handle = next_handle(state, &name);
            let tint = next_tint(state);
            let bot = BotRecord {
                id: Uuid::new_v4().to_string(),
                handle,
                name,
                description,
                tint,
                config: VersionedAgentConfig {
                    revision: 1,
                    config,
                },
            };
            state.bots.push(bot.clone());
            Ok(bot)
        })
    }

    pub(crate) fn update_bot(
        &self,
        id: &str,
        expected_revision: u64,
        name: &str,
        description: &str,
        tint: ProviderTint,
        config: AgentComposition,
    ) -> Result<BotRecord> {
        let name = validate_name(name)?;
        let description = validate_description(description)?;
        validate_agent_composition(&config)?;
        self.update(|state| {
            let bot = find_bot_mut(state, id)?;
            if bot.config.revision != expected_revision {
                return Err(Error::Config(format!(
                    "Bot configuration revision changed from {expected_revision} to {}",
                    bot.config.revision
                )));
            }
            bot.name = name;
            bot.description = description;
            bot.tint = tint;
            bot.config = VersionedAgentConfig {
                revision: expected_revision
                    .checked_add(1)
                    .ok_or_else(|| Error::Config("Bot revision overflow".into()))?,
                config,
            };
            Ok(bot.clone())
        })
    }

    pub(crate) fn prepare_bot_deletion(
        &self,
        id: &str,
        expected_revision: u64,
    ) -> Result<BotDeletion> {
        let state_lock = open_private_lock(self.state_dir.join(STATE_LOCK_FILE))?;
        state_lock.lock()?;
        let state = self.lock_state()?;
        let bot = state
            .bots
            .iter()
            .find(|bot| bot.id == id)
            .ok_or_else(|| Error::Config(format!("unknown Bot `{id}`")))?;
        if bot.handle == MOBIUS_HANDLE {
            return Err(Error::Config(
                "the built-in @mobius Bot cannot be deleted".into(),
            ));
        }
        if bot.config.revision != expected_revision {
            return Err(Error::Config(format!(
                "Bot configuration revision changed from {expected_revision} to {}",
                bot.config.revision
            )));
        }
        let routines = state
            .routines
            .iter()
            .filter(|routine| routine.bot_id == id)
            .cloned()
            .collect::<Vec<_>>();
        let routine_ids = routines
            .iter()
            .map(|routine| routine.id.clone())
            .collect::<BTreeSet<_>>();
        let instructions = routines
            .iter()
            .map(|routine| routine.instructions.clone())
            .collect::<BTreeSet<_>>();
        drop(state);
        let mut routine_locks = Vec::with_capacity(routine_ids.len());
        for routine_id in &routine_ids {
            let Some(lock) = self.try_routine_lock(routine_id)? else {
                return Err(Error::Config(format!(
                    "routine {routine_id} is currently running"
                )));
            };
            routine_locks.push(lock);
        }
        for routine in &routines {
            self.read_routine_instructions(routine)?;
        }
        Ok(BotDeletion {
            bot_id: id.into(),
            expected_revision,
            routine_ids,
            instructions,
            state_lock: Some(state_lock),
            _routine_locks: routine_locks,
        })
    }

    pub(crate) fn record_bot_deletion(
        &self,
        deletion: &mut BotDeletion,
        session_roots: &[String],
        session_ids: &[String],
        swarm: Option<(&str, bool)>,
    ) -> Result<PendingBotDeletion> {
        let intent = PendingBotDeletion {
            bot_id: deletion.bot_id.clone(),
            expected_revision: deletion.expected_revision,
            session_roots: session_roots.to_vec(),
            session_ids: session_ids.to_vec(),
            swarm_id: swarm.map(|(id, _)| id.to_owned()),
            disbanded_swarm: swarm.is_some_and(|(_, disbanded)| disbanded),
            instruction_paths: deletion.instructions.iter().cloned().collect(),
        };
        let intent = self.update_locked(|state| {
            let bot = find_bot_mut(state, &intent.bot_id)?;
            if bot.config.revision != intent.expected_revision {
                return Err(Error::Config(format!(
                    "Bot configuration revision changed from {} to {}",
                    intent.expected_revision, bot.config.revision
                )));
            }
            if let Some(pending) = &state.pending_bot_deletion
                && pending != &intent
            {
                return Err(Error::Config(
                    "another Bot deletion is awaiting recovery".into(),
                ));
            }
            state.pending_bot_deletion = Some(intent.clone());
            Ok(intent.clone())
        })?;
        deletion.release_state_lock();
        Ok(intent)
    }

    pub(crate) fn pending_bot_deletion(&self) -> Result<Option<PendingBotDeletion>> {
        Ok(self.lock_state()?.pending_bot_deletion.clone())
    }

    pub(crate) fn clear_bot_deletion(&self, bot_id: &str) -> Result<()> {
        let state_lock = open_private_lock(self.state_dir.join(STATE_LOCK_FILE))?;
        state_lock.lock()?;
        self.update_locked(|state| {
            let pending = state
                .pending_bot_deletion
                .as_ref()
                .ok_or_else(|| Error::Config("Bot deletion recovery is not pending".into()))?;
            if pending.bot_id != bot_id {
                return Err(Error::Config(
                    "a different Bot deletion is awaiting recovery".into(),
                ));
            }
            state.pending_bot_deletion = None;
            Ok(())
        })
    }

    pub(crate) fn cleanup_bot_deletion_files(&self, intent: &PendingBotDeletion) -> Result<()> {
        for path in &intent.instruction_paths {
            if path.parent() != Some(self.routines_dir.as_path()) {
                return Err(Error::Config(
                    "pending Bot deletion instructions left the private routine directory".into(),
                ));
            }
            remove_if_present(path)?;
        }
        Ok(())
    }

    pub(crate) fn delete_bot(&self, deletion: BotDeletion) -> Result<BotRecord> {
        let BotDeletion {
            bot_id,
            expected_revision,
            routine_ids,
            instructions,
            state_lock,
            _routine_locks,
        } = deletion;
        let state_lock = match state_lock {
            Some(state_lock) => state_lock,
            None => {
                let state_lock = open_private_lock(self.state_dir.join(STATE_LOCK_FILE))?;
                state_lock.lock()?;
                state_lock
            }
        };
        let bot = self.update_locked(|state| {
            if let Some(pending) = &state.pending_bot_deletion
                && (pending.bot_id != bot_id || pending.expected_revision != expected_revision)
            {
                return Err(Error::Config(
                    "a different Bot deletion is awaiting recovery".into(),
                ));
            }
            let index = state
                .bots
                .iter()
                .position(|bot| bot.id == bot_id)
                .ok_or_else(|| Error::Config(format!("unknown Bot `{bot_id}`")))?;
            let bot = &state.bots[index];
            if bot.handle == MOBIUS_HANDLE {
                return Err(Error::Config(
                    "the built-in @mobius Bot cannot be deleted".into(),
                ));
            }
            if bot.config.revision != expected_revision {
                return Err(Error::Config(format!(
                    "Bot configuration revision changed from {expected_revision} to {}",
                    bot.config.revision
                )));
            }
            let current_routine_ids = state
                .routines
                .iter()
                .filter(|routine| routine.bot_id == bot_id)
                .map(|routine| routine.id.clone())
                .collect::<BTreeSet<_>>();
            if current_routine_ids != routine_ids {
                return Err(Error::Config(
                    "Bot routine state changed during deletion".into(),
                ));
            }
            let current_instructions = state
                .routines
                .iter()
                .filter(|routine| routine.bot_id == bot_id)
                .map(|routine| routine.instructions.clone())
                .collect::<BTreeSet<_>>();
            if current_instructions != instructions {
                return Err(Error::Config(
                    "Bot routine instructions changed during deletion".into(),
                ));
            }
            state.routines.retain(|routine| routine.bot_id != bot_id);
            state.runs.retain(|run| run.bot_id != bot_id);
            Ok(state.bots.remove(index))
        })?;
        drop(_routine_locks);
        drop(state_lock);
        for path in &instructions {
            let _ = remove_if_present(path);
        }
        Ok(bot)
    }

    /// Compensates a failed create-and-attach transaction before the Bot is exposed.
    pub(crate) fn rollback_created_bot(
        &self,
        id: &str,
        expected_revision: u64,
    ) -> Result<BotRecord> {
        self.update(|state| {
            let index = state
                .bots
                .iter()
                .position(|bot| bot.id == id)
                .ok_or_else(|| Error::Config(format!("unknown Bot `{id}`")))?;
            let bot = &state.bots[index];
            if bot.config.revision != expected_revision
                || state.routines.iter().any(|routine| routine.bot_id == id)
                || state.runs.iter().any(|run| run.bot_id == id)
            {
                return Err(Error::Config(
                    "new Bot changed before its failed Swarm join could be rolled back".into(),
                ));
            }
            Ok(state.bots.remove(index))
        })
    }

    pub(crate) fn restore_bot(&self, bot: BotRecord) -> Result<()> {
        self.update(|state| {
            let id = bot.id.clone();
            let current = find_bot_mut(state, &id)?;
            if current.handle != bot.handle {
                return Err(Error::Config("Bot handles are immutable".into()));
            }
            *current = bot;
            Ok(())
        })
    }

    pub(crate) fn bots(&self) -> Result<Vec<BotRecord>> {
        Ok(self.lock_state()?.bots.clone())
    }

    pub(crate) fn bot(&self, id: &str) -> Result<BotRecord> {
        self.lock_state()?
            .bots
            .iter()
            .find(|bot| bot.id == id)
            .cloned()
            .ok_or_else(|| Error::Config(format!("unknown Bot `{id}`")))
    }

    #[cfg(test)]
    pub(crate) fn mobius(&self) -> Result<BotRecord> {
        self.lock_state()?
            .bots
            .iter()
            .find(|bot| bot.handle == MOBIUS_HANDLE)
            .cloned()
            .ok_or_else(|| Error::Config("the built-in @mobius Bot is missing".into()))
    }

    /// Writes and registers one Bot-owned routine.
    pub(crate) fn create_routine(
        &self,
        bot_id: &str,
        workspace: &Path,
        instructions: &str,
        schedule: RoutineSchedule,
        ends_at: Option<i64>,
    ) -> Result<StoredRoutine> {
        let workspace = validate_workspace(workspace)?;
        validate_instructions(instructions)?;
        validate_schedule(&schedule, ends_at)?;
        let instructions = instructions.trim();
        let path = self.new_instruction_path();
        write_private_instructions(&self.routines_dir, &path, instructions.as_bytes())?;
        let result = self.update(|state| {
            find_bot_mut(state, bot_id)?;
            let now = Utc::now().timestamp();
            let mut routine = StoredRoutine {
                id: Uuid::new_v4().to_string(),
                bot_id: bot_id.into(),
                workspace,
                instructions: path.clone(),
                schedule,
                ends_at,
                enabled: true,
                next_run_at: Some(now),
                last_matched_minute: None,
            };
            routine.reset_next_run(now)?;
            state.routines.push(routine.clone());
            Ok(routine)
        });
        match result {
            Ok(routine) => Ok(routine),
            Err(error) => match std::fs::remove_file(&path) {
                Ok(()) => Err(error),
                Err(rollback) => Err(Error::Config(format!(
                    "{error}; removing the unregistered routine failed: {rollback}"
                ))),
            },
        }
    }

    pub(crate) fn routine_records(&self, bot_id: Option<&str>, now: i64) -> Result<Vec<Routine>> {
        let state = self.lock_state()?;
        state
            .routines
            .iter()
            .filter(|stored| bot_id.is_none_or(|bot_id| stored.bot_id == bot_id))
            .map(|stored| self.routine_record_from(stored, now))
            .collect()
    }

    pub(crate) fn routine_record(&self, id: &str, now: i64) -> Result<Routine> {
        let state = self.lock_state()?;
        let stored = state
            .routines
            .iter()
            .find(|routine| routine.id == id)
            .ok_or_else(|| Error::Config(format!("unknown routine `{id}`")))?;
        self.routine_record_from(stored, now)
    }

    pub(crate) fn has_active_routines(&self, now: i64) -> Result<bool> {
        let state = self.lock_state()?;
        Ok(state
            .routines
            .iter()
            .any(|routine| routine.enabled && !routine.is_finished(now))
            || state
                .runs
                .iter()
                .any(|run| run.status == RoutineRunStatus::Running))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one routine replacement keeps its validated fields explicit"
    )]
    pub(crate) fn update_routine(
        &self,
        id: &str,
        bot_id: &str,
        workspace: &Path,
        instructions: &str,
        schedule: RoutineSchedule,
        ends_at: Option<i64>,
        enabled: bool,
    ) -> Result<StoredRoutine> {
        self.bot(bot_id)?;
        let workspace = validate_workspace(workspace)?;
        validate_instructions(instructions)?;
        validate_schedule(&schedule, ends_at)?;
        let existing = self.routine(id)?;
        let Some(_lock) = self.try_routine_lock(&existing.id)? else {
            return Err(Error::Config(format!(
                "routine {} is currently running",
                existing.id
            )));
        };
        let path = self.new_instruction_path();
        write_private_instructions(&self.routines_dir, &path, instructions.trim().as_bytes())?;
        let result = self.update(|state| {
            find_bot_mut(state, bot_id)?;
            let index = resolve_routine(&state.routines, &existing.id)?;
            let stored = &mut state.routines[index];
            stored.bot_id = bot_id.into();
            stored.workspace = workspace;
            stored.instructions.clone_from(&path);
            stored.schedule = schedule;
            stored.ends_at = ends_at;
            stored.enabled = enabled;
            stored.reset_next_run(Utc::now().timestamp())?;
            Ok(state.routines[index].clone())
        });
        match result {
            Ok(routine) => {
                let _ = remove_if_present(&existing.instructions);
                Ok(routine)
            }
            Err(error) => match remove_if_present(&path) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(Error::Config(format!(
                    "{error}; removing the unregistered routine instructions failed: {cleanup}"
                ))),
            },
        }
    }

    pub(crate) fn prepare_routine_deletion(&self, id: &str) -> Result<RoutineDeletion> {
        let state_lock = open_private_lock(self.state_dir.join(STATE_LOCK_FILE))?;
        state_lock.lock()?;
        let routine = self.routine(id)?;
        let Some(lock) = self.try_routine_lock(&routine.id)? else {
            return Err(Error::Config(format!(
                "routine {} is currently running",
                routine.id
            )));
        };
        self.read_routine_instructions(&routine)?;
        let state = self.lock_state()?;
        let index = resolve_routine(&state.routines, &routine.id)?;
        let routine = &state.routines[index];
        let session_ids = state
            .runs
            .iter()
            .filter(|run| run.routine_id == routine.id)
            .filter_map(|run| run.session_id.clone())
            .collect();
        Ok(RoutineDeletion {
            routine_id: routine.id.clone(),
            session_ids,
            instructions: routine.instructions.clone(),
            _state_lock: state_lock,
            _lock: lock,
        })
    }

    pub(crate) fn delete_routine(&self, deletion: RoutineDeletion) -> Result<StoredRoutine> {
        let RoutineDeletion {
            routine_id,
            session_ids,
            instructions,
            _state_lock,
            _lock,
        } = deletion;
        let deleted = self.update_locked(|state| {
            let index = resolve_routine(&state.routines, &routine_id)?;
            if state.routines[index].instructions != instructions {
                return Err(Error::Config(
                    "routine instructions changed during deletion".into(),
                ));
            }
            let current_session_ids = state
                .runs
                .iter()
                .filter(|run| run.routine_id == routine_id)
                .filter_map(|run| run.session_id.clone())
                .collect::<BTreeSet<_>>();
            if current_session_ids != session_ids {
                return Err(Error::Config(
                    "routine run state changed during deletion".into(),
                ));
            }
            let deleted = state.routines.remove(index);
            state.runs.retain(|run| run.routine_id != deleted.id);
            Ok(deleted)
        })?;
        drop(_lock);
        drop(_state_lock);
        let _ = remove_if_present(&instructions);
        Ok(deleted)
    }

    pub(crate) fn routine(&self, id: &str) -> Result<StoredRoutine> {
        let state = self.lock_state()?;
        Ok(state.routines[resolve_routine(&state.routines, id)?].clone())
    }

    pub(crate) fn routine_input(&self, id: &str) -> Result<(StoredRoutine, String)> {
        let state = self.lock_state()?;
        let routine = state
            .routines
            .iter()
            .find(|routine| routine.id == id)
            .ok_or_else(|| Error::Config(format!("unknown routine `{id}`")))?;
        let instructions = self.read_routine_instructions(routine)?;
        let input = format!("{ROUTINE_SUBMISSION_PREFIX}\n\n{instructions}");
        Ok((routine.clone(), input))
    }

    fn routine_record_from(&self, stored: &StoredRoutine, now: i64) -> Result<Routine> {
        Ok(Routine {
            id: stored.id.clone(),
            bot_id: stored.bot_id.clone(),
            workspace: stored.workspace.clone(),
            instructions: self.read_routine_instructions(stored)?,
            schedule: stored.schedule.clone(),
            ends_at: stored.ends_at,
            enabled: stored.enabled,
            finished: stored.is_finished(now),
            next_run_at: stored.next_run_at(now),
        })
    }

    fn read_routine_instructions(&self, routine: &StoredRoutine) -> Result<String> {
        let path = std::fs::canonicalize(&routine.instructions)?;
        if !path.is_file() || path.parent() != Some(self.routines_dir.as_path()) {
            return Err(Error::Config(
                "routine instructions must remain inside the private gateway routine directory"
                    .into(),
            ));
        }
        let mut file = File::open(&path)?;
        let opened = file.metadata()?;
        let verified = std::fs::canonicalize(&routine.instructions)?;
        let current = std::fs::metadata(&verified)?;
        if verified != path || !same_file(&opened, &current) {
            return Err(Error::Config(
                "routine instructions changed while they were being opened".into(),
            ));
        }
        let limit = u64::try_from(MAX_ROUTINE_INSTRUCTIONS_BYTES).unwrap_or(u64::MAX);
        let mut bytes = Vec::new();
        std::io::Read::by_ref(&mut file)
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_ROUTINE_INSTRUCTIONS_BYTES {
            return Err(Error::Config(format!(
                "routine instructions exceed the {MAX_ROUTINE_INSTRUCTIONS_BYTES}-byte input limit"
            )));
        }
        let input = String::from_utf8(bytes)
            .map_err(|_| Error::Config("routine instructions are not valid UTF-8".into()))?;
        validate_instructions(&input)?;
        Ok(input)
    }

    /// Reserves due routines and records their invocations atomically.
    pub(crate) fn take_due(&self, now: i64) -> Result<Vec<(String, ActiveRoutineRun)>> {
        let state_lock = open_private_lock(self.state_dir.join(STATE_LOCK_FILE))?;
        state_lock.lock()?;
        if self.pending_bot_deletion()?.is_some() {
            return Ok(Vec::new());
        }
        let minute = now.div_euclid(60);
        self.update_locked(|state| {
            let mut due = Vec::new();
            for index in 0..state.routines.len() {
                let routine = &state.routines[index];
                if !routine.enabled || routine.is_finished(now) {
                    continue;
                }
                let should_run = match routine.schedule.kind {
                    RoutineScheduleKind::Once | RoutineScheduleKind::Interval => {
                        routine.next_run_at.is_some_and(|next| next <= now)
                    }
                    RoutineScheduleKind::Cron => {
                        if routine.last_matched_minute == Some(minute) {
                            false
                        } else {
                            let expression =
                                routine.schedule.expression.as_deref().ok_or_else(|| {
                                    Error::Config("cron schedule is missing its expression".into())
                                })?;
                            let schedule = Cron::from_str(expression).map_err(|error| {
                                Error::Config(format!("invalid persisted cron schedule: {error}"))
                            })?;
                            let time_zone = routine
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
                let routine = state.routines[index].clone();
                {
                    let stored = &mut state.routines[index];
                    stored.last_matched_minute = Some(minute);
                    match stored.schedule.kind {
                        RoutineScheduleKind::Once => stored.next_run_at = None,
                        RoutineScheduleKind::Interval => stored.advance_interval(now)?,
                        RoutineScheduleKind::Cron => {}
                    }
                }
                let Some(lock) = self.try_routine_lock(&routine.id)? else {
                    append_run(
                        state,
                        new_run(
                            &routine,
                            RoutineRunStatus::Skipped,
                            Some("the previous invocation is still running".into()),
                        ),
                    );
                    continue;
                };
                let run = new_run(&routine, RoutineRunStatus::Running, None);
                append_run(state, run.clone());
                due.push((
                    routine.id,
                    ActiveRoutineRun {
                        run_id: run.id,
                        session_id: run
                            .session_id
                            .expect("a running routine reserves its session ID"),
                        _lock: lock,
                    },
                ));
            }
            Ok(due)
        })
    }

    /// Starts an overlap-locked invocation or records an overlap skip.
    pub(crate) fn begin_run(&self, id: &str) -> Result<BeginRun> {
        self.begin_run_inner(id, || {})
    }

    fn begin_run_inner(&self, id: &str, after_resolve: impl FnOnce()) -> Result<BeginRun> {
        let routine = self.stored_routine(id)?;
        after_resolve();
        let Some(lock) = self.try_routine_lock(&routine.id)? else {
            return self.update(|state| {
                let routine = state
                    .routines
                    .iter()
                    .find(|stored| stored.id == routine.id)
                    .cloned()
                    .ok_or_else(|| Error::Config(format!("unknown routine `{}`", routine.id)))?;
                if !state.runs.iter().any(|run| {
                    run.routine_id == routine.id && run.status == RoutineRunStatus::Running
                }) {
                    return Err(Error::Config(format!(
                        "routine {} is currently being modified",
                        routine.id
                    )));
                }
                append_run(
                    state,
                    new_run(
                        &routine,
                        RoutineRunStatus::Skipped,
                        Some("the previous invocation is still running".into()),
                    ),
                );
                Ok(BeginRun::Skipped)
            });
        };
        let routine = self.stored_routine(&routine.id)?;
        let run = new_run(&routine, RoutineRunStatus::Running, None);
        self.update(|state| {
            append_run(state, run.clone());
            Ok(())
        })?;
        Ok(BeginRun::Started(ActiveRoutineRun {
            run_id: run.id,
            session_id: run
                .session_id
                .expect("a running routine reserves its session ID"),
            _lock: lock,
        }))
    }

    /// Completes a running invocation and releases its overlap lock.
    pub(crate) fn finish_run(
        &self,
        run: ActiveRoutineRun,
        status: RoutineRunStatus,
        message: Option<String>,
    ) -> Result<RoutineRun> {
        if status == RoutineRunStatus::Running {
            return Err(Error::Config(
                "a completed routine run cannot remain running".into(),
            ));
        }
        let state_lock = open_private_lock(self.state_dir.join(STATE_LOCK_FILE))?;
        state_lock.lock()?;
        self.update_locked(|state| {
            let stored = find_run_mut(state, &run.run_id)?;
            stored.finished_at = Some(Utc::now().timestamp());
            stored.status = status;
            stored.message = message;
            Ok(stored.clone())
        })
    }

    /// Returns newest-first run history for one routine.
    pub(crate) fn history(&self, id: Option<&str>) -> Result<Vec<RoutineRun>> {
        let state = self.lock_state()?;
        let routine_id = id
            .map(|id| resolve_history_routine(&state, id))
            .transpose()?;
        Ok(state
            .runs
            .iter()
            .rev()
            .filter(|run| routine_id.as_ref().is_none_or(|id| &run.routine_id == id))
            .cloned()
            .collect())
    }

    pub(crate) fn run(&self, id: &str) -> Result<RoutineRun> {
        self.lock_state()?
            .runs
            .iter()
            .find(|run| run.id == id)
            .cloned()
            .ok_or_else(|| Error::Config(format!("unknown routine run `{id}`")))
    }

    pub(crate) fn delete_run(&self, id: &str) -> Result<RoutineRun> {
        self.update(|state| {
            let index = state
                .runs
                .iter()
                .position(|run| run.id == id)
                .ok_or_else(|| Error::Config(format!("unknown routine run `{id}`")))?;
            if state.runs[index].status == RoutineRunStatus::Running {
                return Err(Error::Config(format!(
                    "routine run {id} is currently running"
                )));
            }
            Ok(state.runs.remove(index))
        })
    }

    fn stored_routine(&self, id: &str) -> Result<StoredRoutine> {
        self.lock_state()?
            .routines
            .iter()
            .find(|routine| routine.id == id)
            .cloned()
            .ok_or_else(|| Error::Config(format!("unknown routine `{id}`")))
    }

    fn try_routine_lock(&self, id: &str) -> Result<Option<File>> {
        let file = open_private_lock(self.state_dir.join(format!("routine-{id}.lock")))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    fn new_instruction_path(&self) -> PathBuf {
        self.routines_dir
            .join(format!("{}.md", Uuid::new_v4().as_hyphenated()))
    }

    fn update<T>(&self, mutate: impl FnOnce(&mut BotState) -> Result<T>) -> Result<T> {
        let _file_lock = open_private_lock(self.state_dir.join(STATE_LOCK_FILE))?;
        _file_lock.lock()?;
        self.update_locked(|state| {
            if state.pending_bot_deletion.is_some() {
                return Err(Error::Config(
                    "Bot deletion recovery must finish before changing Bot state".into(),
                ));
            }
            mutate(state)
        })
    }

    fn update_locked<T>(&self, mutate: impl FnOnce(&mut BotState) -> Result<T>) -> Result<T> {
        let mut state = self.lock_state()?;
        let mut next = state.clone();
        let result = mutate(&mut next)?;
        validate_state(&next, &self.routines_dir)?;
        self.save(&next)?;
        *state = next;
        Ok(result)
    }

    fn save(&self, state: &BotState) -> Result<()> {
        let contents = serde_json::to_vec_pretty(state)?;
        if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_STATE_BYTES {
            return Err(Error::Config("Bot state is too large".into()));
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

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, BotState>> {
        self.state
            .lock()
            .map_err(|_| Error::Config("Bot state lock is poisoned".into()))
    }
}

fn validate_session_id(session_id: &str) -> Result<()> {
    if session_id.trim().is_empty() {
        return Err(Error::Config("routine session ID cannot be empty".into()));
    }
    Ok(())
}

fn next_handle(state: &BotState, name: &str) -> String {
    let mut base = String::new();
    let mut separator = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !base.is_empty() && base.len() < MAX_HANDLE_BYTES {
                base.push('-');
            }
            separator = false;
            if base.len() < MAX_HANDLE_BYTES {
                base.push(character.to_ascii_lowercase());
            }
        } else {
            separator = true;
        }
    }
    while base.ends_with('-') {
        base.pop();
    }
    if base.is_empty() {
        base.push_str("bot");
    }
    if base != USER_HANDLE && !state.bots.iter().any(|bot| bot.handle == base) {
        return base;
    }
    for index in 2_u64.. {
        let suffix = format!("-{index}");
        let prefix_len = MAX_HANDLE_BYTES.saturating_sub(suffix.len());
        let prefix = base[..base.len().min(prefix_len)].trim_end_matches('-');
        let candidate = format!("{prefix}{suffix}");
        if candidate != USER_HANDLE && !state.bots.iter().any(|bot| bot.handle == candidate) {
            return candidate;
        }
    }
    unreachable!("the Bot handle suffix space is unbounded")
}

fn next_tint(state: &BotState) -> ProviderTint {
    BOT_TINTS
        .iter()
        .copied()
        .find(|tint| state.bots.iter().all(|bot| bot.tint != *tint))
        .unwrap_or(BOT_TINTS[state.bots.len() % BOT_TINTS.len()])
}

fn validate_handle(handle: &str) -> Result<String> {
    let handle = handle.trim();
    if handle.is_empty()
        || handle.len() > MAX_HANDLE_BYTES
        || !handle.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(Error::Config(format!(
            "Bot handle must be 1–{MAX_HANDLE_BYTES} lowercase ASCII letters, digits, dashes, or underscores"
        )));
    }
    if handle == USER_HANDLE {
        return Err(Error::Config("Bot handle `user` is reserved".into()));
    }
    Ok(handle.into())
}

fn validate_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(Error::Config(format!(
            "Bot name must be 1–{MAX_NAME_BYTES} bytes"
        )));
    }
    Ok(name.into())
}

fn validate_description(description: &str) -> Result<String> {
    let description = description.trim();
    if description.is_empty() || description.len() > MAX_DESCRIPTION_BYTES {
        return Err(Error::Config(format!(
            "Bot description must be 1–{MAX_DESCRIPTION_BYTES} bytes"
        )));
    }
    Ok(description.into())
}

fn validate_workspace(workspace: &Path) -> Result<PathBuf> {
    let workspace = std::fs::canonicalize(workspace)?;
    if !workspace.is_dir() {
        return Err(Error::Config(
            "routine workspace must be a directory".into(),
        ));
    }
    Ok(workspace)
}

fn validate_stored_workspace(workspace: &Path) -> Result<()> {
    if !workspace.is_absolute()
        || workspace
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(Error::Config(
            "persisted routine workspace must be an absolute normalized path".into(),
        ));
    }
    Ok(())
}

fn validate_routine_id_prefix(id: &str) -> Result<()> {
    if id.is_empty() || id.chars().any(char::is_whitespace) {
        return Err(Error::Config("routine ID cannot be empty".into()));
    }
    Ok(())
}

fn validate_instructions(instructions: &str) -> Result<()> {
    let instructions = instructions.trim();
    if instructions.is_empty() {
        return Err(Error::Config("routine instructions cannot be empty".into()));
    }
    if instructions.len() > MAX_ROUTINE_INSTRUCTIONS_BYTES {
        return Err(Error::Config(format!(
            "routine instructions exceed the {MAX_ROUTINE_INSTRUCTIONS_BYTES}-byte input limit"
        )));
    }
    Ok(())
}

fn validate_schedule(schedule: &RoutineSchedule, ends_at: Option<i64>) -> Result<()> {
    let populated = [
        schedule.at.is_some(),
        schedule.every_seconds.is_some(),
        schedule.expression.is_some(),
    ]
    .into_iter()
    .filter(|populated| *populated)
    .count();
    match schedule.kind {
        RoutineScheduleKind::Once
            if populated == 1 && schedule.at.is_some() && schedule.time_zone.is_none() =>
        {
            if ends_at.is_some_and(|ends_at| schedule.at.is_some_and(|at| at > ends_at)) {
                return Err(Error::Config(
                    "a once schedule cannot end before it runs".into(),
                ));
            }
        }
        RoutineScheduleKind::Interval
            if populated == 1
                && schedule.every_seconds.is_some()
                && schedule.time_zone.is_none() =>
        {
            if schedule.every_seconds.unwrap_or_default() < 60 {
                return Err(Error::Config("interval must be at least 60 seconds".into()));
            }
        }
        RoutineScheduleKind::Cron
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

fn validate_state(state: &BotState, routines_dir: &Path) -> Result<()> {
    if state.version != STATE_VERSION {
        return Err(Error::Config(format!(
            "unsupported Bot state version {}",
            state.version
        )));
    }
    let mut bot_ids = BTreeSet::new();
    let mut handles = BTreeSet::new();
    for bot in &state.bots {
        let parsed = Uuid::parse_str(&bot.id)
            .map_err(|_| Error::Config("invalid persisted Bot ID".into()))?;
        if parsed.to_string() != bot.id || !bot_ids.insert(bot.id.as_str()) {
            return Err(Error::Config("duplicate persisted Bot ID".into()));
        }
        if !handles.insert(bot.handle.as_str()) {
            return Err(Error::Config("duplicate persisted Bot handle".into()));
        }
        if validate_handle(&bot.handle)? != bot.handle
            || validate_name(&bot.name)? != bot.name
            || validate_description(&bot.description)? != bot.description
        {
            return Err(Error::Config(
                "persisted Bot identity is not normalized".into(),
            ));
        }
        if bot.config.revision == 0 {
            return Err(Error::Config(
                "persisted Bot revision must be positive".into(),
            ));
        }
        validate_agent_composition(&bot.config.config)?;
    }
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for routine in &state.routines {
        let parsed = Uuid::parse_str(&routine.id)
            .map_err(|_| Error::Config("invalid persisted routine ID".into()))?;
        if parsed.to_string() != routine.id || !ids.insert(routine.id.as_str()) {
            return Err(Error::Config("duplicate persisted routine ID".into()));
        }
        if !bot_ids.contains(routine.bot_id.as_str()) {
            return Err(Error::Config("persisted routine has no Bot".into()));
        }
        validate_stored_workspace(&routine.workspace)?;
        if !routine.instructions.is_absolute()
            || routine.instructions.parent() != Some(routines_dir)
            || !paths.insert(routine.instructions.as_path())
        {
            return Err(Error::Config(
                "persisted routine path is outside the private gateway routine directory".into(),
            ));
        }
        validate_schedule(&routine.schedule, routine.ends_at)?;
        if routine.next_run_at.is_some_and(|next| next <= 0) {
            return Err(Error::Config("invalid persisted routine next run".into()));
        }
    }
    let mut run_ids = BTreeSet::new();
    for run in &state.runs {
        if Uuid::parse_str(&run.id).is_err() || !run_ids.insert(run.id.as_str()) {
            return Err(Error::Config("invalid persisted routine run ID".into()));
        }
        if run.routine_id.is_empty() {
            return Err(Error::Config(
                "persisted routine run has no routine ID".into(),
            ));
        }
        if !bot_ids.contains(run.bot_id.as_str()) {
            return Err(Error::Config("persisted routine run has no Bot".into()));
        }
        if let Some(session_id) = &run.session_id {
            validate_session_id(session_id)?;
        }
    }
    if let Some(pending) = &state.pending_bot_deletion {
        let parsed = Uuid::parse_str(&pending.bot_id)
            .map_err(|_| Error::Config("invalid pending Bot deletion ID".into()))?;
        if parsed.to_string() != pending.bot_id || pending.expected_revision == 0 {
            return Err(Error::Config("invalid pending Bot deletion".into()));
        }
        if let Some(bot) = state.bots.iter().find(|bot| bot.id == pending.bot_id)
            && bot.config.revision != pending.expected_revision
        {
            return Err(Error::Config(
                "pending Bot deletion revision changed".into(),
            ));
        }
        for session_id in pending.session_roots.iter().chain(&pending.session_ids) {
            validate_session_id(session_id)?;
        }
        if pending
            .session_roots
            .iter()
            .any(|root| !pending.session_ids.contains(root))
        {
            return Err(Error::Config(
                "pending Bot deletion root is outside its session set".into(),
            ));
        }
        match (&pending.swarm_id, pending.disbanded_swarm) {
            (Some(id), _) if Uuid::parse_str(id).is_err() => {
                return Err(Error::Config(
                    "invalid pending Bot deletion swarm ID".into(),
                ));
            }
            (None, true) => {
                return Err(Error::Config(
                    "pending Bot deletion cannot disband an unknown swarm".into(),
                ));
            }
            _ => {}
        }
        if pending
            .instruction_paths
            .iter()
            .any(|path| !path.is_absolute() || path.parent() != Some(routines_dir))
        {
            return Err(Error::Config(
                "pending Bot deletion instructions are outside the private routine directory"
                    .into(),
            ));
        }
    }
    Ok(())
}

fn recover_interrupted_runs(state: &mut BotState) -> bool {
    let now = Utc::now().timestamp();
    let mut changed = false;
    for run in &mut state.runs {
        if run.status == RoutineRunStatus::Running {
            run.status = RoutineRunStatus::Failed;
            run.finished_at = Some(now);
            run.message = Some("the gateway stopped before this run completed".into());
            changed = true;
        }
    }
    changed
}

fn resolve_routine(routines: &[StoredRoutine], id: &str) -> Result<usize> {
    validate_routine_id_prefix(id)?;
    if let Some(index) = routines.iter().position(|routine| routine.id == id) {
        return Ok(index);
    }
    let mut matches = routines
        .iter()
        .enumerate()
        .filter(|(_, routine)| routine.id.starts_with(id));
    let (index, _) = matches
        .next()
        .ok_or_else(|| Error::Config(format!("unknown routine `{id}`")))?;
    if matches.next().is_some() {
        return Err(Error::Config(format!(
            "routine ID prefix `{id}` is ambiguous"
        )));
    }
    Ok(index)
}

fn resolve_history_routine(state: &BotState, id: &str) -> Result<String> {
    validate_routine_id_prefix(id)?;
    let mut ids = state
        .routines
        .iter()
        .map(|routine| routine.id.as_str())
        .chain(state.runs.iter().map(|run| run.routine_id.as_str()))
        .filter(|routine_id| routine_id.starts_with(id))
        .collect::<BTreeSet<_>>();
    if ids.contains(id) {
        return Ok(id.into());
    }
    let resolved = ids
        .pop_first()
        .ok_or_else(|| Error::Config(format!("unknown routine `{id}`")))?;
    if !ids.is_empty() {
        return Err(Error::Config(format!(
            "routine ID prefix `{id}` is ambiguous"
        )));
    }
    Ok(resolved.into())
}

fn new_run(
    routine: &StoredRoutine,
    status: RoutineRunStatus,
    message: Option<String>,
) -> RoutineRun {
    let now = Utc::now().timestamp();
    RoutineRun {
        id: Uuid::new_v4().to_string(),
        routine_id: routine.id.clone(),
        bot_id: routine.bot_id.clone(),
        started_at: now,
        finished_at: (status != RoutineRunStatus::Running).then_some(now),
        status,
        session_id: (status == RoutineRunStatus::Running).then(|| Uuid::new_v4().to_string()),
        message,
    }
}

fn append_run(state: &mut BotState, run: RoutineRun) {
    state.runs.push(run);
}

fn find_run_mut<'a>(state: &'a mut BotState, id: &str) -> Result<&'a mut RoutineRun> {
    state
        .runs
        .iter_mut()
        .find(|run| run.id == id)
        .ok_or_else(|| Error::Config(format!("unknown routine run `{id}`")))
}

fn find_bot_mut<'a>(state: &'a mut BotState, id: &str) -> Result<&'a mut BotRecord> {
    state
        .bots
        .iter_mut()
        .find(|bot| bot.id == id)
        .ok_or_else(|| Error::Config(format!("unknown Bot `{id}`")))
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

fn private_routines_dir(state_dir: &Path) -> Result<PathBuf> {
    let path = state_dir.join(ROUTINES_DIR);
    std::fs::create_dir_all(&path)?;
    let path = std::fs::canonicalize(path)?;
    if path.parent() != Some(state_dir) || !path.is_dir() {
        return Err(Error::Config(
            "gateway routine directory must be a real directory inside gateway state".into(),
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

fn write_private_instructions(directory: &Path, path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    #[cfg(unix)]
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(contents)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
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
