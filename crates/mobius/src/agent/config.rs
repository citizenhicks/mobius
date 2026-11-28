//! Agent configuration and child-agent creation.

use crate::SessionId;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Weak;

use serde_json::Value;

use super::Agent;
use super::create_agent;
use crate::Error;
use crate::Result;
use crate::backend::checkpoint::CheckpointStore;
use crate::backend::model::ModelRouter;
use crate::backend::sandbox::Sandbox;
use crate::middleware::MiddlewareStack;
use crate::protocol::ModelChangedEvent;
use crate::protocol::ModelChoice;
use crate::protocol::SessionContext;
use crate::protocol::TokenUsage;

const DEFAULT_INITIAL_REPLAY_BATCHES: usize = 100;

type UsageObserver =
    Arc<dyn for<'a> Fn(&'a str, &'a TokenUsage) -> crate::BoxFuture<'a, Result<()>> + Send + Sync>;

/// Default maximum number of primary model steps in one turn.
pub const DEFAULT_MAX_MODEL_STEPS: usize = 2042;

/// Whether one runtime owns the primary conversation or a delegated child task.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AgentRole {
    #[default]
    /// Selects the main case.
    Main,
    /// Selects the subagent case.
    Subagent {
        /// The parent session identifier.
        parent_session_id: SessionId,
        /// The parent turn identifier.
        parent_turn_id: Arc<str>,
    },
}

/// Dependencies and policy for one agent session.
#[derive(Clone)]
pub struct AgentConfig {
    pub(super) model: Arc<ModelRouter>,
    pub(super) provider: Arc<str>,
    pub(super) sandbox: Arc<Sandbox>,
    pub(super) checkpoints: Arc<dyn CheckpointStore>,
    pub(super) middleware: Arc<MiddlewareStack>,
    pub(super) system_prompt: Arc<str>,
    pub(super) session_id: Option<SessionId>,
    pub(super) context_window: i64,
    default_context_window: i64,
    pub(super) session_context: Arc<SessionContext>,
    pub(super) metadata: BTreeMap<String, Value>,
    pub(super) catalog_visible: bool,
    pub(super) usage_observer: Option<UsageObserver>,
    pub(super) metadata_configured: bool,
    pub(super) model_route_configured: bool,
    pub(super) initial_replay_batches: usize,
    pub(super) max_model_steps: usize,
    pub(super) token_estimate: crate::middleware::TokenEstimate,
    pub(super) role: AgentRole,
    pub(super) children: Option<ChildAgents>,
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentConfig")
            .field("provider", &self.provider)
            .field("session_id", &self.session_id)
            .field("context_window", &self.context_window)
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

impl AgentConfig {
    /// Creates a complete agent configuration.
    pub fn new(
        model: Arc<ModelRouter>,
        sandbox: Arc<Sandbox>,
        checkpoints: Arc<dyn CheckpointStore>,
        middleware: MiddlewareStack,
        system_prompt: impl Into<String>,
    ) -> Self {
        let provider = model.default_provider().into();
        Self {
            model,
            provider,
            sandbox,
            checkpoints,
            middleware: Arc::new(middleware),
            system_prompt: system_prompt.into().into(),
            session_id: None,
            context_window: 272_000,
            default_context_window: 272_000,
            session_context: Arc::default(),
            metadata: BTreeMap::new(),
            catalog_visible: true,
            usage_observer: None,
            metadata_configured: false,
            model_route_configured: false,
            initial_replay_batches: DEFAULT_INITIAL_REPLAY_BATCHES,
            max_model_steps: DEFAULT_MAX_MODEL_STEPS,
            token_estimate: crate::middleware::TokenEstimate::default(),
            role: AgentRole::Main,
            children: None,
        }
    }

    /// Gives a child agent its own sandbox temporary files and execution lifetime.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn isolated_execution(mut self) -> Result<Self> {
        self.sandbox = Arc::new(self.sandbox.isolated_execution()?);
        Ok(self)
    }

    /// Sets a stable ID used to resume a checkpointed session.
    #[must_use]
    pub fn session_id(mut self, session_id: SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    /// Attaches trusted, frontend-visible labels to this session.
    #[must_use]
    pub fn session_context(mut self, context: SessionContext) -> Self {
        self.session_context = Arc::new(context);
        self
    }

    /// Sets whether a newly created session appears in the session catalog.
    #[must_use]
    pub fn catalog_visible(mut self, visible: bool) -> Self {
        self.catalog_visible = visible;
        self
    }

    /// Sets the workload-specific byte estimate shared by all context policies.
    #[must_use]
    pub fn token_estimate(mut self, estimate: crate::middleware::TokenEstimate) -> Self {
        self.token_estimate = estimate;
        self
    }

    /// Sets the provider's context window for usage display and policy.
    #[must_use]
    pub fn context_window(mut self, context_window: i64) -> Self {
        self.context_window = context_window;
        self.default_context_window = context_window;
        self
    }

    /// Sets the maximum durable transcript batches rendered when a session opens.
    /// Zero disables checkpoint-derived presentation replay.
    #[must_use]
    pub fn initial_replay_batches(mut self, max_batches: usize) -> Self {
        self.initial_replay_batches = max_batches;
        self
    }

    /// Sets the maximum number of primary model steps in one turn.
    #[must_use]
    pub fn max_model_steps(mut self, max_steps: usize) -> Self {
        self.max_model_steps = max_steps;
        self
    }

    /// Marks this runtime as a primary agent or delegated subagent.
    #[must_use]
    pub fn role(mut self, role: AgentRole) -> Self {
        self.role = role;
        self
    }

    /// Sets durable framework-internal metadata used by installed capabilities.
    ///
    /// On resume, calling this replaces the saved metadata. Omitting it preserves
    /// the saved value.
    #[must_use]
    pub fn metadata(mut self, metadata: BTreeMap<String, Value>) -> Self {
        self.metadata = metadata;
        self.metadata_configured = true;
        self
    }

    /// Observes normalized token-usage increments with their selected model route.
    ///
    /// The observer is awaited before committing the increment. Returning an error
    /// aborts the active turn before the increment is committed
    /// to the session checkpoint.
    #[must_use]
    pub fn usage_observer(
        mut self,
        observer: impl for<'a> Fn(&'a str, &'a TokenUsage) -> crate::BoxFuture<'a, Result<()>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.usage_observer = Some(Arc::new(observer));
        self
    }

    /// Selects a registered model route and optional reasoning effort.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn model_route(mut self, route: &str, reasoning_effort: Option<&str>) -> Result<Self> {
        self.select_model_with_reasoning(route, reasoning_effort)?;
        self.model_route_configured = true;
        Ok(self)
    }

    /// Makes the configured router default replace a saved route on resume.
    #[must_use]
    pub fn override_saved_model_route(mut self) -> Self {
        self.model_route_configured = true;
        self
    }

    pub(super) fn select_model(&mut self, route: &str) -> Result<ModelChangedEvent> {
        let default_window = self.default_context_window;
        let choice = self.select_model_with_reasoning(route, None)?;
        Ok(ModelChangedEvent {
            route: Arc::clone(&choice.route),
            model: Arc::clone(&choice.model),
            reasoning_effort: choice.reasoning_effort.as_ref().map(Arc::clone),
            model_context_window: Some(choice.context_window.unwrap_or(default_window)),
        })
    }

    fn select_model_with_reasoning(
        &mut self,
        route: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<&ModelChoice> {
        let choice = self.model.resolve_choice(route, reasoning_effort)?;
        self.provider = Arc::clone(&choice.route);
        self.context_window = choice.context_window.unwrap_or(self.default_context_window);
        Ok(choice)
    }

    pub(super) fn child_configuration(&self) -> Self {
        Self {
            model: Arc::clone(&self.model),
            provider: Arc::default(),
            sandbox: Arc::clone(&self.sandbox),
            checkpoints: Arc::clone(&self.checkpoints),
            middleware: Arc::new(self.middleware.for_child_agents()),
            system_prompt: Arc::clone(&self.system_prompt),
            session_id: None,
            context_window: self.context_window,
            default_context_window: self.default_context_window,
            session_context: Arc::clone(&self.session_context),
            metadata: BTreeMap::new(),
            catalog_visible: self.catalog_visible,
            usage_observer: self.usage_observer.as_ref().map(Arc::clone),
            metadata_configured: false,
            model_route_configured: false,
            initial_replay_batches: self.initial_replay_batches,
            max_model_steps: self.max_model_steps,
            token_estimate: self.token_estimate,
            role: AgentRole::Main,
            children: None,
        }
    }
}

/// Creates child agents from the root agent's configuration while that root runs.
///
/// Children and their descendants share the root's configuration; once the root
/// runtime stops, every handle refuses to create more agents.
#[derive(Clone, Default)]
pub struct ChildAgents(pub(super) Weak<AgentConfig>);

impl std::fmt::Debug for ChildAgents {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChildAgents")
            .field("live", &(self.0.strong_count() > 0))
            .finish_non_exhaustive()
    }
}

impl ChildAgents {
    /// Creates and starts one child agent with isolated execution.
    /// # Errors
    ///
    /// Returns an error if the root agent stopped or the child cannot start.
    pub async fn create(
        &self,
        session_id: SessionId,
        metadata: BTreeMap<String, Value>,
        role: AgentRole,
        model: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<Agent> {
        let root = self
            .0
            .upgrade()
            .ok_or_else(|| Error::Stopped("parent agent stopped".into()))?;
        let mut config = root
            .child_configuration()
            .isolated_execution()?
            .session_id(session_id)
            .metadata(metadata)
            .role(role)
            .model_route(model, reasoning_effort)?;
        config.children = Some(Self(Arc::downgrade(&root)));
        drop(root);
        create_agent(config).await
    }
}
