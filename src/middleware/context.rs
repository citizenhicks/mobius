use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::sync::Arc;

use serde_json::Value;

use super::MiddlewareStack;
use super::approximate_tokens;
use super::tools::Catalog;
use super::tools::ToolResult;
use crate::agent::{AgentRole, WeakAgentSender};
use crate::backend::checkpoint::{
    Checkpoint, CheckpointStore, ContextRewriteReason, ExecutionOutcome, MAX_QUEUED_MESSAGES,
    QueuedMessage as DurableQueuedMessage, QueuedMessageBoundary,
};
use crate::backend::model::{ModelRouter, ToolCall, message_input};
use crate::backend::sandbox::ApprovalPolicy;
use crate::protocol::{
    EventMsg, FrontendEvent, MAX_CAPABILITY_INPUT_BYTES, MessageAuthor, MessageEvent,
    MessageSubmission, MessageTarget, ReviewDecision, SessionContext, SessionFileReference,
    TokenUsage, message_metadata,
};
use crate::{Error, Result};

/// Sends middleware-owned UI updates without depending on a concrete frontend.
pub type FrontendEventSink = Arc<dyn Fn(FrontendEvent) -> Result<()> + Send + Sync>;

/// Read-only queued message owned by the middleware receiving it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueuedMessageView<'a> {
    item: &'a DurableQueuedMessage,
}

impl<'a> QueuedMessageView<'a> {
    /// Returns the identity token required by a conditional queue mutation.
    #[must_use]
    pub fn id(&self) -> &'a str {
        self.item.id()
    }

    /// Returns the prepared presentation event.
    #[must_use]
    pub fn event(&self) -> MessageEvent {
        self.item.event()
    }
}

/// Read-only startup snapshot containing only one middleware's queued messages.
#[derive(Clone, Default)]
pub struct QueuedMessageSnapshot {
    items: Vec<DurableQueuedMessage>,
}

impl QueuedMessageSnapshot {
    /// Returns every queued item owned by this middleware, oldest first.
    pub fn views(&self) -> impl Iterator<Item = QueuedMessageView<'_>> {
        self.items.iter().map(|item| QueuedMessageView { item })
    }

    pub(super) fn for_owner(owner: &str, items: &[DurableQueuedMessage]) -> Self {
        Self {
            items: items
                .iter()
                .filter(|item| item.owner() == owner)
                .cloned()
                .collect(),
        }
    }
}

/// Mutable scoped view of messages retained until their delivery boundary.
pub struct MessageQueue<'a> {
    items: &'a mut Vec<DurableQueuedMessage>,
    owner: Option<&'static str>,
}

impl<'a> MessageQueue<'a> {
    pub(crate) fn new(items: &'a mut Vec<DurableQueuedMessage>) -> Self {
        Self { items, owner: None }
    }

    pub(super) fn scope(&mut self, owner: &'static str) {
        self.owner = Some(owner);
    }

    fn owner(&self) -> Result<&'static str> {
        self.owner
            .ok_or_else(|| Error::Config("message queue is not scoped to a middleware".into()))
    }

    /// Returns the number of queued items owned by this middleware.
    #[must_use]
    pub fn count(&self) -> usize {
        let Some(owner) = self.owner else {
            return 0;
        };
        self.items
            .iter()
            .filter(|item| item.owner() == owner)
            .count()
    }

    /// Returns the newest message available to this context.
    #[must_use]
    pub fn latest(&self) -> Option<QueuedMessageView<'_>> {
        let owner = self.owner?;
        self.items
            .iter()
            .rev()
            .find(|item| item.owner() == owner)
            .map(|item| QueuedMessageView { item })
    }

    /// Returns one owned item by its revision identity.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<QueuedMessageView<'_>> {
        let owner = self.owner?;
        self.items
            .iter()
            .find(|item| item.owner() == owner && item.id() == id)
            .map(|item| QueuedMessageView { item })
    }

    /// Appends one prepared message, or returns `false` when it is full or duplicated.
    pub fn enqueue(
        &mut self,
        id: &str,
        boundary: QueuedMessageBoundary,
        event: MessageEvent,
    ) -> Result<bool> {
        let owner = self.owner()?;
        let item = DurableQueuedMessage::new(owner, id, boundary, event)?;
        if self.items.len() >= MAX_QUEUED_MESSAGES {
            return Ok(false);
        }
        if self
            .items
            .iter()
            .any(|item| item.owner() == owner && item.id() == id)
        {
            return Ok(false);
        }
        self.items.push(item);
        Ok(true)
    }

    /// Atomically replaces one owned item while preserving its queue position.
    pub fn replace(&mut self, id: &str, replacement_id: &str, event: MessageEvent) -> Result<bool> {
        let owner = self.owner()?;
        let Some(index) = self
            .items
            .iter()
            .position(|item| item.owner() == owner && item.id() == id)
        else {
            return Ok(false);
        };
        if self.items.iter().enumerate().any(|(candidate, item)| {
            candidate != index && item.owner() == owner && item.id() == replacement_id
        }) {
            return Ok(false);
        }
        self.items[index].replace(replacement_id, event)?;
        Ok(true)
    }

    pub(crate) fn stage_model_messages(&mut self, turn_id: &str) -> Result<Vec<PreparedMessage>> {
        let Some(owner) = self.owner else {
            return Ok(Vec::new());
        };
        self.items
            .extract_if(.., |item| {
                item.owner() == owner
                    && matches!(
                        item.boundary(),
                        QueuedMessageBoundary::Steer { turn_id: target }
                            if target == turn_id
                    )
            })
            .map(PreparedMessage::try_from)
            .collect()
    }

    pub(crate) fn next_turn(&self) -> Result<Option<PreparedMessage>> {
        let owner = self.owner()?;
        self.items
            .iter()
            .find(|item| item.owner() == owner && item.boundary().starts_turn())
            .cloned()
            .map(PreparedMessage::try_from)
            .transpose()
    }

    pub(crate) fn consume_next_turn(&mut self, id: &str) -> Result<()> {
        let owner = self.owner()?;
        let index = self
            .items
            .iter()
            .position(|item| {
                item.owner() == owner && item.id() == id && item.boundary().starts_turn()
            })
            .ok_or_else(|| Error::Checkpoint("prepared message is no longer queued".into()))?;
        self.items.remove(index);
        Ok(())
    }

    pub(crate) fn promote_failed_turn(&mut self, turn_id: &str) -> Result<()> {
        let owner = self.owner()?;
        for item in self.items.iter_mut().filter(|item| {
            item.owner() == owner
                && matches!(
                    item.boundary(),
                    QueuedMessageBoundary::Steer { turn_id: target }
                        if target == turn_id
                )
        }) {
            item.promote_to_next_turn()?;
        }
        Ok(())
    }
}

/// One queued message prepared for its model boundary.
pub(crate) struct PreparedMessage {
    pub(crate) submission_id: String,
    pub(crate) input: Value,
    pub(crate) event: EventMsg,
    pub(crate) title_seed: Option<String>,
    pub(crate) boundary_events: Vec<EventMsg>,
}

impl TryFrom<DurableQueuedMessage> for PreparedMessage {
    type Error = Error;

    fn try_from(message: DurableQueuedMessage) -> Result<Self> {
        let (submission_id, event) = message.into_parts();
        let input = message_input(&event)?;
        let title_seed = matches!(
            event.author,
            MessageAuthor::User | MessageAuthor::Peer { .. }
        )
        .then(|| event.text.trim().to_string())
        .filter(|title| !title.is_empty());
        Ok(Self {
            submission_id,
            input,
            event: EventMsg::Message(event),
            title_seed,
            boundary_events: Vec::new(),
        })
    }
}

/// Durable runtime identity exposed while middleware starts a session.
#[derive(Clone)]
pub struct RuntimeContext {
    pub sender: WeakAgentSender,
    pub checkpoints: Arc<dyn CheckpointStore>,
    pub session_id: String,
    pub model_route: String,
    pub model: String,
    pub approval_policy: ApprovalPolicy,
    pub session_context: SessionContext,
    pub metadata: BTreeMap<String, Value>,
    pub role: AgentRole,
    pub frontend: FrontendEventSink,
}

impl RuntimeContext {
    pub(crate) fn turn_identity<'a>(&'a self, turn_id: &'a str) -> TurnIdentity<'a> {
        TurnIdentity {
            session_id: &self.session_id,
            turn_id,
            model: &self.model,
            approval_policy: self.approval_policy,
        }
    }
}

/// Stable facts shared by hooks that run within one active turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnIdentity<'a> {
    pub session_id: &'a str,
    pub turn_id: &'a str,
    pub model: &'a str,
    pub approval_policy: ApprovalPolicy,
}

/// Why [`Middleware::session_start`](super::Middleware::session_start) is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStartSource {
    Startup,
    Resume,
    Compact,
}

/// Mutable state shared by the declaration-ordered `SessionStart` hooks.
pub struct SessionStartContext<'a> {
    pub runtime: &'a RuntimeContext,
    pub(crate) source: SessionStartSource,
    pub(crate) queued_messages: QueuedMessageSnapshot,
    pub(crate) input: &'a mut Vec<Value>,
    pub(crate) input_changed: bool,
    pub(crate) stop_reason: Option<String>,
}

impl SessionStartContext<'_> {
    #[must_use]
    pub fn source(&self) -> SessionStartSource {
        self.source
    }

    #[must_use]
    pub fn queued_messages(&self) -> &QueuedMessageSnapshot {
        &self.queued_messages
    }

    /// Appends hidden provider context produced while the session starts.
    pub fn push_input(&mut self, item: Value) {
        self.input.push(item);
        self.input_changed = true;
    }

    pub(crate) fn retain_input(&mut self, mut keep: impl FnMut(&Value) -> bool) {
        let input_len = self.input.len();
        self.input.retain(&mut keep);
        self.input_changed |= self.input.len() != input_len;
    }

    /// Stops the active turn after session-start processing completes.
    pub fn stop(&mut self, reason: impl Into<String>) -> Result<()> {
        set_stop_reason(&mut self.stop_reason, "session-start stop", reason)
    }

    /// Returns the first stop requested by the ordered middleware chain.
    #[must_use]
    pub fn stop_reason(&self) -> Option<&str> {
        self.stop_reason.as_deref()
    }
}

/// Mutable state exposed before a prepared next-turn message enters durable context.
pub struct MessageSubmitContext<'a> {
    pub turn: TurnIdentity<'a>,
    pub author: &'a MessageAuthor,
    pub message: &'a str,
    pub attachments: &'a [SessionFileReference],
    pub events: &'a mut Vec<EventMsg>,
    pub(crate) input: Vec<Value>,
    pub(crate) rejection: Option<String>,
}

impl MessageSubmitContext<'_> {
    /// Adds provider-neutral context immediately before the submitted message.
    pub fn push_input(&mut self, item: Value) {
        self.input.push(item);
    }

    /// Rejects the submission without treating the policy decision as a hook failure.
    pub fn reject(&mut self, reason: impl Into<String>) -> Result<()> {
        let reason = hook_message("prompt rejection", reason)?;
        if self.rejection.is_none() {
            self.rejection = Some(reason);
        }
        Ok(())
    }
}

pub(crate) struct MessageSubmitResult {
    pub(crate) input: Vec<Value>,
    pub(crate) rejection: Option<String>,
}

/// Mutable state exposed immediately before a model request.
pub struct ModelContext<'a> {
    pub model: &'a ModelRouter,
    pub provider: &'a str,
    pub session_id: &'a str,
    pub session_context: &'a SessionContext,
    pub metadata: &'a BTreeMap<String, Value>,
    pub turn_id: &'a str,
    pub model_step: usize,
    pub context_window: i64,
    pub instructions: &'a str,
    pub(crate) checkpoint_sequence: u64,
    pub(crate) request_input: &'a mut Vec<Value>,
    pub(crate) available_tools: &'a mut BTreeSet<String>,
    pub(crate) durable_input: &'a mut Vec<Value>,
    pub(crate) transcript_delta: &'a mut Vec<Value>,
    pub(crate) context_epoch: &'a mut u64,
    pub(crate) compaction_count: &'a mut u64,
    pub(crate) rewrite_reasons: &'a mut Vec<ContextRewriteReason>,
    pub(crate) turn_stop: &'a mut Option<String>,
    pub(crate) queued_messages: Vec<DurableQueuedMessage>,
    pub last_usage: Option<&'a TokenUsage>,
    pub tools: &'a Catalog,
    pub events: &'a mut Vec<EventMsg>,
    pub usage: &'a mut Vec<TokenUsage>,
    /// Set when this hook changes durable checkpoint state.
    pub(crate) checkpoint_changed: &'a mut bool,
    pub(crate) runtime: &'a RuntimeContext,
    pub(crate) hooks: &'a MiddlewareStack,
}

/// Live capability state used to hide registered tools at a model boundary.
pub struct ToolExposureContext<'a> {
    pub session_id: &'a str,
    pub(crate) input: &'a [Value],
    pub(crate) available: &'a mut BTreeSet<String>,
}

impl ToolExposureContext<'_> {
    /// Returns the most recent typed conversation message in model context.
    #[must_use]
    pub fn latest_message(&self) -> Option<MessageEvent> {
        self.input.iter().rev().find_map(message_metadata)
    }

    /// Hides registered tools for this boundary.
    pub fn hide(&mut self, names: &[&str]) {
        for name in names {
            self.available.remove(*name);
        }
    }
}

impl ModelContext<'_> {
    /// Returns durable provider-neutral model context.
    #[must_use]
    pub fn input(&self) -> &[Value] {
        self.durable_input
    }

    /// Returns the request input including earlier request-only middleware additions.
    #[must_use]
    pub fn request_input(&self) -> &[Value] {
        self.request_input
    }

    /// Replaces active model context and advances its rewrite epoch once per boundary.
    pub fn rewrite_input(&mut self, reason: ContextRewriteReason, input: Vec<Value>) -> Result<()> {
        if *self.durable_input == input {
            return Ok(());
        }
        if self.rewrite_reasons.is_empty() {
            *self.context_epoch = self
                .context_epoch
                .checked_add(1)
                .ok_or_else(|| Error::Checkpoint("context rewrite epoch overflow".into()))?;
        }
        if !self.rewrite_reasons.contains(&reason) {
            self.rewrite_reasons.push(reason);
        }
        self.durable_input.clone_from(&input);
        *self.request_input = input;
        *self.checkpoint_changed = true;
        Ok(())
    }

    /// Appends a durable replay item without adding it to provider context.
    pub(crate) fn record_transcript_item(&mut self, item: Value) {
        self.transcript_delta.push(item);
        *self.checkpoint_changed = true;
    }

    /// Appends durable provider context without adding synthetic replay history.
    pub fn append_model_input(&mut self, item: Value) {
        self.request_input.push(item.clone());
        self.durable_input.push(item);
        *self.checkpoint_changed = true;
    }

    /// Appends durable input to model context and its transcript journal.
    pub fn push_input(&mut self, item: Value) -> Result<MessageTarget> {
        self.request_input.push(item.clone());
        self.durable_input.push(item.clone());
        self.transcript_delta.push(item);
        *self.checkpoint_changed = true;
        provisional_message_target(self.checkpoint_sequence, self.transcript_delta.len())
    }

    /// Estimates serialized model input at four bytes per token.
    #[must_use]
    pub fn estimated_input_tokens(&self) -> i64 {
        let mut bytes = ByteCounter::default();
        if serde_json::to_writer(&mut bytes, self.durable_input).is_err() {
            return i64::MAX;
        }
        i64::try_from(approximate_tokens(bytes.0)).unwrap_or(i64::MAX)
    }

    pub(crate) async fn pre_compact(&mut self) -> Result<()> {
        let hooks = self.hooks;
        let stop_reason = hooks
            .pre_compact(CompactContext {
                session_id: self.session_id,
                turn_id: self.turn_id,
                model: &self.runtime.model,
                input: self.durable_input,
                events: self.events,
                stop_reason: None,
            })
            .await?;
        set_first(self.turn_stop, stop_reason);
        Ok(())
    }

    pub(crate) async fn post_compact(&mut self) -> Result<()> {
        let hooks = self.hooks;
        let stop_reason = hooks
            .post_compact(CompactContext {
                session_id: self.session_id,
                turn_id: self.turn_id,
                model: &self.runtime.model,
                input: self.durable_input,
                events: self.events,
                stop_reason: None,
            })
            .await?;
        set_first(self.turn_stop, stop_reason);
        if self.turn_stop.is_some() {
            return Ok(());
        }
        let start = hooks
            .session_start(
                self.runtime,
                &self.queued_messages,
                SessionStartSource::Compact,
                self.durable_input,
            )
            .await?;
        set_first(self.turn_stop, start.stop_reason);
        self.request_input.clone_from(self.durable_input);
        Ok(())
    }

    #[must_use]
    pub(crate) fn turn_stopped(&self) -> bool {
        self.turn_stop.is_some()
    }
}

/// Request-only model input exposed after every durable `PreModel` hook.
pub struct ModelRequestContext<'a> {
    pub model: &'a ModelRouter,
    pub provider: &'a str,
    pub session_id: &'a str,
    pub turn_id: &'a str,
    pub model_step: usize,
    pub(crate) input: &'a mut Vec<Value>,
}

impl ModelRequestContext<'_> {
    /// Returns the input currently prepared for this one model request.
    #[must_use]
    pub fn input(&self) -> &[Value] {
        self.input
    }

    /// Replaces only the input sent by this model request.
    pub fn replace_input(&mut self, input: Vec<Value>) {
        *self.input = input;
    }
}

/// Mutable policy boundary for one normalized model-requested tool call.
pub struct PreToolUseContext<'a> {
    pub turn: TurnIdentity<'a>,
    pub events: &'a mut Vec<EventMsg>,
    pub(crate) tools: &'a Catalog,
    pub(crate) call: &'a mut ToolCall,
    pub(crate) input: Vec<Value>,
    pub(crate) denial: Option<String>,
}

impl PreToolUseContext<'_> {
    /// Returns the call after any earlier middleware rewrites.
    #[must_use]
    pub fn call(&self) -> &ToolCall {
        self.call
    }

    /// Replaces the tool name and arguments while preserving the provider call ID.
    pub fn replace(&mut self, name: impl Into<String>, arguments: Value) -> Result<()> {
        self.call.replace(name.into(), arguments)
    }

    /// Adds durable provider-neutral context before this call at a tool-complete boundary.
    pub fn push_input(&mut self, item: Value) {
        self.input.push(item);
    }

    /// Denies the call. Later middleware may observe but cannot undo the denial.
    pub fn deny(&mut self, reason: impl Into<String>) -> Result<()> {
        let reason = hook_message("tool denial", reason)?;
        if self.denial.is_none() {
            self.denial = Some(reason);
        }
        Ok(())
    }

    /// Returns the first denial made by the ordered middleware chain.
    #[must_use]
    pub fn denial(&self) -> Option<&str> {
        self.denial.as_deref()
    }
}

/// Mutable policy boundary for a sandbox approval request.
pub struct PermissionRequestContext<'a> {
    pub turn: TurnIdentity<'a>,
    pub calls: &'a [ToolCall],
    pub requested_call_ids: &'a [String],
    pub reason: &'a str,
    pub events: &'a mut Vec<EventMsg>,
    pub(crate) tools: &'a Catalog,
    pub(crate) decision: Option<ReviewDecision>,
}

impl PermissionRequestContext<'_> {
    /// Returns the decision accumulated from earlier middleware.
    #[must_use]
    pub fn decision(&self) -> Option<&ReviewDecision> {
        self.decision.as_ref()
    }

    /// Allows this request unless an earlier middleware denied it.
    pub fn allow(&mut self) {
        if !matches!(self.decision, Some(ReviewDecision::Denied { .. })) {
            self.decision = Some(ReviewDecision::Approved);
        }
    }

    /// Denies this request. The decision cannot be weakened by later middleware.
    pub fn deny(&mut self, reason: impl Into<String>) -> Result<()> {
        let reason = hook_message("permission denial", reason)?;
        if !matches!(self.decision, Some(ReviewDecision::Denied { .. })) {
            self.decision = Some(ReviewDecision::Denied { rejection: reason });
        }
        Ok(())
    }
}

/// Mutable model-visible result exposed after an executed tool call.
pub struct PostToolUseContext<'a> {
    pub turn: TurnIdentity<'a>,
    pub call: &'a ToolCall,
    pub events: &'a mut Vec<EventMsg>,
    pub(crate) tools: &'a Catalog,
    pub(crate) result: &'a mut ToolResult,
}

impl PostToolUseContext<'_> {
    /// Returns the result after any earlier middleware changes.
    #[must_use]
    pub fn result(&self) -> &ToolResult {
        self.result
    }

    /// Replaces the feedback returned to the model without changing past side effects.
    pub fn replace(&mut self, output: impl Into<String>) {
        self.result.replace(output.into());
    }

    /// Adds provider-neutral context immediately after this tool output.
    pub fn push_input(&mut self, item: Value) {
        self.result.additional_input.push(item);
    }
}

/// State exposed immediately before or after context compaction.
pub struct CompactContext<'a> {
    pub session_id: &'a str,
    pub turn_id: &'a str,
    pub model: &'a str,
    pub input: &'a [Value],
    pub events: &'a mut Vec<EventMsg>,
    pub(crate) stop_reason: Option<String>,
}

impl CompactContext<'_> {
    /// Stops the active turn at this compaction boundary.
    pub fn stop(&mut self, reason: impl Into<String>) -> Result<()> {
        set_stop_reason(&mut self.stop_reason, "compaction stop", reason)
    }

    /// Returns the first stop requested by the ordered middleware chain.
    #[must_use]
    pub fn stop_reason(&self) -> Option<&str> {
        self.stop_reason.as_deref()
    }
}

/// Mutable policy boundary immediately before normal turn completion.
pub struct StopContext<'a> {
    pub turn: TurnIdentity<'a>,
    pub events: &'a mut Vec<EventMsg>,
    pub(crate) role: &'a AgentRole,
    pub(crate) stop_hook_active: bool,
    pub(crate) last_assistant_message: Option<&'a str>,
    pub(crate) continuation: Option<String>,
}

impl StopContext<'_> {
    #[must_use]
    pub fn role(&self) -> &AgentRole {
        self.role
    }

    #[must_use]
    pub fn stop_hook_active(&self) -> bool {
        self.stop_hook_active
    }

    #[must_use]
    pub fn last_assistant_message(&self) -> Option<&str> {
        self.last_assistant_message
    }

    /// Returns the first continuation requested by the middleware chain.
    #[must_use]
    pub fn continuation(&self) -> Option<&str> {
        self.continuation.as_deref()
    }

    /// Requests one more model step with hidden context.
    pub fn continue_with(&mut self, prompt: impl Into<String>) -> Result<()> {
        if self.stop_hook_active {
            return Err(Error::Config(
                "a stop hook may continue a turn only once".into(),
            ));
        }
        let prompt = hook_message("stop continuation prompt", prompt)?;
        if self.continuation.is_none() {
            self.continuation = Some(prompt);
        }
        Ok(())
    }
}

fn hook_message(name: &str, value: impl Into<String>) -> Result<String> {
    let value = value.into();
    if value.trim().is_empty() || value.len() > MAX_CAPABILITY_INPUT_BYTES {
        return Err(Error::Config(format!("{name} is empty or too long")));
    }
    Ok(value)
}

fn set_stop_reason(
    target: &mut Option<String>,
    name: &str,
    reason: impl Into<String>,
) -> Result<()> {
    let reason = hook_message(name, reason)?;
    if target.is_none() {
        *target = Some(reason);
    }
    Ok(())
}

fn set_first(target: &mut Option<String>, value: Option<String>) {
    if target.is_none() {
        *target = value;
    }
}

pub(super) fn provisional_message_target(
    checkpoint_sequence: u64,
    batch_item_count: usize,
) -> Result<MessageTarget> {
    Ok(MessageTarget {
        checkpoint_sequence: checkpoint_sequence
            .checked_add(1)
            .ok_or_else(|| Error::Checkpoint("checkpoint sequence overflow".into()))?,
        batch_item_count,
    })
}

#[derive(Default)]
struct ByteCounter(usize);

impl Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Mutable state exposed to the middleware preparing conversation messages.
pub struct MessageRouteContext<'a> {
    pub submission_id: &'a str,
    pub message: &'a MessageSubmission,
    pub active_turn_id: Option<&'a str>,
    pub queued_messages: MessageQueue<'a>,
    pub events: &'a mut Vec<EventMsg>,
}

/// Mutable turn state exposed to a capability command that can run immediately.
pub struct ActiveCommandContext<'a> {
    pub submission_id: &'a str,
    pub session_id: &'a str,
    pub metadata: &'a BTreeMap<String, Value>,
    pub active_turn_id: &'a str,
    pub command: &'a str,
    pub arguments: &'a str,
    pub input: Option<&'a str>,
    pub target: Option<MessageTarget>,
    pub queued_messages: MessageQueue<'a>,
    pub events: &'a mut Vec<EventMsg>,
}

/// Result of one middleware-owned submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionResult {
    Accepted {
        input_changed: bool,
    },
    /// The operation completed without changing durable turn state; publish its events now.
    Handled,
    Rejected(String),
}

/// State exposed when the loop finishes or aborts a turn.
pub struct TurnEndContext<'a> {
    pub session_id: &'a str,
    pub turn_id: &'a str,
    pub(crate) outcome: ExecutionOutcome,
    pub(crate) queued_messages: &'a [DurableQueuedMessage],
    pub(crate) owner: Option<&'static str>,
    pub events: &'a mut Vec<EventMsg>,
}

impl TurnEndContext<'_> {
    #[must_use]
    pub fn outcome(&self) -> ExecutionOutcome {
        self.outcome
    }

    /// Returns queued messages still pending for this middleware, oldest first.
    pub fn queued_messages(&self) -> impl Iterator<Item = QueuedMessageView<'_>> {
        let owner = self.owner;
        self.queued_messages
            .iter()
            .filter(move |item| owner.is_some_and(|owner| item.owner() == owner))
            .map(|item| QueuedMessageView { item })
    }
}

/// State available to a middleware-owned frontend command.
pub struct MiddlewareCommandContext<'a> {
    pub command: &'a str,
    pub arguments: &'a str,
    pub input: Option<&'a str>,
    pub target: Option<MessageTarget>,
    pub session_id: &'a str,
    pub session_context: &'a SessionContext,
    pub checkpoint: &'a Checkpoint,
    pub checkpoints: Arc<dyn CheckpointStore>,
}
