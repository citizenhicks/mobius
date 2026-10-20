//! Agent handles and the single linear command dispatch loop.

use crate::SessionId;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
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
use crate::backend::session_files::session_file_limits;
use crate::middleware::FrontendExtensions;
use crate::middleware::MiddlewareCommandContext;
use crate::middleware::MiddlewareStack;
use crate::middleware::RuntimeContext;
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
mod context_estimate;
mod input;
mod recorder;
mod startup;
mod tool_step;
mod turn;

pub use self::startup::create_agent;

use self::recorder::EventRecorder;

const SUBMISSION_QUEUE_CAPACITY: usize = 64;
const EVENT_QUEUE_CAPACITY: usize = 256;
const MAX_OPERATION_BYTES: usize = 256;
const DEFAULT_INITIAL_REPLAY_BATCHES: usize = 100;

type UsageObserver =
    Arc<dyn for<'a> Fn(&'a str, &'a TokenUsage) -> crate::BoxFuture<'a, Result<()>> + Send + Sync>;

/// Default maximum number of primary model steps in one turn.
pub const DEFAULT_MAX_MODEL_STEPS: usize = 2042;

/// Journal reason for an explicit turn interruption.
pub const TURN_INTERRUPTED_REASON: &str = "interrupted";
/// Journal reason for an unfinished turn recovered after a runtime restart.
pub const TURN_RESTARTED_REASON: &str = "interrupted by restart";
/// Journal reason for a closed frontend submission channel.
pub const FRONTEND_DISCONNECTED_REASON: &str = "frontend disconnected";

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
    model: Arc<ModelRouter>,
    provider: Arc<str>,
    sandbox: Arc<Sandbox>,
    checkpoints: Arc<dyn CheckpointStore>,
    middleware: Arc<MiddlewareStack>,
    system_prompt: Arc<str>,
    session_id: Option<SessionId>,
    context_window: i64,
    default_context_window: i64,
    session_context: Arc<SessionContext>,
    metadata: BTreeMap<String, Value>,
    catalog_visible: bool,
    usage_observer: Option<UsageObserver>,
    metadata_configured: bool,
    model_route_configured: bool,
    initial_replay_batches: usize,
    max_model_steps: usize,
    token_estimate: crate::middleware::TokenEstimate,
    role: AgentRole,
    children: Option<ChildAgents>,
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

impl std::fmt::Debug for Agent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Agent")
            .field("session", &self.session)
            .field("model", &self.model)
            .field("tool_count", &self.tool_count)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for AgentEvents {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentEvents")
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

    /// Replaces the effective middleware stack for this runtime.
    #[must_use]
    pub fn middleware(mut self, middleware: MiddlewareStack) -> Self {
        self.middleware = Arc::new(middleware);
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

    fn select_model(&mut self, route: &str) -> Result<ModelChangedEvent> {
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

    fn child_configuration(&self) -> Self {
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

/// Cloneable command side of a running agent.
///
/// [`Self::send`] and [`Self::submit`] never wait for queue capacity: they return
/// [`Error::Busy`] when full and [`Error::Stopped`] when disconnected. Successful
/// enqueueing does not mean the operation was accepted; consume the event stream
/// for its correlated outcome.
///
/// Dropping all strong senders requests shutdown. Use [`Agent::into_parts`] to
/// release the command half while draining [`AgentEvents`] until it closes.
#[derive(Clone)]
pub struct AgentSender {
    ingress: Arc<SubmissionIngress>,
}

impl std::fmt::Debug for AgentSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentSender")
            .finish_non_exhaustive()
    }
}

/// Durable outcome of admitting a message to its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageAcceptance {
    /// The message was committed to the durable input queue.
    Accepted,
    /// This session already admitted the submission ID, including before restart.
    AlreadyAccepted,
}

/// Completion handle for a message's durable admission, separate from its turn.
#[derive(Debug)]
#[must_use = "wait for durable admission, or use AgentSender::send without an acknowledgment"]
pub struct MessageAdmission {
    receiver: oneshot::Receiver<Result<MessageAcceptance>>,
}

impl MessageAdmission {
    /// Waits until the message is committed, rejected, or the runtime stops.
    /// # Errors
    ///
    /// Returns an error when admission fails or the runtime stops before acknowledging it.
    pub async fn wait(self) -> Result<MessageAcceptance> {
        self.receiver.await.map_err(|error| {
            Error::Stopped(format!("agent stopped before message admission: {error}"))
        })?
    }
}

type AdmissionReply = oneshot::Sender<Result<MessageAcceptance>>;

/// Non-owning command handle for middleware that must address its current agent.
#[derive(Clone)]
pub struct WeakAgentSender {
    ingress: Weak<SubmissionIngress>,
}

impl std::fmt::Debug for WeakAgentSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WeakAgentSender")
            .field("live", &(self.ingress.strong_count() > 0))
            .finish_non_exhaustive()
    }
}

/// A submission checked at ingress and kept immutable until it reaches the agent.
#[derive(Debug)]
pub struct ValidatedSubmission(Submission);

impl ValidatedSubmission {
    /// Checks a raw submission before any admission side effects.
    /// # Errors
    /// Returns an error when identifiers, message content, or operation fields are invalid.
    pub fn new(submission: Submission) -> Result<Self> {
        validate_submission(&submission)?;
        Ok(Self(submission))
    }

    /// Borrows the checked operation without allowing it to be changed.
    #[must_use]
    pub fn submission(&self) -> &Submission {
        &self.0
    }

    /// Recovers raw input for rewriting; the rewritten value must be checked again.
    #[must_use]
    pub fn into_submission(self) -> Submission {
        self.0
    }

    /// Asks a checked message to queue behind active work; delivery is not a validated field.
    #[must_use]
    pub fn with_queued_delivery(mut self) -> Self {
        if let Op::Message { message } = &mut self.0.op {
            message.requested_delivery = Some(crate::protocol::ActiveMessageDelivery::Queue);
        }
        self
    }
}

impl AgentSender {
    /// Returns a handle that does not keep the agent submission channel open.
    #[must_use]
    pub fn downgrade(&self) -> WeakAgentSender {
        WeakAgentSender {
            ingress: Arc::downgrade(&self.ingress),
        }
    }

    /// Sends a submission with a caller-controlled correlation ID.
    /// # Errors
    ///
    /// Returns an error if the transport fails or returns invalid data.
    pub fn send(&self, submission: Submission) -> Result<()> {
        self.send_validated(ValidatedSubmission::new(submission)?)
    }

    /// Enqueues a submission already checked at its ingress boundary.
    /// # Errors
    /// Returns an error if the queue is closed, full, or unavailable.
    pub fn send_validated(&self, submission: ValidatedSubmission) -> Result<()> {
        self.enqueue(submission, None)
    }

    /// Sends a message through the normal ingress and returns its durable admission handle.
    ///
    /// Acknowledgment covers input admission, not completion of the resulting turn.
    /// # Errors
    ///
    /// Returns an error for non-message operations, invalid input, or a full or closed queue.
    pub fn send_with_admission(&self, submission: Submission) -> Result<MessageAdmission> {
        self.send_validated_with_admission(ValidatedSubmission::new(submission)?)
    }

    /// Enqueues a checked message and returns its durable admission handle.
    /// # Errors
    /// Returns an error for a non-message operation or an unavailable queue.
    pub fn send_validated_with_admission(
        &self,
        submission: ValidatedSubmission,
    ) -> Result<MessageAdmission> {
        if !matches!(submission.submission().op, Op::Message { .. }) {
            return Err(Error::Config(
                "admission acknowledgments require a message".into(),
            ));
        }
        let (reply, receiver) = oneshot::channel();
        self.enqueue(submission, Some(reply))?;
        Ok(MessageAdmission { receiver })
    }

    fn enqueue(
        &self,
        submission: ValidatedSubmission,
        admission: Option<AdmissionReply>,
    ) -> Result<()> {
        let submission = submission.0;
        let mut last_sequence =
            self.ingress.last_sequence.lock().map_err(|error| {
                Error::Stopped(format!("agent submission queue poisoned: {error}"))
            })?;
        let sequence = last_sequence
            .checked_add(1)
            .ok_or_else(|| Error::Busy("agent submission sequence exhausted".into()))?;
        self.ingress
            .sender
            .try_send(SequencedSubmission {
                sequence,
                submission,
                admission,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    Error::Busy("agent submission queue is full".into())
                }
                mpsc::error::TrySendError::Closed(_) => {
                    Error::Stopped("agent submission channel closed".into())
                }
            })?;
        *last_sequence = sequence;
        drop(last_sequence);
        Ok(())
    }

    /// Submits a command and returns its correlation ID.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn submit(&self, op: Op) -> Result<Arc<str>> {
        let id: Arc<str> = Uuid::new_v4().to_string().into();
        self.send(Submission {
            id: Arc::clone(&id),
            op,
        })?;
        Ok(id)
    }
}

impl WeakAgentSender {
    /// Returns a live sender while another owner keeps the agent connected.
    #[must_use]
    pub fn upgrade(&self) -> Option<AgentSender> {
        self.ingress
            .upgrade()
            .map(|ingress| AgentSender { ingress })
    }
}

struct SubmissionIngress {
    sender: mpsc::Sender<SequencedSubmission>,
    last_sequence: Arc<Mutex<u64>>,
}

struct SequencedSubmission {
    sequence: u64,
    submission: Submission,
    admission: Option<AdmissionReply>,
}

struct ReceivedSubmission {
    submission: Submission,
    admission: Option<AdmissionReply>,
}

struct SubmissionInbox {
    receiver: mpsc::Receiver<SequencedSubmission>,
    last_sent_sequence: Arc<Mutex<u64>>,
    last_sequence: u64,
}

impl SubmissionInbox {
    async fn recv(&mut self) -> Option<ReceivedSubmission> {
        let queued = self.receiver.recv().await?;
        self.last_sequence = queued.sequence;
        Some(ReceivedSubmission {
            submission: queued.submission,
            admission: queued.admission,
        })
    }

    fn cutoff(&self) -> Result<u64> {
        self.last_sent_sequence
            .lock()
            .map(|sequence| *sequence)
            .map_err(|error| Error::Stopped(format!("agent submission queue poisoned: {error}")))
    }
}

/// Creates child agents from the root agent's configuration while that root runs.
///
/// Children and their descendants share the root's configuration; once the root
/// runtime stops, every handle refuses to create more agents.
#[derive(Clone, Default)]
pub struct ChildAgents(Weak<AgentConfig>);

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

fn submission_channel(capacity: usize) -> (AgentSender, SubmissionInbox) {
    let (sender, receiver) = mpsc::channel(capacity);
    let last_sequence = Arc::new(Mutex::new(0));
    let ingress = Arc::new(SubmissionIngress {
        sender,
        last_sequence: Arc::clone(&last_sequence),
    });
    (
        AgentSender { ingress },
        SubmissionInbox {
            receiver,
            last_sent_sequence: last_sequence,
            last_sequence: 0,
        },
    )
}

#[cfg(test)]
pub(crate) fn test_sender() -> WeakAgentSender {
    submission_channel(1).0.downgrade()
}

/// Validates one submission before callers perform more expensive boundary work.
/// # Errors
///
/// Returns an error if the supplied value is invalid.
pub fn validate_submission(submission: &Submission) -> Result<()> {
    crate::validate_identifier("submission ID", &submission.id, crate::MAX_IDENTIFIER_BYTES)?;
    match &submission.op {
        Op::Message { message } => message.validate(session_file_limits()),
        Op::Interrupt { turn_id } => {
            crate::validate_identifier("turn ID", turn_id, crate::MAX_IDENTIFIER_BYTES)
        }
        Op::ExecApproval { id, .. } => {
            crate::validate_identifier("approval ID", id, crate::MAX_IDENTIFIER_BYTES)
        }
        Op::CapabilityCommand {
            capability,
            command,
            arguments,
            input,
            ..
        } => {
            crate::validate_identifier("capability ID", capability, MAX_OPERATION_BYTES)?;
            crate::validate_identifier("command", command, MAX_OPERATION_BYTES)?;
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
            Ok(())
        }
        Op::SetModel { route } => {
            crate::validate_identifier("model route", route, crate::MAX_IDENTIFIER_BYTES)
        }
        Op::ResumeSession { .. } => Ok(()),
    }
}

/// Bidirectional handle consumed by a frontend.
pub struct Agent {
    sender: AgentSender,
    events: mpsc::Receiver<JournalEvent>,
    model_router: Arc<ModelRouter>,
    frontend: FrontendExtensions,
    frontend_sink: crate::middleware::FrontendEventSink,
    session: Arc<SessionConfiguredEvent>,
    model: ModelInfo,
    tool_count: usize,
    next_before_sequence: Option<u64>,
}

impl Agent {
    /// Returns a cloneable command sender.
    #[must_use]
    pub fn sender(&self) -> AgentSender {
        AgentSender {
            ingress: Arc::clone(&self.sender.ingress),
        }
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

    /// Returns the existing recorder for capability-owned frontend updates.
    /// This sink does not keep a stopped Agent or its event stream alive.
    #[must_use]
    pub fn frontend_sink(&self) -> crate::middleware::FrontendEventSink {
        Arc::clone(&self.frontend_sink)
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
    pub fn model_choices(
        &self,
    ) -> impl DoubleEndedIterator<Item = &ModelChoice> + ExactSizeIterator {
        self.model_router.choices()
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

    /// Moves command/event halves and startup descriptors into a durable frontend owner.
    #[must_use]
    pub fn into_recorded_parts(
        self,
    ) -> (
        AgentSender,
        mpsc::Receiver<JournalEvent>,
        Arc<SessionConfiguredEvent>,
        FrontendExtensions,
    ) {
        (self.sender, self.events, self.session, self.frontend)
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
    /// # Errors
    ///
    /// Returns an error if no event is ready or the event channel has closed.
    pub fn try_recv(&mut self) -> std::result::Result<Event, mpsc::error::TryRecvError> {
        self.inner.try_recv().map(|record| record.event)
    }
}

struct Runner {
    config: AgentConfig,
    runtime: Arc<RuntimeContext>,
    system_prompt: Arc<str>,
    /// The system prompt's parts, estimated once when it is assembled.
    prompt_estimate: context_estimate::ContextEstimate,
    /// The last model request's parts, scaled once its usage arrives.
    context_estimate: Option<context_estimate::ContextEstimate>,
    catalog: Arc<Catalog>,
    state: LiveCheckpoint,
    transcript_delta: Vec<Arc<Value>>,
    pending_session_start_stop: Option<String>,
    turn_end_turn_id: Option<Arc<str>>,
    events: EventRecorder,
}

/// The actor mutates uniquely owned state; recorder snapshots share it until their ack.
struct LiveCheckpoint(Arc<Checkpoint>);

impl std::ops::Deref for LiveCheckpoint {
    type Target = Checkpoint;

    fn deref(&self) -> &Checkpoint {
        &self.0
    }
}

impl LiveCheckpoint {
    fn make_mut(&mut self) -> &mut Checkpoint {
        Arc::make_mut(&mut self.0)
    }
}

impl Runner {
    async fn run(&mut self, mut inbox: SubmissionInbox) -> Result<()> {
        self.stop_resumed_turn_at_session_start().await?;
        let pending = self.state.pending_approval.clone(); // rules-allow: the checkpoint keeps the approval until it resolves
        if let Some(mut pending) = pending {
            if let Err(error) = self.resume_pending(&mut inbox, &mut pending).await {
                self.fail_turn(&pending.submission_id, error).await?;
            }
        } else if let Some(active) = self.state.active_execution.as_ref() {
            let submission_id = Arc::clone(&active.submission_id);
            let turn_id = Arc::clone(&active.turn_id);
            if let Err(error) = self
                .continue_turn(&mut inbox, &submission_id, &turn_id)
                .await
            {
                self.fail_turn(&submission_id, error).await?;
            }
        }
        loop {
            if let Some(message) = self
                .config
                .middleware
                .next_turn(&mut self.state.make_mut().pending_messages)?
            {
                let submission_id = Arc::clone(&message.submission_id);
                if let Err(error) = self.start_message_turn(&mut inbox, message).await {
                    self.fail_turn(&submission_id, error).await?;
                }
                continue;
            }
            let submission = {
                let Some(submission) = inbox.recv().await else {
                    return Ok(());
                };
                submission
            };
            let ReceivedSubmission {
                submission,
                admission,
            } = submission;
            match submission.op {
                Op::Message { message } => {
                    self.admit_idle_message(submission.id, message, admission)
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

    async fn set_model(&mut self, submission_id: Arc<str>, route: String) -> Result<()> {
        let choice = match self.config.select_model(&route) {
            Ok(choice) => choice,
            Err(error) => {
                self.emit(
                    submission_id,
                    EventMsg::Warning(WarningEvent {
                        message: error.to_string().into(),
                    }),
                )
                .await?;
                return Ok(());
            }
        };
        let active_route = Arc::clone(&choice.route);
        let active_model = Arc::clone(&choice.model);
        self.state.make_mut().model_route = Some(Arc::clone(&choice.route));
        self.persist_with_events(
            vec![Event {
                submission_id: Some(submission_id),
                msg: EventMsg::ModelChanged(choice),
            }],
            None,
        )
        .await?;
        let runtime = Arc::make_mut(&mut self.runtime);
        runtime.model_route = active_route;
        runtime.model = active_model;
        Ok(())
    }

    async fn request_resume(&self, submission_id: Arc<str>, session_id: SessionId) -> Result<()> {
        let result = async {
            let checkpoint = self
                .config
                .checkpoints
                .load(&session_id)
                .await?
                .ok_or_else(|| Error::Unknown(format!("session `{session_id}`")))?;
            if checkpoint.version != CHECKPOINT_VERSION || *checkpoint.session_id != *session_id {
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
                    message: error.to_string().into(),
                }),
            ),
        }
        .await
    }

    async fn capability_command(
        &self,
        submission_id: Arc<str>,
        capability: Arc<str>,
        command: Arc<str>,
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
                    session_id: &self.runtime.session_id,
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
                    self.emit(Arc::clone(&submission_id), EventMsg::Frontend(event))
                        .await?;
                }
            }
            Err(error) => {
                self.emit(
                    submission_id,
                    EventMsg::Warning(WarningEvent {
                        message: error.to_string().into(),
                    }),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn save(&mut self) -> Result<u64> {
        self.persist(None).await
    }

    async fn persist(&mut self, execution: Option<ExecutionRecord>) -> Result<u64> {
        self.persist_with_events(Vec::new(), execution).await
    }

    pub(super) async fn persist_with_events(
        &mut self,
        events: Vec<Event>,
        execution: Option<ExecutionRecord>,
    ) -> Result<u64> {
        let previous_sequence = self.state.sequence;
        self.state.make_mut().sequence = self
            .state
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::Checkpoint("checkpoint sequence overflow".into()))?;
        if let Err(error) = self
            .events
            .save(
                Arc::clone(&self.state.0),
                &self.transcript_delta,
                execution,
                events,
            )
            .await
        {
            self.state.make_mut().sequence = previous_sequence;
            return Err(error);
        }
        self.transcript_delta.clear();
        Ok(self.state.sequence)
    }

    fn record_model_call(&mut self) -> Result<()> {
        let active = self
            .state
            .make_mut()
            .active_execution
            .as_mut()
            .ok_or_else(|| Error::Checkpoint("model called without an active execution".into()))?;
        active.model_calls = active
            .model_calls
            .checked_add(1)
            .ok_or_else(|| Error::Checkpoint("execution model-call count overflow".into()))?;
        Ok(())
    }

    async fn record_usage(&mut self, route: &str, usage: &TokenUsage) -> Result<()> {
        let state = self.state.make_mut();
        // Stage usage totals so overflow or observer failure leaves durable counters unchanged.
        let mut total_usage = state.total_usage;
        total_usage.checked_add(usage).ok_or_else(|| {
            Error::Provider("provider token usage exceeds the supported range".into())
        })?;
        let active = state.active_execution.as_mut().ok_or_else(|| {
            Error::Checkpoint("usage recorded without an active execution".into())
        })?;
        // Stage execution usage alongside totals before committing either counter.
        let mut execution_usage = active.usage;
        execution_usage.checked_add(usage).ok_or_else(|| {
            Error::Provider("provider token usage exceeds the supported range".into())
        })?;
        if let Some(observer) = &self.config.usage_observer {
            observer(route, usage).await?;
        }
        state.total_usage = total_usage;
        active.usage = execution_usage;
        Ok(())
    }

    fn record_tools(&mut self, tool_calls: u64, failed_tool_calls: u64) -> Result<()> {
        let active = self
            .state
            .make_mut()
            .active_execution
            .as_mut()
            .ok_or_else(|| {
                Error::Checkpoint("tools recorded without an active execution".into())
            })?;
        let tool_calls = active
            .tool_calls
            .checked_add(tool_calls)
            .ok_or_else(|| Error::Checkpoint("execution tool-call count overflow".into()))?;
        let failed_tool_calls = active
            .failed_tool_calls
            .checked_add(failed_tool_calls)
            .ok_or_else(|| Error::Checkpoint("execution failed-tool count overflow".into()))?;
        active.tool_calls = tool_calls;
        active.failed_tool_calls = failed_tool_calls;
        Ok(())
    }

    fn finish_execution(&mut self, outcome: ExecutionOutcome) -> Result<ExecutionRecord> {
        self.state
            .make_mut()
            .finish_execution(outcome, unix_timestamp_ms()?)
    }

    async fn finish_and_persist_execution(
        &mut self,
        outcome: ExecutionOutcome,
        events: Vec<Event>,
    ) -> Result<u64> {
        // Restore the active execution if its completed record cannot be persisted.
        let active_execution = self.state.active_execution.clone(); // rules-allow: restored if the completed record cannot be persisted
        let execution_stats = self.state.execution_stats;
        let execution = self.finish_execution(outcome)?;
        match self.persist_with_events(events, Some(execution)).await {
            Ok(sequence) => Ok(sequence),
            Err(error) => {
                self.state.make_mut().active_execution = active_execution;
                self.state.make_mut().execution_stats = execution_stats;
                Err(error)
            }
        }
    }

    fn push_context(&mut self, item: Value) {
        let item = Arc::new(item);
        Arc::make_mut(&mut self.state.make_mut().context).push(Arc::clone(&item));
        self.transcript_delta.push(item);
    }

    fn extend_context(&mut self, items: Vec<Value>) {
        for item in items {
            if crate::middleware::delivery_once::accept(
                &mut self.state.make_mut().delivered_once,
                &item,
            ) {
                self.push_context(item);
            }
        }
    }

    async fn emit(&self, submission_id: Arc<str>, msg: EventMsg) -> Result<()> {
        send_event(
            &self.events,
            Event {
                submission_id: Some(submission_id),
                msg,
            },
        )
        .await
    }
}

fn unix_timestamp_ms() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            Error::Checkpoint(format!("system clock predates the Unix epoch: {error}"))
        })?;
    i64::try_from(elapsed.as_millis()).map_err(|error| {
        Error::Checkpoint(format!("system clock exceeds the supported range: {error}"))
    })
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
