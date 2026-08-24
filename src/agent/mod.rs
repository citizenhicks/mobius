//! Agent handles and the single linear command dispatch loop.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::Error;
use crate::Result;
use crate::backend::checkpoint::CHECKPOINT_VERSION;
use crate::backend::checkpoint::Checkpoint;
use crate::backend::checkpoint::CheckpointStore;
use crate::backend::checkpoint::ExecutionOutcome;
use crate::backend::checkpoint::ExecutionRecord;
use crate::backend::checkpoint::JournalEvent;
use crate::backend::model::ModelRouter;
use crate::backend::sandbox::Sandbox;
use crate::middleware::FrontendExtensions;
use crate::middleware::MiddlewareCommandContext;
use crate::middleware::MiddlewareStack;
use crate::middleware::RuntimeContext;
use crate::middleware::session_files::session_file_limits;
use crate::middleware::tools::Catalog;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::ModelChangedEvent;
use crate::protocol::ModelChoice;
use crate::protocol::ModelInfo;
use crate::protocol::Op;
use crate::protocol::SessionConfiguredEvent;
use crate::protocol::SessionContext;
use crate::protocol::SessionResumeRequestedEvent;
use crate::protocol::Submission;
use crate::protocol::TokenUsage;
use crate::protocol::WarningEvent;

mod approval;
mod input;
mod recorder;
mod startup;
mod tool_step;
mod turn;

pub use self::startup::create_agent;

use self::recorder::EventRecorder;

const COMMAND_QUEUE_CAPACITY: usize = 64;
const EVENT_QUEUE_CAPACITY: usize = 256;
const MAX_DEFERRED_SUBMISSIONS: usize = 64;
const MAX_IDENTIFIER_BYTES: usize = 4 * 1024;
const MAX_OPERATION_BYTES: usize = 256;
const DEFAULT_INITIAL_REPLAY_BATCHES: usize = 100;

type UsageObserver = Arc<dyn Fn(&str, &TokenUsage) -> Result<()> + Send + Sync>;

/// Default maximum number of primary model steps in one turn.
pub const DEFAULT_MAX_MODEL_STEPS: usize = 256;

/// Whether one runtime owns the primary conversation or a delegated child task.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AgentRole {
    #[default]
    Main,
    Subagent {
        parent_session_id: String,
        parent_turn_id: String,
    },
}

/// Dependencies and policy for one agent session.
#[derive(Clone)]
pub struct AgentConfig {
    model: Arc<ModelRouter>,
    provider: String,
    sandbox: Arc<Sandbox>,
    checkpoints: Arc<dyn CheckpointStore>,
    middleware: MiddlewareStack,
    system_prompt: String,
    session_id: String,
    context_window: i64,
    default_context_window: i64,
    session_context: SessionContext,
    metadata: BTreeMap<String, Value>,
    usage_observer: Option<UsageObserver>,
    metadata_configured: bool,
    model_route_configured: bool,
    initial_replay_batches: usize,
    max_model_steps: usize,
    role: AgentRole,
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
        let provider = model.default_provider().to_string();
        let session_id = Uuid::new_v4().to_string();
        Self {
            model,
            provider,
            sandbox,
            checkpoints,
            middleware,
            system_prompt: system_prompt.into(),
            session_id,
            context_window: 272_000,
            default_context_window: 272_000,
            session_context: SessionContext::default(),
            metadata: BTreeMap::new(),
            usage_observer: None,
            metadata_configured: false,
            model_route_configured: false,
            initial_replay_batches: DEFAULT_INITIAL_REPLAY_BATCHES,
            max_model_steps: DEFAULT_MAX_MODEL_STEPS,
            role: AgentRole::Main,
        }
    }

    /// Sets a stable ID used to resume a checkpointed session.
    #[must_use]
    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }

    /// Attaches trusted, frontend-visible labels to this session.
    #[must_use]
    pub fn session_context(mut self, context: SessionContext) -> Self {
        self.session_context = context;
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
    /// Returning an error aborts the active turn before the increment is committed
    /// to the session checkpoint.
    #[must_use]
    pub fn usage_observer(
        mut self,
        observer: impl Fn(&str, &TokenUsage) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        self.usage_observer = Some(Arc::new(observer));
        self
    }

    /// Selects a registered model route and optional reasoning effort.
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

    fn select_model(&mut self, route: &str) -> Result<ModelChoice> {
        self.select_model_with_reasoning(route, None)
    }

    fn select_model_with_reasoning(
        &mut self,
        route: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<ModelChoice> {
        let choice = self.model.resolve_choice(route, reasoning_effort)?.clone();
        self.provider.clone_from(&choice.route);
        self.context_window = choice.context_window.unwrap_or(self.default_context_window);
        Ok(choice)
    }
}

/// Cloneable command side of a running agent.
#[derive(Clone)]
pub struct AgentSender {
    commands: mpsc::Sender<Submission>,
}

impl AgentSender {
    /// Sends a submission with a caller-controlled correlation ID.
    pub fn send(&self, submission: Submission) -> Result<()> {
        validate_submission(&submission)?;
        self.commands
            .try_send(submission)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    Error::Busy("agent command queue is full".into())
                }
                mpsc::error::TrySendError::Closed(_) => {
                    Error::Stopped("agent command channel closed".into())
                }
            })
    }

    /// Submits a command and returns its correlation ID.
    pub fn submit(&self, op: Op) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        self.send(Submission { id: id.clone(), op })?;
        Ok(id)
    }
}

/// Validates one submission before callers perform more expensive boundary work.
pub fn validate_submission(submission: &Submission) -> Result<()> {
    validate_identifier("submission ID", &submission.id, MAX_IDENTIFIER_BYTES)?;
    match &submission.op {
        Op::UserInput { text, attachments } => validate_user_input(text, attachments),
        Op::ActiveInput {
            operation,
            turn_id,
            text,
        } => {
            validate_identifier("active operation", operation, MAX_OPERATION_BYTES)?;
            validate_identifier("turn ID", turn_id, MAX_IDENTIFIER_BYTES)?;
            validate_user_input(text, &[])
        }
        Op::Interrupt { turn_id } => validate_identifier("turn ID", turn_id, MAX_IDENTIFIER_BYTES),
        Op::ExecApproval { id, .. } => validate_identifier("approval ID", id, MAX_IDENTIFIER_BYTES),
        Op::CapabilityCommand {
            capability,
            command,
            arguments,
            input,
            target,
        } => {
            validate_identifier("capability ID", capability, MAX_OPERATION_BYTES)?;
            validate_identifier("command", command, MAX_OPERATION_BYTES)?;
            if arguments.len() > crate::protocol::MAX_CAPABILITY_INPUT_BYTES {
                return Err(Error::Config(
                    "middleware command arguments exceed size limit".into(),
                ));
            }
            if input
                .as_ref()
                .is_some_and(|input| input.len() > crate::protocol::MAX_CAPABILITY_INPUT_BYTES)
            {
                return Err(Error::Config(
                    "middleware command input exceeds size limit".into(),
                ));
            }
            if target.is_some_and(|target| target.batch_item_count == 0) {
                return Err(Error::Config(
                    "message target item count must be positive".into(),
                ));
            }
            Ok(())
        }
        Op::SetModel { route } => validate_identifier("model route", route, MAX_IDENTIFIER_BYTES),
        Op::ResumeSession { session_id } => {
            validate_identifier("session ID", session_id, MAX_IDENTIFIER_BYTES)
        }
    }
}

fn validate_user_input(
    text: &str,
    attachments: &[crate::protocol::SessionFileReference],
) -> Result<()> {
    if text.trim().is_empty() && attachments.is_empty() {
        return Err(Error::Config("user input cannot be empty".into()));
    }
    if text.len() > crate::protocol::MAX_USER_INPUT_BYTES {
        return Err(Error::Config("user input exceeds size limit".into()));
    }
    let limits = session_file_limits();
    if attachments.len() > limits.max_attachment_references {
        return Err(Error::Config(format!(
            "user input cannot reference more than {} attachments",
            limits.max_attachment_references
        )));
    }
    let mut attachment_ids = std::collections::BTreeSet::new();
    let mut attachment_bytes = 0_u64;
    for attachment in attachments {
        if !attachment_ids.insert(&attachment.id) {
            return Err(Error::Config(
                "attachment IDs must be unique per message".into(),
            ));
        }
        if Uuid::parse_str(&attachment.id).is_err() {
            return Err(Error::Config("attachment ID must be a UUID".into()));
        }
        validate_identifier("attachment name", &attachment.name, 255)?;
        validate_identifier("attachment media type", &attachment.media_type, 127)?;
        if !(1..=limits.max_file_bytes).contains(&attachment.size) {
            return Err(Error::Config(format!(
                "attachment size must be 1–{} bytes",
                limits.max_file_bytes
            )));
        }
        attachment_bytes = attachment_bytes
            .checked_add(attachment.size)
            .ok_or_else(|| Error::Config("attachment sizes overflowed".into()))?;
    }
    if attachment_bytes > limits.max_session_bytes {
        return Err(Error::Config(format!(
            "user input attachments exceed the {}-byte session limit",
            limits.max_session_bytes
        )));
    }
    Ok(())
}

fn validate_identifier(name: &str, value: &str, limit: usize) -> Result<()> {
    if value.trim().is_empty() {
        return Err(Error::Config(format!("{name} cannot be empty")));
    }
    if value.len() > limit {
        return Err(Error::Config(format!("{name} exceeds size limit")));
    }
    Ok(())
}

/// Bidirectional handle consumed by a frontend.
pub struct Agent {
    sender: AgentSender,
    events: mpsc::Receiver<JournalEvent>,
    model_router: Arc<ModelRouter>,
    frontend: FrontendExtensions,
    session: SessionConfiguredEvent,
    model: ModelInfo,
    model_choices: Vec<ModelChoice>,
    tool_count: usize,
    next_before_sequence: Option<u64>,
}

impl Agent {
    /// Returns a cloneable command sender.
    #[must_use]
    pub fn sender(&self) -> AgentSender {
        self.sender.clone()
    }

    /// Returns the immutable provider router owned by this runtime.
    #[must_use]
    pub fn model_router(&self) -> Arc<ModelRouter> {
        Arc::clone(&self.model_router)
    }

    /// Receives the next agent event.
    ///
    /// Frontends must keep draining events while the agent is running.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await.map(|record| record.event)
    }

    /// Returns the commands and status data exported by installed middleware.
    #[must_use]
    pub fn frontend(&self) -> &FrontendExtensions {
        &self.frontend
    }

    /// Returns the immutable session descriptor emitted at startup.
    #[must_use]
    pub fn session(&self) -> &SessionConfiguredEvent {
        &self.session
    }

    /// Returns frontend-safe settings for the selected model route.
    #[must_use]
    pub fn model(&self) -> &ModelInfo {
        &self.model
    }

    /// Returns the selected model route at frontend startup.
    #[must_use]
    pub fn model_route(&self) -> &str {
        &self.session.model.route
    }

    /// Returns every model route exposed to frontend selectors.
    #[must_use]
    pub fn model_choices(&self) -> &[ModelChoice] {
        &self.model_choices
    }

    /// Returns the number of tools registered for this agent.
    #[must_use]
    pub const fn tool_count(&self) -> usize {
        self.tool_count
    }

    /// Returns the cursor immediately preceding the initial transcript replay.
    #[must_use]
    pub const fn next_before_sequence(&self) -> Option<u64> {
        self.next_before_sequence
    }

    /// Separates command and event halves for a frontend event loop.
    ///
    /// The returned event receiver must be drained while commands are active.
    #[must_use]
    pub fn into_parts(self) -> (AgentSender, AgentEvents) {
        (self.sender, AgentEvents { inner: self.events })
    }

    /// Separates command and durably recorded event halves.
    #[must_use]
    pub fn into_recorded_parts(self) -> (AgentSender, mpsc::Receiver<JournalEvent>) {
        (self.sender, self.events)
    }
}

/// Event receiver for frontends that do not own durable event recording.
pub struct AgentEvents {
    inner: mpsc::Receiver<JournalEvent>,
}

impl AgentEvents {
    /// Receives the next event.
    pub async fn recv(&mut self) -> Option<Event> {
        self.inner.recv().await.map(|record| record.event)
    }

    /// Attempts to receive the next event without waiting.
    pub fn try_recv(&mut self) -> std::result::Result<Event, mpsc::error::TryRecvError> {
        self.inner.try_recv().map(|record| record.event)
    }
}

struct Runner {
    config: AgentConfig,
    runtime: RuntimeContext,
    system_prompt: Arc<str>,
    catalog: Catalog,
    state: Checkpoint,
    review_session_id: String,
    transcript_delta: Vec<Value>,
    deferred: VecDeque<Submission>,
    pending_session_start_stop: Option<String>,
    turn_end_turn_id: Option<String>,
    events: EventRecorder,
}

impl Runner {
    async fn run(&mut self, mut commands: mpsc::Receiver<Submission>) -> Result<()> {
        self.stop_resumed_turn_at_session_start().await?;
        if let Some(pending) = self.state.pending_approval.clone() {
            let submission_id = pending.submission_id.clone();
            if let Err(error) = self.resume_pending(&mut commands, pending).await {
                self.fail_turn(&submission_id, error).await?;
            }
        }
        loop {
            let submission = match self.deferred.pop_front() {
                Some(submission) => submission,
                None => {
                    let Some(submission) = commands.recv().await else {
                        return Ok(());
                    };
                    submission
                }
            };
            match submission.op {
                Op::UserInput { text, attachments } => {
                    if let Err(error) = self
                        .start_turn(&mut commands, submission.id.clone(), text, attachments)
                        .await
                    {
                        self.fail_turn(&submission.id, error).await?;
                    }
                }
                Op::ActiveInput { .. } => {
                    self.emit(
                        submission.id,
                        EventMsg::Warning(WarningEvent {
                            message: "there is no active turn".into(),
                        }),
                    )
                    .await?;
                }
                Op::Interrupt { .. } => {
                    self.emit(
                        submission.id,
                        EventMsg::Warning(WarningEvent {
                            message: "no active turn to interrupt".into(),
                        }),
                    )
                    .await?;
                }
                Op::ExecApproval { .. } => {
                    self.emit(
                        submission.id,
                        EventMsg::Warning(WarningEvent {
                            message: "no approval request is active".into(),
                        }),
                    )
                    .await?;
                }
                Op::CapabilityCommand {
                    capability,
                    command,
                    arguments,
                    input,
                    target,
                } => {
                    self.capability_command(
                        submission.id,
                        capability,
                        command,
                        arguments,
                        input,
                        target,
                    )
                    .await?;
                }
                Op::SetModel { route } => {
                    self.set_model(submission.id, route).await?;
                }
                Op::ResumeSession { session_id } => {
                    self.request_resume(submission.id, session_id).await?;
                }
            }
        }
    }

    async fn set_model(&mut self, submission_id: String, route: String) -> Result<()> {
        let choice = match self.config.select_model(&route) {
            Ok(choice) => choice,
            Err(error) => {
                self.emit(
                    submission_id,
                    EventMsg::Warning(WarningEvent {
                        message: error.to_string(),
                    }),
                )
                .await?;
                return Ok(());
            }
        };
        let active_route = choice.route.clone();
        let active_model = choice.model.clone();
        self.state.model_route = Some(choice.route.clone());
        self.persist_with_events(
            vec![Event {
                submission_id: Some(submission_id),
                msg: EventMsg::ModelChanged(ModelChangedEvent {
                    route: choice.route,
                    model: choice.model,
                    reasoning_effort: choice.reasoning_effort,
                    model_context_window: Some(self.config.context_window),
                }),
            }],
            None,
        )
        .await?;
        self.runtime.model_route = active_route;
        self.runtime.model = active_model;
        Ok(())
    }

    async fn request_resume(&self, submission_id: String, session_id: String) -> Result<()> {
        let result = async {
            if session_id.trim().is_empty() {
                return Err(Error::Config("session ID cannot be empty".into()));
            }
            let checkpoint = self
                .config
                .checkpoints
                .load(&session_id)
                .await?
                .ok_or_else(|| Error::Unknown(format!("session `{session_id}`")))?;
            if checkpoint.version != CHECKPOINT_VERSION || checkpoint.session_id != session_id {
                return Err(Error::Checkpoint(
                    "checkpoint does not match the requested session".into(),
                ));
            }
            Ok(checkpoint.session_context)
        }
        .await;
        match result {
            Ok(context) => self.emit(
                submission_id,
                EventMsg::SessionResumeRequested(SessionResumeRequestedEvent {
                    session_id,
                    context,
                }),
            ),
            Err(error) => self.emit(
                submission_id,
                EventMsg::Warning(WarningEvent {
                    message: error.to_string(),
                }),
            ),
        }
        .await
    }

    async fn capability_command(
        &mut self,
        submission_id: String,
        capability: String,
        command: String,
        arguments: String,
        input: Option<String>,
        target: Option<crate::protocol::MessageTarget>,
    ) -> Result<()> {
        let output = self
            .config
            .middleware
            .command(
                &capability,
                MiddlewareCommandContext {
                    command: &command,
                    arguments: &arguments,
                    input: input.as_deref(),
                    target,
                    session_id: &self.config.session_id,
                    session_context: &self.config.session_context,
                    checkpoint: &self.state,
                    checkpoints: Arc::clone(&self.config.checkpoints),
                },
            )
            .await
            .map(|output| output.events);
        match output {
            Ok(events) => {
                for event in events {
                    self.emit(&submission_id, EventMsg::Frontend(event)).await?;
                }
            }
            Err(error) => {
                self.emit(
                    submission_id,
                    EventMsg::Warning(WarningEvent {
                        message: error.to_string(),
                    }),
                )
                .await?
            }
        }
        Ok(())
    }

    async fn save(&mut self) -> Result<u64> {
        self.persist(None).await
    }

    async fn persist(&mut self, execution: Option<&ExecutionRecord>) -> Result<u64> {
        self.persist_with_events(Vec::new(), execution).await
    }

    pub(super) async fn persist_with_events(
        &mut self,
        events: Vec<Event>,
        execution: Option<&ExecutionRecord>,
    ) -> Result<u64> {
        let previous_sequence = self.state.sequence;
        self.state.sequence = self
            .state
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::Checkpoint("checkpoint sequence overflow".into()))?;
        if let Err(error) = self
            .events
            .save(&self.state, &self.transcript_delta, execution, events)
            .await
        {
            self.state.sequence = previous_sequence;
            return Err(error);
        }
        self.transcript_delta.clear();
        Ok(self.state.sequence)
    }

    fn record_model_call(&mut self) -> Result<()> {
        let active =
            self.state.active_execution.as_mut().ok_or_else(|| {
                Error::Checkpoint("model called without an active execution".into())
            })?;
        active.model_calls = active
            .model_calls
            .checked_add(1)
            .ok_or_else(|| Error::Checkpoint("execution model-call count overflow".into()))?;
        Ok(())
    }

    fn record_usage(&mut self, route: &str, usage: &TokenUsage) -> Result<()> {
        let mut total_usage = self.state.total_usage.clone();
        total_usage.checked_add(usage).ok_or_else(|| {
            Error::Provider("provider token usage exceeds the supported range".into())
        })?;
        let active = self.state.active_execution.as_mut().ok_or_else(|| {
            Error::Checkpoint("usage recorded without an active execution".into())
        })?;
        let mut execution_usage = active.usage.clone();
        execution_usage.checked_add(usage).ok_or_else(|| {
            Error::Provider("provider token usage exceeds the supported range".into())
        })?;
        if let Some(observer) = &self.config.usage_observer {
            observer(route, usage)?;
        }
        self.state.total_usage = total_usage;
        active.usage = execution_usage;
        Ok(())
    }

    fn record_tools(&mut self, tool_calls: u64, failed_tool_calls: u64) -> Result<()> {
        let active = self.state.active_execution.as_mut().ok_or_else(|| {
            Error::Checkpoint("tools recorded without an active execution".into())
        })?;
        active.tool_calls = active
            .tool_calls
            .checked_add(tool_calls)
            .ok_or_else(|| Error::Checkpoint("execution tool-call count overflow".into()))?;
        active.failed_tool_calls = active
            .failed_tool_calls
            .checked_add(failed_tool_calls)
            .ok_or_else(|| Error::Checkpoint("execution failed-tool count overflow".into()))?;
        Ok(())
    }

    fn finish_execution(&mut self, outcome: ExecutionOutcome) -> Result<ExecutionRecord> {
        self.state.finish_execution(outcome, unix_timestamp_ms()?)
    }

    async fn finish_and_persist_execution(
        &mut self,
        outcome: ExecutionOutcome,
        events: Vec<Event>,
    ) -> Result<u64> {
        let active_execution = self.state.active_execution.clone();
        let execution_stats = self.state.execution_stats.clone();
        let execution = self.finish_execution(outcome)?;
        match self.persist_with_events(events, Some(&execution)).await {
            Ok(sequence) => Ok(sequence),
            Err(error) => {
                self.state.active_execution = active_execution;
                self.state.execution_stats = execution_stats;
                Err(error)
            }
        }
    }

    fn push_context(&mut self, item: Value) {
        self.state.context.push(item.clone());
        self.transcript_delta.push(item);
    }

    fn extend_context(&mut self, items: Vec<Value>) {
        self.state.context.extend(items.iter().cloned());
        self.transcript_delta.extend(items);
    }

    async fn emit(&self, submission_id: impl Into<String>, msg: EventMsg) -> Result<()> {
        send_event(
            &self.events,
            Event {
                submission_id: Some(submission_id.into()),
                msg,
            },
        )
        .await
    }
}

fn unix_timestamp_ms() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Checkpoint("system clock predates the Unix epoch".into()))?;
    i64::try_from(elapsed.as_millis())
        .map_err(|_| Error::Checkpoint("system clock exceeds the supported range".into()))
}

async fn send_event(events: &EventRecorder, event: Event) -> Result<()> {
    events.record(event).await
}

fn try_send_event(events: &EventRecorder, event: Event) -> Result<()> {
    events.try_record(event)
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
