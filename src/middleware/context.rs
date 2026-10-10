use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::Value;
use uuid::Uuid;

use super::MiddlewareStack;
use super::TokenEstimate;
use super::delivery_once::{DeliveryOnce, Receipts};
use super::tools::Catalog;
use super::tools::ToolResult;
use crate::agent::{AgentRole, WeakAgentSender};
use crate::backend::checkpoint::{
    Checkpoint, CheckpointStore, ContextRewriteReason, ExecutionOutcome, MAX_QUEUED_MESSAGES,
    QueuedMessage as DurableQueuedMessage, QueuedMessageBoundary,
};
use crate::backend::model::{ModelCancellation, ModelInput, ModelRouter, message_input};
use crate::backend::sandbox::ApprovalPolicy;
use crate::protocol::{
    EventMsg, FrontendBlock, FrontendBlockRole, FrontendBlockState, FrontendEvent, FrontendTone,
    MAX_CAPABILITY_INPUT_BYTES, MessageAuthor, MessageDelivery, MessageEvent, MessageReply,
    MessageSubmission, MessageTarget, ReviewDecision, SessionContext, SessionFileReference,
    TokenUsage, ToolCall, message_metadata,
};
use crate::{Error, Result};

/// Sends middleware-owned UI updates without depending on a concrete frontend.
pub type FrontendEventSink = Arc<dyn Fn(FrontendEvent) -> Result<()> + Send + Sync>;

// Retain live notices until the preparation transaction accepts or discards them.
pub(crate) struct PreparationNotice {
    capability: &'static str,
    id: Uuid,
    complete: (&'static str, FrontendTone),
    failed: (&'static str, FrontendTone),
    cancelled: (&'static str, FrontendTone),
}

impl PreparationNotice {
    fn start(
        frontend: &FrontendEventSink,
        capability: &'static str,
        pending: (&'static str, FrontendTone),
        complete: (&'static str, FrontendTone),
        failed: (&'static str, FrontendTone),
        cancelled: (&'static str, FrontendTone),
    ) -> Result<Self> {
        let notice = Self {
            capability,
            id: Uuid::new_v4(),
            complete,
            failed,
            cancelled,
        };
        (frontend)(notice.event(pending, FrontendBlockState::Pending))?;
        Ok(notice)
    }

    fn event(
        &self,
        (title, tone): (&str, FrontendTone),
        state: FrontendBlockState,
    ) -> FrontendEvent {
        FrontendEvent::Render {
            capability: self.capability.into(),
            block: FrontendBlock {
                id: Some(self.id.to_string()),
                state,
                role: FrontendBlockRole::Notice,
                title: title.into(),
                tone,
                ..Default::default()
            },
        }
    }
}

pub(crate) struct PreparationNotices<'a> {
    frontend: &'a FrontendEventSink,
    pub(crate) notices: Vec<PreparationNotice>,
    failed: bool,
}

impl<'a> PreparationNotices<'a> {
    pub(crate) fn new(frontend: &'a FrontendEventSink) -> Self {
        Self {
            frontend,
            notices: Vec::new(),
            failed: false,
        }
    }

    pub(crate) fn completions(&self, accepted: bool) -> impl Iterator<Item = EventMsg> + '_ {
        self.notices.iter().map(move |notice| {
            EventMsg::Frontend(notice.event(
                if accepted {
                    notice.complete
                } else {
                    notice.failed
                },
                FrontendBlockState::Complete,
            ))
        })
    }

    pub(crate) fn cancellations(&self) -> impl Iterator<Item = EventMsg> + '_ {
        self.notices.iter().map(|notice| {
            EventMsg::Frontend(notice.event(notice.cancelled, FrontendBlockState::Complete))
        })
    }

    pub(crate) fn fail(&mut self) {
        self.failed = true;
    }

    pub(crate) fn settle(&mut self) {
        self.notices.clear();
    }
}

impl Drop for PreparationNotices<'_> {
    fn drop(&mut self) {
        for notice in &self.notices {
            let fallback = if self.failed {
                notice.failed
            } else {
                notice.cancelled
            };
            if let Err(error) =
                (self.frontend)(notice.event(fallback, FrontendBlockState::Complete))
            {
                tracing::warn!(%error, "failed to close preparation notice");
            }
        }
    }
}

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

    /// Borrows the initiating author without materializing a presentation event.
    #[must_use]
    pub fn author(&self) -> &'a MessageAuthor {
        self.item.author()
    }

    /// Returns the queued message's delivery boundary.
    #[must_use]
    pub fn delivery(&self) -> MessageDelivery {
        self.item.boundary().delivery()
    }

    /// Borrows the queued message text.
    #[must_use]
    pub fn text(&self) -> &'a str {
        self.item.text()
    }

    /// Borrows the references retained until delivery or a validated replacement.
    #[must_use]
    pub fn attachments(&self) -> &'a [SessionFileReference] {
        self.item.attachments()
    }

    /// Borrows the reply snapshot retained with this queued message.
    #[must_use]
    pub fn reply(&self) -> Option<&'a MessageReply> {
        self.item.reply()
    }

    /// Returns the prepared presentation event.
    #[must_use]
    pub fn event(&self) -> MessageEvent {
        self.item.event()
    }
}

/// Read-only startup snapshot containing only one middleware's queued messages.
#[derive(Clone, Copy, Default)]
pub struct QueuedMessageSnapshot<'a> {
    items: &'a [DurableQueuedMessage],
    owner: &'a str,
}

impl<'a> QueuedMessageSnapshot<'a> {
    /// Returns every queued item owned by this middleware, oldest first.
    pub fn views(&self) -> impl Iterator<Item = QueuedMessageView<'_>> {
        self.items
            .iter()
            .filter(|item| item.owner() == self.owner)
            .map(|item| QueuedMessageView { item })
    }

    pub(super) fn for_owner(owner: &'a str, items: &'a [DurableQueuedMessage]) -> Self {
        Self { items, owner }
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
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
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
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
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
            // Keep the queued message durable until the prepared turn is successfully admitted.
            .cloned()
            .map(PreparedMessage::try_from)
            .transpose()
    }

    pub(crate) fn consume_next_turn(&mut self, id: &str) -> Result<(usize, DurableQueuedMessage)> {
        let owner = self.owner()?;
        let index = self
            .items
            .iter()
            .position(|item| {
                item.owner() == owner && item.id() == id && item.boundary().starts_turn()
            })
            .ok_or_else(|| Error::Checkpoint("prepared message is no longer queued".into()))?;
        Ok((index, self.items.remove(index)))
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
            item.promote_to_next_turn();
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
            MessageAuthor::User | MessageAuthor::Source { .. }
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
    /// The sender.
    pub sender: WeakAgentSender,
    /// The checkpoints.
    pub checkpoints: Arc<dyn CheckpointStore>,
    /// The session identifier.
    pub session_id: String,
    /// The model route.
    pub model_route: String,
    /// The model.
    pub model: String,
    /// The approval policy.
    pub approval_policy: ApprovalPolicy,
    /// The session context.
    pub session_context: SessionContext,
    /// The metadata.
    pub metadata: BTreeMap<String, Value>,
    /// The role.
    pub role: AgentRole,
    /// Creates child agents from the root agent's configuration.
    pub children: crate::agent::ChildAgents,
    /// The frontend.
    pub frontend: FrontendEventSink,
}

impl RuntimeContext {
    pub(crate) fn turn_identity<'a>(
        &'a self,
        turn_id: &'a str,
        author: &'a MessageAuthor,
    ) -> TurnIdentity<'a> {
        TurnIdentity {
            session_id: &self.session_id,
            turn_id,
            model: &self.model,
            approval_policy: self.approval_policy,
            author,
        }
    }
}

/// Stable facts shared by hooks that run within one active turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnIdentity<'a> {
    /// Trusted provenance of the message that initiated the turn.
    pub author: &'a MessageAuthor,
    /// The session identifier.
    pub session_id: &'a str,
    /// The turn identifier.
    pub turn_id: &'a str,
    /// The model.
    pub model: &'a str,
    /// The approval policy.
    pub approval_policy: ApprovalPolicy,
}

/// Why [`Middleware::session_start`](super::Middleware::session_start) is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStartSource {
    /// Selects the startup case.
    Startup,
    /// Selects the resume case.
    Resume,
    /// Selects the compact case.
    Compact,
}

/// Mutable state shared by the declaration-ordered `SessionStart` hooks.
pub struct SessionStartContext<'a> {
    /// The runtime.
    pub runtime: &'a RuntimeContext,
    pub(crate) delivery_once: DeliveryOnce<'a>,
    pub(crate) source: SessionStartSource,
    pub(crate) queued_messages: QueuedMessageSnapshot<'a>,
    pub(crate) input: &'a mut Vec<Arc<Value>>,
    pub(crate) input_changed: bool,
    pub(crate) stop_reason: Option<String>,
}

impl SessionStartContext<'_> {
    /// Appends guidance once per session and middleware, at this hook's normal position.
    /// The receipt becomes durable only when the appended input is accepted.
    /// # Errors
    /// Returns an error for an invalid key or a non-guidance context item.
    pub fn deliver_once(&mut self, key: &str, make: impl FnOnce() -> Value) -> Result<bool> {
        let appended = self.delivery_once.deliver(self.input, Some(key), make)?;
        self.input_changed |= appended;
        Ok(appended)
    }

    #[must_use]
    /// Returns the session-start source.
    pub fn source(&self) -> SessionStartSource {
        self.source
    }

    #[must_use]
    /// Returns the queued messages.
    pub fn queued_messages(&self) -> &QueuedMessageSnapshot<'_> {
        &self.queued_messages
    }

    /// Appends hidden provider context produced while the session starts.
    pub fn push_input(&mut self, item: Value) {
        self.input.push(Arc::new(item));
        self.input_changed = true;
    }

    /// Stops the active turn after session-start processing completes.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
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
    /// The turn.
    pub turn: TurnIdentity<'a>,
    /// The author.
    pub author: &'a MessageAuthor,
    /// The message.
    pub message: &'a str,
    /// The attachments.
    pub attachments: &'a [SessionFileReference],
    /// The events.
    pub events: &'a mut Vec<EventMsg>,
    pub(crate) delivery_once: DeliveryOnce<'a>,
    pub(crate) input: Vec<Value>,
    pub(crate) rejection: Option<String>,
}

impl MessageSubmitContext<'_> {
    /// Appends guidance once per session and middleware, before this submitted message.
    /// Rejected submissions do not consume the delivery receipt.
    /// # Errors
    /// Returns an error for an invalid key or a non-guidance context item.
    pub fn deliver_once(&mut self, key: &str, make: impl FnOnce() -> Value) -> Result<bool> {
        self.delivery_once.deliver(&mut self.input, Some(key), make)
    }

    /// Adds provider-neutral context immediately before the submitted message.
    pub fn push_input(&mut self, item: Value) {
        self.input.push(item);
    }

    /// Rejects the submission without treating the policy decision as a hook failure.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
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

/// Provisional history borrows its immutable prefix and owns only new or replaced items.
pub(crate) struct StagedInput {
    original: Arc<Vec<Arc<Value>>>,
    appended: Vec<Arc<Value>>,
    replacement: Option<Vec<Arc<Value>>>,
}

impl StagedInput {
    pub(crate) fn new(original: Arc<Vec<Arc<Value>>>) -> Self {
        Self {
            original,
            appended: Vec::new(),
            replacement: None,
        }
    }

    pub(crate) fn input(&self) -> ModelInput<'_> {
        ModelInput::shared_parts(
            self.replacement.as_deref().unwrap_or(&self.original),
            &self.appended,
        )
    }

    pub(crate) fn push(&mut self, item: Arc<Value>) {
        self.appended.push(item);
    }

    pub(crate) fn replace(&mut self, input: Vec<Arc<Value>>) {
        self.replacement = Some(input);
        self.appended.clear();
    }

    pub(crate) fn make_mut(&mut self) -> &mut Vec<Arc<Value>> {
        let input = self
            .replacement
            .get_or_insert_with(|| self.original.iter().map(Arc::clone).collect());
        input.append(&mut self.appended);
        input
    }

    pub(crate) fn commit(self, history: &mut Arc<Vec<Arc<Value>>>) {
        let Self {
            original,
            appended,
            replacement,
        } = self;
        drop(original);
        if let Some(mut input) = replacement {
            input.extend(appended);
            *history = Arc::new(input);
        } else if !appended.is_empty() {
            Arc::make_mut(history).extend(appended);
        }
    }
}

/// Mutable state exposed immediately before a model request.
pub struct ModelContext<'a> {
    /// Trusted provenance of the message that initiated the active turn.
    pub author: &'a MessageAuthor,
    /// The model.
    pub model: &'a ModelRouter,
    /// The provider.
    pub provider: &'a str,
    /// The route whose accepted output last contributed to durable context.
    pub context_provider: &'a str,
    /// The session identifier.
    pub session_id: &'a str,
    /// Cause recorded before model preparation is cancelled.
    pub cancellation: Option<&'a ModelCancellation>,
    /// The session context.
    pub session_context: &'a SessionContext,
    /// The metadata.
    pub metadata: &'a BTreeMap<String, Value>,
    /// The turn identifier.
    pub turn_id: &'a str,
    /// The model step.
    pub model_step: usize,
    /// The context window.
    pub context_window: i64,
    /// Shared byte-based estimate used by context policies.
    pub token_estimate: TokenEstimate,
    /// The instructions.
    pub instructions: &'a str,
    pub(crate) checkpoint_sequence: u64,
    pub(crate) available_tools: &'a mut BTreeSet<String>,
    pub(crate) allow_hosted_tools: &'a mut bool,
    pub(crate) durable_input: &'a mut StagedInput,
    pub(crate) delivered_once: &'a mut Receipts,
    pub(crate) transcript_delta: &'a mut Vec<Arc<Value>>,
    pub(crate) context_epoch: &'a mut u64,
    pub(crate) rewrite_reasons: &'a mut Vec<ContextRewriteReason>,
    pub(crate) turn_stop: &'a mut Option<String>,
    pub(crate) queued_messages: Vec<DurableQueuedMessage>,
    /// The last usage.
    pub last_usage: Option<&'a TokenUsage>,
    /// The tools.
    pub tools: &'a Catalog,
    /// The events.
    pub events: &'a mut Vec<EventMsg>,
    /// Completed preparation calls, with their actual routes and usage.
    pub usage: &'a mut Vec<(String, TokenUsage)>,
    /// Set when this hook changes durable checkpoint state.
    pub(crate) checkpoint_changed: &'a mut bool,
    pub(crate) runtime: &'a RuntimeContext,
    pub(crate) hooks: &'a MiddlewareStack,
    pub(crate) preparation_notices: &'a mut Vec<PreparationNotice>,
}

/// Live capability state used to hide registered tools at a model boundary.
pub struct ToolExposureContext<'a> {
    /// The session identifier.
    pub session_id: &'a str,
    pub(crate) supports_tool_image_input: bool,
    pub(crate) input: ModelInput<'a>,
    pub(crate) available: &'a mut BTreeSet<String>,
}

impl ToolExposureContext<'_> {
    /// Reports whether the active model accepts image input.
    #[must_use]
    pub fn supports_tool_image_input(&self) -> bool {
        self.supports_tool_image_input
    }

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
    /// Accounts for a preparation call on its actual model route.
    pub fn record_usage(&mut self, route: &str, usage: TokenUsage) {
        self.usage.push((route.into(), usage));
    }

    pub(crate) fn start_preparation_notice(
        &mut self,
        capability: &'static str,
        pending: (&'static str, FrontendTone),
        complete: (&'static str, FrontendTone),
        failed: (&'static str, FrontendTone),
        cancelled: (&'static str, FrontendTone),
    ) -> Result<()> {
        self.preparation_notices.push(PreparationNotice::start(
            &self.runtime.frontend,
            capability,
            pending,
            complete,
            failed,
            cancelled,
        )?);
        Ok(())
    }

    /// Prevents provider-hosted tools for this model step.
    pub fn disable_hosted_tools(&mut self) {
        *self.allow_hosted_tools = false;
    }

    /// Returns durable provider-neutral model context.
    #[must_use]
    pub fn input(&self) -> ModelInput<'_> {
        self.durable_input.input()
    }

    /// Replaces active model context and advances its rewrite epoch once per boundary.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn rewrite_input(
        &mut self,
        reason: ContextRewriteReason,
        input: impl IntoIterator<Item = impl Into<Arc<Value>>>,
    ) -> Result<()> {
        let mut input = input.into_iter().map(Into::into).collect::<Vec<_>>();
        if self.input().iter().eq(input.iter().map(AsRef::as_ref)) {
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
        crate::backend::model::reset_shared_prompt_cache_breakpoint(&mut input);
        self.durable_input.replace(input);
        self.last_usage = None;
        *self.checkpoint_changed = true;
        Ok(())
    }

    /// Appends a durable replay item without adding it to provider context.
    pub(crate) fn record_transcript_item(&mut self, item: Value) {
        self.transcript_delta.push(Arc::new(item));
        *self.checkpoint_changed = true;
    }

    /// Appends durable provider context without adding synthetic replay history.
    pub fn append_model_input(&mut self, item: Value) {
        self.durable_input.push(Arc::new(item));
        *self.checkpoint_changed = true;
    }

    /// Appends durable input to model context and its transcript journal.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn push_input(&mut self, item: Value) -> Result<MessageTarget> {
        let item = Arc::new(item);
        self.durable_input.push(Arc::clone(&item));
        self.transcript_delta.push(item);
        *self.checkpoint_changed = true;
        provisional_message_target(self.checkpoint_sequence, self.transcript_delta.len())
    }

    /// Estimates visible history, instructions, and tool schemas using the session policy.
    #[must_use]
    pub fn estimated_input_tokens(&self) -> i64 {
        let Ok(tools) = self
            .tools
            .prepare(self.input(), Cow::Borrowed(self.available_tools))
        else {
            return i64::MAX;
        };
        let Ok(tool_bytes) = tools.serialized_schema_bytes() else {
            return i64::MAX;
        };
        let same_context_model = self
            .model
            .same_context_model(self.context_provider, self.provider);
        let history = self
            .input()
            .iter()
            .filter(|item| {
                same_context_model || !crate::backend::model::is_provider_reasoning(item)
            })
            .map(|item| self.token_estimate.item_tokens(item))
            .fold(0usize, usize::saturating_add);
        i64::try_from(
            history.saturating_add(
                self.token_estimate
                    .tokens(tool_bytes.saturating_add(self.instructions.len())),
            ),
        )
        .unwrap_or(i64::MAX)
    }

    pub(crate) async fn pre_compact(&mut self) -> Result<()> {
        let hooks = self.hooks;
        let stop_reason = hooks
            .pre_compact(CompactContext {
                session_id: self.session_id,
                turn_id: self.turn_id,
                model: &self.runtime.model,
                input: self.durable_input.input(),
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
                input: self.durable_input.input(),
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
                self.durable_input.make_mut(),
                self.delivered_once,
            )
            .await?;
        set_first(self.turn_stop, start.stop_reason);
        Ok(())
    }

    #[must_use]
    pub(crate) fn turn_stopped(&self) -> bool {
        self.turn_stop.is_some()
    }
}

/// Request-only model input exposed after every durable `PreModel` hook.
pub struct ModelRequestContext<'a> {
    /// Trusted provenance of the message that initiated the active turn.
    pub author: &'a MessageAuthor,
    /// The role.
    pub role: &'a AgentRole,
    /// The model.
    pub model: &'a ModelRouter,
    /// The provider.
    pub provider: &'a str,
    /// The session identifier.
    pub session_id: &'a str,
    /// The turn identifier.
    pub turn_id: &'a str,
    /// The model step.
    pub model_step: usize,
    pub(crate) input: ModelInput<'a>,
    pub(crate) replacement: Option<Vec<Value>>,
}

impl ModelRequestContext<'_> {
    /// Returns the input currently prepared for this one model request.
    #[must_use]
    pub fn input(&self) -> ModelInput<'_> {
        self.replacement
            .as_deref()
            .map_or(self.input, ModelInput::from)
    }

    /// Replaces only the input sent by this model request.
    pub fn replace_input(&mut self, input: Vec<Value>) {
        self.replacement = Some(input);
    }
}

/// Mutable policy boundary for one normalized model-requested tool call.
pub struct PreToolUseContext<'a> {
    /// The turn.
    pub turn: TurnIdentity<'a>,
    /// The events.
    pub events: &'a mut Vec<EventMsg>,
    pub(crate) delivery_once: DeliveryOnce<'a>,
    pub(crate) tools: &'a Catalog,
    pub(crate) call: &'a mut ToolCall,
    pub(crate) changed: bool,
    pub(crate) input: Vec<Value>,
    pub(crate) denial: Option<String>,
}

impl PreToolUseContext<'_> {
    /// Appends guidance once per session and middleware, before this tool call.
    /// Interrupted preparation does not consume the delivery receipt.
    /// # Errors
    /// Returns an error for an invalid key or a non-guidance context item.
    pub fn deliver_once(&mut self, key: &str, make: impl FnOnce() -> Value) -> Result<bool> {
        self.delivery_once.deliver(&mut self.input, Some(key), make)
    }

    /// Returns the call after any earlier middleware rewrites.
    #[must_use]
    pub fn call(&self) -> &ToolCall {
        self.call
    }

    /// Returns whether the current call targets a tool that declares itself read-only.
    #[must_use]
    pub fn call_is_read_only(&self) -> bool {
        self.tools.is_read_only(&self.call.name)
    }

    /// Replaces the tool name and arguments while preserving the provider call ID.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn replace(&mut self, name: impl Into<String>, arguments: Value) -> Result<()> {
        let name = name.into();
        let changed = self.call.name != name || self.call.arguments != arguments;
        self.call.replace(name, arguments)?;
        self.changed |= changed;
        Ok(())
    }

    /// Adds durable provider-neutral context before this call at a tool-complete boundary.
    pub fn push_input(&mut self, item: Value) {
        self.input.push(item);
    }

    /// Denies the call. Later middleware may observe but cannot undo the denial.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
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
    /// The turn.
    pub turn: TurnIdentity<'a>,
    /// The calls.
    pub calls: &'a [ToolCall],
    /// The requested call identifiers.
    pub requested_call_ids: &'a [String],
    /// The reason.
    pub reason: &'a str,
    /// The events.
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
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
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
    /// The turn.
    pub turn: TurnIdentity<'a>,
    /// The call.
    pub call: &'a ToolCall,
    /// The events.
    pub events: &'a mut Vec<EventMsg>,
    pub(crate) delivery_once: DeliveryOnce<'a>,
    pub(crate) tools: &'a Catalog,
    pub(crate) result: &'a mut ToolResult,
}

impl PostToolUseContext<'_> {
    /// Appends guidance once per session and middleware, after this tool result.
    /// The receipt is saved atomically with the accepted result context.
    /// # Errors
    /// Returns an error for an invalid key or a non-guidance context item.
    pub fn deliver_once(&mut self, key: &str, make: impl FnOnce() -> Value) -> Result<bool> {
        self.delivery_once
            .deliver(&mut self.result.additional_input, Some(key), make)
    }

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
    /// The session identifier.
    pub session_id: &'a str,
    /// The turn identifier.
    pub turn_id: &'a str,
    /// The model.
    pub model: &'a str,
    /// The input.
    pub input: ModelInput<'a>,
    /// The events.
    pub events: &'a mut Vec<EventMsg>,
    pub(crate) stop_reason: Option<String>,
}

impl CompactContext<'_> {
    /// Stops the active turn at this compaction boundary.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
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
    /// The turn.
    pub turn: TurnIdentity<'a>,
    /// The events.
    pub events: &'a mut Vec<EventMsg>,
    pub(crate) role: &'a AgentRole,
    pub(crate) stop_hook_active: bool,
    pub(crate) last_assistant_message: Option<&'a str>,
    pub(crate) continuation: Option<String>,
}

impl StopContext<'_> {
    #[must_use]
    /// Returns the agent role.
    pub fn role(&self) -> &AgentRole {
        self.role
    }

    #[must_use]
    /// Stops hook active.
    pub fn stop_hook_active(&self) -> bool {
        self.stop_hook_active
    }

    #[must_use]
    /// Returns the last assistant message.
    pub fn last_assistant_message(&self) -> Option<&str> {
        self.last_assistant_message
    }

    /// Returns the first continuation requested by the middleware chain.
    #[must_use]
    pub fn continuation(&self) -> Option<&str> {
        self.continuation.as_deref()
    }

    /// Requests one more model step with hidden context.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
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

/// Mutable state exposed to the middleware preparing conversation messages.
pub struct MessageRouteContext<'a> {
    /// The checkpoints.
    pub checkpoints: &'a dyn CheckpointStore,
    /// The session identifier.
    pub session_id: &'a str,
    /// The submission identifier.
    pub submission_id: &'a str,
    /// The message; the preparer may move its contents into the queue.
    pub message: &'a mut MessageSubmission,
    /// The active turn identifier.
    pub active_turn_id: Option<&'a str>,
    /// The queued messages.
    pub queued_messages: MessageQueue<'a>,
    /// The events.
    pub events: &'a mut Vec<EventMsg>,
}

/// Mutable turn state exposed to a capability command that can run immediately.
pub struct ActiveCommandContext<'a> {
    /// The checkpoints.
    pub checkpoints: &'a dyn CheckpointStore,
    /// The submission identifier.
    pub submission_id: &'a str,
    /// The session identifier.
    pub session_id: &'a str,
    /// The metadata.
    pub metadata: &'a BTreeMap<String, Value>,
    /// The active turn identifier.
    pub active_turn_id: &'a str,
    /// The command.
    pub command: &'a str,
    /// The arguments.
    pub arguments: &'a str,
    /// The input.
    pub input: Option<&'a str>,
    /// The target.
    pub target: Option<MessageTarget>,
    /// The queued messages.
    pub queued_messages: MessageQueue<'a>,
    /// The events.
    pub events: &'a mut Vec<EventMsg>,
}

/// Result of one middleware-owned submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionResult {
    /// Selects the accepted case.
    Accepted {
        /// The input changed.
        input_changed: bool,
    },
    /// The operation completed without changing durable turn state; publish its events now.
    Handled,
    /// Selects the rejected case.
    Rejected(String),
}

/// State exposed when the loop finishes or aborts a turn.
pub struct TurnEndContext<'a> {
    /// The session identifier.
    pub session_id: &'a str,
    /// The turn identifier.
    pub turn_id: &'a str,
    pub(crate) outcome: ExecutionOutcome,
    pub(crate) queued_messages: &'a [DurableQueuedMessage],
    pub(crate) owner: Option<&'static str>,
    /// The events.
    pub events: &'a mut Vec<EventMsg>,
}

impl TurnEndContext<'_> {
    #[must_use]
    /// Returns the turn outcome.
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
    /// The command.
    pub command: &'a str,
    /// The arguments.
    pub arguments: &'a str,
    /// The input.
    pub input: Option<&'a str>,
    /// The target.
    pub target: Option<MessageTarget>,
    /// The session identifier.
    pub session_id: &'a str,
    /// The session context.
    pub session_context: &'a SessionContext,
    /// The checkpoint.
    pub checkpoint: &'a Checkpoint,
    /// The checkpoints.
    pub checkpoints: Arc<dyn CheckpointStore>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::model::{Model, ModelEventSink, ModelOutput, ModelRequest};

    struct NoModel;

    impl Model for NoModel {
        fn respond<'a>(
            &'a self,
            _request: ModelRequest<'a>,
            _events: ModelEventSink,
        ) -> crate::BoxFuture<'a, crate::Result<ModelOutput>> {
            Box::pin(async { Err(crate::Error::Provider("unused".into())) })
        }
    }

    #[test]
    fn request_input_is_borrowed_until_replaced() {
        let original = vec![Value::String("original".into())];
        let role = AgentRole::Main;
        let router = ModelRouter::new("test", Arc::new(NoModel));
        let mut context = ModelRequestContext {
            author: &MessageAuthor::User,
            role: &role,
            model: &router,
            provider: "test",
            session_id: "session",
            turn_id: "turn",
            model_step: 0,
            input: original.as_slice().into(),
            replacement: None,
        };

        assert!(std::ptr::eq(context.input().get(0).unwrap(), &original[0]));
        context.replace_input(vec![Value::String("replacement".into())]);
        assert!(context.replacement.is_some());
        assert_eq!(original, [Value::String("original".into())]);
    }

    #[test]
    fn staged_history_shares_original_items_and_commits_only_when_accepted() {
        let original = Arc::new(Value::String("original".into()));
        let mut history = Arc::new(vec![Arc::clone(&original)]);
        let mut rejected = StagedInput::new(Arc::clone(&history));
        rejected.push(Arc::new(Value::String("rejected".into())));
        assert_eq!(rejected.input().len(), 2);
        assert!(Arc::ptr_eq(
            rejected.input().shared_item(0).unwrap(),
            &original
        ));
        drop(rejected);
        assert_eq!(history.len(), 1);

        let appended = Arc::new(Value::String("accepted".into()));
        let mut accepted = StagedInput::new(Arc::clone(&history));
        accepted.push(Arc::clone(&appended));
        accepted.commit(&mut history);
        assert!(Arc::ptr_eq(&history[0], &original));
        assert!(Arc::ptr_eq(&history[1], &appended));

        let mut rewritten = StagedInput::new(Arc::clone(&history));
        rewritten.replace(vec![Arc::clone(&appended)]);
        rewritten.commit(&mut history);
        assert_eq!(history.len(), 1);
        assert!(Arc::ptr_eq(&history[0], &appended));
    }
}
