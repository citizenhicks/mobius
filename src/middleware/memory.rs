//! Durable memories: one concise fact each, owned by the Bot that wrote it in one project, or
//! global for every Bot in every project.

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

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
    EventMsg, FrontendBlock, FrontendCommand, FrontendContribution, FrontendTone, SessionContext,
};
use crate::{BoxFuture, Error, Result};

mod text {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Definition {
        pub(super) write_memory: crate::middleware::tools::ToolSpec,
        pub(super) forget_memory: crate::middleware::tools::ToolSpec,
        pub(super) prompt_projection_header: String,
        pub(super) prompt_projection_global: String,
        pub(super) prompt_projection_owner: String,
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
        pub(super) error_global_update: String,
        pub(super) error_management: String,
        pub(super) error_missing: String,
        pub(super) error_memory_count: String,
        pub(super) error_memory_length: String,
        pub(super) error_scope_budget: String,
        pub(super) manifest_description: String,
        pub(super) manifest_label: String,
        pub(super) message_added: String,
        pub(super) message_agent_observation: String,
        pub(super) message_existing: String,
        pub(super) message_forgot: String,
        pub(super) message_global_heading: String,
        pub(super) message_no_project: String,
        pub(super) message_no_memories: String,
        pub(super) message_updated: String,
        pub(super) message_user_confirmed: String,
        pub(super) prompt_main: String,
        pub(super) widget_global_title: String,
        pub(super) widget_text: String,
    }
    crate::embedded_config! { pub(super) static DEFINITION: Definition = include_str!("memory.toml") => super::limit_schema; }
}
mod presentation;
mod projection;
mod tools;

use presentation::{format_snapshot, publish_widgets, surface_widgets, usage, widget_events};
use projection::is_projection_item;
use projection::next_projection;
use tools::{ForgetMemory, WriteMemory};

const SCOPE: &str = "memory";
const GLOBAL_KEY: &str = "global";
const MAX_MEMORIES: usize = 20;
const MAX_MEMORY_BYTES: usize = 500;

fn limit_schema(definition: &mut text::Definition) {
    super::tools::limit_schema(
        &mut definition.write_memory.tool,
        [("/properties/memory", "maxLength", MAX_MEMORY_BYTES.into())],
    );
}
// Two full scopes with their ids, plus the headers.
const MAX_INJECTION_BYTES: usize = 8 * 1024;
const MAX_SCOPE_BYTES: usize = 1_900;
const PROJECTION_KIND: &str = "memory";

super::manifest::middleware_manifest! {
/// Configuration and presentation metadata for durable memories.
    "memory", text::DEFINITION, required: false, settings: &[]
}

/// The project a memory belongs to: the workspace of the chat that wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryProject {
    /// The opaque workspace identifier.
    pub id: Arc<str>,
    /// The workspace label when the chat had one.
    pub label: Option<Arc<str>>,
}

/// The Bot and project a non-global memory is visible to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryOwner {
    /// The owning Bot (the session owner).
    pub bot: Arc<str>,
    /// The project, or `None` for chats without one.
    pub project: Option<MemoryProject>,
}

impl MemoryOwner {
    /// The owner a chat with `context` writes as: its session owner and workspace.
    #[must_use]
    pub fn of(context: &SessionContext) -> Self {
        Self {
            bot: Arc::clone(&context.owner_id),
            project: context.workspace_id.as_deref().map(|id| MemoryProject {
                id: id.into(),
                label: context.workspace_label.as_deref().map(Into::into),
            }),
        }
    }
}

/// One saved memory, as a host lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryItem {
    /// The identifier.
    pub id: Arc<str>,
    /// The memory.
    pub text: Arc<str>,
    /// Visible to every Bot in every project rather than only to `owner`.
    pub global: bool,
    /// The Bot and project that own it; a global memory's writer, when a chat wrote it.
    pub owner: Option<Arc<MemoryOwner>>,
    /// Unix seconds of the last write.
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    text: String,
    basis: Basis,
    updated_at: u64,
    // The chat that wrote a global memory; scoped memories belong to their scope's owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    author: Option<Arc<MemoryOwner>>,
}

// Declaration order is strength order: a stronger basis replaces a weaker one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Basis {
    AgentObservation,
    UserConfirmed,
}

/// One stored scope: every memory of one owner, or the global memories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Partition {
    owner: Option<Arc<MemoryOwner>>,
    memories: Vec<Entry>,
}

impl Partition {
    const fn empty(owner: Option<Arc<MemoryOwner>>) -> Self {
        Self {
            owner,
            memories: Vec::new(),
        }
    }

    fn key(&self) -> Result<String> {
        key(self.owner.as_deref())
    }
}

fn key(owner: Option<&MemoryOwner>) -> Result<String> {
    Ok(match owner {
        None => GLOBAL_KEY.into(),
        Some(owner) => serde_json::to_string(&(
            &*owner.bot,
            owner.project.as_ref().map(|project| &*project.id),
        ))?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOutcome {
    Added,
    Updated,
    Existing,
}

/// Which saved memories an operation may find by id.
#[derive(Clone, Copy)]
enum Reach<'a> {
    /// The global memories and this owner's.
    Visible(&'a Arc<MemoryOwner>),
    /// Every memory, for the host's management surface.
    All,
}

enum MemoryCommand<'a> {
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
) -> Result<Option<MemoryCommand<'a>>> {
    if command != "memory" {
        return Err(Error::Unknown(format!("memory command `{command}`")));
    }
    let mut arguments = arguments.split_whitespace();
    let operation = arguments.next().unwrap_or("read");
    let id = arguments.next();
    if arguments.next().is_some() {
        return Ok(None);
    }
    Ok(match (operation, id, input) {
        ("read", None, None) => Some(MemoryCommand::Read),
        ("refresh", None, None) => Some(MemoryCommand::Refresh),
        ("add", None, Some(memory)) => Some(MemoryCommand::Add(memory)),
        ("edit", Some(id), Some(memory)) => Some(MemoryCommand::Edit(id, memory)),
        ("forget", Some(id), None) => Some(MemoryCommand::Forget(id)),
        _ => None,
    })
}

type Access<'a> = tokio::sync::MutexGuard<'a, ()>;

struct MemoryStore {
    checkpoints: Arc<dyn CheckpointStore>,
    // Serializes whole-value updates across every scope.
    access: Mutex<()>,
}

impl MemoryStore {
    /// Saves `memory` in `owner`'s scope, or in the global scope as written by `author`.
    async fn write_locked(
        &self,
        owner: Option<&Arc<MemoryOwner>>,
        author: Option<&Arc<MemoryOwner>>,
        memory: &str,
        basis: Basis,
        _access: &Access<'_>,
    ) -> Result<WriteOutcome> {
        let memory = canonical_memory(memory).map_err(Error::Tool)?;
        let mut partition = self.load(owner).await?;
        let author = author.filter(|_| owner.is_none()).map(Arc::clone);
        let outcome = insert(&mut partition.memories, memory, basis, author)?;
        if outcome != WriteOutcome::Existing {
            validate_scope_budget(&partition.memories).map_err(Error::Tool)?;
            self.save(&partition).await?;
        }
        Ok(outcome)
    }

    async fn forget_locked(&self, reach: Reach<'_>, id: &str, access: &Access<'_>) -> Result<()> {
        let mut partition = self.find_locked(reach, id, access).await?;
        partition.memories.retain(|entry| entry.id != id);
        self.save(&partition).await
    }

    async fn edit_locked(
        &self,
        reach: Reach<'_>,
        id: &str,
        memory: &str,
        basis: Basis,
        access: &Access<'_>,
    ) -> Result<()> {
        let memory = canonical_memory(memory).map_err(Error::Tool)?;
        let mut partition = self.find_locked(reach, id, access).await?;
        let entries = &mut partition.memories;
        if entries
            .iter()
            .any(|entry| entry.id != id && entry.text == memory)
        {
            return Err(Error::Tool(
                text::DEFINITION.error_duplicate.as_str().into(),
            ));
        }
        let previous_bytes = scope_bytes(entries).map_err(Error::Tool)?;
        let entry = entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .ok_or_else(missing)?;
        entry.text = memory;
        entry.basis = entry.basis.max(basis);
        entry.updated_at = now();
        if scope_bytes(entries).map_err(Error::Tool)? > previous_bytes {
            validate_scope_budget(entries).map_err(Error::Tool)?;
        }
        self.save(&partition).await
    }

    /// The scope within `reach` that holds `id`.
    async fn find_locked(
        &self,
        reach: Reach<'_>,
        id: &str,
        _access: &Access<'_>,
    ) -> Result<Partition> {
        validate_id(id).map_err(Error::Tool)?;
        let candidates = match reach {
            Reach::Visible(owner) => Vec::from(self.visible(owner).await?),
            Reach::All => self.all().await?,
        };
        candidates
            .into_iter()
            .find(|partition| partition.memories.iter().any(|entry| entry.id == id))
            .ok_or_else(missing)
    }

    /// The global scope and `owner`'s, in that order.
    async fn visible(&self, owner: &Arc<MemoryOwner>) -> Result<[Partition; 2]> {
        Ok([self.load(None).await?, self.load(Some(owner)).await?])
    }

    async fn load(&self, owner: Option<&Arc<MemoryOwner>>) -> Result<Partition> {
        let key = key(owner.map(AsRef::as_ref))?;
        let Some(value) = self.checkpoints.load_state(SCOPE, &key).await? else {
            return Ok(Partition::empty(owner.map(Arc::clone)));
        };
        parse_partition(&key, value)
    }

    async fn all(&self) -> Result<Vec<Partition>> {
        self.checkpoints
            .list_state(SCOPE)
            .await?
            .into_iter()
            .map(|(key, value)| parse_partition(&key, value))
            .collect()
    }

    async fn save(&self, partition: &Partition) -> Result<()> {
        self.checkpoints
            .save_state(SCOPE, &partition.key()?, &serde_json::to_value(partition)?)
            .await
    }
}

fn parse_partition(key: &str, value: serde_json::Value) -> Result<Partition> {
    let partition: Partition =
        serde_json::from_value(value).map_err(|error| invalid_state(&error.to_string()))?;
    if partition.key()? != key {
        return Err(invalid_state("stored owner does not match its key"));
    }
    validate_entries(&partition.memories).map_err(|error| invalid_state(&error))?;
    Ok(partition)
}

fn invalid_state(error: &str) -> Error {
    Error::Checkpoint(format!("invalid memory state: {error}"))
}

/// Bounded durable memories, without exposing persistence details to the agent loop.
///
/// [`Memory::new`] is the host's handle on the store; [`Memory::for_chat`] is the middleware
/// one chat installs. A chat's owner (Bot and project) comes from its session context.
pub struct Memory {
    store: Arc<MemoryStore>,
    owner: OnceLock<Arc<MemoryOwner>>,
}

impl std::fmt::Debug for Memory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Memory").finish_non_exhaustive()
    }
}

impl Memory {
    /// Creates the memory store backed by one tenant-scoped checkpoint store.
    #[must_use]
    pub fn new(checkpoints: Arc<dyn CheckpointStore>) -> Self {
        Self {
            store: Arc::new(MemoryStore {
                checkpoints,
                access: Mutex::new(()),
            }),
            owner: OnceLock::new(),
        }
    }

    /// Returns a chat's memory middleware sharing this store.
    #[must_use]
    pub fn for_chat(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            owner: OnceLock::new(),
        }
    }

    /// Every saved memory, scope by scope.
    /// # Errors
    ///
    /// Returns an error if the saved memories cannot be loaded.
    pub async fn list(&self) -> Result<Vec<MemoryItem>> {
        let _access = self.store.access.lock().await;
        Ok(items(self.store.all().await?))
    }

    /// Replaces the text of the memory `id`, as the user confirmed it.
    /// # Errors
    ///
    /// Returns [`Error::Tool`] for an unknown id or a rejected text.
    pub async fn edit(&self, id: &str, memory: &str) -> Result<()> {
        let access = self.store.access.lock().await;
        self.store
            .edit_locked(Reach::All, id, memory, Basis::UserConfirmed, &access)
            .await
    }

    /// Forgets the memory `id`.
    /// # Errors
    ///
    /// Returns [`Error::Tool`] for an unknown id.
    pub async fn forget(&self, id: &str) -> Result<()> {
        let access = self.store.access.lock().await;
        self.store.forget_locked(Reach::All, id, &access).await
    }

    /// Returns the host's memory management surface.
    /// # Errors
    ///
    /// Returns an error if the saved memories cannot be loaded.
    pub async fn global_contribution(&self) -> Result<FrontendContribution> {
        let access = self.store.access.lock().await;
        self.global_contribution_locked(&access).await
    }

    /// Runs a host management operation through the command dispatcher and
    /// returns the refreshed management surface.
    /// # Errors
    ///
    /// Returns [`Error::Tool`] when the operation is not a valid memory command
    /// or the change is rejected.
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
        if &**capability != MANIFEST.id {
            return Err(invalid());
        }
        let parsed = parse_command(command, arguments, input.as_deref())?.ok_or_else(invalid)?;
        let access = self.store.access.lock().await;
        self.execute_locked(parsed, None, &access).await?;
        self.global_contribution_locked(&access).await
    }

    async fn global_contribution_locked(
        &self,
        _access: &Access<'_>,
    ) -> Result<FrontendContribution> {
        let all = items(self.store.all().await?);
        Ok(FrontendContribution {
            capability: MANIFEST.id.into(),
            widgets: surface_widgets(&all),
            ..FrontendContribution::default()
        })
    }

    fn owner(&self) -> Result<&Arc<MemoryOwner>> {
        self.owner
            .get()
            .ok_or_else(|| Error::Config("memory is not registered for a chat".into()))
    }

    async fn command_locked(
        &self,
        command: &str,
        arguments: &str,
        input: Option<&str>,
        access: &Access<'_>,
    ) -> Result<MiddlewareCommandOutput> {
        match parse_command(command, arguments, input)? {
            Some(parsed) => {
                self.execute_locked(parsed, Some(self.owner()?), access)
                    .await
            }
            None => Ok(usage()),
        }
    }

    /// Runs one command for a chat's `owner`, or for the host when `None`.
    async fn execute_locked(
        &self,
        command: MemoryCommand<'_>,
        owner: Option<&Arc<MemoryOwner>>,
        access: &Access<'_>,
    ) -> Result<MiddlewareCommandOutput> {
        let reach = owner.map_or(Reach::All, Reach::Visible);
        match command {
            MemoryCommand::Read => {
                let text = match owner {
                    Some(owner) => format_snapshot(&self.store.visible(owner).await?),
                    None => format_snapshot(&self.store.all().await?),
                };
                Ok(MiddlewareCommandOutput::render(
                    self.name(),
                    text,
                    FrontendTone::Neutral,
                ))
            }
            MemoryCommand::Refresh => Ok(MiddlewareCommandOutput::events(widget_events(&items(
                self.store.all().await?,
            )))),
            MemoryCommand::Add(memory) => {
                let message = match self
                    .store
                    .write_locked(owner, None, memory, Basis::UserConfirmed, access)
                    .await?
                {
                    WriteOutcome::Added => &text::DEFINITION.message_added,
                    WriteOutcome::Updated => &text::DEFINITION.message_updated,
                    WriteOutcome::Existing => &text::DEFINITION.message_existing,
                };
                self.command_updated(message).await
            }
            MemoryCommand::Edit(id, memory) => {
                self.store
                    .edit_locked(reach, id, memory, Basis::UserConfirmed, access)
                    .await?;
                self.command_updated(&text::DEFINITION.message_updated)
                    .await
            }
            MemoryCommand::Forget(id) => {
                self.store.forget_locked(reach, id, access).await?;
                self.command_updated(&text::DEFINITION.message_forgot).await
            }
        }
    }

    async fn command_updated(&self, message: &str) -> Result<MiddlewareCommandOutput> {
        let mut events = widget_events(&items(self.store.all().await?));
        events.extend(
            MiddlewareCommandOutput::render(self.name(), message, FrontendTone::Success).events,
        );
        Ok(MiddlewareCommandOutput::events(events))
    }
}

impl Middleware for Memory {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        let owner = Arc::new(MemoryOwner::of(&runtime.session_context));
        if let Err(owner) = self.owner.set(owner)
            && self.owner.get() != Some(&owner)
        {
            return Err(Error::Config(
                "one memory middleware serves one Bot and project".into(),
            ));
        }
        let owner = self.owner()?;
        catalog.register(Arc::new(WriteMemory {
            store: Arc::clone(&self.store),
            owner: Arc::clone(owner),
            frontend: Arc::clone(&runtime.frontend),
        }))?;
        catalog.register(Arc::new(ForgetMemory {
            store: Arc::clone(&self.store),
            owner: Arc::clone(owner),
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
            count_label: None,
            commands: vec![FrontendCommand {
                name: "memory".into(),
                arguments: text::DEFINITION.command_arguments.clone().into(), // rules-allow: owned frontend record built from static text
                description: text::DEFINITION.command_description.clone().into(), // rules-allow: owned frontend record built from static text
                requires_idle: false,
            }],
            widgets: surface_widgets(&[]),
            references: Vec::new(),
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        [
            &text::DEFINITION.write_memory,
            &text::DEFINITION.forget_memory,
        ]
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
            let (visible, all) = {
                let _access = self.store.access.lock().await;
                (
                    self.store.visible(self.owner()?).await?,
                    self.store.all().await?,
                )
            };
            if matches!(
                context.source(),
                SessionStartSource::Startup | SessionStartSource::Compact
            ) && let Some(item) = next_projection(context.input.as_slice().into(), &visible)?
            {
                context.push_input(item);
            }
            if context.source() != SessionStartSource::Compact {
                publish_widgets(&context.runtime.frontend, &items(all))?;
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
            let visible = {
                let _access = self.store.access.lock().await;
                self.store.visible(self.owner()?).await?
            };
            if let Some(item) = next_projection(context.input(), &visible)? {
                context.append_model_input(item);
            }
            Ok(())
        })
    }
}

fn items(partitions: Vec<Partition>) -> Vec<MemoryItem> {
    partitions
        .into_iter()
        .flat_map(|partition| {
            let owner = partition.owner;
            partition.memories.into_iter().map(move |entry| MemoryItem {
                id: entry.id.into(),
                text: entry.text.into(),
                global: owner.is_none(),
                owner: owner.as_ref().map(Arc::clone).or(entry.author),
                updated_at: entry.updated_at,
            })
        })
        .collect()
}

fn now() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp()).unwrap_or_default()
}

fn insert(
    entries: &mut Vec<Entry>,
    memory: String,
    basis: Basis,
    author: Option<Arc<MemoryOwner>>,
) -> Result<WriteOutcome> {
    if let Some(entry) = entries.iter_mut().find(|entry| entry.text == memory) {
        if basis > entry.basis {
            entry.basis = basis;
            entry.updated_at = now();
            return Ok(WriteOutcome::Updated);
        }
        return Ok(WriteOutcome::Existing);
    }
    if entries.len() >= MAX_MEMORIES {
        return Err(Error::Tool(
            text::DEFINITION
                .error_memory_count
                .replace("{max}", &MAX_MEMORIES.to_string()),
        ));
    }
    entries.push(Entry {
        id: Uuid::new_v4().to_string(),
        text: memory,
        basis,
        updated_at: now(),
        author,
    });
    Ok(WriteOutcome::Added)
}

fn validate_entries(entries: &[Entry]) -> std::result::Result<(), String> {
    if entries.len() > MAX_MEMORIES {
        return Err(format!("memory count exceeds {MAX_MEMORIES}"));
    }
    let mut ids = BTreeSet::new();
    let mut texts = BTreeSet::new();
    for entry in entries {
        validate_id(&entry.id)?;
        if canonical_memory(&entry.text)? != entry.text {
            return Err("stored memory is not canonical".into());
        }
        if !ids.insert(entry.id.as_str()) {
            return Err("duplicate memory ID".into());
        }
        if !texts.insert(entry.text.as_str()) {
            return Err("duplicate memory content".into());
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
            serde_json::to_string(&entry.text).map(|text| bytes + text.len() + 3)
        })
        .map_err(|error| error.to_string())
}

fn missing() -> Error {
    Error::Tool(text::DEFINITION.error_missing.as_str().into())
}

fn validate_id(id: &str) -> std::result::Result<(), String> {
    Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|error| format!("invalid memory ID: {error}"))
}

fn canonical_memory(memory: &str) -> std::result::Result<String, String> {
    let memory = memory.replace("\r\n", "\n").replace('\r', "\n");
    let memory = memory.trim();
    if memory.is_empty() || memory.len() > MAX_MEMORY_BYTES {
        return Err(text::DEFINITION
            .error_memory_length
            .replace("{max}", &MAX_MEMORY_BYTES.to_string()));
    }
    Ok(memory.into())
}

#[cfg(test)]
mod tests;
