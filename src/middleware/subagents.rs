//! Durable asynchronous child-agent middleware.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use super::ActiveCommandContext;
use super::Middleware;
use super::MiddlewareCommandContext;
use super::MiddlewareCommandOutput;
use super::ModelContext;
use super::PromptSection;
use super::RuntimeContext;
use super::SessionStartContext;
use super::SessionStartSource;
use super::SubmissionResult;
use super::manifest::MiddlewareSettingManifest;
use super::tools::Catalog;
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::agent::{Agent, AgentRole, ChildAgents};
use crate::backend::checkpoint::Checkpoint;
use crate::backend::checkpoint::CheckpointStore;
use crate::backend::model::internal_user_message;
use crate::protocol::EventMsg;
use crate::protocol::FrontendBlock;
use crate::protocol::FrontendBlockUpdate;
use crate::protocol::FrontendCommand;
use crate::protocol::FrontendContribution;
use crate::protocol::FrontendEvent;
use crate::protocol::FrontendPreviewUpdate;
use crate::protocol::MessageAuthor;
use crate::protocol::Op;
use crate::protocol::internal_message_kind;
use crate::protocol::message_metadata;

use self::runtime::Shared;

mod runtime;
mod tools;

use self::tools::{InterruptAgent, ListAgents, SendMessage, SpawnAgent, WaitAgent, fork_context};
#[cfg(test)]
use self::tools::{cleanup_error, supervise, wait_definition, wait_timeout};

const MAX_TASK_NAME_BYTES: usize = 64;
const IDENTITY_KEY: &str = "subagents.identity";
const SPAWN_CONTEXT_KEY: &str = "subagents.spawn_context";
mod text {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Definition {
        pub(super) wait_agent: crate::middleware::tools::ToolSpec,
        pub(super) interrupt_agent: crate::middleware::tools::ToolSpec,
        pub(super) list_agents: crate::middleware::tools::ToolSpec,
        pub(super) send_message: crate::middleware::tools::ToolSpec,
        pub(super) spawn_agent: crate::middleware::tools::ToolSpec,
        #[serde(deserialize_with = "crate::middleware::manifest::deserialize_settings")]
        pub(super) settings: Vec<crate::middleware::manifest::MiddlewareSettingManifest>,
        pub(super) default_enabled: bool,
        pub(super) approval_denied: String,
        pub(super) command_description: String,
        pub(super) defaults_wait_ms: u64,
        pub(super) error_empty_text: String,
        pub(super) error_fork_turns: String,
        pub(super) error_task_name: String,
        pub(super) error_title: String,
        pub(super) fork_all: String,
        pub(super) fork_last: String,
        pub(super) fork_last_one: String,
        pub(super) fork_none: String,
        pub(super) manifest_description: String,
        pub(super) manifest_label: String,
        pub(super) prompt_child: String,
        pub(super) prompt_root: String,
        pub(super) render_open: String,
        pub(super) render_open_empty: String,
        pub(super) status_title: String,
    }
    crate::embedded_config! { pub(super) static DEFINITION: Definition = include_str!("subagents.toml"); }
}
const MIN_WAIT_MS: u64 = 10_000;
const MAX_WAIT_MS: u64 = 120_000;

static DEFAULT_WAIT_MS: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
    let wait = text::DEFINITION.defaults_wait_ms;
    assert!((MIN_WAIT_MS..=MAX_WAIT_MS).contains(&wait));
    wait
});

#[derive(Clone, Copy)]
enum Command {
    Open,
}

impl Command {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "subagents",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        (value == Self::Open.as_str()).then_some(Self::Open)
    }
}
/// Trusted operator ceilings for the per-Bot subagent settings.
/// These govern resource policy, while Rust's integer representation remains a safety bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubagentCeilings {
    max_depth: u8,
    max_concurrency: usize,
    max_agents: usize,
}

impl Default for SubagentCeilings {
    fn default() -> Self {
        let ceiling = |id| match text::DEFINITION
            .settings
            .iter()
            .find(|setting| setting.id() == id)
        {
            Some(MiddlewareSettingManifest::Integer { max: Some(max), .. }) => *max,
            _ => panic!("missing embedded subagent ceiling {id}"),
        };
        Self::new(
            ceiling("max_depth")
                .try_into()
                .expect("depth ceiling must fit"),
            ceiling("max_concurrency")
                .try_into()
                .expect("concurrency ceiling must fit"),
            ceiling("max_agents")
                .try_into()
                .expect("agent ceiling must fit"),
        )
        .expect("valid embedded subagent ceilings")
    }
}

impl SubagentCeilings {
    /// Sets operator ceilings independently of a Bot's selected limits.
    /// # Errors
    /// Returns an error for zero depth, fewer than two concurrent slots, an agent
    /// ceiling below concurrency, or values outside signed configuration integers.
    pub fn new(max_depth: u8, max_concurrency: usize, max_agents: usize) -> Result<Self> {
        if max_depth == 0
            || max_concurrency < 2
            || max_agents < max_concurrency
            || i64::try_from(max_concurrency).is_err()
            || i64::try_from(max_agents).is_err()
        {
            return Err(Error::Config("subagent ceilings require positive depth and at least two agents and concurrent slots within signed integer bounds".into()));
        }
        Ok(Self {
            max_depth,
            max_concurrency,
            max_agents,
        })
    }

    /// Returns the operator's maximum nesting depth.
    #[must_use]
    pub const fn max_depth(self) -> u8 {
        self.max_depth
    }
    /// Returns the operator's maximum concurrent agent count, including the root.
    #[must_use]
    pub const fn max_concurrency(self) -> usize {
        self.max_concurrency
    }
    /// Returns the operator's maximum retained agent count, including the root.
    #[must_use]
    pub const fn max_agents(self) -> usize {
        self.max_agents
    }

    /// Returns this middleware's settings with the trusted operator's bounds.
    #[must_use]
    pub fn settings(self) -> Vec<MiddlewareSettingManifest> {
        text::DEFINITION
            .settings
            .iter()
            .cloned()
            .map(|mut setting| {
                if let MiddlewareSettingManifest::Integer {
                    id, max, default, ..
                } = &mut setting
                {
                    let ceiling = match id.as_str() {
                        "max_depth" => i64::from(self.max_depth),
                        "max_concurrency" => i64::try_from(self.max_concurrency)
                            .expect("validated concurrency ceiling must fit"),
                        "max_agents" => i64::try_from(self.max_agents)
                            .expect("validated agent ceiling must fit"),
                        _ => return setting,
                    };
                    *max = Some(ceiling);
                    *default = (*default).min(ceiling);
                }
                setting
            })
            .collect()
    }

    /// Validates a Bot's configured limits against trusted operator ceilings.
    /// # Errors
    /// Returns an error for a limit above the ceilings or an invalid tree relationship.
    pub fn limits(
        self,
        max_depth: u8,
        max_concurrency: usize,
        max_agents: usize,
    ) -> Result<SubagentLimits> {
        if max_depth == 0 || max_depth > self.max_depth {
            return Err(Error::Config(format!(
                "subagent max depth must be between 1 and {}",
                self.max_depth
            )));
        }
        if !(2..=self.max_concurrency).contains(&max_concurrency) {
            return Err(Error::Config(format!(
                "subagent max concurrency must be between 2 and {}",
                self.max_concurrency
            )));
        }
        if max_agents < max_concurrency || max_agents > self.max_agents {
            return Err(Error::Config(format!(
                "subagent max agents must be at least concurrency and no greater than {}",
                self.max_agents
            )));
        }
        Ok(SubagentLimits {
            max_depth,
            max_concurrency,
            max_agents,
        })
    }
}

/// A Bot's subagent limits, validated against operator ceilings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubagentLimits {
    max_depth: u8,
    max_concurrency: usize,
    max_agents: usize,
}

super::manifest::middleware_manifest! {
/// Configuration and presentation metadata for child-agent collaboration.
    "subagents", text::DEFINITION, required: false, settings: &text::DEFINITION.settings
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ForkTurns {
    #[default]
    None,
    All,
    Last(usize),
}

impl ForkTurns {
    fn label(self) -> String {
        match self {
            Self::None => text::DEFINITION.fork_none.as_str().into(),
            Self::All => text::DEFINITION.fork_all.as_str().into(),
            Self::Last(1) => text::DEFINITION.fork_last_one.as_str().into(),
            Self::Last(turns) => text::DEFINITION
                .fork_last
                .replace("{turns}", &turns.to_string()),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentIdentity {
    root_session_id: String,
    agent_path: String,
    depth: u8,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewCursor {
    path: String,
    before_sequence: u64,
}

impl AgentIdentity {
    fn read(session_id: &str, metadata: &BTreeMap<String, Value>) -> Result<Self> {
        let Some(value) = metadata.get(IDENTITY_KEY) else {
            return Ok(Self {
                root_session_id: session_id.into(),
                agent_path: "/root".into(),
                depth: 0,
            });
        };
        Ok(serde::Deserialize::deserialize(value)?)
    }

    fn metadata(&self, mut metadata: BTreeMap<String, Value>) -> Result<BTreeMap<String, Value>> {
        metadata.insert(IDENTITY_KEY.into(), serde_json::to_value(self)?);
        Ok(metadata)
    }
}

struct AgentScope {
    files: Option<Arc<crate::backend::session_files::SessionFileStore>>,
    checkpoints: Arc<dyn CheckpointStore>,
    children: ChildAgents,
    session_id: String,
    root_session_id: Arc<str>,
    agent_path: String,
    depth: u8,
    model: String,
}

impl AgentScope {
    fn child_role(&self, parent_turn_id: String) -> AgentRole {
        AgentRole::Subagent {
            parent_session_id: self.session_id.clone(),
            parent_turn_id,
        }
    }

    fn next_depth(&self) -> Result<u8> {
        self.depth.checked_add(1).ok_or_else(|| {
            Error::Config("subagent depth exceeds its integer representation".into())
        })
    }

    fn new(
        runtime: &RuntimeContext,
        files: Option<Arc<crate::backend::session_files::SessionFileStore>>,
    ) -> Result<Self> {
        let identity = AgentIdentity::read(&runtime.session_id, &runtime.metadata)?;
        Ok(Self {
            files,
            checkpoints: Arc::clone(&runtime.checkpoints),
            children: ChildAgents::clone(&runtime.children),
            session_id: runtime.session_id.clone(),
            root_session_id: identity.root_session_id.into(),
            agent_path: identity.agent_path,
            depth: identity.depth,
            model: runtime.model_route.clone(),
        })
    }

    async fn fork(
        &self,
        session_id: String,
        agent_path: String,
        model: String,
        reasoning_effort: Option<String>,
        turns: ForkTurns,
        parent_turn_id: String,
    ) -> Result<Agent> {
        let parent = self
            .checkpoints
            .load(&self.session_id)
            .await?
            .ok_or_else(|| Error::Checkpoint("parent checkpoint is missing".into()))?;
        let parent_sequence = parent.sequence;
        let pending = parent
            .pending_tools
            .iter()
            .map(|call| call.call_id.as_str())
            .collect::<BTreeSet<_>>();
        let mut checkpoint = Checkpoint::empty(&session_id);
        checkpoint.catalog_visible = false;
        checkpoint.context = Arc::new(fork_context(&parent.context, turns, &pending));
        checkpoint.context_model_route = parent.context_model_route.or(parent.model_route);
        checkpoint.session_context = parent.session_context;
        let mut metadata = AgentIdentity {
            root_session_id: self.root_session_id.to_string(),
            agent_path,
            depth: self.next_depth()?,
        }
        .metadata(parent.metadata)?;
        metadata.insert(SPAWN_CONTEXT_KEY.into(), Value::String(turns.label()));
        checkpoint.metadata = metadata;
        crate::backend::session_files::grant_context(
            self.files.as_deref(),
            &self.session_id,
            &session_id,
            &checkpoint.context,
        )
        .await?;
        self.checkpoints
            .fork(&self.session_id, parent_sequence, &checkpoint)
            .await?;
        self.children
            .create(
                session_id,
                checkpoint.metadata,
                self.child_role(parent_turn_id),
                &model,
                reasoning_effort.as_deref(),
            )
            .await
    }

    async fn resume(
        &self,
        session_id: String,
        agent_path: String,
        depth: u8,
        model: String,
        parent_turn_id: String,
    ) -> Result<Agent> {
        let checkpoint = self.checkpoints.load(&session_id).await?.ok_or_else(|| {
            Error::Checkpoint(format!("checkpoint for `{agent_path}` is missing"))
        })?;
        let metadata = AgentIdentity {
            root_session_id: self.root_session_id.to_string(),
            agent_path,
            depth,
        }
        .metadata(checkpoint.metadata)?;
        self.children
            .create(
                session_id,
                metadata,
                self.child_role(parent_turn_id),
                &model,
                None,
            )
            .await
    }
}

/// Contributes asynchronous collaboration tools.
pub struct Subagents {
    files: Option<Arc<crate::backend::session_files::SessionFileStore>>,
    max_depth: u8,
    default_model: Option<Arc<str>>,
    shared: Arc<Shared>,
}

impl Subagents {
    /// Injects durable media storage used for child observation grants.
    #[must_use]
    pub fn session_files(mut self, files: crate::backend::session_files::SessionFileStore) -> Self {
        self.files = Some(Arc::new(files));
        self
    }

    /// Creates a child-agent capability with validated depth, concurrency, and agent limits.
    ///
    /// Concurrency counts active agents and the agent limit counts retained agents;
    /// both include the root.
    #[must_use]
    pub fn new(limits: SubagentLimits) -> Self {
        Self {
            max_depth: limits.max_depth,
            files: None,
            default_model: None,
            shared: Arc::new(Shared::new(limits.max_concurrency, limits.max_agents)),
        }
    }

    /// Selects a registered provider/model route for children by default.
    #[must_use]
    pub fn default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(Arc::from(model.into()));
        self
    }

    fn section(&self, identity: &AgentIdentity) -> PromptSection {
        if identity.depth == 0 {
            PromptSection::new(text::DEFINITION.prompt_root.as_str())
        } else {
            PromptSection::new(
                text::DEFINITION
                    .prompt_child
                    .replace("{path}", &identity.agent_path),
            )
        }
    }

    async fn read_command(
        &self,
        session_id: &str,
        metadata: &BTreeMap<String, Value>,
        arguments: &str,
    ) -> Result<MiddlewareCommandOutput> {
        let path = arguments.trim();
        if path.starts_with('{') {
            return self.read_preview_page(session_id, metadata, path).await;
        }
        let identity = AgentIdentity::read(session_id, metadata)?;
        if !path.is_empty() {
            return self
                .preview_page(&identity.root_session_id, path, None)
                .await;
        }
        let options = self
            .shared
            .resume_options(&identity.root_session_id)
            .await?;
        let title = if options.is_empty() {
            &text::DEFINITION.render_open_empty
        } else {
            &text::DEFINITION.render_open
        };
        Ok(MiddlewareCommandOutput::events(vec![
            FrontendEvent::Picker {
                title: title.as_str().into(),
                options,
            },
        ]))
    }

    async fn read_preview_page(
        &self,
        session_id: &str,
        metadata: &BTreeMap<String, Value>,
        arguments: &str,
    ) -> Result<MiddlewareCommandOutput> {
        let identity = AgentIdentity::read(session_id, metadata)?;
        let cursor: PreviewCursor = serde_json::from_str(arguments)
            .map_err(|_| Error::Tool("invalid subagent preview cursor".into()))?;
        if cursor.path.trim() != cursor.path
            || cursor.path.is_empty()
            || cursor.before_sequence == 0
        {
            return Err(Error::Tool("invalid subagent preview cursor".into()));
        }
        self.preview_page(
            &identity.root_session_id,
            &cursor.path,
            Some(cursor.before_sequence),
        )
        .await
    }

    async fn preview_page(
        &self,
        root_session_id: &str,
        path: &str,
        before_sequence: Option<u64>,
    ) -> Result<MiddlewareCommandOutput> {
        let page = self
            .shared
            .preview(root_session_id, path, before_sequence)
            .await?;
        let next = page
            .next
            .map(|before_sequence| -> Result<Op> {
                Ok(Op::command(
                    MANIFEST.id,
                    Command::Open.as_str(),
                    serde_json::to_string(&PreviewCursor {
                        path: path.into(),
                        before_sequence,
                    })?,
                ))
            })
            .transpose()?;
        Ok(MiddlewareCommandOutput::events(vec![
            FrontendEvent::Preview {
                symbol: None,
                duration_ms: None,
                started_at_ms: None,
                id: path.into(),
                title: path.rsplit('/').next().unwrap_or(path).into(),
                subtitle: page.subtitle,
                page_id: page.page_id,
                update: if before_sequence.is_some() {
                    FrontendPreviewUpdate::Prepend
                } else {
                    FrontendPreviewUpdate::Replace
                },
                events: page.events,
                next,
            },
        ]))
    }
}

impl Middleware for Subagents {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        if context.source() == SessionStartSource::Compact {
            return Box::pin(async { Ok(()) });
        }
        Box::pin(self.shared.session_start(context.runtime))
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        let scope = Arc::new(AgentScope::new(
            runtime,
            self.files.as_ref().map(Arc::clone),
        )?);
        if scope.depth < self.max_depth {
            catalog.register(Arc::new(SpawnAgent {
                default_model: self.default_model.as_ref().map(Arc::clone),
                shared: Arc::clone(&self.shared),
                scope: Arc::clone(&scope),
            }))?;
        }
        catalog.register(Arc::new(SendMessage {
            shared: Arc::clone(&self.shared),
            scope: Arc::clone(&scope),
        }))?;
        catalog.register(Arc::new(ListAgents {
            shared: Arc::clone(&self.shared),
            scope: Arc::clone(&scope),
        }))?;
        catalog.register(Arc::new(InterruptAgent {
            shared: Arc::clone(&self.shared),
            scope: Arc::clone(&scope),
        }))?;
        catalog.register(Arc::new(WaitAgent {
            shared: Arc::clone(&self.shared),
            scope,
        }))
    }

    fn prompt_section(&self, runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        let identity = AgentIdentity::read(&runtime.session_id, &runtime.metadata)?;
        Ok(Some(self.section(&identity)))
    }

    fn frontend(&self, _session_id: &str) -> FrontendContribution {
        FrontendContribution {
            capability: self.name().into(),
            count: None,
            commands: vec![FrontendCommand {
                name: Command::Open.as_str().into(),
                arguments: String::new(),
                description: text::DEFINITION.command_description.clone(),
                requires_idle: false,
            }],
            widgets: Vec::new(),
            references: Vec::new(),
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        let mut block = [
            &text::DEFINITION.wait_agent,
            &text::DEFINITION.interrupt_agent,
            &text::DEFINITION.list_agents,
            &text::DEFINITION.send_message,
            &text::DEFINITION.spawn_agent,
        ]
        .into_iter()
        .find_map(|spec| spec.render(event))?;
        if let EventMsg::ToolCallBegin(call) = event
            && call.name == "send_message"
            && let Some(message) = call.arguments.get("text").and_then(Value::as_str)
        {
            FrontendBlockUpdate::Append.apply(&mut block.text, message);
        }
        Some(block)
    }

    fn command<'a>(
        &'a self,
        context: MiddlewareCommandContext<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareCommandOutput>> {
        Box::pin(async move {
            match Command::parse(context.command) {
                Some(Command::Open) => {
                    self.read_command(
                        context.session_id,
                        &context.checkpoint.metadata,
                        context.arguments,
                    )
                    .await
                }
                None => Err(Error::Unknown(format!(
                    "subagents command `{}`",
                    context.command
                ))),
            }
        })
    }

    fn active_command<'a>(
        &'a self,
        context: &'a mut ActiveCommandContext<'_>,
    ) -> BoxFuture<'a, Result<Option<SubmissionResult>>> {
        Box::pin(async move {
            let output = match Command::parse(context.command) {
                Some(Command::Open) => {
                    self.read_command(context.session_id, context.metadata, context.arguments)
                        .await
                }
                None => return Ok(None),
            };
            match output {
                Ok(output) => {
                    context
                        .events
                        .extend(output.events.into_iter().map(EventMsg::Frontend));
                    Ok(Some(SubmissionResult::Handled))
                }
                Err(error) => Ok(Some(SubmissionResult::Rejected(error.to_string()))),
            }
        })
    }

    fn pre_model<'a>(&'a self, context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let identity = AgentIdentity::read(context.session_id, context.metadata)?;
            let acknowledged = context
                .input()
                .iter()
                .filter_map(internal_message_kind)
                .filter_map(|kind| kind.strip_prefix("subagent_update:"))
                .collect();
            let delivered_message_ids = context
                .input()
                .iter()
                .filter_map(message_metadata)
                .filter_map(|message| match message.author {
                    MessageAuthor::Source { message_id, .. } => Some(message_id),
                    MessageAuthor::User => None,
                })
                .collect();
            let updates = self
                .shared
                .receive_updates(
                    &identity.root_session_id,
                    &identity.agent_path,
                    &acknowledged,
                )
                .await?;
            for update in updates {
                context.push_input(internal_user_message(
                    &update.internal_kind(),
                    &update.render(&delivered_message_ids),
                ))?;
            }
            Ok(())
        })
    }

    fn has_background_work<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(self.shared.has_active_children(session_id))
    }

    fn session_end<'a>(&'a self, runtime: &'a RuntimeContext) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let identity = AgentIdentity::read(&runtime.session_id, &runtime.metadata)?;
            if matches!(runtime.role, AgentRole::Main) && identity.depth == 0 {
                self.shared.remove_root(&identity.root_session_id).await?;
            } else {
                self.shared
                    .remove_sender(&identity.root_session_id, &identity.agent_path)
                    .await;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
