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
use super::manifest::{MiddlewareManifest, MiddlewareSettingChoices, MiddlewareSettingManifest};
use super::tools::Catalog;
use super::tools::labeled_tool_heading;
use super::tools::render_tool_event;
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::agent::{Agent, AgentRole};
use crate::backend::checkpoint::Checkpoint;
use crate::backend::checkpoint::CheckpointStore;
use crate::backend::model::internal_user_message;
use crate::protocol::EventMsg;
use crate::protocol::FrontendBlock;
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

use self::tools::{
    FollowupTask, InterruptAgent, ListAgents, SendMessage, SpawnAgent, WaitAgent, fork_context,
};
#[cfg(test)]
use self::tools::{cleanup_error, supervise, wait_parameters, wait_timeout};

const MAX_TASK_NAME_BYTES: usize = 64;
const IDENTITY_KEY: &str = "subagents.identity";
const SPAWN_CONTEXT_KEY: &str = "subagents.spawn_context";
mod text {
    include!(concat!(
        env!("OUT_DIR"),
        "/src_middleware_subagents_text.rs"
    ));
}

const MIN_WAIT_MS: u64 = 10_000;
const MAX_WAIT_MS: u64 = 120_000;
const MAX_CONFIGURED_DEPTH: u8 = 16;
const MAX_CONFIGURED_CONCURRENCY: usize = 64;
const MAX_CONFIGURED_AGENTS: usize = 256;
const _: () = {
    assert!(text::DEFAULTS_WAIT_MS >= MIN_WAIT_MS as i64);
    assert!(text::DEFAULTS_WAIT_MS <= MAX_WAIT_MS as i64);
    assert!(text::DEFAULTS_MAX_DEPTH >= 1);
    assert!(text::DEFAULTS_MAX_DEPTH <= MAX_CONFIGURED_DEPTH as i64);
    assert!(text::DEFAULTS_MAX_CONCURRENCY >= 2);
    assert!(text::DEFAULTS_MAX_CONCURRENCY <= MAX_CONFIGURED_CONCURRENCY as i64);
    assert!(text::DEFAULTS_MAX_AGENTS >= text::DEFAULTS_MAX_CONCURRENCY);
    assert!(text::DEFAULTS_MAX_AGENTS <= MAX_CONFIGURED_AGENTS as i64);
    assert!(text::SETTING_MAX_DEPTH_STEP > 0);
    assert!(text::SETTING_MAX_CONCURRENCY_STEP > 0);
    assert!(text::SETTING_MAX_AGENTS_STEP > 0);
};
const DEFAULT_WAIT_MS: u64 = text::DEFAULTS_WAIT_MS as u64;
/// Default maximum child-agent nesting depth.
pub const DEFAULT_MAX_DEPTH: u8 = text::DEFAULTS_MAX_DEPTH as u8;
/// Default number of concurrently active agents, including the root.
pub const DEFAULT_MAX_CONCURRENCY: usize = text::DEFAULTS_MAX_CONCURRENCY as usize;
/// Default number of retained agents, including the root.
pub const DEFAULT_MAX_AGENTS: usize = text::DEFAULTS_MAX_AGENTS as usize;
const SETTINGS: &[MiddlewareSettingManifest] = &[
    MiddlewareSettingManifest::Select {
        id: "model_route",
        label: text::SETTING_MODEL_ROUTE_LABEL,
        description: text::SETTING_MODEL_ROUTE_DESCRIPTION,
        choices: MiddlewareSettingChoices::ModelRoutes,
        unset_label: Some(text::SETTING_MODEL_ROUTE_UNSET_LABEL),
        default: None,
        max_bytes: 4 * 1024,
        composer: false,
    },
    MiddlewareSettingManifest::Integer {
        id: "max_depth",
        label: text::SETTING_MAX_DEPTH_LABEL,
        description: text::SETTING_MAX_DEPTH_DESCRIPTION,
        min: 1,
        max: Some(MAX_CONFIGURED_DEPTH as i64),
        step: text::SETTING_MAX_DEPTH_STEP,
        default: DEFAULT_MAX_DEPTH as i64,
    },
    MiddlewareSettingManifest::Integer {
        id: "max_concurrency",
        label: text::SETTING_MAX_CONCURRENCY_LABEL,
        description: text::SETTING_MAX_CONCURRENCY_DESCRIPTION,
        min: 2,
        max: Some(MAX_CONFIGURED_CONCURRENCY as i64),
        step: text::SETTING_MAX_CONCURRENCY_STEP,
        default: DEFAULT_MAX_CONCURRENCY as i64,
    },
    MiddlewareSettingManifest::Integer {
        id: "max_agents",
        label: text::SETTING_MAX_AGENTS_LABEL,
        description: text::SETTING_MAX_AGENTS_DESCRIPTION,
        min: 2,
        max: Some(MAX_CONFIGURED_AGENTS as i64),
        step: text::SETTING_MAX_AGENTS_STEP,
        default: DEFAULT_MAX_AGENTS as i64,
    },
];

/// Configuration and presentation metadata for child-agent collaboration.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "subagents",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: false,
    default_enabled: true,
    settings: SETTINGS,
};

/// Child-agent parameters owned by the subagent capability.
#[derive(Clone)]
pub struct SubagentLaunch {
    pub session_id: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub metadata: BTreeMap<String, Value>,
    pub role: AgentRole,
}

/// Creates one child agent for this capability.
pub type SubagentLauncher =
    Arc<dyn Fn(SubagentLaunch) -> BoxFuture<'static, Result<Agent>> + Send + Sync>;

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
            Self::None => "No context".into(),
            Self::All => "Full context".into(),
            Self::Last(1) => "Last 1 turn".into(),
            Self::Last(turns) => format!("Last {turns} turns"),
        }
    }
}

#[derive(Clone, Deserialize)]
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
        Ok(serde_json::from_value(value.clone())?)
    }

    fn metadata(&self, mut metadata: BTreeMap<String, Value>) -> BTreeMap<String, Value> {
        metadata.insert(
            IDENTITY_KEY.into(),
            serde_json::json!({
                "root_session_id": self.root_session_id,
                "agent_path": self.agent_path,
                "depth": self.depth,
            }),
        );
        metadata
    }
}

#[derive(Clone)]
struct AgentScope {
    checkpoints: Arc<dyn CheckpointStore>,
    launch_agent: SubagentLauncher,
    session_id: String,
    root_session_id: String,
    agent_path: String,
    depth: u8,
    model: String,
    metadata: BTreeMap<String, Value>,
}

impl AgentScope {
    fn new(runtime: &RuntimeContext, launch_agent: SubagentLauncher) -> Result<Self> {
        let identity = AgentIdentity::read(&runtime.session_id, &runtime.metadata)?;
        Ok(Self {
            checkpoints: Arc::clone(&runtime.checkpoints),
            launch_agent,
            session_id: runtime.session_id.clone(),
            root_session_id: identity.root_session_id,
            agent_path: identity.agent_path,
            depth: identity.depth,
            model: runtime.model_route.clone(),
            metadata: runtime.metadata.clone(),
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
            .map(|call| call.call_id.clone())
            .collect::<BTreeSet<_>>();
        let context = parent
            .context
            .into_iter()
            .filter(|item| {
                item.get("type").and_then(Value::as_str) != Some("function_call")
                    || item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .is_none_or(|call_id| !pending.contains(call_id))
            })
            .collect::<Vec<_>>();
        let mut checkpoint = Checkpoint::empty(&session_id);
        checkpoint.catalog_visible = false;
        checkpoint.context = fork_context(&context, turns);
        checkpoint.session_context = parent.session_context;
        let mut metadata = AgentIdentity {
            root_session_id: self.root_session_id.clone(),
            agent_path: agent_path.clone(),
            depth: self.depth + 1,
        }
        .metadata(self.metadata.clone());
        metadata.insert(SPAWN_CONTEXT_KEY.into(), Value::String(turns.label()));
        checkpoint.metadata.clone_from(&metadata);
        self.checkpoints
            .fork(&self.session_id, parent_sequence, &checkpoint)
            .await?;
        (self.launch_agent)(SubagentLaunch {
            role: AgentRole::Subagent {
                parent_session_id: self.session_id.clone(),
                parent_turn_id,
            },
            session_id,
            model,
            reasoning_effort,
            metadata,
        })
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
        (self.launch_agent)(SubagentLaunch {
            role: AgentRole::Subagent {
                parent_session_id: self.session_id.clone(),
                parent_turn_id,
            },
            session_id,
            model,
            reasoning_effort: None,
            metadata: AgentIdentity {
                root_session_id: self.root_session_id.clone(),
                agent_path,
                depth,
            }
            .metadata(checkpoint.metadata),
        })
        .await
    }
}

/// Contributes asynchronous collaboration tools.
pub struct Subagents {
    max_depth: u8,
    launch_agent: SubagentLauncher,
    default_model: Option<String>,
    default_reasoning: Option<String>,
    prompt: String,
    shared: Arc<Shared>,
}

impl Subagents {
    /// Creates a child-agent capability with hard depth, concurrency, and agent limits.
    ///
    /// `max_concurrency` counts active agents and `max_agents` counts retained agents;
    /// both include the root.
    pub fn new(
        max_depth: u8,
        max_concurrency: usize,
        max_agents: usize,
        launch_agent: SubagentLauncher,
    ) -> Result<Self> {
        if max_depth == 0 || max_depth > MAX_CONFIGURED_DEPTH {
            return Err(Error::Config(format!(
                "subagent max depth must be between 1 and {MAX_CONFIGURED_DEPTH}"
            )));
        }
        if max_concurrency > MAX_CONFIGURED_CONCURRENCY {
            return Err(Error::Config(format!(
                "subagent max concurrency cannot exceed {MAX_CONFIGURED_CONCURRENCY}"
            )));
        }
        if max_agents > MAX_CONFIGURED_AGENTS {
            return Err(Error::Config(format!(
                "subagent max agents cannot exceed {MAX_CONFIGURED_AGENTS}"
            )));
        }
        Ok(Self {
            max_depth,
            launch_agent,
            default_model: None,
            default_reasoning: None,
            prompt: text::PROMPT_DEFAULT.into(),
            shared: Arc::new(Shared::new(max_concurrency, max_agents)?),
        })
    }

    /// Selects a registered provider/model route for children by default.
    #[must_use]
    pub fn default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Selects a reasoning effort for children by default.
    pub fn default_reasoning(mut self, reasoning: impl Into<String>) -> Result<Self> {
        let reasoning = reasoning.into();
        if reasoning.trim().is_empty() {
            return Err(Error::Config(
                "subagent reasoning effort cannot be empty".into(),
            ));
        }
        self.default_reasoning = Some(reasoning);
        Ok(self)
    }

    /// Overrides the instruction given to child agents.
    pub fn prompt(mut self, prompt: impl Into<String>) -> Result<Self> {
        let prompt = prompt.into();
        if prompt.trim().is_empty() {
            return Err(Error::Config("subagent prompt cannot be empty".into()));
        }
        self.prompt = prompt;
        Ok(self)
    }

    fn section(&self, identity: &AgentIdentity) -> PromptSection {
        let body = if identity.depth == 0 {
            text::PROMPT_ROOT.into()
        } else {
            format!(
                "You are `{}`, a child agent.\n{}",
                identity.agent_path,
                self.prompt.trim()
            )
        };
        PromptSection::new(body)
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
        if options.is_empty() {
            return Ok(MiddlewareCommandOutput::events(vec![
                FrontendEvent::Picker {
                    title: format!("{} · {}", text::RENDER_OPEN, text::RENDER_EMPTY),
                    options,
                },
            ]));
        }
        Ok(MiddlewareCommandOutput::events(vec![
            FrontendEvent::Picker {
                title: text::RENDER_OPEN.into(),
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
                Ok(Op::CapabilityCommand {
                    capability: MANIFEST.id.into(),
                    command: "subagents".into(),
                    arguments: serde_json::to_string(&PreviewCursor {
                        path: path.into(),
                        before_sequence,
                    })?,
                    input: None,
                    target: None,
                })
            })
            .transpose()?;
        Ok(MiddlewareCommandOutput::events(vec![
            FrontendEvent::Preview {
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
        Box::pin(self.shared.session_start((*context.runtime).clone()))
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        let scope = Arc::new(AgentScope::new(runtime, Arc::clone(&self.launch_agent))?);
        if scope.depth < self.max_depth {
            catalog.register(Arc::new(SpawnAgent {
                default_model: self.default_model.clone(),
                default_reasoning: self.default_reasoning.clone(),
                shared: Arc::clone(&self.shared),
                scope: Arc::clone(&scope),
            }))?;
        }
        catalog.register(Arc::new(SendMessage {
            shared: Arc::clone(&self.shared),
            scope: Arc::clone(&scope),
        }))?;
        catalog.register(Arc::new(FollowupTask {
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

    fn frontend(&self) -> FrontendContribution {
        FrontendContribution {
            capability: self.name().into(),
            accepts_file_attachments: false,
            count: None,
            commands: vec![FrontendCommand {
                name: "subagents".into(),
                arguments: String::new(),
                description: text::COMMAND_DESCRIPTION.into(),
                requires_idle: false,
            }],
            widgets: Vec::new(),
            references: Vec::new(),
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        render_tool_event(
            event,
            |name| {
                matches!(
                    name,
                    "spawn_agent"
                        | "send_message"
                        | "followup_task"
                        | "list_agents"
                        | "interrupt_agent"
                        | "wait_agent"
                )
            },
            |name, arguments| match name {
                _ if matches!(event, EventMsg::ToolCallEnd(_)) => name.into(),
                "spawn_agent" => labeled_tool_heading(text::RENDER_AGENT, "task_name", arguments),
                "send_message" => labeled_tool_heading(text::RENDER_MESSAGE, "target", arguments),
                "followup_task" => {
                    labeled_tool_heading(text::RENDER_FOLLOW_UP, "target", arguments)
                }
                "list_agents" => {
                    labeled_tool_heading(text::RENDER_AGENTS, "path_prefix", arguments)
                }
                "interrupt_agent" => {
                    labeled_tool_heading(text::RENDER_INTERRUPT, "target", arguments)
                }
                "wait_agent" => labeled_tool_heading(text::RENDER_WAIT, "timeout_ms", arguments),
                _ => name.to_string().into(),
            },
        )
    }

    fn command<'a>(
        &'a self,
        context: MiddlewareCommandContext<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareCommandOutput>> {
        Box::pin(async move {
            match context.command {
                "subagents" => {
                    self.read_command(
                        context.session_id,
                        &context.checkpoint.metadata,
                        context.arguments,
                    )
                    .await
                }
                command => Err(Error::Unknown(format!("subagents command `{command}`"))),
            }
        })
    }

    fn active_command<'a>(
        &'a self,
        context: &'a mut ActiveCommandContext<'_>,
    ) -> BoxFuture<'a, Result<Option<SubmissionResult>>> {
        Box::pin(async move {
            let output = match context.command {
                "subagents" => {
                    self.read_command(context.session_id, context.metadata, context.arguments)
                        .await
                }
                _ => return Ok(None),
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
                .map(str::to_owned)
                .collect();
            let delivered_message_ids = context
                .input()
                .iter()
                .filter_map(message_metadata)
                .filter_map(|message| match message.author {
                    MessageAuthor::Peer { message_id, .. } => Some(message_id),
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

    fn session_end<'a>(&'a self, runtime: &'a RuntimeContext) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let identity = AgentIdentity::read(&runtime.session_id, &runtime.metadata)?;
            if matches!(runtime.role, AgentRole::Main) && identity.depth == 0 {
                self.shared.remove_root(&identity.root_session_id).await;
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
