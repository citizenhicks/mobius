//! Durable session, Swarm, and global notes for agent self-improvement.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::bots::BotsBackend;
use super::manifest::MiddlewareManifest;
use super::tools::{Catalog, labeled_tool_heading, render_tool_event};
use super::{
    ActiveCommandContext, Middleware, MiddlewareCommandContext, MiddlewareCommandOutput,
    ModelContext, PromptSection, RuntimeContext, SessionStartContext, SessionStartSource,
    SubmissionResult,
};
use crate::backend::checkpoint::{CheckpointStore, ContextRewriteReason};
use crate::protocol::{
    EventMsg, FrontendBlock, FrontendCommand, FrontendContribution, FrontendTone,
};
use crate::{BoxFuture, Error, Result};

mod text {
    include!(concat!(
        env!("OUT_DIR"),
        "/src_middleware_scratchpad_text.rs"
    ));
}

mod presentation;
mod projection;
mod tools;

#[cfg(test)]
use presentation::action_list_item;
use presentation::{
    command_confirmation, format_snapshot, global_widget, parse_scope, publish_widgets,
    surface_widgets, swarm_widget, usage, widget_events,
};
pub(crate) use projection::is_projection_item;
use projection::{next_projection, scratchpad_message, without_projection_items};
use tools::{PromoteScratchpad, WriteScratchpad};

const SESSION_STATE_KEY: &str = "scratchpad.v1";
const GLOBAL_SCOPE: &str = "scratchpad.global";
const GLOBAL_STATE_KEY: &str = "entries.v1";
const SWARM_SCOPE_PREFIX: &str = "scratchpad.swarm:";
const SWARM_STATE_KEY: &str = "entries.v1";
const MAX_NOTES: usize = 20;
const MAX_NOTE_BYTES: usize = 500;
const MAX_INJECTION_BYTES: usize = 4 * 1024;
const PROJECTION_FIELD: &str = "_mobius_scratchpad_projection";
const BASELINE_KIND: &str = "scratchpad_baseline";
const DELTA_KIND: &str = "scratchpad_delta";

/// Configuration and presentation metadata for durable agent notes.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "scratchpad",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: false,
    default_enabled: true,
    settings: &[],
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    note: String,
    basis: Basis,
    created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Basis {
    AgentObservation,
    UserConfirmed,
}

impl Basis {
    const fn strength(&self) -> u8 {
        match self {
            Self::AgentObservation => 0,
            Self::UserConfirmed => 1,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    session: Vec<Entry>,
    #[serde(deserialize_with = "deserialize_required_option")]
    swarm: Option<Vec<Entry>>,
    global: Vec<Entry>,
}

fn deserialize_required_option<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<Entry>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Session,
    Swarm,
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PromotionTarget {
    Global,
    Swarm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOutcome {
    Added,
    Updated,
    Existing,
}

#[derive(Clone)]
struct SwarmScope {
    backend: Arc<dyn BotsBackend>,
    bot_id: String,
}

impl SwarmScope {
    async fn resolve(&self) -> Result<Option<String>> {
        self.backend.scratchpad_scope(&self.bot_id).await
    }
}

/// Cloneable scratchpad persistence shared by agent runtimes and management commands.
#[derive(Clone)]
pub struct ScratchpadStore {
    checkpoints: Arc<dyn CheckpointStore>,
    // ponytail: one process-wide lock keeps whole-value writes correct; split by scope only if
    // measured contention justifies the extra lock registry.
    access: Arc<Mutex<()>>,
}

impl ScratchpadStore {
    /// Wraps one tenant-scoped checkpoint store with serialized note mutations.
    #[must_use]
    pub fn new(checkpoints: Arc<dyn CheckpointStore>) -> Self {
        Self {
            checkpoints,
            access: Arc::new(Mutex::new(())),
        }
    }

    async fn lock_access(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.access.lock().await
    }

    fn try_lock_access(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        self.access.try_lock().ok()
    }

    async fn snapshot(&self, session_id: &str, swarm_id: Option<&str>) -> Result<Snapshot> {
        let access = self.lock_access().await;
        self.snapshot_locked(session_id, swarm_id, &access).await
    }

    async fn snapshot_locked(
        &self,
        session_id: &str,
        swarm_id: Option<&str>,
        _access: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<Snapshot> {
        Ok(Snapshot {
            session: self.load(Scope::Session, session_id).await?,
            swarm: match swarm_id {
                Some(swarm_id) => Some(self.load(Scope::Swarm, swarm_id).await?),
                None => None,
            },
            global: self.load(Scope::Global, GLOBAL_SCOPE).await?,
        })
    }

    /// Returns the persisted gateway-wide scratchpad management surface.
    pub async fn global_contribution(&self) -> Result<FrontendContribution> {
        let access = self.lock_access().await;
        self.global_contribution_locked(&access).await
    }

    /// Adds one user-confirmed gateway-wide note and returns its refreshed surface.
    pub async fn add_global(&self, note: &str) -> Result<FrontendContribution> {
        let note = canonical_note(note).map_err(Error::Tool)?;
        let access = self.lock_access().await;
        let mut entries = self.load(Scope::Global, GLOBAL_SCOPE).await?;
        let outcome = insert(&mut entries, note, Basis::UserConfirmed)?;
        if outcome != WriteOutcome::Existing {
            self.save(Scope::Global, GLOBAL_SCOPE, &entries).await?;
        }
        self.global_contribution_locked(&access).await
    }

    /// Edits one gateway-wide note and returns the refreshed management surface.
    pub async fn edit_global(&self, id: &str, note: &str) -> Result<FrontendContribution> {
        validate_id(id).map_err(Error::Tool)?;
        let access = self.lock_access().await;
        self.edit_locked(GLOBAL_SCOPE, None, Scope::Global, id, note, &access)
            .await?;
        self.global_contribution_locked(&access).await
    }

    /// Forgets one gateway-wide note and returns the refreshed management surface.
    pub async fn forget_global(&self, id: &str) -> Result<FrontendContribution> {
        validate_id(id).map_err(Error::Tool)?;
        let access = self.lock_access().await;
        self.forget_locked(GLOBAL_SCOPE, None, Scope::Global, id, &access)
            .await?;
        self.global_contribution_locked(&access).await
    }

    /// Returns the persisted management surface for one stable Swarm.
    pub async fn swarm_contribution(&self, swarm_id: &str) -> Result<FrontendContribution> {
        validate_swarm_id(swarm_id).map_err(Error::Tool)?;
        let access = self.lock_access().await;
        self.swarm_contribution_locked(swarm_id, &access).await
    }

    /// Adds one user-confirmed Swarm note and returns its refreshed surface.
    pub async fn add_swarm(&self, swarm_id: &str, note: &str) -> Result<FrontendContribution> {
        validate_swarm_id(swarm_id).map_err(Error::Tool)?;
        let note = canonical_note(note).map_err(Error::Tool)?;
        let access = self.lock_access().await;
        let mut entries = self.load(Scope::Swarm, swarm_id).await?;
        let outcome = insert(&mut entries, note, Basis::UserConfirmed)?;
        if outcome != WriteOutcome::Existing {
            self.save(Scope::Swarm, swarm_id, &entries).await?;
        }
        self.swarm_contribution_locked(swarm_id, &access).await
    }

    /// Edits one user-confirmed Swarm note and returns its refreshed surface.
    pub async fn edit_swarm(
        &self,
        swarm_id: &str,
        id: &str,
        note: &str,
    ) -> Result<FrontendContribution> {
        validate_swarm_id(swarm_id).map_err(Error::Tool)?;
        validate_id(id).map_err(Error::Tool)?;
        let access = self.lock_access().await;
        self.edit_locked(swarm_id, Some(swarm_id), Scope::Swarm, id, note, &access)
            .await?;
        self.swarm_contribution_locked(swarm_id, &access).await
    }

    /// Forgets one Swarm note and returns its refreshed management surface.
    pub async fn forget_swarm(&self, swarm_id: &str, id: &str) -> Result<FrontendContribution> {
        validate_swarm_id(swarm_id).map_err(Error::Tool)?;
        validate_id(id).map_err(Error::Tool)?;
        let access = self.lock_access().await;
        self.forget_locked(swarm_id, Some(swarm_id), Scope::Swarm, id, &access)
            .await?;
        self.swarm_contribution_locked(swarm_id, &access).await
    }

    /// Clears notes owned by a permanently disbanded Swarm.
    pub async fn clear_swarm(&self, swarm_id: &str) -> Result<()> {
        validate_swarm_id(swarm_id).map_err(Error::Tool)?;
        let _access = self.lock_access().await;
        self.save(Scope::Swarm, swarm_id, &[]).await
    }

    async fn global_contribution_locked(
        &self,
        _access: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<FrontendContribution> {
        let entries = self.load(Scope::Global, GLOBAL_SCOPE).await?;
        Ok(FrontendContribution {
            capability: MANIFEST.id.into(),
            widgets: vec![global_widget(&entries)],
            ..FrontendContribution::default()
        })
    }

    async fn swarm_contribution_locked(
        &self,
        swarm_id: &str,
        _access: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<FrontendContribution> {
        let entries = self.load(Scope::Swarm, swarm_id).await?;
        Ok(FrontendContribution {
            capability: MANIFEST.id.into(),
            widgets: vec![swarm_widget(&entries)],
            ..FrontendContribution::default()
        })
    }

    async fn write_session(&self, session_id: &str, note: &str) -> Result<WriteOutcome> {
        let note = canonical_note(note).map_err(Error::Tool)?;
        let _guard = self.access.lock().await;
        let mut entries = self.load(Scope::Session, session_id).await?;
        let outcome = insert(&mut entries, note, Basis::AgentObservation)?;
        if outcome != WriteOutcome::Existing {
            self.save(Scope::Session, session_id, &entries).await?;
        }
        Ok(outcome)
    }

    #[cfg(test)]
    async fn promote_note(
        &self,
        session_id: &str,
        swarm_id: Option<&str>,
        note: &str,
        target: PromotionTarget,
    ) -> Result<WriteOutcome> {
        let access = self.lock_access().await;
        self.promote_note_locked(session_id, swarm_id, note, target, &access)
            .await
    }

    async fn promote_note_locked(
        &self,
        session_id: &str,
        swarm_id: Option<&str>,
        note: &str,
        target: PromotionTarget,
        _access: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<WriteOutcome> {
        let note = canonical_note(note).map_err(Error::Tool)?;
        let session = self.load(Scope::Session, session_id).await?;
        let entry = session
            .into_iter()
            .find(|entry| entry.note == note)
            .ok_or_else(|| {
                Error::Tool("the exact note no longer exists in this session scratchpad".into())
            })?;
        self.promote_locked(swarm_id, entry, false, target).await
    }

    #[cfg(test)]
    async fn promote_id(
        &self,
        session_id: &str,
        swarm_id: Option<&str>,
        id: &str,
        target: PromotionTarget,
    ) -> Result<WriteOutcome> {
        validate_id(id).map_err(Error::Tool)?;
        let access = self.lock_access().await;
        self.promote_id_locked(session_id, swarm_id, id, target, &access)
            .await
    }

    async fn promote_id_locked(
        &self,
        session_id: &str,
        swarm_id: Option<&str>,
        id: &str,
        target: PromotionTarget,
        _access: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<WriteOutcome> {
        let session = self.load(Scope::Session, session_id).await?;
        let entry = session
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
            .ok_or_else(|| Error::Tool("the session scratchpad note no longer exists".into()))?;
        self.promote_locked(swarm_id, entry, true, target).await
    }

    async fn promote_locked(
        &self,
        swarm_id: Option<&str>,
        entry: Entry,
        user_confirmed: bool,
        target: PromotionTarget,
    ) -> Result<WriteOutcome> {
        let (scope, owner_id) = promotion_location(target, swarm_id)?;
        let mut entries = self.load(scope, owner_id).await?;
        let basis = match entry.basis {
            Basis::AgentObservation if user_confirmed => Basis::UserConfirmed,
            basis => basis,
        };
        let outcome = insert(&mut entries, entry.note, basis)?;
        if outcome != WriteOutcome::Existing {
            self.save(scope, owner_id, &entries).await?;
        }
        Ok(outcome)
    }

    async fn forget_locked(
        &self,
        session_id: &str,
        swarm_id: Option<&str>,
        scope: Scope,
        id: &str,
        _access: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<()> {
        let owner_id = scope_owner(scope, session_id, swarm_id)?;
        let mut entries = self.load(scope, owner_id).await?;
        let previous_len = entries.len();
        entries.retain(|entry| entry.id != id);
        if entries.len() == previous_len {
            return Err(Error::Tool("the scratchpad note no longer exists".into()));
        }
        self.save(scope, owner_id, &entries).await
    }

    #[cfg(test)]
    async fn edit(
        &self,
        session_id: &str,
        swarm_id: Option<&str>,
        scope: Scope,
        id: &str,
        note: &str,
    ) -> Result<()> {
        validate_id(id).map_err(Error::Tool)?;
        let access = self.lock_access().await;
        self.edit_locked(session_id, swarm_id, scope, id, note, &access)
            .await
    }

    async fn edit_locked(
        &self,
        session_id: &str,
        swarm_id: Option<&str>,
        scope: Scope,
        id: &str,
        note: &str,
        _access: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<()> {
        let note = canonical_note(note).map_err(Error::Tool)?;
        let owner_id = scope_owner(scope, session_id, swarm_id)?;
        let mut entries = self.load(scope, owner_id).await?;
        if entries
            .iter()
            .any(|entry| entry.id != id && entry.note == note)
        {
            return Err(Error::Tool(
                "the scratchpad already contains that note".into(),
            ));
        }
        let entry = entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .ok_or_else(|| Error::Tool("the scratchpad note no longer exists".into()))?;
        entry.note = note;
        entry.basis = Basis::UserConfirmed;
        self.save(scope, owner_id, &entries).await
    }

    async fn load(&self, scope: Scope, session_id: &str) -> Result<Vec<Entry>> {
        let (scope, key) = storage_location(scope, session_id);
        let mut entries: Vec<Entry> = self
            .checkpoints
            .load_state(scope.as_ref(), key)
            .await?
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| Error::Checkpoint(format!("invalid scratchpad state: {error}")))?
            .unwrap_or_default();
        validate_entries(&mut entries)
            .map_err(|error| Error::Checkpoint(format!("invalid scratchpad state: {error}")))?;
        Ok(entries)
    }

    async fn save(&self, scope: Scope, session_id: &str, entries: &[Entry]) -> Result<()> {
        let (scope, key) = storage_location(scope, session_id);
        self.checkpoints
            .save_state(scope.as_ref(), key, &serde_json::to_value(entries)?)
            .await
    }
}

/// Adds bounded durable notes without exposing persistence details to the agent loop.
#[derive(Clone)]
pub struct Scratchpad {
    store: ScratchpadStore,
    swarm: SwarmScope,
    agent_enabled: bool,
}

impl Scratchpad {
    /// Creates scratchpad middleware for one Bot backed by shared durable stores.
    #[must_use]
    pub fn new(
        store: ScratchpadStore,
        swarm: Arc<dyn BotsBackend>,
        bot_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            swarm: SwarmScope {
                backend: swarm,
                bot_id: bot_id.into(),
            },
            agent_enabled: true,
        }
    }

    /// Controls agent access while retaining the read-only management surface.
    #[must_use]
    pub fn agent_enabled(mut self, enabled: bool) -> Self {
        self.agent_enabled = enabled;
        self
    }

    async fn snapshot(&self, session_id: &str) -> Result<Snapshot> {
        let swarm_id = self.swarm.resolve().await?;
        self.store.snapshot(session_id, swarm_id.as_deref()).await
    }
}

impl Scratchpad {
    async fn execute_command_locked(
        &self,
        session_id: &str,
        command: &str,
        arguments: &str,
        input: Option<&str>,
        swarm_id: Option<&str>,
        access: tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<MiddlewareCommandOutput> {
        let _access = access;
        if command != "scratchpad" {
            return Err(Error::Unknown(format!("scratchpad command `{command}`")));
        }
        let mut arguments = arguments.split_whitespace();
        let operation = arguments.next().unwrap_or("read");
        if !self.agent_enabled && !matches!(operation, "read" | "refresh") {
            return Err(Error::Tool("scratchpad is disabled for this chat".into()));
        }
        match operation {
            "read" if arguments.next().is_none() && input.is_none() => {
                let snapshot = self
                    .store
                    .snapshot_locked(session_id, swarm_id, &_access)
                    .await?;
                Ok(MiddlewareCommandOutput::render(
                    self.name(),
                    format_snapshot(&snapshot),
                    FrontendTone::Neutral,
                ))
            }
            "refresh" if arguments.next().is_none() && input.is_none() => {
                let snapshot = self
                    .store
                    .snapshot_locked(session_id, swarm_id, &_access)
                    .await?;
                Ok(MiddlewareCommandOutput::events(widget_events(&snapshot)))
            }
            "promote" if input.is_none() => {
                match (arguments.next(), arguments.next(), arguments.next()) {
                    (Some(target), Some(id), None) => {
                        let Some(target) = parse_promotion_target(target) else {
                            return Ok(usage());
                        };
                        let outcome = self
                            .store
                            .promote_id_locked(session_id, swarm_id, id, target, &_access)
                            .await?;
                        let snapshot = self
                            .store
                            .snapshot_locked(session_id, swarm_id, &_access)
                            .await?;
                        Ok(command_confirmation(target, outcome, &snapshot))
                    }
                    _ => Ok(usage()),
                }
            }
            "edit" => match (arguments.next(), arguments.next(), arguments.next(), input) {
                (Some(scope), Some(id), None, Some(note)) => {
                    let Some(scope) = parse_scope(scope) else {
                        return Ok(usage());
                    };
                    self.store
                        .edit_locked(session_id, swarm_id, scope, id, note, &_access)
                        .await?;
                    let snapshot = self
                        .store
                        .snapshot_locked(session_id, swarm_id, &_access)
                        .await?;
                    let mut events = widget_events(&snapshot);
                    events.extend(
                        MiddlewareCommandOutput::render(
                            self.name(),
                            text::MESSAGE_UPDATED,
                            FrontendTone::Success,
                        )
                        .events,
                    );
                    Ok(MiddlewareCommandOutput::events(events))
                }
                _ => Ok(usage()),
            },
            "forget" if input.is_none() => {
                match (arguments.next(), arguments.next(), arguments.next()) {
                    (Some(scope), Some(id), None) => {
                        let Some(scope) = parse_scope(scope) else {
                            return Ok(usage());
                        };
                        self.store
                            .forget_locked(session_id, swarm_id, scope, id, &_access)
                            .await?;
                        let snapshot = self
                            .store
                            .snapshot_locked(session_id, swarm_id, &_access)
                            .await?;
                        let mut events = widget_events(&snapshot);
                        events.extend(
                            MiddlewareCommandOutput::render(
                                self.name(),
                                text::MESSAGE_FORGOT,
                                FrontendTone::Success,
                            )
                            .events,
                        );
                        Ok(MiddlewareCommandOutput::events(events))
                    }
                    _ => Ok(usage()),
                }
            }
            _ => Ok(usage()),
        }
    }
}

impl Middleware for Scratchpad {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        if !self.agent_enabled {
            return Ok(());
        }
        catalog.register(Arc::new(WriteScratchpad {
            store: self.store.clone(),
            swarm: self.swarm.clone(),
            session_id: runtime.session_id.clone(),
            frontend: Arc::clone(&runtime.frontend),
        }))?;
        catalog.register(Arc::new(PromoteScratchpad {
            store: self.store.clone(),
            swarm: self.swarm.clone(),
            session_id: runtime.session_id.clone(),
            frontend: Arc::clone(&runtime.frontend),
        }))
    }

    fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(self
            .agent_enabled
            .then(|| PromptSection::new(text::PROMPT_MAIN)))
    }

    fn frontend(&self) -> FrontendContribution {
        FrontendContribution {
            capability: self.name().into(),
            accepts_file_attachments: false,
            count: None,
            commands: vec![FrontendCommand {
                name: "scratchpad".into(),
                arguments: text::COMMAND_ARGUMENTS.into(),
                description: text::COMMAND_DESCRIPTION.into(),
                requires_idle: false,
            }],
            widgets: surface_widgets(&Snapshot::default()),
            references: Vec::new(),
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        render_tool_event(
            event,
            |name| matches!(name, "write_scratchpad" | "promote_scratchpad"),
            |name, arguments| match name {
                "write_scratchpad" => {
                    labeled_tool_heading(text::RENDER_REMEMBER, "note", arguments)
                }
                "promote_scratchpad" => {
                    labeled_tool_heading(text::RENDER_PROMOTE, "note", arguments)
                }
                _ => unreachable!("renderer is guarded by the owned tool names"),
            },
        )
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let snapshot = self.snapshot(&context.runtime.session_id).await?;
            if self.agent_enabled
                && matches!(
                    context.source(),
                    SessionStartSource::Startup | SessionStartSource::Compact
                )
                && !context.input.iter().any(is_projection_item)
                && let Some(item) = scratchpad_message(&snapshot)
            {
                context.push_input(item);
            }
            if context.source() != SessionStartSource::Compact {
                publish_widgets(&context.runtime.frontend, &snapshot)?;
            }
            Ok(())
        })
    }

    fn command<'a>(
        &'a self,
        context: MiddlewareCommandContext<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareCommandOutput>> {
        Box::pin(async move {
            let access = self.store.lock_access().await;
            let swarm_id = self.swarm.resolve().await?;
            self.execute_command_locked(
                context.session_id,
                context.command,
                context.arguments,
                context.input,
                swarm_id.as_deref(),
                access,
            )
            .await
        })
    }

    fn active_command<'a>(
        &'a self,
        context: &'a mut ActiveCommandContext<'_>,
    ) -> BoxFuture<'a, Result<Option<SubmissionResult>>> {
        Box::pin(async move {
            let Some(access) = self.store.try_lock_access() else {
                return Ok(None);
            };
            let swarm_id = self.swarm.resolve().await?;
            let output = self
                .execute_command_locked(
                    context.session_id,
                    context.command,
                    context.arguments,
                    context.input,
                    swarm_id.as_deref(),
                    access,
                )
                .await?;
            context
                .events
                .extend(output.events.into_iter().map(EventMsg::Frontend));
            Ok(Some(SubmissionResult::Handled))
        })
    }

    fn pre_model<'a>(&'a self, context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if !self.agent_enabled {
                if let Some(input) = without_projection_items(context.input()) {
                    context.rewrite_input(ContextRewriteReason::Scratchpad, input)?;
                }
                return Ok(());
            }
            let snapshot = self.snapshot(context.session_id).await?;
            if let Some(item) = next_projection(context.input(), &snapshot)? {
                context.append_model_input(item);
            }
            Ok(())
        })
    }
}

fn storage_location(scope: Scope, owner_id: &str) -> (Cow<'_, str>, &'static str) {
    match scope {
        Scope::Session => (Cow::Borrowed(owner_id), SESSION_STATE_KEY),
        Scope::Swarm => (
            Cow::Owned(format!("{SWARM_SCOPE_PREFIX}{owner_id}")),
            SWARM_STATE_KEY,
        ),
        Scope::Global => (Cow::Borrowed(GLOBAL_SCOPE), GLOBAL_STATE_KEY),
    }
}

fn promotion_location(target: PromotionTarget, swarm_id: Option<&str>) -> Result<(Scope, &str)> {
    match target {
        PromotionTarget::Global => Ok((Scope::Global, GLOBAL_SCOPE)),
        PromotionTarget::Swarm => swarm_id
            .map(|swarm_id| (Scope::Swarm, swarm_id))
            .ok_or_else(|| Error::Tool("this Bot is not currently in a swarm".into())),
    }
}

fn scope_owner<'a>(
    scope: Scope,
    session_id: &'a str,
    swarm_id: Option<&'a str>,
) -> Result<&'a str> {
    match scope {
        Scope::Session => Ok(session_id),
        Scope::Swarm => {
            swarm_id.ok_or_else(|| Error::Tool("this Bot is not currently in a swarm".into()))
        }
        Scope::Global => Ok(GLOBAL_SCOPE),
    }
}

fn parse_promotion_target(target: &str) -> Option<PromotionTarget> {
    match target {
        "global" => Some(PromotionTarget::Global),
        "swarm" => Some(PromotionTarget::Swarm),
        _ => None,
    }
}

fn insert(entries: &mut Vec<Entry>, note: String, basis: Basis) -> Result<WriteOutcome> {
    if let Some(entry) = entries.iter_mut().find(|entry| entry.note == note) {
        if basis.strength() > entry.basis.strength() {
            entry.basis = basis;
            return Ok(WriteOutcome::Updated);
        }
        return Ok(WriteOutcome::Existing);
    }
    if entries.len() >= MAX_NOTES {
        return Err(Error::Tool(format!(
            "scratchpad already contains the maximum {MAX_NOTES} notes"
        )));
    }
    entries.push(Entry {
        id: Uuid::new_v4().to_string(),
        note,
        basis,
        created_at: created_at()?,
    });
    Ok(WriteOutcome::Added)
}

fn validate_entries(entries: &mut [Entry]) -> std::result::Result<(), String> {
    if entries.len() > MAX_NOTES {
        return Err(format!("note count exceeds {MAX_NOTES}"));
    }
    let mut ids = BTreeSet::new();
    let mut notes = BTreeSet::new();
    for entry in entries {
        validate_id(&entry.id)?;
        let note = canonical_note(&entry.note)?;
        if note != entry.note {
            return Err("stored note is not canonical".into());
        }
        if !ids.insert(entry.id.as_str()) {
            return Err("duplicate note ID".into());
        }
        if !notes.insert(entry.note.as_str()) {
            return Err("duplicate note content".into());
        }
        let created_at = entry
            .created_at
            .parse::<u64>()
            .map_err(|_| "invalid scratchpad creation time")?;
        if created_at.to_string() != entry.created_at {
            return Err("scratchpad creation time is not canonical".into());
        }
    }
    Ok(())
}

fn created_at() -> Result<String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .map_err(|error| Error::Tool(format!("system clock is before the Unix epoch: {error}")))
}

fn validate_id(id: &str) -> std::result::Result<(), String> {
    Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| "invalid scratchpad note ID".into())
}

fn validate_swarm_id(id: &str) -> std::result::Result<(), String> {
    Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| "invalid Swarm ID".into())
}

fn canonical_note(note: &str) -> std::result::Result<String, String> {
    let note = note.replace("\r\n", "\n").replace('\r', "\n");
    let note = note.trim();
    if note.is_empty() || note.len() > MAX_NOTE_BYTES {
        return Err(format!(
            "scratchpad note must be 1–{MAX_NOTE_BYTES} UTF-8 bytes"
        ));
    }
    Ok(note.into())
}

#[cfg(test)]
mod tests;
