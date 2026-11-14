//! Durable memory: a small wiki of linked Markdown pages, owned by the Bot that wrote it in one
//! project, or global for every Bot in every project. Chats see each scope's index; page bodies
//! are read on demand. While a chat sits idle, memory "dreams": it distills the chat's new
//! transcript into pages and tidies them, the way compaction runs on the same idle timer.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::tools::Catalog;
use super::{
    ActiveCommandContext, Middleware, MiddlewareCommandContext, MiddlewareCommandOutput,
    ModelContext, PromptSection, RuntimeContext, SessionStartContext, SessionStartSource,
    SubmissionResult, TurnEndContext,
};
use crate::backend::checkpoint::CheckpointStore;
use crate::protocol::{
    EventMsg, FrontendBlock, FrontendCommand, FrontendContribution, FrontendSettingValue,
    FrontendTone, SessionContext,
};
use crate::{BoxFuture, Error, Result, SessionId};

mod text {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Definition {
        pub(super) patch_memory: crate::middleware::tools::ToolSpec,
        #[serde(deserialize_with = "crate::middleware::manifest::deserialize_settings")]
        pub(super) settings: Vec<crate::middleware::manifest::MiddlewareSettingManifest>,
        pub(super) prompt_projection_header: String,
        pub(super) prompt_projection_global: String,
        pub(super) prompt_projection_owner: String,
        pub(super) prompt_projection_empty: String,
        pub(super) prompt_projection_more: String,
        pub(super) prompt_projection_footer: String,
        pub(super) prompt_changes_header: String,
        pub(super) prompt_changes_global: String,
        pub(super) prompt_changes_owner: String,
        pub(super) prompt_changes_footer: String,
        pub(super) prompt_dream: String,
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
        pub(super) error_exists: String,
        pub(super) error_management: String,
        pub(super) error_missing: String,
        pub(super) error_page_count: String,
        pub(super) manifest_description: String,
        pub(super) manifest_label: String,
        pub(super) message_added: String,
        pub(super) message_agent_observation: String,
        pub(super) message_forgot: String,
        pub(super) message_global_heading: String,
        pub(super) message_no_project: String,
        pub(super) message_no_memories: String,
        pub(super) message_patched: String,
        pub(super) message_updated: String,
        pub(super) message_user_confirmed: String,
        pub(super) prompt_main: String,
        pub(super) widget_global_title: String,
        pub(super) widget_text: String,
    }
    crate::embedded_config! { pub(super) static DEFINITION: Definition = include_str!("memory.toml") => super::limit_schema; }
}
mod dream;
mod pages;
mod presentation;
mod projection;
mod tools;

use pages::{Page, Pages, Scope};
use presentation::{format_snapshot, publish_widgets, surface_widgets, usage, widget_events};
use projection::{is_projection_item, next_projection};
use tools::PatchMemory;

const GLOBAL_DIR: &str = "global";
const GLOBAL_KEY: &str = "global";
const PROJECTION_KIND: &str = "memory";
const MAX_OPS: usize = 32;

fn limit_schema(definition: &mut text::Definition) {
    super::tools::limit_schema(
        &mut definition.patch_memory.tool,
        [
            ("/properties/ops", "maxItems", MAX_OPS.into()),
            (
                "/properties/ops/items/properties/page",
                "maxLength",
                pages::MAX_NAME_BYTES.into(),
            ),
            (
                "/properties/ops/items/properties/description",
                "maxLength",
                pages::MAX_DESCRIPTION_BYTES.into(),
            ),
            (
                "/properties/ops/items/properties/body",
                "maxLength",
                pages::MAX_BODY_BYTES.into(),
            ),
        ],
    );
}

super::manifest::middleware_manifest! {
/// Configuration and presentation metadata for durable memory.
    "memory", text::DEFINITION, required: false, settings: &text::DEFINITION.settings
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

/// A wiki link resolved by memory's scope rules, shared by tools and frontends.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryLink {
    /// The name written inside `[[name]]`.
    pub name: Arc<str>,
    /// The target page id, or `None` when no visible page has that name.
    pub target: Option<Arc<str>>,
}

/// One saved page, as a host lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryItem {
    /// The identifier, `<scope>/<name>`, unique across every scope.
    pub id: Arc<str>,
    /// The page name other pages link to with `[[name]]`.
    pub name: Arc<str>,
    /// One line on what the page covers.
    pub description: Arc<str>,
    /// The Markdown body.
    pub body: Arc<str>,
    /// Links resolved within this page's scope, then the global scope.
    pub links: Vec<MemoryLink>,
    /// Visible to every Bot in every project rather than only to `owner`.
    pub global: bool,
    /// The Bot and project that own it; a global page's writer, when a chat wrote it.
    pub owner: Option<Arc<MemoryOwner>>,
    /// Unix seconds of the last write.
    pub updated_at: u64,
}

// Declaration order is strength order: a stronger basis survives a weaker rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Basis {
    AgentObservation,
    UserConfirmed,
}

impl Basis {
    const fn as_str(self) -> &'static str {
        match self {
            Self::AgentObservation => "agent_observation",
            Self::UserConfirmed => "user_confirmed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [Self::AgentObservation, Self::UserConfirmed]
            .into_iter()
            .find(|basis| basis.as_str() == value)
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

/// One change `patch_memory` or a dream applies, validated at the tool boundary.
#[derive(Debug, PartialEq, Eq)]
struct Op {
    page: String,
    global: bool,
    change: Change,
    /// When set, the op applies only while the page is still at this version (0 = absent).
    base_updated_at: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
enum Change {
    Set { description: String, body: String },
    Delete,
}

/// Who an applied patch is recorded as.
#[derive(Clone, Copy)]
enum Writer<'a> {
    /// A chat's own `patch_memory` call.
    Chat,
    /// The chat's idle dream.
    Dream(&'a SessionId),
}

type Access<'a> = tokio::sync::MutexGuard<'a, ()>;

struct MemoryStore {
    pages: Pages,
    checkpoints: Arc<dyn CheckpointStore>,
    // Serializes read-modify-write across every scope.
    access: Mutex<()>,
}

impl MemoryStore {
    /// The global scope and `owner`'s, in that order.
    async fn visible(&self, owner: &Arc<MemoryOwner>) -> Result<[Scope; 2]> {
        Ok([
            self.pages.load(None).await?,
            self.pages.load(Some(owner)).await?,
        ])
    }

    /// Rejects conflicting batches before writing; saves replacements before deleting sources.
    async fn apply_locked(
        &self,
        owner: &Arc<MemoryOwner>,
        mut ops: Vec<Op>,
        basis: Basis,
        writer: Writer<'_>,
        _access: &Access<'_>,
    ) -> Result<Vec<String>> {
        let scopes = self.visible(owner).await?;
        validate_patch(&scopes, &ops, writer)?;
        ops.sort_by_key(|op| matches!(op.change, Change::Delete));
        let mut results = Vec::with_capacity(ops.len());
        let (mut global_changes, mut scoped_changes) = (Vec::new(), Vec::new());
        for op in ops {
            let scope_owner = (!op.global).then_some(owner);
            let [global_scope, owner_scope] = &scopes;
            let scope = if op.global { global_scope } else { owner_scope };
            let changes = if op.global {
                &mut global_changes
            } else {
                &mut scoped_changes
            };
            match op.change {
                Change::Delete => {
                    self.pages.remove(scope_owner, &op.page).await?;
                    results.push(format!("{}: deleted", op.page));
                    changes.push(format!("-{}", op.page));
                }
                Change::Set { description, body } => {
                    let existing = scope.get(&op.page);
                    let page = Page {
                        updated_at: next_version(existing),
                        basis: existing.map_or(basis, |page| page.basis.max(basis)),
                        author: op.global.then(|| Arc::clone(owner)),
                        digest: pages::Digest::default(),
                        name: op.page,
                        description,
                        body,
                    };
                    self.pages.put(scope_owner, &page).await?;
                    results.push(format!(
                        "{}: applied (updated_at {})",
                        page.name, page.updated_at
                    ));
                    changes.push(page.name);
                }
            }
        }
        let source = match writer {
            Writer::Chat => "patch".to_owned(),
            Writer::Dream(session) => format!("dream from chat {session}"),
        };
        let when = chrono::Utc::now().format("%Y-%m-%d %H:%M");
        for (global, names) in [(true, &global_changes), (false, &scoped_changes)] {
            if !names.is_empty() {
                self.pages
                    .log(
                        (!global).then_some(owner),
                        &format!("{when} {source}: {}", names.join(", ")),
                    )
                    .await?;
            }
        }
        Ok(results)
    }

    /// Sets or deletes one page a host addresses by id, as the user confirmed it.
    async fn host_locked(
        &self,
        id: &str,
        change: Option<(Option<&str>, &str)>,
        _access: &Access<'_>,
    ) -> Result<()> {
        let (scope, name) = self.pages.locate(id).await?;
        let existing = scope.get(&name).ok_or_else(missing)?;
        let owner = scope.owner.as_ref();
        let Some((description, body)) = change else {
            return self.pages.remove(owner, &name).await;
        };
        let page = Page {
            description: match description {
                Some(description) => {
                    pages::validate_description(description).map_err(Error::Tool)?
                }
                None => existing.description.clone(), // rules-allow: the rewritten page keeps its description
            },
            body: pages::canonical_body(body).map_err(Error::Tool)?,
            basis: Basis::UserConfirmed,
            updated_at: next_version(Some(existing)),
            author: existing.author.as_ref().map(Arc::clone),
            digest: pages::Digest::default(),
            name,
        };
        self.pages.put(owner, &page).await
    }

    /// Adds a page the user typed, named from its first words.
    async fn add_locked(
        &self,
        owner: Option<&Arc<MemoryOwner>>,
        author: Option<&Arc<MemoryOwner>>,
        text: &str,
        _access: &Access<'_>,
    ) -> Result<()> {
        let body = pages::canonical_body(text).map_err(Error::Tool)?;
        let first = body.lines().next().unwrap_or_default();
        let mut description = first.trim_start_matches(['#', ' ']).to_owned();
        description.truncate(description.floor_char_boundary(pages::MAX_DESCRIPTION_BYTES));
        let name = slug(&description);
        let scope = self.pages.load(owner).await?;
        if scope.get(&name).is_some() {
            return Err(Error::Tool(
                text::DEFINITION.error_exists.replace("{page}", &name),
            ));
        }
        if scope.pages.len() >= pages::MAX_PAGES {
            return Err(Error::Tool(
                text::DEFINITION
                    .error_page_count
                    .replace("{max}", &pages::MAX_PAGES.to_string()),
            ));
        }
        let page = Page {
            name,
            description: pages::validate_description(&description).map_err(Error::Tool)?,
            body,
            basis: Basis::UserConfirmed,
            updated_at: next_version(None),
            author: owner.is_none().then_some(author).flatten().map(Arc::clone),
            digest: pages::Digest::default(),
        };
        self.pages.put(owner, &page).await
    }
}

/// Checks the freshly read snapshot once, including the resulting scope sizes.
fn validate_patch(scopes: &[Scope; 2], ops: &[Op], writer: Writer<'_>) -> Result<()> {
    let [global_scope, owner_scope] = scopes;
    let (mut global_count, mut owner_count) = (global_scope.pages.len(), owner_scope.pages.len());
    let mut names = BTreeSet::new();
    for op in ops {
        let (scope, count) = if op.global {
            (global_scope, &mut global_count)
        } else {
            (owner_scope, &mut owner_count)
        };
        let existing = scope.get(&op.page);
        if !names.insert((op.global, &op.page)) {
            return Err(Error::Tool("a batch can change each page only once".into()));
        }
        let reason = match (op.base_updated_at, existing) {
            (None, _) if matches!(writer, Writer::Dream(_)) => Some("base_updated_at is required"),
            (Some(0), Some(_)) => Some("the page already exists"),
            (Some(base), Some(page)) if page.updated_at != base => {
                Some("the page changed since it was read")
            }
            (Some(1..), None) => Some("the page no longer exists"),
            _ => None,
        };
        if let Some(reason) = reason {
            return Err(Error::Tool(format!(
                "{}: batch rejected, {reason}",
                op.page
            )));
        }
        match (&op.change, existing) {
            (Change::Delete, None) => return Err(missing()),
            (Change::Delete, Some(_)) => *count -= 1,
            (Change::Set { .. }, None) => *count += 1,
            (Change::Set { .. }, Some(_)) => {}
        }
    }
    if [global_count, owner_count]
        .into_iter()
        .any(|count| count > pages::MAX_PAGES)
    {
        return Err(Error::Tool(
            text::DEFINITION
                .error_page_count
                .replace("{max}", &pages::MAX_PAGES.to_string()),
        ));
    }
    Ok(())
}

/// A strictly increasing version, so `base_updated_at` detects every rewrite.
fn next_version(existing: Option<&Page>) -> u64 {
    now().max(existing.map_or(0, |page| page.updated_at + 1))
}

fn slug(text: &str) -> String {
    let words = text
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(6)
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("-");
    let mut name = if words.is_empty() {
        "note".to_owned()
    } else {
        words
    };
    name.truncate(pages::MAX_NAME_BYTES);
    name.trim_end_matches('-').to_owned()
}

/// Read access to the pages a chat can see, for `search_memory` and `read_memory`.
pub struct MemoryPages {
    store: Arc<MemoryStore>,
}

impl std::fmt::Debug for MemoryPages {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemoryPages")
            .finish_non_exhaustive()
    }
}

impl MemoryPages {
    /// Another handle on the same store.
    #[must_use]
    pub fn share(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
        }
    }

    /// Every page `owner`'s chats can see, global first, each scope newest first.
    pub(crate) async fn documents(&self, owner: &Arc<MemoryOwner>) -> Result<Vec<MemoryItem>> {
        let visible = {
            let _access = self.store.access.lock().await;
            self.store.visible(owner).await?
        };
        items(&visible)
    }

    pub(crate) async fn read(
        &self,
        owner: &Arc<MemoryOwner>,
        name: &str,
        global: bool,
    ) -> Result<String> {
        let documents = self.documents(owner).await?;
        let page = documents
            .iter()
            .find(|document| &*document.name == name && document.global == global)
            .ok_or_else(|| Error::Tool(format!("no memory page `{name}` in that scope")))?;
        let backlinks = documents
            .iter()
            .filter(|document| {
                document
                    .links
                    .iter()
                    .any(|link| link.target.as_ref() == Some(&page.id))
            })
            .map(|document| serde_json::json!({"page": document.name, "global": document.global}))
            .collect::<Vec<_>>();
        Ok(serde_json::json!({
            "page": page.name, "global": page.global, "description": page.description,
            "updated_at": page.updated_at, "body": page.body, "links": page.links,
            "backlinks": backlinks,
        })
        .to_string())
    }
}

/// Durable memory pages, without exposing persistence details to the agent loop.
///
/// [`Memory::new`] is the host's handle on the store; [`Memory::for_chat`] is the middleware
/// one chat installs. A chat's owner (Bot and project) comes from its session context.
pub struct Memory {
    store: Arc<MemoryStore>,
    owner: OnceLock<Arc<MemoryOwner>>,
    dream_after: Option<Duration>,
    // Chats with a normal turn since their last dream.
    dreamable: std::sync::Mutex<BTreeSet<SessionId>>,
}

impl std::fmt::Debug for Memory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Memory")
            .field("dream_after", &self.dream_after)
            .finish_non_exhaustive()
    }
}

impl Memory {
    /// Creates the store: pages under `root`, dream progress in `checkpoints`.
    #[must_use]
    pub fn new(checkpoints: Arc<dyn CheckpointStore>, root: PathBuf) -> Self {
        Self {
            store: Arc::new(MemoryStore {
                pages: Pages::new(root),
                checkpoints,
                access: Mutex::new(()),
            }),
            owner: OnceLock::new(),
            dream_after: None,
            dreamable: std::sync::Mutex::default(),
        }
    }

    /// Returns a chat's memory middleware sharing this store, configured from this module's
    /// declared settings with embedded defaults for unset values.
    /// # Errors
    ///
    /// Returns an error when a setting has the wrong type.
    pub fn for_chat<'a>(
        &self,
        setting: impl Fn(&str) -> Option<&'a FrontendSettingValue>,
    ) -> Result<Self> {
        let dreaming = match setting("dreaming") {
            None => super::manifest::string_default(&text::DEFINITION.settings, "dreaming") == "on",
            Some(FrontendSettingValue::String(value)) if value == "on" => true,
            Some(FrontendSettingValue::String(value)) if value == "off" => false,
            Some(_) => return Err(Error::Config("memory.dreaming must be on or off".into())),
        };
        let minutes = match setting("dream_minutes") {
            None => super::manifest::integer_default(&text::DEFINITION.settings, "dream_minutes"),
            Some(FrontendSettingValue::Integer(minutes)) => *minutes,
            Some(_) => {
                return Err(Error::Config(
                    "memory.dream_minutes must be an integer".into(),
                ));
            }
        };
        Ok(Self {
            store: Arc::clone(&self.store),
            owner: OnceLock::new(),
            dream_after: dreaming.then(|| {
                Duration::from_secs(u64::try_from(minutes).unwrap_or(1).saturating_mul(60))
            }),
            dreamable: std::sync::Mutex::default(),
        })
    }

    /// Read access to visible pages for the chat history tools.
    #[must_use]
    pub fn pages(&self) -> MemoryPages {
        MemoryPages {
            store: Arc::clone(&self.store),
        }
    }

    /// Every saved page, scope by scope.
    /// # Errors
    ///
    /// Returns an error if the saved pages cannot be loaded.
    pub async fn list(&self) -> Result<Vec<MemoryItem>> {
        let _access = self.store.access.lock().await;
        items(&self.store.pages.all().await?)
    }

    /// Replaces the description and body of the page `id`, as the user confirmed them.
    /// # Errors
    ///
    /// Returns [`Error::Tool`] for an unknown id or rejected text.
    pub async fn edit(&self, id: &str, description: &str, body: &str) -> Result<()> {
        let access = self.store.access.lock().await;
        self.store
            .host_locked(id, Some((Some(description), body)), &access)
            .await
    }

    /// Deletes the page `id` into its scope's trash.
    /// # Errors
    ///
    /// Returns [`Error::Tool`] for an unknown id.
    pub async fn forget(&self, id: &str) -> Result<()> {
        let access = self.store.access.lock().await;
        self.store.host_locked(id, None, &access).await
    }

    /// Returns the host's memory management surface.
    /// # Errors
    ///
    /// Returns an error if the saved pages cannot be loaded.
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
        let all = items(&self.store.pages.all().await?)?;
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

    fn dreamable(&self) -> Result<std::sync::MutexGuard<'_, BTreeSet<SessionId>>> {
        self.dreamable
            .lock()
            .map_err(|_poisoned| Error::Config("memory dream state is poisoned".into()))
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
        match command {
            MemoryCommand::Read => {
                let text = match owner {
                    Some(owner) => format_snapshot(&self.store.visible(owner).await?)?,
                    None => format_snapshot(&self.store.pages.all().await?)?,
                };
                Ok(MiddlewareCommandOutput::render(
                    self.name(),
                    text,
                    FrontendTone::Neutral,
                ))
            }
            MemoryCommand::Refresh => Ok(MiddlewareCommandOutput::events(widget_events(&items(
                &self.store.pages.all().await?,
            )?))),
            MemoryCommand::Add(text) => {
                self.store.add_locked(owner, owner, text, access).await?;
                self.command_updated(&text::DEFINITION.message_added).await
            }
            MemoryCommand::Edit(id, body) => {
                self.visible_id(owner, id).await?;
                self.store
                    .host_locked(id, Some((None, body)), access)
                    .await?;
                self.command_updated(&text::DEFINITION.message_updated)
                    .await
            }
            MemoryCommand::Forget(id) => {
                self.visible_id(owner, id).await?;
                self.store.host_locked(id, None, access).await?;
                self.command_updated(&text::DEFINITION.message_forgot).await
            }
        }
    }

    /// A chat's command reaches only the pages that chat can see; the host reaches all.
    async fn visible_id(&self, owner: Option<&Arc<MemoryOwner>>, id: &str) -> Result<()> {
        let Some(owner) = owner else {
            return Ok(());
        };
        let (scope, _) = self.store.pages.locate(id).await?;
        let visible = match &scope.owner {
            None => true,
            Some(scope) => key(Some(scope))? == key(Some(owner))?,
        };
        visible.then_some(()).ok_or_else(missing)
    }

    async fn command_updated(&self, message: &str) -> Result<MiddlewareCommandOutput> {
        let mut events = widget_events(&items(&self.store.pages.all().await?)?);
        events.extend(
            MiddlewareCommandOutput::render(self.name(), message, FrontendTone::Success).events,
        );
        Ok(MiddlewareCommandOutput::events(events))
    }
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
        ("add", None, Some(text)) => Some(MemoryCommand::Add(text)),
        ("edit", Some(id), Some(body)) => Some(MemoryCommand::Edit(id, body)),
        ("forget", Some(id), None) => Some(MemoryCommand::Forget(id)),
        _ => None,
    })
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
        catalog.register(Arc::new(PatchMemory {
            store: Arc::clone(&self.store),
            owner: Arc::clone(self.owner()?),
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
        text::DEFINITION.patch_memory.render(event)
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
                    self.store.pages.all().await?,
                )
            };
            if matches!(
                context.source(),
                SessionStartSource::Startup | SessionStartSource::Compact
            ) && let Some(item) = next_projection(context.input.as_slice().into(), &visible)
            {
                context.push_input(item);
            }
            if context.source() != SessionStartSource::Compact {
                publish_widgets(&context.runtime.frontend, &items(&all)?)?;
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
            if let Some(item) = next_projection(context.input(), &visible) {
                context.append_model_input(item);
            }
            Ok(())
        })
    }

    fn turn_end<'a>(&'a self, context: &'a mut TurnEndContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.dream_after.is_some() && !context.is_idle() {
                self.dreamable()?.insert(context.session_id.share());
            }
            Ok(())
        })
    }

    // No `has_background_work`: a pending dream must not hold the chat busy or the gateway
    // awake. If the chat unloads first, the watermark lets the next dream catch up.
    fn idle_after(&self, session_id: &SessionId) -> Option<Duration> {
        let dreamable = self.dreamable.lock().ok()?.contains(session_id);
        dreamable.then_some(self.dream_after).flatten()
    }

    fn idle<'a>(&'a self, context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // The stack calls every middleware's idle hook; dream only when this chat is due.
            if self.dreamable()?.remove(context.session_id) {
                dream::run(self, context).await;
            }
            Ok(())
        })
    }

    fn session_end<'a>(&'a self, runtime: &'a RuntimeContext) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.dreamable()?.remove(&runtime.session_id);
            Ok(())
        })
    }
}

fn items(scopes: &[Scope]) -> Result<Vec<MemoryItem>> {
    let directories = scopes
        .iter()
        .map(Scope::dir_name)
        .collect::<Result<Vec<_>>>()?;
    let global = scopes
        .iter()
        .zip(&directories)
        .find(|(scope, _)| scope.owner.is_none());
    let mut items = Vec::new();
    for (scope, directory) in scopes.iter().zip(&directories) {
        for page in scope.pages.iter() {
            let mut links = Vec::<MemoryLink>::new();
            for name in page.links() {
                if links.iter().any(|link| &*link.name == name) {
                    continue;
                }
                let target = [Some((scope, directory)), global]
                    .into_iter()
                    .flatten()
                    .find(|(scope, _)| scope.get(name).is_some())
                    .map(|(_, directory)| format!("{directory}/{name}").into());
                links.push(MemoryLink {
                    name: name.into(),
                    target,
                });
            }
            items.push(MemoryItem {
                id: format!("{directory}/{}", page.name).into(),
                links,
                name: page.name.as_str().into(),
                description: page.description.as_str().into(),
                body: page.body.as_str().into(),
                global: scope.owner.is_none(),
                owner: scope
                    .owner
                    .as_ref()
                    .or(page.author.as_ref())
                    .map(Arc::clone),
                updated_at: page.updated_at,
            });
        }
    }
    Ok(items)
}

fn now() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp()).unwrap_or_default()
}

fn missing() -> Error {
    Error::Tool(text::DEFINITION.error_missing.as_str().into())
}

#[cfg(test)]
mod tests;
