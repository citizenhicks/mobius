//! Approved global knowledge.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::tools::Catalog;
use super::{
    ActiveCommandContext, Middleware, MiddlewareCommandContext, MiddlewareCommandOutput,
    ModelContext, PromptSection, RuntimeContext, SessionStartContext, SessionStartSource,
    SubmissionResult,
};
use crate::backend::checkpoint::CheckpointStore;
use crate::protocol::{
    EventMsg, FrontendBlock, FrontendCommand, FrontendContribution, FrontendTone,
};
use crate::{BoxFuture, Error, Result};

mod text {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Definition {
        pub(super) write_scratchpad: crate::middleware::tools::ToolSpec,
        pub(super) prompt_projection_header: String,
        pub(super) prompt_projection_global: String,
        pub(super) prompt_projection_empty: String,
        pub(super) prompt_projection_footer: String,
        pub(super) default_enabled: bool,
        pub(super) action_add_global: String,
        pub(super) action_delete: String,
        pub(super) action_edit: String,
        pub(super) command_arguments: String,
        pub(super) command_description: String,
        pub(super) command_usage: String,
        pub(super) editor_global_description: String,
        pub(super) editor_global_title: String,
        pub(super) editor_label: String,
        pub(super) editor_submit: String,
        pub(super) error_duplicate: String,
        pub(super) error_management: String,
        pub(super) error_missing: String,
        pub(super) error_note_count: String,
        pub(super) error_note_length: String,
        pub(super) error_scope_budget: String,
        pub(super) manifest_description: String,
        pub(super) manifest_label: String,
        pub(super) message_added: String,
        pub(super) message_agent_observation: String,
        pub(super) message_existing: String,
        pub(super) message_forgot: String,
        pub(super) message_global_heading: String,
        pub(super) message_no_notes: String,
        pub(super) message_updated: String,
        pub(super) message_user_confirmed: String,
        pub(super) prompt_main: String,
        pub(super) widget_global_title: String,
        pub(super) widget_text: String,
    }
    crate::embedded_config! { pub(super) static DEFINITION: Definition = include_str!("scratchpad.toml"); }
}
mod presentation;
mod projection;
mod tools;

#[cfg(test)]
use presentation::action_list_item;
use presentation::{format_snapshot, publish_widgets, surface_widgets, usage, widget_events};
use projection::is_projection_item;
use projection::next_projection;
use tools::WriteScratchpad;

const GLOBAL_SCOPE: &str = "scratchpad.global";
const GLOBAL_STATE_KEY: &str = "entries.v1";
const MAX_NOTES: usize = 20;
const MAX_NOTE_BYTES: usize = 500;
const MAX_INJECTION_BYTES: usize = 4 * 1024;
const MAX_SCOPE_BYTES: usize = 1_900;
const PROJECTION_KIND: &str = "shared_scratchpad";

super::manifest::middleware_manifest! {
/// Configuration and presentation metadata for durable agent notes.
    "scratchpad", text::DEFINITION, required: false, settings: &[]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    note: String,
    basis: Basis,
    created_at: String,
}

// Declaration order is strength order: a stronger basis replaces a weaker one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Basis {
    AgentObservation,
    UserConfirmed,
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

fn parse_command<'a>(
    command: &str,
    arguments: &'a str,
    input: Option<&'a str>,
) -> Result<Option<ScratchpadCommand<'a>>> {
    if command != "scratchpad" {
        return Err(Error::Unknown(format!("scratchpad command `{command}`")));
    }
    let mut arguments = arguments.split_whitespace();
    let operation = arguments.next().unwrap_or("read");
    let id = arguments.next();
    if arguments.next().is_some() {
        return Ok(None);
    }
    Ok(match (operation, id, input) {
        ("read", None, None) => Some(ScratchpadCommand::Read),
        ("refresh", None, None) => Some(ScratchpadCommand::Refresh),
        ("add", None, Some(note)) => Some(ScratchpadCommand::Add(note)),
        ("edit", Some(id), Some(note)) => Some(ScratchpadCommand::Edit(id, note)),
        ("forget", Some(id), None) => Some(ScratchpadCommand::Forget(id)),
        _ => None,
    })
}

type Access<'a> = tokio::sync::MutexGuard<'a, ()>;

struct ScratchpadStore {
    checkpoints: Arc<dyn CheckpointStore>,
    // Serializes whole-value updates to the one global notebook.
    access: Mutex<()>,
}

impl ScratchpadStore {
    async fn snapshot_locked(&self, _access: &Access<'_>) -> Result<Vec<Entry>> {
        self.load().await
    }

    async fn write_locked(
        &self,
        note: &str,
        basis: Basis,
        _access: &Access<'_>,
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

    async fn forget_locked(&self, id: &str, _access: &Access<'_>) -> Result<()> {
        validate_id(id).map_err(Error::Tool)?;
        let mut entries = self.load().await?;
        let previous_len = entries.len();
        entries.retain(|entry| entry.id != id);
        if entries.len() == previous_len {
            return Err(missing());
        }
        self.save(&entries).await
    }

    async fn edit_locked(&self, id: &str, note: &str, _access: &Access<'_>) -> Result<()> {
        validate_id(id).map_err(Error::Tool)?;
        let note = canonical_note(note).map_err(Error::Tool)?;
        let mut entries = self.load().await?;
        if entries
            .iter()
            .any(|entry| entry.id != id && entry.note == note)
        {
            return Err(Error::Tool(
                text::DEFINITION.error_duplicate.as_str().into(),
            ));
        }
        let previous_bytes = scope_bytes(&entries).map_err(Error::Tool)?;
        let entry = entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .ok_or_else(missing)?;
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
pub struct Scratchpad {
    store: Arc<ScratchpadStore>,
}

impl Scratchpad {
    /// Creates the gateway-wide scratchpad backed by one tenant-scoped checkpoint store.
    #[must_use]
    pub fn new(checkpoints: Arc<dyn CheckpointStore>) -> Self {
        Self {
            store: Arc::new(ScratchpadStore {
                checkpoints,
                access: Mutex::new(()),
            }),
        }
    }

    /// Returns a chat scratchpad sharing this notebook.
    #[must_use]
    pub fn for_chat(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
        }
    }

    /// Returns the persisted gateway-wide scratchpad management surface.
    /// # Errors
    ///
    /// Returns an error if the saved notes cannot be loaded.
    pub async fn global_contribution(&self) -> Result<FrontendContribution> {
        let access = self.store.access.lock().await;
        self.global_contribution_locked(&access).await
    }

    /// Runs a gateway management operation through the command dispatcher and
    /// returns the refreshed management surface.
    /// # Errors
    ///
    /// Returns [`Error::Tool`] when the operation is not a valid scratchpad command
    /// or the note change is rejected.
    pub async fn manage(&self, operation: &crate::protocol::Op) -> Result<FrontendContribution> {
        let invalid = || Error::Tool(text::DEFINITION.error_management.as_str().into());
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
        if capability != MANIFEST.id {
            return Err(invalid());
        }
        let parsed = parse_command(command, arguments, input.as_deref())?.ok_or_else(invalid)?;
        let access = self.store.access.lock().await;
        self.execute_locked(parsed, &access).await?;
        self.global_contribution_locked(&access).await
    }

    async fn global_contribution_locked(
        &self,
        access: &Access<'_>,
    ) -> Result<FrontendContribution> {
        let snapshot = self.store.snapshot_locked(access).await?;
        Ok(FrontendContribution {
            capability: MANIFEST.id.into(),
            widgets: surface_widgets(&snapshot),
            ..FrontendContribution::default()
        })
    }

    async fn snapshot(&self) -> Result<Vec<Entry>> {
        let access = self.store.access.lock().await;
        self.store.snapshot_locked(&access).await
    }

    async fn command_locked(
        &self,
        command: &str,
        arguments: &str,
        input: Option<&str>,
        access: &Access<'_>,
    ) -> Result<MiddlewareCommandOutput> {
        match parse_command(command, arguments, input)? {
            Some(parsed) => self.execute_locked(parsed, access).await,
            None => Ok(usage()),
        }
    }

    async fn execute_locked(
        &self,
        command: ScratchpadCommand<'_>,
        access: &Access<'_>,
    ) -> Result<MiddlewareCommandOutput> {
        match command {
            ScratchpadCommand::Read => {
                let snapshot = self.store.snapshot_locked(access).await?;
                Ok(MiddlewareCommandOutput::render(
                    self.name(),
                    format_snapshot(&snapshot),
                    FrontendTone::Neutral,
                ))
            }
            ScratchpadCommand::Refresh => {
                let snapshot = self.store.snapshot_locked(access).await?;
                Ok(MiddlewareCommandOutput::events(widget_events(&snapshot)))
            }
            ScratchpadCommand::Add(note) => {
                let message = match self
                    .store
                    .write_locked(note, Basis::UserConfirmed, access)
                    .await?
                {
                    WriteOutcome::Added => &text::DEFINITION.message_added,
                    WriteOutcome::Updated => &text::DEFINITION.message_updated,
                    WriteOutcome::Existing => &text::DEFINITION.message_existing,
                };
                self.command_updated(message, access).await
            }
            ScratchpadCommand::Edit(id, note) => {
                self.store.edit_locked(id, note, access).await?;
                self.command_updated(&text::DEFINITION.message_updated, access)
                    .await
            }
            ScratchpadCommand::Forget(id) => {
                self.store.forget_locked(id, access).await?;
                self.command_updated(&text::DEFINITION.message_forgot, access)
                    .await
            }
        }
    }

    async fn command_updated(
        &self,
        message: &str,
        access: &Access<'_>,
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
        catalog.register(Arc::new(WriteScratchpad {
            store: Arc::clone(&self.store),
            frontend: Arc::clone(&runtime.frontend),
        }))
    }

    fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(Some(PromptSection::new(
            text::DEFINITION.prompt_main.as_str(),
        )))
    }

    fn frontend(&self, _session_id: &str) -> FrontendContribution {
        FrontendContribution {
            capability: self.name().into(),
            count: None,
            commands: vec![FrontendCommand {
                name: "scratchpad".into(),
                arguments: text::DEFINITION.command_arguments.clone(),
                description: text::DEFINITION.command_description.clone(),
                requires_idle: false,
            }],
            widgets: surface_widgets(&[]),
            references: Vec::new(),
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        [&text::DEFINITION.write_scratchpad]
            .into_iter()
            .find_map(|spec| spec.render(event))
    }

    fn prepare_compacted_input(
        &self,
        _original: crate::backend::model::ModelInput<'_>,
        compacted: &mut Vec<Arc<serde_json::Value>>,
    ) {
        compacted.retain(|item| !is_projection_item(item));
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let snapshot = self.snapshot().await?;
            if matches!(
                context.source(),
                SessionStartSource::Startup | SessionStartSource::Compact
            ) && let Some(item) = next_projection(context.input.as_slice().into(), &snapshot)?
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
            let access = self.store.access.lock().await;
            self.command_locked(context.command, context.arguments, context.input, &access)
                .await
        })
    }

    fn active_command<'a>(
        &'a self,
        context: &'a mut ActiveCommandContext<'_>,
    ) -> BoxFuture<'a, Result<Option<SubmissionResult>>> {
        Box::pin(async move {
            let Ok(access) = self.store.access.try_lock() else {
                return Ok(None);
            };
            let output = self
                .command_locked(context.command, context.arguments, context.input, &access)
                .await?;
            context
                .events
                .extend(output.events.into_iter().map(EventMsg::Frontend));
            Ok(Some(SubmissionResult::Handled))
        })
    }

    fn pre_model<'a>(&'a self, context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
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
        if basis > entry.basis {
            entry.basis = basis;
            return Ok(WriteOutcome::Updated);
        }
        return Ok(WriteOutcome::Existing);
    }
    if entries.len() >= MAX_NOTES {
        return Err(Error::Tool(
            text::DEFINITION
                .error_note_count
                .replace("{max}", &MAX_NOTES.to_string()),
        ));
    }
    entries.push(Entry {
        id: Uuid::new_v4().to_string(),
        note,
        basis,
        created_at: chrono::Utc::now().timestamp().to_string(),
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
        return Err(text::DEFINITION
            .error_scope_budget
            .replace("{max}", &MAX_SCOPE_BYTES.to_string()));
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

fn missing() -> Error {
    Error::Tool(text::DEFINITION.error_missing.as_str().into())
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
        return Err(text::DEFINITION
            .error_note_length
            .replace("{max}", &MAX_NOTE_BYTES.to_string()));
    }
    Ok(note.into())
}

#[cfg(test)]
mod tests;
