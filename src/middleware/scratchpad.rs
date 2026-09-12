//! Approved global knowledge.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

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
    pub const ACTION_ADD_GLOBAL: &str = "Add Global Note";
    pub const ACTION_DELETE: &str = "Delete";
    pub const ACTION_EDIT: &str = "Edit";
    pub const COMMAND_ARGUMENTS: &str = "[read|refresh|edit <note-id>|forget <note-id>]";
    pub const COMMAND_DESCRIPTION: &str = "read or manage global notes";
    pub const COMMAND_USAGE: &str =
        "! usage: scratchpad [read|refresh|edit <note-id>|forget <note-id>]";
    pub const EDITOR_GLOBAL_DESCRIPTION: &str =
        "This note becomes durable context for every gateway conversation.";
    pub const EDITOR_GLOBAL_TITLE: &str = "Add global note";
    pub const EDITOR_LABEL: &str = "Note";
    pub const EDITOR_SUBMIT: &str = "Add";
    pub const MANIFEST_DESCRIPTION: &str = "Keep explicitly approved global knowledge";
    pub const MANIFEST_LABEL: &str = "Scratchpad";
    pub const MESSAGE_ADDED: &str = "Added the shared scratchpad note.";
    pub const MESSAGE_AGENT_OBSERVATION: &str = "agent observation";
    pub const MESSAGE_EXISTING: &str = "The shared scratchpad already contains that note.";
    pub const MESSAGE_FORGOT: &str = "Forgot the scratchpad note.";
    pub const MESSAGE_GLOBAL_HEADING: &str = "Global";
    pub const MESSAGE_NO_NOTES: &str = "No notes.";
    pub const MESSAGE_UPDATED: &str = "Updated the scratchpad note.";
    pub const MESSAGE_USER_CONFIRMED: &str = "user confirmed";
    pub const PROMPT_MAIN: &str = "Use `write_scratchpad` for a concise fact, preference, or reusable lesson that should help every future chat; shared writes require approval. Shared notes are knowledge, not a task handoff or reasoning log. Never store private reasoning, raw outputs, secrets, credentials, or transient progress.";
    pub const RENDER_REMEMBER: &str = "Remember shared note";
    pub const TOOL_WRITE_SCRATCHPAD_DESCRIPTION: &str = "Add one concise shared fact, preference, or lesson after approval. Never store reasoning, raw outputs, secrets, or task progress.";
    pub const TOOL_WRITE_SCRATCHPAD_PARAMETER_NOTE_DESCRIPTION: &str = "Concise reusable knowledge, at most 500 UTF-8 bytes. The scratchpad has a small total size limit.";
    pub const WIDGET_GLOBAL_TITLE: &str = "Global Scratchpad";
    pub const WIDGET_TEXT: &str = "Scratchpad";
}
mod presentation;
mod projection;
mod tools;

#[cfg(test)]
use presentation::action_list_item;
use presentation::{
    format_snapshot, global_widget, publish_widgets, surface_widgets, usage, widget_events,
};
use projection::is_projection_item;
use projection::{next_projection, without_projection_items};
use tools::WriteScratchpad;

const GLOBAL_SCOPE: &str = "scratchpad.global";
const GLOBAL_STATE_KEY: &str = "entries.v1";
const MAX_NOTES: usize = 20;
const MAX_NOTE_BYTES: usize = 500;
const MAX_INJECTION_BYTES: usize = 4 * 1024;
const MAX_SCOPE_BYTES: usize = 1_900;
const PROJECTION_KIND: &str = "shared_scratchpad";

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Snapshot {
    global: Vec<Entry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOutcome {
    Added,
    Updated,
    Existing,
}

enum ScratchpadCommand<'a> {
    Read,
    Refresh,
    Add(&'a str),
    Edit(&'a str, &'a str),
    Forget(&'a str),
}

fn parse_command<'a>(arguments: &'a str, input: Option<&'a str>) -> Option<ScratchpadCommand<'a>> {
    let mut arguments = arguments.split_whitespace();
    let operation = arguments.next().unwrap_or("read");
    let id = arguments.next();
    if arguments.next().is_some() {
        return None;
    }
    match (operation, id, input) {
        ("read", None, None) => Some(ScratchpadCommand::Read),
        ("refresh", None, None) => Some(ScratchpadCommand::Refresh),
        ("add", None, Some(note)) => Some(ScratchpadCommand::Add(note)),
        ("edit", Some(id), Some(note)) => Some(ScratchpadCommand::Edit(id, note)),
        ("forget", Some(id), None) => Some(ScratchpadCommand::Forget(id)),
        _ => None,
    }
}

/// Cloneable scratchpad persistence shared by agent runtimes and management commands.
#[derive(Clone)]
pub struct ScratchpadStore {
    checkpoints: Arc<dyn CheckpointStore>,
    // Serializes whole-value updates to the one global notebook.
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

    #[cfg(test)]
    async fn snapshot(&self) -> Result<Snapshot> {
        let access = self.lock_access().await;
        self.snapshot_locked(&access).await
    }

    async fn snapshot_locked(&self, _access: &tokio::sync::MutexGuard<'_, ()>) -> Result<Snapshot> {
        Ok(Snapshot {
            global: self.load().await?,
        })
    }

    /// Executes a human management action using the capability's command grammar.
    pub async fn management_command(
        &self,
        operation: &crate::protocol::Op,
    ) -> Result<FrontendContribution> {
        let invalid = || {
            Error::Tool("scratchpad operation does not match its selected management scope".into())
        };
        let crate::protocol::Op::CapabilityCommand {
            capability,
            command,
            arguments,
            input,
            target: None,
        } = operation
        else {
            return Err(invalid());
        };
        if capability != MANIFEST.id || command != "scratchpad" {
            return Err(invalid());
        }
        match parse_command(arguments, input.as_deref()) {
            Some(ScratchpadCommand::Refresh) => self.global_contribution().await,
            Some(ScratchpadCommand::Add(note)) => self.add_global(note).await,
            Some(ScratchpadCommand::Edit(id, note)) => self.edit_global(id, note).await,
            Some(ScratchpadCommand::Forget(id)) => self.forget_global(id).await,
            _ => Err(invalid()),
        }
    }

    /// Returns the persisted gateway-wide scratchpad management surface.
    pub async fn global_contribution(&self) -> Result<FrontendContribution> {
        let access = self.lock_access().await;
        self.global_contribution_locked(&access).await
    }

    /// Adds one user-confirmed gateway-wide note and returns its refreshed surface.
    pub async fn add_global(&self, note: &str) -> Result<FrontendContribution> {
        let access = self.lock_access().await;
        self.write_locked(note, Basis::UserConfirmed, &access)
            .await?;
        self.global_contribution_locked(&access).await
    }

    /// Edits one gateway-wide note and returns the refreshed management surface.
    pub async fn edit_global(&self, id: &str, note: &str) -> Result<FrontendContribution> {
        validate_id(id).map_err(Error::Tool)?;
        let access = self.lock_access().await;
        self.edit_locked(id, note, &access).await?;
        self.global_contribution_locked(&access).await
    }

    /// Forgets one gateway-wide note and returns the refreshed management surface.
    pub async fn forget_global(&self, id: &str) -> Result<FrontendContribution> {
        validate_id(id).map_err(Error::Tool)?;
        let access = self.lock_access().await;
        self.forget_locked(id, &access).await?;
        self.global_contribution_locked(&access).await
    }

    async fn global_contribution_locked(
        &self,
        _access: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<FrontendContribution> {
        let entries = self.load().await?;
        Ok(FrontendContribution {
            capability: MANIFEST.id.into(),
            widgets: vec![global_widget(&entries)],
            ..FrontendContribution::default()
        })
    }

    async fn write_locked(
        &self,
        note: &str,
        basis: Basis,
        _access: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<WriteOutcome> {
        let note = canonical_note(note).map_err(Error::Tool)?;
        let mut entries = self.load().await?;
        let outcome = insert(&mut entries, note, basis)?;
        if outcome != WriteOutcome::Existing {
            validate_scope_budget(&entries).map_err(Error::Tool)?;
            self.save(&entries).await?;
        }
        Ok(outcome)
    }

    async fn forget_locked(
        &self,
        id: &str,
        _access: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<()> {
        let mut entries = self.load().await?;
        let previous_len = entries.len();
        entries.retain(|entry| entry.id != id);
        if entries.len() == previous_len {
            return Err(Error::Tool("the scratchpad note no longer exists".into()));
        }
        self.save(&entries).await
    }

    async fn edit_locked(
        &self,
        id: &str,
        note: &str,
        _access: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<()> {
        let note = canonical_note(note).map_err(Error::Tool)?;
        let mut entries = self.load().await?;
        if entries
            .iter()
            .any(|entry| entry.id != id && entry.note == note)
        {
            return Err(Error::Tool(
                "the scratchpad already contains that note".into(),
            ));
        }
        let previous_bytes = scope_bytes(&entries).map_err(Error::Tool)?;
        let entry = entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .ok_or_else(|| Error::Tool("the scratchpad note no longer exists".into()))?;
        entry.note = note;
        entry.basis = Basis::UserConfirmed;
        if scope_bytes(&entries).map_err(Error::Tool)? > previous_bytes {
            validate_scope_budget(&entries).map_err(Error::Tool)?;
        }
        self.save(&entries).await
    }

    async fn load(&self) -> Result<Vec<Entry>> {
        let entries: Vec<Entry> = self
            .checkpoints
            .load_state(GLOBAL_SCOPE, GLOBAL_STATE_KEY)
            .await?
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| Error::Checkpoint(format!("invalid scratchpad state: {error}")))?
            .unwrap_or_default();
        validate_entries(&entries)
            .map_err(|error| Error::Checkpoint(format!("invalid scratchpad state: {error}")))?;
        Ok(entries)
    }

    async fn save(&self, entries: &[Entry]) -> Result<()> {
        self.checkpoints
            .save_state(
                GLOBAL_SCOPE,
                GLOBAL_STATE_KEY,
                &serde_json::to_value(entries)?,
            )
            .await
    }
}

/// Adds bounded durable notes without exposing persistence details to the agent loop.
#[derive(Clone)]
pub struct Scratchpad {
    store: ScratchpadStore,
    agent_enabled: bool,
}

impl Scratchpad {
    /// Creates scratchpad middleware for one Bot backed by shared durable stores.
    #[must_use]
    pub fn new(store: ScratchpadStore) -> Self {
        Self {
            store,
            agent_enabled: true,
        }
    }

    /// Controls agent access while retaining the read-only management surface.
    #[must_use]
    pub fn agent_enabled(mut self, enabled: bool) -> Self {
        self.agent_enabled = enabled;
        self
    }

    async fn snapshot(&self) -> Result<Snapshot> {
        let access = self.store.lock_access().await;
        self.store.snapshot_locked(&access).await
    }
}

impl Scratchpad {
    async fn execute_command_locked(
        &self,
        command: &str,
        arguments: &str,
        input: Option<&str>,
        access: tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<MiddlewareCommandOutput> {
        let _access = access;
        if command != "scratchpad" {
            return Err(Error::Unknown(format!("scratchpad command `{command}`")));
        }
        let parsed = parse_command(arguments, input);
        if !self.agent_enabled
            && !matches!(
                arguments.split_whitespace().next().unwrap_or("read"),
                "read" | "refresh"
            )
        {
            return Err(Error::Tool("scratchpad is disabled for this chat".into()));
        }
        let Some(parsed) = parsed else {
            return Ok(usage());
        };
        match parsed {
            ScratchpadCommand::Read => {
                let snapshot = self.store.snapshot_locked(&_access).await?;
                Ok(MiddlewareCommandOutput::render(
                    self.name(),
                    format_snapshot(&snapshot),
                    FrontendTone::Neutral,
                ))
            }
            ScratchpadCommand::Refresh => {
                let snapshot = self.store.snapshot_locked(&_access).await?;
                Ok(MiddlewareCommandOutput::events(widget_events(&snapshot)))
            }
            ScratchpadCommand::Edit(id, note) => {
                self.store.edit_locked(id, note, &_access).await?;
                self.command_updated(text::MESSAGE_UPDATED, &_access).await
            }
            ScratchpadCommand::Forget(id) => {
                self.store.forget_locked(id, &_access).await?;
                self.command_updated(text::MESSAGE_FORGOT, &_access).await
            }
            ScratchpadCommand::Add(_) => Ok(usage()),
        }
    }

    async fn command_updated(
        &self,
        message: &str,
        access: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<MiddlewareCommandOutput> {
        let snapshot = self.store.snapshot_locked(access).await?;
        let mut events = widget_events(&snapshot);
        events.extend(
            MiddlewareCommandOutput::render(self.name(), message, FrontendTone::Success).events,
        );
        Ok(MiddlewareCommandOutput::events(events))
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
            |name| name == "write_scratchpad",
            |name, arguments| {
                if matches!(event, EventMsg::ToolCallEnd(_)) {
                    name.into()
                } else {
                    labeled_tool_heading(text::RENDER_REMEMBER, "note", arguments)
                }
            },
        )
    }

    fn retain_compacted_input(&self, item: &serde_json::Value) -> bool {
        !is_projection_item(item)
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let snapshot = self.snapshot().await?;
            if self.agent_enabled
                && matches!(
                    context.source(),
                    SessionStartSource::Startup | SessionStartSource::Compact
                )
                && let Some(item) = next_projection(context.input, &snapshot)?
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
            self.execute_command_locked(context.command, context.arguments, context.input, access)
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
            let output = self
                .execute_command_locked(context.command, context.arguments, context.input, access)
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
            let snapshot = self.snapshot().await?;
            if let Some(item) = next_projection(context.input(), &snapshot)? {
                context.append_model_input(item);
            }
            Ok(())
        })
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

fn validate_entries(entries: &[Entry]) -> std::result::Result<(), String> {
    if entries.len() > MAX_NOTES {
        return Err(format!("note count exceeds {MAX_NOTES}"));
    }
    let mut ids = BTreeSet::new();
    let mut notes = BTreeSet::new();
    for entry in entries.iter() {
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

fn validate_scope_budget(entries: &[Entry]) -> std::result::Result<(), String> {
    let bytes = scope_bytes(entries)?;
    if bytes > MAX_SCOPE_BYTES {
        return Err(format!(
            "shared scratchpad scope exceeds {MAX_SCOPE_BYTES} rendered bytes; shorten or remove a note"
        ));
    }
    Ok(())
}

fn scope_bytes(entries: &[Entry]) -> std::result::Result<usize, String> {
    entries
        .iter()
        .try_fold(0, |bytes, entry| {
            serde_json::to_string(&entry.note).map(|note| bytes + note.len() + 3)
        })
        .map_err(|error| error.to_string())
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
