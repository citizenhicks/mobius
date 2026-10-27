//! Gateway-owned Bot profiles, routines, run history, and schedule matching.

use mobius::SessionId;
mod events;
mod storage;
#[cfg(test)]
pub(crate) use events::event_selector;
pub(crate) use events::{
    MAX_HOOK_ANCESTRY, PendingHookAction, TelemetryEvent, report_text, subscription_source,
};

use std::collections::BTreeSet;
use std::fs::{File, TryLockError};
use std::io::Read as _;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr as _;
use std::sync::Arc;

use chrono::{TimeZone as _, Timelike as _, Utc};
use chrono_tz::Tz;
use croner::Cron;
use mobius::protocol::MAX_MESSAGE_BYTES;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use self::storage::{BotStorage, CatalogStamp};
use crate::config::CanonicalWorkspace;
use crate::harness::ApprovalMode;
use crate::wire::{AgentComposition, HarnessConfig};
use crate::wire::{
    BotRecord, BotShape, HookData, HookEvent, HookSelector, HookSource, ProviderTint, Routine,
    RoutineAction, RoutineBinding, RoutineDefinition, RoutineRun, RoutineRunStatus,
    RoutineSchedule, RoutineScheduleKind, VersionedAgentConfig,
};
use crate::{Error, Result};

const STATE_VERSION: u32 = 10;
const STATE_FILE: &str = storage::STATE_FILE;
const STATE_LOCK_FILE: &str = "bots-state.lock";
const ROUTINES_DIR: &str = "routines";
const ROUTINE_SUBMISSION_PREFIX: &str =
    "# Routine\n\nThe instructions below relate to a routine task.";
const MAX_ROUTINE_INSTRUCTIONS_BYTES: usize =
    MAX_MESSAGE_BYTES - ROUTINE_SUBMISSION_PREFIX.len() - 2;
const MAX_STATE_BYTES: u64 = 1024 * 1024;
// Last Unix second in RFC 3339's four-digit calendar year range.
pub(crate) const MAX_SCHEDULE_TIMESTAMP: i64 = 253_402_300_799;
const MAX_HANDLE_BYTES: usize = 64;
/// Maximum UTF-8 byte length of a Bot display name.
pub const MAX_BOT_NAME_BYTES: usize = 128;
/// Maximum UTF-8 byte length of a Bot description.
pub const MAX_BOT_DESCRIPTION_BYTES: usize = 2 * 1024;
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

/// What a Bot update sets besides its agent configuration.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BotIdentity<'a> {
    pub(crate) name: &'a str,
    pub(crate) description: &'a str,
    pub(crate) tint: ProviderTint,
    pub(crate) shape: BotShape,
}

/// Gateway-wide persistent Bot profiles, routines, and run history.
pub(crate) struct BotStore {
    state_dir: PathBuf,
    routines_dir: PathBuf,
    storage: BotStorage,
    pub(crate) prepared: tokio::sync::Mutex<
        std::collections::BTreeMap<Arc<str>, Arc<crate::harness::mobius::assembly::PreparedBot>>,
    >,
    pub(crate) preparation_generation: std::sync::atomic::AtomicU64,
    /// The last parsed catalog, reused until any connection commits.
    cache: std::sync::Mutex<Option<(CatalogStamp, Arc<BotState>)>>,
    /// The gateway's current voice routes, which Bot records resolve their voice against.
    voices: std::sync::RwLock<Arc<crate::harness::mobius::provider_catalog::VoiceRoutes>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredRoutine {
    pub(crate) id: Arc<str>,
    pub(crate) bot_id: Arc<str>,
    pub(crate) workspace: PathBuf,
    pub(crate) instructions: PathBuf,
    pub(crate) bindings: Vec<StoredRoutineBinding>,
    pub(crate) enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredRoutineBinding {
    pub(crate) definition: RoutineBinding,
    pub(crate) next_due_at: Option<i64>,
    pub(crate) last_matched_minute: Option<i64>,
}

impl StoredRoutineBinding {
    fn new(definition: RoutineBinding, now: i64, resume: bool) -> Result<Self> {
        let mut binding = Self {
            definition,
            next_due_at: None,
            last_matched_minute: None,
        };
        binding.reset(now, resume)?;
        Ok(binding)
    }
    fn reset(&mut self, now: i64, resume: bool) -> Result<()> {
        self.last_matched_minute = None;
        self.next_due_at = match &self.definition.on {
            HookSelector::Schedule { schedule, .. } => match schedule.kind {
                RoutineScheduleKind::Once => schedule.at.filter(|at| !resume || *at > now),
                RoutineScheduleKind::Interval => Some(
                    now.checked_add(
                        i64::try_from(schedule.every_seconds.ok_or_else(|| {
                            Error::Config("interval schedule is missing its interval".into())
                        })?)
                        .map_err(|error| {
                            Error::Config(format!("interval schedule is too large: {error}"))
                        })?,
                    )
                    .ok_or_else(|| {
                        Error::Config("interval schedule overflows its timestamp".into())
                    })?,
                ),
                RoutineScheduleKind::Cron => Some(next_cron_occurrence(schedule, now, !resume)?),
            },
            HookSelector::Event { .. } => None,
        };
        Ok(())
    }
    fn next_at(&self, now: i64) -> Option<i64> {
        let HookSelector::Schedule { schedule, ends_at } = &self.definition.on else {
            return None;
        };
        let next = if schedule.kind == RoutineScheduleKind::Cron {
            self.next_due_at
                .or_else(|| next_cron_occurrence(schedule, now, false).ok())
        } else {
            self.next_due_at
        }?;
        ends_at.map_or(Some(next), |end| (next <= end).then_some(next))
    }
    fn advance(&mut self, now: i64) -> Result<()> {
        let HookSelector::Schedule { schedule, .. } = &self.definition.on else {
            return Ok(());
        };
        self.last_matched_minute = Some(now.div_euclid(60));
        self.next_due_at = match schedule.kind {
            RoutineScheduleKind::Once => None,
            RoutineScheduleKind::Cron => Some(next_cron_occurrence(schedule, now, false)?),
            RoutineScheduleKind::Interval => {
                let every = i64::try_from(
                    schedule
                        .every_seconds
                        .ok_or_else(|| Error::Config("interval is missing".into()))?,
                )
                .map_err(|error| Error::Config(format!("interval is too large: {error}")))?;
                let next = self
                    .next_due_at
                    .ok_or_else(|| Error::Config("interval has no next occurrence".into()))?;
                let missed = (now.saturating_sub(next) / every).saturating_add(1);
                Some(
                    next.checked_add(every.saturating_mul(missed))
                        .ok_or_else(|| Error::Config("interval timestamp overflows".into()))?,
                )
            }
        };
        Ok(())
    }
}

impl StoredRoutine {
    fn is_finished(&self, now: i64) -> bool {
        !self.bindings.is_empty()
            && self.bindings.iter().all(|binding| {
                matches!(binding.definition.on, HookSelector::Schedule { .. })
                    && binding.next_at(now).is_none()
            })
    }
    fn next_run_at(&self, now: i64) -> Option<i64> {
        if !self.enabled {
            return None;
        }
        self.bindings
            .iter()
            .filter_map(|binding| binding.next_at(now))
            .min()
    }
}

fn next_cron_occurrence(
    schedule: &RoutineSchedule,
    now: i64,
    inclusive_current_minute: bool,
) -> Result<i64> {
    let expression = schedule
        .expression
        .as_deref()
        .ok_or_else(|| Error::Config("cron schedule is missing its expression".into()))?;
    let cron = Cron::from_str(expression)
        .map_err(|error| Error::Config(format!("invalid persisted cron schedule: {error}")))?;
    let time_zone = schedule
        .time_zone
        .as_deref()
        .ok_or_else(|| Error::Config("cron schedule is missing its time zone".into()))?
        .parse::<Tz>()
        .map_err(|error| Error::Config(format!("invalid persisted cron time zone: {error}")))?;
    let utc_minute = Utc
        .timestamp_opt(now, 0)
        .single()
        .and_then(|time| time.with_second(0))
        .ok_or_else(|| Error::Config("cron timestamp is outside the supported range".into()))?;
    let local_time = utc_minute.with_timezone(&time_zone);
    let minimum = local_time.timestamp();
    let mut cursor = local_time;
    let mut inclusive = inclusive_current_minute;
    let mut previous = None;
    loop {
        let next = cron
            .find_next_occurrence(&cursor, inclusive)
            .map_err(|error| Error::Config(format!("invalid persisted cron schedule: {error}")))?;
        let timestamp = next.timestamp();
        if timestamp > minimum || (inclusive_current_minute && timestamp == minimum) {
            return Ok(timestamp);
        }
        if previous.is_some_and(|previous| timestamp <= previous) {
            return Err(Error::Config(
                "persisted cron schedule did not advance its timestamp".into(),
            ));
        }
        previous = Some(timestamp);
        cursor = next;
        inclusive = false;
    }
}

/// One scheduler tick derived from a single locked catalog snapshot.
pub(crate) struct RoutinePoll {
    pub(crate) active: bool,
    pub(crate) events: Vec<HookEvent>,
}

/// Result of reserving one task invocation.
pub(crate) enum BeginRun {
    Started(ActiveRoutineRun),
    Skipped,
    /// This command already reserved or skipped its invocation.
    AlreadyRecorded,
}

/// A durable running invocation whose file lock is held until completion.
pub(crate) struct ActiveRoutineRun {
    run_id: Arc<str>,
    session_id: SessionId,
    _lock: RoutineLock,
}

#[derive(Debug)]
struct RoutineLock(File);

impl Drop for RoutineLock {
    fn drop(&mut self) {
        // Closing alone leaves the lock held while a fork inherits the file descriptor.
        if let Err(error) = self.0.unlock() {
            tracing::warn!(%error, "routine lock release failed");
        }
    }
}

/// Validated Bot deletion whose routine locks stay held through gateway cleanup.
#[derive(Debug)]
pub(crate) struct BotDeletion {
    bot_id: Arc<str>,
    expected_revision: u64,
    routine_ids: BTreeSet<Arc<str>>,
    instructions: BTreeSet<PathBuf>,
    state_lock: Option<File>,
    _routine_locks: Vec<RoutineLock>,
}

impl BotDeletion {
    pub(crate) fn release_state_lock(&mut self) {
        drop(self.state_lock.take());
    }
}

/// Validated routine deletion whose lock stays held through gateway cleanup.
#[derive(Debug)]
pub(crate) struct RoutineDeletion {
    routine_id: Arc<str>,
    session_ids: BTreeSet<SessionId>,
    instructions: PathBuf,
    _state_lock: File,
    _lock: RoutineLock,
}

/// Durable forward-recovery record for a cross-owner Bot cascade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingBotDeletion {
    pub(crate) bot_id: Arc<str>,
    pub(crate) expected_revision: u64,
    pub(crate) session_roots: Vec<SessionId>,
    pub(crate) session_ids: Vec<SessionId>,
    instruction_paths: Vec<PathBuf>,
}

impl RoutineDeletion {
    pub(crate) fn session_ids(&self) -> &BTreeSet<SessionId> {
        &self.session_ids
    }
}

impl ActiveRoutineRun {
    pub(crate) fn id(&self) -> &str {
        &self.run_id
    }
    pub(crate) fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BotState {
    version: u32,
    bots: Vec<StoredBot>,
    routines: Vec<StoredRoutine>,
    pending_bot_deletion: Option<PendingBotDeletion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "PersistedBot")]
struct StoredBot {
    id: Arc<str>,
    handle: Arc<str>,
    name: Arc<str>,
    description: Arc<str>,
    tint: ProviderTint,
    shape: BotShape,
    config: Arc<VersionedAgentConfig>,
    /// Parsed once from `config` when it was validated or read from disk.
    #[serde(skip_serializing)]
    approval: ApprovalMode,
}

/// A möbius Bot record with the composition and approval policy it was validated to.
pub(crate) struct RuntimeBot {
    pub(crate) record: Arc<BotRecord>,
    pub(crate) composition: Arc<AgentComposition>,
    pub(crate) approval_policy: mobius::backend::sandbox::ApprovalPolicy,
}

/// One Bot as written to disk, before its composition is checked.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedBot {
    id: Arc<str>,
    handle: Arc<str>,
    name: Arc<str>,
    description: Arc<str>,
    tint: ProviderTint,
    shape: BotShape,
    config: Arc<VersionedAgentConfig>,
}

impl TryFrom<PersistedBot> for StoredBot {
    type Error = Error;

    fn try_from(bot: PersistedBot) -> Result<Self> {
        let PersistedBot {
            id,
            handle,
            name,
            description,
            tint,
            shape,
            config,
        } = bot;
        let approval = crate::harness::validate_stored(&config.config)?;
        Ok(Self {
            id,
            handle,
            name,
            description,
            tint,
            shape,
            config,
            approval,
        })
    }
}

impl StoredBot {
    fn record(
        &self,
        voices: &crate::harness::mobius::provider_catalog::VoiceRoutes,
    ) -> Result<BotRecord> {
        Ok(BotRecord {
            conversation_session_id: SessionId::new(conversation_session_id(&self.id))?,
            id: Arc::clone(&self.id),
            handle: Arc::clone(&self.handle),
            name: Arc::clone(&self.name),
            description: Arc::clone(&self.description),
            tint: self.tint,
            shape: self.shape,
            harness: self.config.config.kind(),
            config: Arc::clone(&self.config),
            capabilities: crate::harness::bot_capabilities(&self.config.config, voices)?,
            routine_interaction_policy: self.approval.routine_interaction_policy(),
        })
    }
}

#[cfg(test)]
impl From<&BotRecord> for StoredBot {
    fn from(bot: &BotRecord) -> Self {
        Self {
            id: Arc::clone(&bot.id),
            handle: Arc::clone(&bot.handle),
            name: Arc::clone(&bot.name),
            description: Arc::clone(&bot.description),
            tint: bot.tint,
            shape: bot.shape,
            config: Arc::clone(&bot.config),
            approval: crate::harness::validate_stored(&bot.config.config)
                .expect("valid test composition"),
        }
    }
}

impl Default for BotState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            bots: Vec::new(),
            routines: Vec::new(),
            pending_bot_deletion: None,
        }
    }
}

impl BotStore {
    pub(crate) fn attach_telemetry_notify(
        &self,
        notify: &std::sync::Arc<tokio::sync::Notify>,
        activity: &std::sync::Arc<crate::host::WorkActivity>,
    ) {
        self.storage.attach_telemetry_notify(notify, activity);
    }

    /// Opens or creates owner-only Bot state.
    pub(crate) fn open(state_dir: &Path) -> Result<Self> {
        let state_dir = std::fs::canonicalize(state_dir)?;
        let routines_dir = private_routines_dir(&state_dir)?;
        let path = state_dir.join(STATE_FILE);
        let (storage, persisted) = BotStorage::open(&path)?;
        let store = Self {
            state_dir,
            routines_dir,
            storage,
            prepared: tokio::sync::Mutex::default(),
            preparation_generation: std::sync::atomic::AtomicU64::default(),
            cache: std::sync::Mutex::default(),
            voices: std::sync::RwLock::default(),
        };
        let state = store.fresh_state()?;
        if persisted && !state.bots.iter().any(|bot| &*bot.handle == MOBIUS_HANDLE) {
            return Err(Error::Config(
                "persisted Bot state has no built-in @mobius Bot".into(),
            ));
        }
        let bot_ids = state
            .bots
            .iter()
            .map(|bot| &*bot.id)
            .collect::<BTreeSet<_>>();
        store.storage.validate_run_owners(&bot_ids)?;
        Ok(store)
    }

    /// Resolves later Bot records against the gateway's current voice routes.
    pub(crate) fn set_voice_routes(
        &self,
        voices: crate::harness::mobius::provider_catalog::VoiceRoutes,
    ) -> Result<()> {
        *self
            .voices
            .write()
            .map_err(crate::poisoned("Bot voice routes"))? = Arc::new(voices);
        Ok(())
    }

    fn voice_routes(&self) -> Result<Arc<crate::harness::mobius::provider_catalog::VoiceRoutes>> {
        Ok(Arc::clone(
            &*self
                .voices
                .read()
                .map_err(crate::poisoned("Bot voice routes"))?,
        ))
    }

    /// Creates the ordinary built-in Bot only before Bot state has ever existed.
    pub(crate) fn seed_default(
        &self,
        defaults: &VersionedAgentConfig<AgentComposition>,
    ) -> Result<Option<BotRecord>> {
        let _file_lock = self.lock_state()?;
        if self.storage.load_catalog()?.is_some() {
            return Ok(None);
        }
        let config = defaults.config.clone(); // rules-allow: callers keep their borrowed defaults; the seeded Bot owns its config
        let approval = ApprovalMode::Mobius(crate::config::configured_approval_policy(
            &config.middleware,
        )?);
        let mut state = BotState::default();
        let bot = StoredBot {
            id: Uuid::new_v4().to_string().into(),
            handle: MOBIUS_HANDLE.into(),
            name: MOBIUS_NAME.into(),
            description: MOBIUS_DESCRIPTION.into(),
            tint: ProviderTint::default(),
            shape: BotShape::Circle,
            config: Arc::new(VersionedAgentConfig {
                revision: 1,
                config: HarnessConfig::Mobius(Arc::new(config)),
            }),
            approval,
        };
        let record = bot.record(&*self.voice_routes()?)?;
        state.bots.push(bot);
        validate_state(&state)?;
        self.save(&state)?;
        Ok(Some(record))
    }

    pub(crate) fn create_bot(
        &self,
        name: &str,
        description: &str,
        config: crate::harness::ValidatedConfig,
    ) -> Result<BotRecord> {
        let name = validate_name(name)?;
        let description = validate_description(description)?;
        let (config, approval) = config.into_parts();
        let voices = self.voice_routes()?;
        self.update(|state| {
            let id = Uuid::new_v4().to_string();
            let handle = next_handle(state, &name, &id);
            let tint = next_tint(state);
            let shape = next_shape(state);
            let bot = StoredBot {
                id: id.into(),
                handle: handle.into(),
                name: name.into(),
                description: description.into(),
                tint,
                shape,
                config: Arc::new(VersionedAgentConfig {
                    revision: 1,
                    config,
                }),
                approval,
            };
            let record = bot.record(&voices)?;
            state.bots.push(bot);
            Ok(record)
        })
    }

    /// Saves `identity` and a host-validated `config`, whose revision must directly follow the
    /// stored one.
    pub(crate) fn update_bot(
        &self,
        id: &str,
        identity: BotIdentity<'_>,
        config: Arc<VersionedAgentConfig>,
        approval: ApprovalMode,
    ) -> Result<BotRecord> {
        let BotIdentity {
            name,
            description,
            tint,
            shape,
        } = identity;
        let name = validate_name(name)?;
        let description = validate_description(description)?;
        let voices = self.voice_routes()?;
        self.update(|state| {
            let handle = next_handle(state, &name, id);
            let bot = find_bot_mut(state, id)?;
            if bot.config.revision.checked_add(1) != Some(config.revision) {
                return Err(Error::Config(format!(
                    "Bot configuration revision changed from {} to {}",
                    config.revision.saturating_sub(1),
                    bot.config.revision
                )));
            }
            if *bot.name != *name && &*bot.handle != MOBIUS_HANDLE {
                bot.handle = handle.into();
            }
            bot.name = name.into();
            bot.description = description.into();
            bot.tint = tint;
            bot.shape = shape;
            bot.config = config;
            bot.approval = approval;
            bot.record(&voices)
        })
    }

    pub(crate) fn prepare_bot_deletion(
        &self,
        id: &str,
        expected_revision: u64,
    ) -> Result<BotDeletion> {
        let state_lock = self.lock_state()?;
        let state = self.fresh_state()?;
        let bot = state
            .bots
            .iter()
            .find(|bot| *bot.id == *id)
            .ok_or_else(|| Error::Config(format!("unknown Bot `{id}`")))?;
        if &*bot.handle == MOBIUS_HANDLE {
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
            .into_iter()
            .filter(|routine| *routine.bot_id == *id)
            .collect::<Vec<_>>();
        let routine_ids = routines
            .iter()
            .map(|routine| &*routine.id)
            .collect::<BTreeSet<_>>();
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
        let (routine_ids, instructions) = routines
            .into_iter()
            .map(|routine| (routine.id, routine.instructions))
            .unzip();
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
        session_roots: &[SessionId],
        session_ids: &[SessionId],
    ) -> Result<PendingBotDeletion> {
        let intent = PendingBotDeletion {
            bot_id: Arc::clone(&deletion.bot_id),
            expected_revision: deletion.expected_revision,
            session_roots: session_roots.to_vec(),
            session_ids: session_ids.to_vec(),
            instruction_paths: deletion.instructions.iter().cloned().collect(), // rules-allow: the deletion keeps its identity for delete_bot; the durable intent owns a copy
        };
        let mut state = self.fresh_state()?;
        let bot = find_bot_mut(&mut state, &intent.bot_id)?;
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
        state.pending_bot_deletion = Some(intent);
        validate_state(&state)?;
        self.save(&state)?;
        deletion.release_state_lock();
        state
            .pending_bot_deletion
            .ok_or_else(|| Error::Config("recorded Bot deletion is missing".into()))
    }

    pub(crate) fn pending_bot_deletion(&self) -> Result<Option<PendingBotDeletion>> {
        Ok(self.fresh_state()?.pending_bot_deletion)
    }

    pub(crate) fn has_pending_bot_deletion(&self) -> Result<bool> {
        Ok(self.current_state()?.pending_bot_deletion.is_some())
    }

    pub(crate) fn clear_bot_deletion(&self, bot_id: &str) -> Result<()> {
        let _state_lock = self.lock_state()?;
        self.update_locked(|state| {
            let pending = state
                .pending_bot_deletion
                .as_ref()
                .ok_or_else(|| Error::Config("Bot deletion recovery is not pending".into()))?;
            if *pending.bot_id != *bot_id {
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
            _routine_locks: routine_locks,
        } = deletion;
        let state_lock = if let Some(state_lock) = state_lock {
            state_lock
        } else {
            self.lock_state()?
        };
        let mut state = self.fresh_state()?;
        let index = {
            if let Some(pending) = &state.pending_bot_deletion
                && (pending.bot_id != bot_id || pending.expected_revision != expected_revision)
            {
                return Err(Error::Config(
                    "a different Bot deletion is awaiting recovery".into(),
                ));
            }
            let (index, bot) = state
                .bots
                .iter()
                .enumerate()
                .find(|(_, bot)| *bot.id == *bot_id)
                .ok_or_else(|| Error::Config(format!("unknown Bot `{bot_id}`")))?;
            if &*bot.handle == MOBIUS_HANDLE {
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
                .filter(|routine| *routine.bot_id == *bot_id)
                .map(|routine| &*routine.id)
                .collect::<BTreeSet<_>>();
            if !current_routine_ids
                .into_iter()
                .eq(routine_ids.iter().map(|id| &**id))
            {
                return Err(Error::Config(
                    "Bot routine state changed during deletion".into(),
                ));
            }
            let current_instructions = state
                .routines
                .iter()
                .filter(|routine| *routine.bot_id == *bot_id)
                .map(|routine| &routine.instructions)
                .collect::<BTreeSet<_>>();
            if !current_instructions.into_iter().eq(&instructions) {
                return Err(Error::Config(
                    "Bot routine instructions changed during deletion".into(),
                ));
            }
            index
        };
        let bot = state.bots.remove(index).record(&*self.voice_routes()?)?;
        state.routines.retain(|routine| *routine.bot_id != *bot_id);
        validate_state(&state)?;
        let catalog = catalog_json(&state)?;
        self.storage
            .delete_runs_and_save_catalog(&catalog, None, Some(&bot_id), None, None)?;
        drop(routine_locks);
        drop(state_lock);
        for path in &instructions {
            if let Err(error) = remove_if_present(path) {
                tracing::warn!(%error, path = %path.display(), "deleted Bot routine instructions were not removed");
            }
        }
        Ok(bot)
    }

    pub(crate) fn bots(&self) -> Result<Vec<BotRecord>> {
        let voices = self.voice_routes()?;
        self.current_state()?
            .bots
            .iter()
            .map(|bot| bot.record(&voices))
            .collect()
    }

    pub(crate) fn bot(&self, id: &str) -> Result<BotRecord> {
        self.find_bot(id)?
            .ok_or_else(|| Error::Config(format!("unknown Bot `{id}`")))
    }

    pub(crate) fn find_bot(&self, id: &str) -> Result<Option<BotRecord>> {
        let voices = self.voice_routes()?;
        self.current_state()?
            .bots
            .iter()
            .find(|bot| *bot.id == *id)
            .map(|bot| bot.record(&voices))
            .transpose()
    }

    /// One möbius Bot with the composition and approval policy its runtime enforces.
    pub(crate) fn runtime_bot(&self, id: &str) -> Result<RuntimeBot> {
        let voices = self.voice_routes()?;
        let state = self.current_state()?;
        let bot = state
            .bots
            .iter()
            .find(|bot| *bot.id == *id)
            .ok_or_else(|| Error::Config(format!("unknown Bot `{id}`")))?;
        let (HarnessConfig::Mobius(composition), ApprovalMode::Mobius(approval_policy)) =
            (&bot.config.config, bot.approval)
        else {
            return Err(Error::Config(format!("Bot `{id}` does not run on möbius")));
        };
        Ok(RuntimeBot {
            record: Arc::new(bot.record(&voices)?),
            composition: Arc::clone(composition),
            approval_policy,
        })
    }

    /// Registers a definition and its consumers in the same durable transaction.
    pub(crate) fn create_routine(
        &self,
        bot_id: &str,
        definition: &RoutineDefinition,
        workspace: CanonicalWorkspace,
        cause: Option<&HookEvent>,
    ) -> Result<StoredRoutine> {
        validate_definition(definition, 0, Some(MAX_SCHEDULE_TIMESTAMP))?;
        let workspace = workspace.into_path();
        let path = self.new_instruction_path();
        if let Err(error) =
            crate::publication::publish(&path, definition.instructions.trim().as_bytes(), true)
        {
            if matches!(error, Error::PublicationApplied { .. }) {
                // No catalog references this newly allocated path yet; remove the applied but unconfirmed staging file.
                remove_if_present(&path)?;
            }
            return Err(error);
        }
        let prepared = (|| {
            let state_lock = self.lock_state()?;
            let mut state = self.fresh_state()?;
            reject_bot_mutation_if_deleting(&state)?;
            find_bot_mut(&mut state, bot_id)?;
            let now = Utc::now().timestamp();
            let bindings = definition
                .bindings
                .iter()
                .map(|binding| StoredRoutineBinding::new(binding.clone(), now, false)) // rules-allow: the definition is borrowed; the catalog owns its bindings
                .collect::<Result<_>>()?;
            Ok((state_lock, state, now, bindings))
        })();
        let (state_lock, mut state, now, bindings) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return Err(discard_staged(&path, error)),
        };
        let index = state.routines.len();
        state.routines.push(StoredRoutine {
            id: Uuid::new_v4().to_string().into(),
            bot_id: bot_id.into(),
            workspace,
            instructions: path,
            bindings,
            enabled: true,
        });
        let committed = (|| {
            validate_state(&state)?;
            let event = routine_event(
                routine_at(&state.routines, index)?,
                |routine_id| HookData::RoutineCreated { routine_id },
                now,
                cause,
            )?;
            self.storage.save_catalog_with_hooks(
                &catalog_json(&state)?,
                &state.routines,
                &[event],
                &[],
                now,
                None,
            )
        })();
        let routine = state.routines.swap_remove(index);
        drop(state_lock);
        match committed {
            Ok(()) => Ok(routine),
            Err(error) => Err(discard_staged(&routine.instructions, error)),
        }
    }

    pub(crate) fn routine_records(&self, bot_id: Option<&str>, now: i64) -> Result<Vec<Routine>> {
        let state = self.fresh_state()?;
        state
            .routines
            .into_iter()
            .filter(|stored| bot_id.is_none_or(|bot_id| *stored.bot_id == *bot_id))
            .map(|stored| self.routine_record_from(stored, now))
            .collect()
    }

    pub(crate) fn routine_record(&self, id: &str, now: i64) -> Result<Routine> {
        let state = self.fresh_state()?;
        let stored = state
            .routines
            .into_iter()
            .find(|routine| *routine.id == *id)
            .ok_or_else(|| Error::Config(format!("unknown routine `{id}`")))?;
        self.routine_record_from(stored, now)
    }

    pub(crate) fn running_routine_count(&self) -> Result<u64> {
        self.storage.running_routine_count()
    }

    pub(crate) fn has_running_routines(&self) -> Result<bool> {
        self.storage.has_running_routines()
    }

    pub(crate) fn next_routine_at(&self, now: i64) -> Result<Option<chrono::DateTime<Utc>>> {
        self.current_state()?
            .routines
            .iter()
            .filter_map(|routine| routine.next_run_at(now))
            .min()
            .map(|timestamp| {
                chrono::DateTime::from_timestamp(timestamp, 0)
                    .ok_or_else(|| Error::Config("routine timestamp is invalid".into()))
            })
            .transpose()
    }

    pub(crate) fn has_active_routines(&self, now: i64) -> Result<bool> {
        let state = self.current_state()?;
        Ok(state
            .routines
            .iter()
            .any(|routine| routine.enabled && !routine.is_finished(now))
            || self.storage.has_running_routines()?)
    }

    pub(crate) fn update_routine(
        &self,
        id: &str,
        definition: &RoutineDefinition,
        workspace: CanonicalWorkspace,
        cause: Option<&HookEvent>,
        accepted_action_id: Option<&str>,
    ) -> Result<StoredRoutine> {
        validate_definition(definition, 0, Some(MAX_SCHEDULE_TIMESTAMP))?;
        let workspace = workspace.into_path();
        let _state_lock = self.lock_state()?;
        let mut state = self.fresh_state()?;
        reject_bot_mutation_if_deleting(&state)?;
        let index = resolve_routine(&state.routines, id)?;
        let existing = routine_at(&state.routines, index)?;
        if existing.workspace == workspace
            && self.read_routine_instructions(existing)? == definition.instructions.trim()
            && existing
                .bindings
                .iter()
                .map(|binding| &binding.definition)
                .eq(&definition.bindings)
        {
            if let Some(id) = accepted_action_id {
                self.storage.action_accepted(id)?;
            }
            return Ok(state.routines.swap_remove(index));
        }
        let Some(_lock) = self.try_routine_lock(&existing.id)? else {
            return Err(Error::Config(format!(
                "routine {} is currently running",
                existing.id
            )));
        };
        let path = self.new_instruction_path();
        if let Err(error) =
            crate::publication::publish(&path, definition.instructions.trim().as_bytes(), true)
        {
            if matches!(error, Error::PublicationApplied { .. }) {
                // No catalog references this newly allocated path yet; remove the applied but unconfirmed staging file.
                remove_if_present(&path)?;
            }
            return Err(error);
        }
        let stored = match routine_at_mut(&mut state.routines, index) {
            Ok(stored) => stored,
            Err(error) => return Err(discard_staged(&path, error)),
        };
        let previous_instructions = std::mem::take(&mut stored.instructions);
        let mut previous_bindings = std::mem::take(&mut stored.bindings);
        let now = Utc::now().timestamp();
        let bindings = definition
            .bindings
            .iter()
            .map(|binding| {
                if let Some(index) = previous_bindings
                    .iter()
                    .position(|prior| prior.definition == *binding)
                {
                    Ok(previous_bindings.swap_remove(index))
                } else {
                    StoredRoutineBinding::new(binding.clone(), now, false) // rules-allow: the definition is borrowed; the catalog owns its bindings
                }
            })
            .collect::<Result<_>>();
        stored.bindings = match bindings {
            Ok(bindings) => bindings,
            Err(error) => return Err(discard_staged(&path, error)),
        };
        stored.workspace = workspace;
        stored.instructions = path;
        let committed = (|| {
            validate_state(&state)?;
            let event = routine_event(
                routine_at(&state.routines, index)?,
                |routine_id| HookData::RoutineUpdated { routine_id },
                now,
                cause,
            )?;
            self.storage.save_catalog_with_hooks(
                &catalog_json(&state)?,
                &state.routines,
                &[event],
                &[],
                now,
                accepted_action_id,
            )
        })();
        let routine = state.routines.swap_remove(index);
        if let Err(error) = committed {
            return Err(discard_staged(&routine.instructions, error));
        }
        if let Err(error) = remove_if_present(&previous_instructions) {
            tracing::warn!(%error, "replaced routine instructions were not removed");
        }
        Ok(routine)
    }

    /// Changes future admission without acquiring or interrupting an invocation lock.
    pub(crate) fn set_routine_enabled(
        &self,
        id: &str,
        enabled: bool,
        cause: Option<&HookEvent>,
        accepted_action_id: Option<&str>,
    ) -> Result<StoredRoutine> {
        let _state_lock = self.lock_state()?;
        let mut state = self.fresh_state()?;
        reject_bot_mutation_if_deleting(&state)?;
        let index = resolve_routine(&state.routines, id)?;
        if routine_at(&state.routines, index)?.enabled == enabled {
            if let Some(id) = accepted_action_id {
                self.storage.action_accepted(id)?;
            }
            return Ok(state.routines.swap_remove(index));
        }
        let now = Utc::now().timestamp();
        let stored = routine_at_mut(&mut state.routines, index)?;
        stored.enabled = enabled;
        if enabled {
            for binding in &mut stored.bindings {
                binding.reset(now, true)?;
            }
        }
        validate_state(&state)?;
        let data: fn(Arc<str>) -> HookData = if enabled {
            |routine_id| HookData::RoutineResumed { routine_id }
        } else {
            |routine_id| HookData::RoutinePaused { routine_id }
        };
        let event = routine_event(routine_at(&state.routines, index)?, data, now, cause)?;
        self.storage.save_catalog_with_hooks(
            &catalog_json(&state)?,
            &state.routines,
            &[event],
            &[],
            now,
            accepted_action_id,
        )?;
        Ok(state.routines.swap_remove(index))
    }

    pub(crate) fn prepare_routine_deletion(&self, id: &str) -> Result<RoutineDeletion> {
        let state_lock = self.lock_state()?;
        let routine = self.routine(id)?;
        let Some(lock) = self.try_routine_lock(&routine.id)? else {
            return Err(Error::Config(format!(
                "routine {} is currently running",
                routine.id
            )));
        };
        self.read_routine_instructions(&routine)?;
        let mut state = self.fresh_state()?;
        let index = resolve_routine(&state.routines, &routine.id)?;
        let routine = state.routines.swap_remove(index);
        let session_ids = self
            .storage
            .session_ids_for_routine(&routine.id)?
            .into_iter()
            .collect();
        Ok(RoutineDeletion {
            routine_id: routine.id,
            session_ids,
            instructions: routine.instructions,
            _state_lock: state_lock,
            _lock: lock,
        })
    }

    pub(crate) fn delete_routine(
        &self,
        deletion: RoutineDeletion,
        cause: Option<&HookEvent>,
        accepted_action_id: Option<&str>,
    ) -> Result<StoredRoutine> {
        let RoutineDeletion {
            routine_id,
            session_ids,
            instructions,
            _state_lock: state_lock,
            _lock: lock,
        } = deletion;
        let mut state = self.fresh_state()?;
        let index = {
            let index = resolve_routine(&state.routines, &routine_id)?;
            if routine_at(&state.routines, index)?.instructions != instructions {
                return Err(Error::Config(
                    "routine instructions changed during deletion".into(),
                ));
            }
            let current_session_ids = self
                .storage
                .session_ids_for_routine(&routine_id)?
                .into_iter()
                .collect::<BTreeSet<_>>();
            if current_session_ids != session_ids {
                return Err(Error::Config(
                    "routine run state changed during deletion".into(),
                ));
            }
            index
        };
        let deleted = state.routines.remove(index);
        validate_state(&state)?;
        let catalog = catalog_json(&state)?;
        let event = routine_event(
            &deleted,
            |routine_id| HookData::RoutineDeleted { routine_id },
            Utc::now().timestamp(),
            cause,
        )?;
        self.storage.delete_runs_and_save_catalog(
            &catalog,
            Some(&routine_id),
            None,
            Some(&event),
            accepted_action_id,
        )?;
        drop(lock);
        drop(state_lock);
        if let Err(error) = remove_if_present(&instructions) {
            tracing::warn!(%error, "deleted routine instructions were not removed");
        }
        Ok(deleted)
    }

    pub(crate) fn routine(&self, id: &str) -> Result<StoredRoutine> {
        let mut state = self.fresh_state()?;
        let index = resolve_routine(&state.routines, id)?;
        Ok(state.routines.swap_remove(index))
    }

    pub(crate) fn routine_input(&self, id: &str) -> Result<(StoredRoutine, String)> {
        let state = self.fresh_state()?;
        let routine = state
            .routines
            .into_iter()
            .find(|routine| *routine.id == *id)
            .ok_or_else(|| Error::Config(format!("unknown routine `{id}`")))?;
        let instructions = self.read_routine_instructions(&routine)?;
        let input = format!("{ROUTINE_SUBMISSION_PREFIX}\n\n{instructions}");
        Ok((routine, input))
    }

    fn routine_record_from(&self, stored: StoredRoutine, now: i64) -> Result<Routine> {
        let instructions = self.read_routine_instructions(&stored)?;
        let finished = stored.is_finished(now);
        let next_run_at = stored.next_run_at(now);
        Ok(Routine {
            id: stored.id,
            bot_id: stored.bot_id,
            workspace: stored.workspace,
            instructions,
            bindings: stored
                .bindings
                .into_iter()
                .map(|binding| binding.definition)
                .collect(),
            enabled: stored.enabled,
            finished,
            next_run_at,
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
        let input = String::from_utf8(bytes).map_err(|error| {
            Error::Config(format!("routine instructions are not valid UTF-8: {error}"))
        })?;
        validate_instructions(&input)?;
        Ok(input)
    }

    /// Commits timer facts and advances dates; only the command handler reserves work.
    pub(crate) fn poll_due(&self, now: i64) -> Result<RoutinePoll> {
        let _state_lock = self.lock_state()?;
        let mut state = self.fresh_state()?;
        let active = state
            .routines
            .iter()
            .any(|routine| routine.enabled && routine.next_run_at(now).is_some())
            || self.storage.has_running_routines()?;
        if state.pending_bot_deletion.is_some() {
            return Ok(RoutinePoll {
                active,
                events: Vec::new(),
            });
        }
        let mut events = Vec::new();
        let mut changed = false;
        for routine in &mut state.routines {
            if !routine.enabled {
                continue;
            }
            for binding in &mut routine.bindings {
                let HookSelector::Schedule { schedule, .. } = &binding.definition.on else {
                    continue;
                };
                if schedule.kind == RoutineScheduleKind::Cron
                    && binding
                        .next_due_at
                        .is_none_or(|next| next.div_euclid(60) < now.div_euclid(60))
                {
                    binding.next_due_at = Some(next_cron_occurrence(schedule, now, true)?);
                    changed = true;
                }
                let Some(at) = binding.next_at(now) else {
                    continue;
                };
                if at > now || binding.last_matched_minute == Some(now.div_euclid(60)) {
                    continue;
                }
                let id = events::stable_id(
                    "schedule",
                    &format!("{}:{}", routine.id, binding.definition.id),
                    &at.to_string(),
                );
                events.push(events::caused_event(
                    id.into(),
                    Arc::clone(&routine.bot_id),
                    HookSource::Schedule {
                        routine_id: Arc::clone(&routine.id),
                        binding_id: Arc::clone(&binding.definition.id),
                    },
                    HookData::ScheduleDue {
                        binding_id: Arc::clone(&binding.definition.id),
                    },
                    now,
                    None,
                )?);
                binding.advance(now)?;
                changed = true;
            }
        }
        if changed {
            validate_state(&state)?;
            self.storage.save_catalog_with_hooks(
                &catalog_json(&state)?,
                &state.routines,
                &events,
                &[],
                now,
                None,
            )?;
        }
        Ok(RoutinePoll { active, events })
    }

    /// Starts through one command identity; retries reuse the durable reservation.
    pub(crate) fn begin_run_with_cause(
        &self,
        id: &str,
        command_id: &str,
        cause: Option<&HookEvent>,
    ) -> Result<BeginRun> {
        self.begin_run_inner(id, command_id, cause, || {})
    }
    fn begin_run_inner(
        &self,
        id: &str,
        command_id: &str,
        cause: Option<&HookEvent>,
        after_resolve: impl FnOnce(),
    ) -> Result<BeginRun> {
        let resolved = self.stored_routine(id)?;
        after_resolve();
        let lock = self.try_routine_lock(&resolved.id)?;
        let _state_lock = self.lock_state()?;
        let state = self.fresh_state()?;
        reject_bot_mutation_if_deleting(&state)?;
        let routine = state
            .routines
            .into_iter()
            .find(|stored| stored.id == resolved.id)
            .ok_or_else(|| Error::Config("routine was deleted".into()))?;
        if let Some(run) = self.storage.command_run(command_id)? {
            if run.routine_id != routine.id {
                return Err(Error::Config(
                    "routine command identity targets another routine".into(),
                ));
            }
            return Ok(BeginRun::AlreadyRecorded);
        }
        if !routine.enabled {
            return Err(Error::Config("routine is paused".into()));
        }
        let Some(lock) = lock else {
            if !self.storage.has_running(&routine.id)? {
                return Err(Error::Config("routine is currently being modified".into()));
            }
            self.storage.insert_run_with_cause(
                &new_run(
                    routine,
                    RoutineRunStatus::Skipped,
                    Some("the previous invocation is still running".into()),
                ),
                command_id,
                cause,
            )?;
            return Ok(BeginRun::Skipped);
        };
        let run = new_run(routine, RoutineRunStatus::Running, None);
        self.storage
            .insert_run_with_cause(&run, command_id, cause)?;
        Ok(BeginRun::Started(ActiveRoutineRun {
            run_id: run.id,
            session_id: run
                .session_id
                .ok_or_else(|| Error::Config("running routine has no session".into()))?,
            _lock: lock,
        }))
    }
    #[cfg(test)]
    pub(crate) fn begin_run(&self, id: &str) -> Result<BeginRun> {
        self.begin_run_with_cause(id, &Uuid::new_v4().to_string(), None)
    }
    pub(crate) fn request_run_stop(
        &self,
        run_id: &str,
        command_id: &str,
        cause: Option<&HookEvent>,
    ) -> Result<()> {
        self.storage.request_run_stop(run_id, command_id, cause)
    }
    pub(crate) fn run_cancel_requested(&self, run_id: &str) -> Result<bool> {
        self.storage.run_cancel_requested(run_id)
    }

    /// Completes a running invocation and releases its overlap lock.
    pub(crate) fn finish_run(
        &self,
        run: ActiveRoutineRun,
        status: RoutineRunStatus,
        message: Option<Arc<str>>,
    ) -> Result<RoutineRun> {
        if status == RoutineRunStatus::Running {
            return Err(Error::Config(
                "a completed routine run cannot remain running".into(),
            ));
        }
        let state_lock = self.lock_state()?;
        let finished =
            self.storage
                .finish_run(&run.run_id, status, Utc::now().timestamp(), message);
        drop(state_lock);
        drop(run);
        finished
    }

    /// Returns newest-first run history for one routine.
    pub(crate) fn history(&self, id: Option<&str>) -> Result<Vec<RoutineRun>> {
        let state = self.fresh_state()?;
        let routine_id = id
            .map(|id| self.resolve_history_routine(&state, id))
            .transpose()?;
        self.storage.history(routine_id.as_deref())
    }

    fn resolve_history_routine(&self, state: &BotState, id: &str) -> Result<String> {
        validate_routine_id_prefix(id)?;
        let history_ids = self.storage.history_routine_candidates(id)?;
        let mut ids = state
            .routines
            .iter()
            .map(|routine| &*routine.id)
            .chain(history_ids.iter().map(String::as_str))
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

    pub(crate) fn run(&self, id: &str) -> Result<RoutineRun> {
        let _state = self.fresh_state()?;
        self.storage.run(id)
    }

    pub(crate) fn delete_run(&self, id: &str) -> Result<RoutineRun> {
        let _file_lock = self.lock_state()?;
        let state = self.fresh_state()?;
        if state.pending_bot_deletion.is_some() {
            return Err(Error::Config(
                "Bot deletion recovery must finish before changing Bot state".into(),
            ));
        }
        self.storage.delete_run(id)
    }

    pub(crate) fn subscriptions(&self, bot_id: &str) -> Result<Vec<crate::wire::BotSubscription>> {
        self.bot(bot_id)?;
        self.storage.subscriptions(bot_id)
    }
    pub(crate) fn record_hook(&self, event: &HookEvent) -> Result<bool> {
        self.storage.record_hook(event)
    }
    pub(crate) fn hook_event(&self, id: &str) -> Result<Option<HookEvent>> {
        self.storage.hook_event(id)
    }
    pub(crate) fn unpublished_events(&self, limit: usize) -> Result<Vec<HookEvent>> {
        self.storage.unpublished_events(limit)
    }
    pub(crate) fn event_published(&self, id: &str) -> Result<()> {
        self.storage.event_published(id)
    }
    pub(crate) fn pending_actions(&self, now: i64, limit: usize) -> Result<Vec<PendingHookAction>> {
        self.storage.pending_actions(now, limit)
    }
    pub(crate) fn has_monitored_sessions(&self) -> Result<bool> {
        self.storage.has_monitored_sessions()
    }
    pub(crate) fn close_session_sources(&self, events: &[HookEvent]) -> Result<()> {
        let _state_lock = self.lock_state()?;
        let mut state = self.fresh_state()?;
        let ids = events
            .iter()
            .filter_map(|event| {
                if let HookSource::Session { session_id } = &event.source {
                    Some(session_id.as_str())
                } else {
                    None
                }
            })
            .collect::<BTreeSet<_>>();
        for routine in &mut state.routines {
            routine.bindings.retain(|binding| !matches!(&binding.definition.on,HookSelector::Event{source:HookSource::Session{session_id},..} if ids.contains(session_id.as_str())));
        }
        validate_state(&state)?;
        self.storage
            .close_session_sources(events, &catalog_json(&state)?)
    }
    pub(crate) fn set_subscription(
        &self,
        subscription: &crate::wire::BotSubscription,
        after_sequence: u64,
        now: i64,
    ) -> Result<()> {
        let _state_lock = self.lock_state()?;
        self.bot(&subscription.bot_id)?;
        self.storage
            .set_subscription(subscription, after_sequence, now)
    }
    pub(crate) fn advance_source_cursor(
        &self,
        session_id: &SessionId,
        bot_id: &str,
        sequence: u64,
    ) -> Result<()> {
        self.storage
            .advance_source_cursor(session_id, bot_id, sequence)
    }
    pub(crate) fn resume_run(&self, id: &str) -> Result<ActiveRoutineRun> {
        let run = self.storage.run(id)?;
        if run.status != RoutineRunStatus::Running {
            return Err(Error::Config("only running routines can resume".into()));
        }
        let lock = self
            .try_routine_lock(&run.routine_id)?
            .ok_or_else(|| Error::Config("routine is already resident".into()))?;
        Ok(ActiveRoutineRun {
            run_id: run.id,
            session_id: run
                .session_id
                .ok_or_else(|| Error::Config("running routine has no session".into()))?,
            _lock: lock,
        })
    }
    pub(crate) fn source_cursor(&self, session_id: &SessionId) -> Result<(u64, Option<Arc<str>>)> {
        self.storage.source_cursor(session_id)
    }
    pub(crate) fn project_session(&self, event: &HookEvent, sequence: u64) -> Result<bool> {
        self.storage.project_session(event, sequence)
    }
    pub(crate) fn action_pending(&self, id: &str) -> Result<bool> {
        self.storage.action_pending(id)
    }
    pub(crate) fn action_accepted(&self, id: &str) -> Result<()> {
        self.storage.action_accepted(id)
    }
    pub(crate) fn action_failed(&self, id: &str, error: &str, retry_at: Option<i64>) -> Result<()> {
        self.storage.action_failed(id, error, retry_at)
    }
    pub(crate) fn has_pending_deliveries(&self) -> Result<bool> {
        self.storage.has_pending_deliveries()
    }
    pub(crate) fn recover_run(
        &self,
        id: &str,
        status: RoutineRunStatus,
        finished_at: i64,
        message: Option<Arc<str>>,
    ) -> Result<RoutineRun> {
        self.storage.finish_run(id, status, finished_at, message)
    }

    fn stored_routine(&self, id: &str) -> Result<StoredRoutine> {
        self.fresh_state()?
            .routines
            .into_iter()
            .find(|routine| *routine.id == *id)
            .ok_or_else(|| Error::Config(format!("unknown routine `{id}`")))
    }

    fn try_routine_lock(&self, id: &str) -> Result<Option<RoutineLock>> {
        let file = open_private_lock(&self.state_dir.join(format!("routine-{id}.lock")))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(RoutineLock(file))),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    fn new_instruction_path(&self) -> PathBuf {
        self.routines_dir
            .join(format!("{}.md", Uuid::new_v4().as_hyphenated()))
    }

    fn update<T>(&self, mutate: impl FnOnce(&mut BotState) -> Result<T>) -> Result<T> {
        let _file_lock = self.lock_state()?;
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
        let mut state = self.fresh_state()?;
        let result = mutate(&mut state)?;
        validate_state(&state)?;
        self.save(&state)?;
        Ok(result)
    }

    fn save(&self, state: &BotState) -> Result<()> {
        self.storage.save_catalog(&catalog_json(state)?)
    }

    /// Holds the cross-process Bot state lock until the returned file drops.
    fn lock_state(&self) -> Result<File> {
        let lock = open_private_lock(&self.state_dir.join(STATE_LOCK_FILE))?;
        lock.lock()?;
        Ok(lock)
    }

    fn fresh_state(&self) -> Result<BotState> {
        self.parse_state(self.storage.load_catalog()?)
    }

    /// The persisted catalog for reads, parsed again only after a commit.
    fn current_state(&self) -> Result<std::sync::Arc<BotState>> {
        let mut cache = self
            .cache
            .lock()
            .map_err(crate::poisoned("Bot catalog cache"))?;
        let stamp = cache.as_ref().map(|(stamp, _)| *stamp);
        if let Some((stamp, contents)) = self.storage.catalog_since(stamp)? {
            *cache = Some((stamp, std::sync::Arc::new(self.parse_state(contents)?)));
        }
        cache
            .as_ref()
            .map(|(_, state)| std::sync::Arc::clone(state))
            .ok_or_else(|| Error::Config("Bot catalog is unavailable".into()))
    }

    fn parse_state(&self, contents: Option<String>) -> Result<BotState> {
        #[cfg(test)]
        CATALOG_PARSES.with(|count| count.set(count.get() + 1));
        let state = contents
            .map(|contents| serde_json::from_str(&contents))
            .transpose()?
            .unwrap_or_default();
        validate_records(&state, &self.routines_dir)?;
        Ok(state)
    }
}

#[cfg(test)]
thread_local! {
    static CATALOG_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn catalog_json(state: &BotState) -> Result<String> {
    let contents = serde_json::to_string_pretty(state)?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_STATE_BYTES {
        return Err(Error::Config("Bot state is too large".into()));
    }
    Ok(contents)
}

fn next_handle(state: &BotState, name: &str, id: &str) -> String {
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
    if base != USER_HANDLE
        && !state
            .bots
            .iter()
            .any(|bot| *bot.id != *id && *bot.handle == *base)
    {
        return base;
    }
    for index in 2_u64.. {
        let suffix = format!("-{index}");
        let prefix_len = MAX_HANDLE_BYTES.saturating_sub(suffix.len());
        let prefix = base
            .get(..base.len().min(prefix_len))
            .unwrap_or(&base)
            .trim_end_matches('-');
        let candidate = format!("{prefix}{suffix}");
        if candidate != USER_HANDLE
            && !state
                .bots
                .iter()
                .any(|bot| *bot.id != *id && *bot.handle == *candidate)
        {
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
        .or_else(|| BOT_TINTS.iter().copied().cycle().nth(state.bots.len()))
        .unwrap_or_default()
}

/// The first shape no Bot wears yet, then round again, as tints are handed out.
fn next_shape(state: &BotState) -> BotShape {
    BotShape::ALL
        .into_iter()
        .find(|shape| state.bots.iter().all(|bot| bot.shape != *shape))
        .or_else(|| BotShape::ALL.into_iter().cycle().nth(state.bots.len()))
        .unwrap_or(BotShape::Circle)
}

fn validate_handle(handle: &str) -> Result<String> {
    let handle = handle.trim();
    if !mobius::identifier::valid_ascii_identifier(
        handle,
        MAX_HANDLE_BYTES,
        mobius::identifier::AsciiCase::Lower,
        b"_-",
    ) {
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
    if name.is_empty() || name.len() > MAX_BOT_NAME_BYTES {
        return Err(Error::Config(format!(
            "Bot name must be 1–{MAX_BOT_NAME_BYTES} bytes"
        )));
    }
    Ok(name.into())
}

fn validate_description(description: &str) -> Result<String> {
    let description = description.trim();
    if description.is_empty() || description.len() > MAX_BOT_DESCRIPTION_BYTES {
        return Err(Error::Config(format!(
            "Bot description must be 1–{MAX_BOT_DESCRIPTION_BYTES} bytes"
        )));
    }
    Ok(description.into())
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

pub(crate) fn validate_definition(
    definition: &RoutineDefinition,
    depth: usize,
    timestamp_limit: Option<i64>,
) -> Result<()> {
    validate_stored_workspace(&definition.workspace)?;
    validate_instructions(&definition.instructions)?;
    validate_bindings(definition.bindings.iter(), depth, timestamp_limit)
}

fn validate_bindings<'a>(
    bindings: impl ExactSizeIterator<Item = &'a RoutineBinding>,
    depth: usize,
    timestamp_limit: Option<i64>,
) -> Result<()> {
    if depth > 4 || bindings.len() > events::MAX_ROUTINE_BINDINGS {
        return Err(Error::Config(
            "routine bindings exceed their count or nesting bound".into(),
        ));
    }
    let mut ids = BTreeSet::new();
    for binding in bindings {
        if binding.id.is_empty()
            || binding.id.len() > 128
            || binding.id.chars().any(char::is_control)
            || !ids.insert(&binding.id)
        {
            return Err(Error::Config(
                "routine binding identities must be distinct and contain 1 to 128 safe bytes"
                    .into(),
            ));
        }
        events::validate_selector(&binding.on)?;
        if let HookSelector::Schedule { schedule, ends_at } = &binding.on {
            validate_schedule(schedule, *ends_at, timestamp_limit)?;
        }
        validate_routine_action(&binding.action, depth, timestamp_limit)?;
    }
    Ok(())
}

pub(crate) fn validate_routine_action(
    action: &RoutineAction,
    depth: usize,
    timestamp_limit: Option<i64>,
) -> Result<()> {
    match action {
        RoutineAction::Update { definition } => {
            validate_definition(definition, depth + 1, timestamp_limit)
        }
        RoutineAction::Stop { run_id }
            if run_id.trim().is_empty()
                || run_id.len() > 256
                || run_id.chars().any(char::is_control) =>
        {
            Err(Error::Config(
                "stop action requires a safe invocation identity".into(),
            ))
        }
        RoutineAction::Start
        | RoutineAction::Stop { .. }
        | RoutineAction::Pause
        | RoutineAction::Resume
        | RoutineAction::Delete => Ok(()),
    }
}

fn validate_schedule(
    schedule: &RoutineSchedule,
    ends_at: Option<i64>,
    timestamp_limit: Option<i64>,
) -> Result<()> {
    if schedule
        .at
        .is_some_and(|at| timestamp_limit.is_some_and(|limit| !(1..=limit).contains(&at)))
        || ends_at.is_some_and(|end| !(1..=timestamp_limit.unwrap_or(i64::MAX)).contains(&end))
    {
        return Err(Error::Config(
            "schedule timestamps must be Unix epoch seconds within the supported range, not milliseconds".into(),
        ));
    }
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
    Ok(())
}

/// Checks each record read from disk; in-process records are built from checked input.
fn validate_records(state: &BotState, routines_dir: &Path) -> Result<()> {
    if state.version != STATE_VERSION {
        return Err(Error::Config(format!(
            "unsupported Bot state version {}",
            state.version
        )));
    }
    for bot in &state.bots {
        let parsed = Uuid::parse_str(&bot.id)
            .map_err(|error| Error::Config(format!("invalid persisted Bot ID: {error}")))?;
        if *parsed.to_string() != *bot.id {
            return Err(Error::Config("invalid persisted Bot ID".into()));
        }
        if *validate_handle(&bot.handle)? != *bot.handle
            || *validate_name(&bot.name)? != *bot.name
            || *validate_description(&bot.description)? != *bot.description
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
    }
    for routine in &state.routines {
        let parsed = Uuid::parse_str(&routine.id)
            .map_err(|error| Error::Config(format!("invalid persisted routine ID: {error}")))?;
        if *parsed.to_string() != *routine.id {
            return Err(Error::Config("invalid persisted routine ID".into()));
        }
        validate_stored_workspace(&routine.workspace)?;
        if !routine.instructions.is_absolute()
            || routine.instructions.parent() != Some(routines_dir)
        {
            return Err(Error::Config(
                "persisted routine path is outside the private gateway routine directory".into(),
            ));
        }
        validate_bindings(
            routine.bindings.iter().map(|binding| &binding.definition),
            0,
            None,
        )?;
        if routine
            .bindings
            .iter()
            .any(|binding| binding.next_due_at.is_some_and(|next| next <= 0))
        {
            return Err(Error::Config("invalid persisted routine next run".into()));
        }
    }
    if let Some(pending) = &state.pending_bot_deletion {
        let parsed = Uuid::parse_str(&pending.bot_id)
            .map_err(|error| Error::Config(format!("invalid pending Bot deletion ID: {error}")))?;
        if *parsed.to_string() != *pending.bot_id || pending.expected_revision == 0 {
            return Err(Error::Config("invalid pending Bot deletion".into()));
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
    validate_state(state)
}

/// Checks the rules that span records, before every save.
fn validate_state(state: &BotState) -> Result<()> {
    let mut bot_ids = BTreeSet::new();
    let mut handles = BTreeSet::new();
    for bot in &state.bots {
        if !bot_ids.insert(&*bot.id) {
            return Err(Error::Config("duplicate persisted Bot ID".into()));
        }
        if !handles.insert(&*bot.handle) {
            return Err(Error::Config("duplicate persisted Bot handle".into()));
        }
    }
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for routine in &state.routines {
        if !ids.insert(&*routine.id) {
            return Err(Error::Config("duplicate persisted routine ID".into()));
        }
        if !bot_ids.contains(&*routine.bot_id) {
            return Err(Error::Config("persisted routine has no Bot".into()));
        }
        if !paths.insert(routine.instructions.as_path()) {
            return Err(Error::Config(
                "persisted routine path is outside the private gateway routine directory".into(),
            ));
        }
    }
    if let Some(pending) = &state.pending_bot_deletion
        && let Some(bot) = state.bots.iter().find(|bot| *bot.id == *pending.bot_id)
        && bot.config.revision != pending.expected_revision
    {
        return Err(Error::Config(
            "pending Bot deletion revision changed".into(),
        ));
    }
    Ok(())
}

fn resolve_routine(routines: &[StoredRoutine], id: &str) -> Result<usize> {
    validate_routine_id_prefix(id)?;
    if let Some(index) = routines.iter().position(|routine| *routine.id == *id) {
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

fn new_run(
    routine: StoredRoutine,
    status: RoutineRunStatus,
    message: Option<Arc<str>>,
) -> RoutineRun {
    let now = Utc::now().timestamp();
    RoutineRun {
        id: Uuid::new_v4().to_string().into(),
        routine_id: routine.id,
        bot_id: routine.bot_id,
        started_at: now,
        finished_at: (status != RoutineRunStatus::Running).then_some(now),
        status,
        session_id: (status == RoutineRunStatus::Running).then(SessionId::generate),
        message,
    }
}

/// Removes a staged instruction file after a failed commit, keeping the commit error
/// unless the removal itself fails.
fn discard_staged(path: &Path, error: Error) -> Error {
    match remove_if_present(path) {
        Ok(()) => error,
        Err(removal) => removal,
    }
}

fn routine_at(routines: &[StoredRoutine], index: usize) -> Result<&StoredRoutine> {
    routines
        .get(index)
        .ok_or_else(|| Error::Config("resolved routine is missing from the catalog".into()))
}

fn routine_at_mut(routines: &mut [StoredRoutine], index: usize) -> Result<&mut StoredRoutine> {
    routines
        .get_mut(index)
        .ok_or_else(|| Error::Config("resolved routine is missing from the catalog".into()))
}

/// A lifecycle event for one routine, owned apart from the catalog entry.
fn routine_event(
    routine: &StoredRoutine,
    data: impl FnOnce(Arc<str>) -> HookData,
    now: i64,
    cause: Option<&HookEvent>,
) -> Result<HookEvent> {
    events::caused_event(
        Uuid::new_v4().to_string().into(),
        Arc::clone(&routine.bot_id),
        HookSource::Routine {
            routine_id: Arc::clone(&routine.id),
        },
        data(Arc::clone(&routine.id)),
        now,
        cause,
    )
}

fn find_bot_mut<'a>(state: &'a mut BotState, id: &str) -> Result<&'a mut StoredBot> {
    state
        .bots
        .iter_mut()
        .find(|bot| *bot.id == *id)
        .ok_or_else(|| Error::Config(format!("unknown Bot `{id}`")))
}

fn open_private_lock(path: &Path) -> Result<File> {
    #[cfg(unix)]
    let file = mobius::owner_only::open(path)?;
    #[cfg(not(unix))]
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
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
    std::fs::set_permissions(&path, mobius::owner_only::dir())?;
    Ok(path)
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
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

const CONVERSATION_PREFIX: &str = "persistent-";

/// One stable conversation identity per Bot.
pub(crate) fn conversation_session_id(bot_id: &str) -> String {
    format!("{CONVERSATION_PREFIX}{bot_id}")
}

/// The Bot whose permanent conversation `session_id` names.
pub(crate) fn conversation_owner(session_id: &str) -> Option<&str> {
    session_id.strip_prefix(CONVERSATION_PREFIX)
}

/// Whether `session_id` is `owner_id`'s permanent conversation.
pub(crate) fn is_persistent(session_id: &str, owner_id: &str) -> bool {
    conversation_owner(session_id) == Some(owner_id)
}

/// Whether `summary` is one of `bot_id`'s listed top-level chats.
pub(crate) fn is_listed_root(
    summary: &mobius::backend::checkpoint::SessionSummary,
    bot_id: &str,
) -> bool {
    *summary.session_context.owner_id == *bot_id
        && summary.catalog_visible
        && summary.parent_session_id.is_none()
}

fn reject_bot_mutation_if_deleting(state: &BotState) -> Result<()> {
    if state.pending_bot_deletion.is_some() {
        return Err(Error::Config(
            "Bot deletion recovery must finish before changing Bot work".into(),
        ));
    }
    Ok(())
}
