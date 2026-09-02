//! Ordered middleware and capability registration.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::Value;

use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::backend::checkpoint::{ExecutionOutcome, QueuedMessage as DurableQueuedMessage};
use crate::backend::sandbox::Sandbox;
use crate::protocol::EventMsg;
use crate::protocol::FrontendActionListItem;
use crate::protocol::FrontendBlock;
use crate::protocol::FrontendBlockRole;
use crate::protocol::FrontendBlockState;
use crate::protocol::FrontendBlockUpdate;
use crate::protocol::FrontendContribution;
use crate::protocol::FrontendEvent;
use crate::protocol::FrontendSlot;
use crate::protocol::FrontendTone;
use crate::protocol::FrontendWidgetContent;
use crate::protocol::RenderedBlock;
use crate::protocol::ToolCallBeginEvent;
use crate::protocol::ToolCallEndEvent;

pub mod artifacts;
pub mod attachments;
pub mod bots;
pub mod compaction;
mod context;
pub mod context_offloading;
pub mod extensions;
pub mod instructions;
pub mod manifest;
pub mod messages;
pub mod scratchpad;
pub mod session_files;
pub mod sessions;
pub mod subagents;
pub mod tasks;
pub mod tools;

pub use context::{
    ActiveCommandContext, CompactContext, FrontendEventSink, MessageQueue, MessageRouteContext,
    MessageSubmitContext, MiddlewareCommandContext, ModelContext, ModelRequestContext,
    PermissionRequestContext, PostToolUseContext, PreToolUseContext, QueuedMessageSnapshot,
    QueuedMessageView, RuntimeContext, SessionStartContext, SessionStartSource, StopContext,
    SubmissionResult, ToolExposureContext, TurnEndContext, TurnIdentity,
};
pub(crate) use context::{MessageSubmitResult, PreparedMessage};

use tools::Catalog;

const ESTIMATED_BYTES_PER_TOKEN: usize = 4;

/// Result of a middleware-owned frontend command.
pub struct MiddlewareCommandOutput {
    pub events: Vec<FrontendEvent>,
}

/// Read-only middleware UI surface consumed by a frontend shell.
#[derive(Clone)]
pub struct FrontendExtensions {
    stack: MiddlewareStack,
    session_id: Arc<str>,
    contributions: Arc<[FrontendContribution]>,
}

impl FrontendExtensions {
    pub(crate) fn new(stack: MiddlewareStack, session_id: impl Into<Arc<str>>) -> Result<Self> {
        let contributions = stack.frontend()?;
        Ok(Self {
            stack,
            session_id: session_id.into(),
            contributions: contributions.into(),
        })
    }

    /// Returns command and widget manifests in capability order.
    #[must_use]
    pub fn contributions(&self) -> &[FrontendContribution] {
        &self.contributions
    }

    /// Lets installed middleware render capability-specific events.
    #[must_use]
    pub fn render(&self, event: &EventMsg) -> Vec<RenderedBlock> {
        event
            .presentation()
            .into_iter()
            .chain(self.stack.entries.iter().filter_map(|entry| {
                entry
                    .render(event, &self.session_id)
                    .map(|block| RenderedBlock {
                        capability: entry.name().into(),
                        block,
                    })
            }))
            .collect()
    }
}

impl MiddlewareCommandOutput {
    /// Returns UI updates without replacing the active session.
    #[must_use]
    pub fn events(events: Vec<FrontendEvent>) -> Self {
        Self { events }
    }

    /// Returns one capability-scoped transcript block.
    #[must_use]
    pub fn render(
        capability: impl Into<String>,
        text: impl Into<String>,
        tone: FrontendTone,
    ) -> Self {
        let title = text.into();
        Self::events(vec![FrontendEvent::Render {
            capability: capability.into(),
            block: FrontendBlock {
                id: None,
                group: None,
                update: FrontendBlockUpdate::Replace,
                state: FrontendBlockState::Complete,
                role: FrontendBlockRole::Notice,
                title,
                text: String::new(),
                symbol: None,
                files: Vec::new(),
                format: crate::protocol::FrontendBlockFormat::PlainText,
                tone,
            },
        }])
    }
}

/// A capability contribution to the single ordered agent pipeline.
pub trait Middleware: Send + Sync {
    /// Stable ID used to reject duplicate registrations.
    fn name(&self) -> &'static str;

    /// Adds tools to the catalog while the agent is created.
    fn register(&self, _catalog: &mut Catalog, _runtime: &RuntimeContext) -> Result<()> {
        Ok(())
    }

    /// Contributes one immutable system-prompt section while the agent is created.
    fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(None)
    }

    /// Declares commands and status data that any frontend may render.
    fn frontend(&self) -> FrontendContribution {
        FrontendContribution::default()
    }

    /// Renders an event owned by this capability for the destination session.
    ///
    /// Session-bound handles must only be exposed when they belong to `session_id`.
    fn render(&self, _event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        None
    }

    /// Handles a command declared by this middleware's frontend contribution.
    fn command<'a>(
        &'a self,
        context: MiddlewareCommandContext<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareCommandOutput>> {
        Box::pin(async move {
            Err(Error::Unknown(format!(
                "middleware command `{}/{}`",
                self.name(),
                context.command
            )))
        })
    }

    /// Starts or re-enters a session lifecycle.
    fn session_start<'a>(
        &'a self,
        _context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Reports whether this middleware is the session's conversation-message preparer.
    fn handles_messages(&self) -> bool {
        false
    }

    /// Validates and prepares one conversation message for its lifecycle boundary.
    fn route_message(&self, _context: &mut MessageRouteContext<'_>) -> Result<SubmissionResult> {
        Err(Error::Config(format!(
            "middleware `{}` claimed messages but did not prepare them",
            self.name()
        )))
    }

    /// Produces capability UI cleanup when one queued message reaches its boundary.
    fn message_boundary_events(&self, _submission_id: &str) -> Vec<EventMsg> {
        Vec::new()
    }

    /// Intercepts a prepared conversation message at its delivery boundary.
    fn message_submit<'a>(
        &'a self,
        _context: &'a mut MessageSubmitContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Handles a capability command while a turn is active.
    ///
    /// The active model, tool, or hook future is not polled until this returns. Implementations
    /// must keep work bounded and must not await a resource held by that active future. Return
    /// `None` when the command is unavailable during the active turn.
    fn active_command<'a>(
        &'a self,
        _context: &'a mut ActiveCommandContext<'_>,
    ) -> BoxFuture<'a, Result<Option<SubmissionResult>>> {
        Box::pin(async { Ok(None) })
    }

    /// Mutates durable context before the next model request is assembled.
    fn pre_model<'a>(&'a self, _context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Applies request-only context after every durable transform has completed.
    fn model_request<'a>(
        &'a self,
        _context: &'a mut ModelRequestContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Hides registered tools whose capability is unavailable at this boundary.
    fn tool_exposure<'a>(
        &'a self,
        _context: &'a mut ToolExposureContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Intercepts one normalized tool call before authorization and persistence.
    fn pre_tool_use<'a>(
        &'a self,
        _context: &'a mut PreToolUseContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Decides whether an approval request should be deferred, allowed, or denied.
    fn permission_request<'a>(
        &'a self,
        _context: &'a mut PermissionRequestContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Transforms model-visible feedback after a tool has executed.
    fn post_tool_use<'a>(
        &'a self,
        _context: &'a mut PostToolUseContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Intercepts context immediately before compaction and may stop the active turn.
    fn pre_compact<'a>(
        &'a self,
        _context: &'a mut CompactContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Intercepts the committed compacted context and may stop the active turn.
    fn post_compact<'a>(
        &'a self,
        _context: &'a mut CompactContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Decides whether a naturally stopped model should continue once more.
    fn stop<'a>(&'a self, _context: &'a mut StopContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Observes terminal bookkeeping for a completed or aborted turn.
    fn turn_end<'a>(&'a self, _context: &'a mut TurnEndContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Releases session-local state when the agent runtime stops.
    fn session_end<'a>(&'a self, _runtime: &'a RuntimeContext) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// One middleware-owned section of the assembled system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSection {
    title: Option<&'static str>,
    body: String,
}

impl PromptSection {
    /// Uses the contributing middleware's stable name as the Markdown heading.
    #[must_use]
    pub fn new(body: impl Into<String>) -> Self {
        Self {
            title: None,
            body: body.into(),
        }
    }

    /// Uses an explicit Markdown heading when the middleware name is ambiguous.
    #[must_use]
    pub fn titled(title: &'static str, body: impl Into<String>) -> Self {
        Self {
            title: Some(title),
            body: body.into(),
        }
    }
}

/// A validated, declaration-ordered middleware pipeline.
#[derive(Clone)]
pub struct MiddlewareStack {
    entries: Vec<Arc<dyn Middleware>>,
}

#[derive(Debug)]
pub(crate) struct SessionStartResult {
    pub(crate) stop_reason: Option<String>,
    pub(crate) input_changed: bool,
}

impl MiddlewareStack {
    /// Creates a stack and rejects duplicate middleware IDs.
    pub fn new(entries: Vec<Arc<dyn Middleware>>) -> Result<Self> {
        let mut names = BTreeSet::new();
        let mut message_handler = None;
        for entry in &entries {
            if !names.insert(entry.name()) {
                return Err(Error::Duplicate(format!("middleware `{}`", entry.name())));
            }
            if entry.handles_messages()
                && let Some(owner) = message_handler.replace(entry.name())
            {
                return Err(Error::Config(format!(
                    "conversation messages are owned by both `{owner}` and `{}`",
                    entry.name()
                )));
            }
        }
        Ok(Self { entries })
    }

    pub(crate) fn with_sandbox(&self, sandbox: Arc<Sandbox>) -> Result<Self> {
        let mut entries: Vec<Arc<dyn Middleware>> = vec![sandbox];
        entries.extend(self.entries.iter().cloned());
        Self::new(entries)
    }

    /// Builds the immutable tool catalog once.
    pub fn catalog(&self, runtime: &RuntimeContext) -> Result<Catalog> {
        let mut catalog = Catalog::default();
        for entry in &self.entries {
            let registered = catalog
                .registered_definitions()
                .iter()
                .map(|definition| definition.name.clone())
                .collect::<BTreeSet<_>>();
            entry.register(&mut catalog, runtime)?;
            for definition in catalog
                .registered_definitions()
                .iter()
                .filter(|definition| !registered.contains(&definition.name))
            {
                validate_tool_rendering(entry.as_ref(), &definition.name, &runtime.session_id)?;
            }
        }
        catalog.finalize()?;
        if catalog
            .registered_definitions()
            .iter()
            .any(|definition| definition.name == crate::backend::model::TOOLS_SEARCH_NAME)
        {
            let tools = self
                .entries
                .iter()
                .find(|entry| entry.name() == tools::MANIFEST.id)
                .ok_or_else(|| Error::Config("tools_search has no owning middleware".into()))?;
            validate_tool_rendering(
                tools.as_ref(),
                crate::backend::model::TOOLS_SEARCH_NAME,
                &runtime.session_id,
            )?;
        }
        Ok(catalog)
    }

    pub(crate) fn system_prompt(&self, base: &str, runtime: &RuntimeContext) -> Result<String> {
        let mut prompt = format!("**instructions**\n\n{}", base.trim());
        for entry in &self.entries {
            let Some(section) = entry.prompt_section(runtime)? else {
                continue;
            };
            let body = section.body.trim();
            if body.is_empty() {
                return Err(Error::Config(format!(
                    "middleware `{}` returned an empty prompt section",
                    entry.name()
                )));
            }
            let title = section.title.unwrap_or_else(|| entry.name()).trim();
            if title.is_empty() || title.lines().count() != 1 {
                return Err(Error::Config(format!(
                    "middleware `{}` returned an invalid prompt section title",
                    entry.name()
                )));
            }
            prompt.push_str("\n\n**");
            prompt.push_str(title);
            prompt.push_str("**\n\n");
            prompt.push_str(body);
        }
        Ok(prompt)
    }

    /// Builds and validates the frontend-neutral capability catalog.
    pub fn frontend(&self) -> Result<Vec<FrontendContribution>> {
        let contributions = self.declared_frontend()?;
        validate_frontend(&contributions)?;
        Ok(contributions)
    }

    fn declared_frontend(&self) -> Result<Vec<FrontendContribution>> {
        let mut contributions = Vec::new();
        for entry in &self.entries {
            let contribution = entry.frontend();
            if contribution.capability.is_empty()
                && contribution.commands.is_empty()
                && contribution.widgets.is_empty()
                && contribution.references.is_empty()
            {
                continue;
            }
            if contribution.capability != entry.name() {
                return Err(Error::Config(format!(
                    "middleware `{}` exported frontend metadata for `{}`",
                    entry.name(),
                    contribution.capability
                )));
            }
            contributions.push(contribution);
        }
        Ok(contributions)
    }

    fn message_handler(&self) -> Option<&Arc<dyn Middleware>> {
        self.entries.iter().find(|entry| entry.handles_messages())
    }

    fn message_handler_required(&self) -> Result<&Arc<dyn Middleware>> {
        self.message_handler()
            .ok_or_else(|| Error::Config("conversation message middleware is not installed".into()))
    }

    pub(crate) fn require_message_handler(&self) -> Result<()> {
        self.message_handler_required().map(|_| ())
    }

    pub(crate) fn validate_message_queue(&self, queued: &[DurableQueuedMessage]) -> Result<()> {
        let owner = self.message_handler_required()?.name();
        if queued.iter().all(|item| item.owner() == owner) {
            Ok(())
        } else {
            Err(Error::Checkpoint(
                "queued message is owned by unavailable middleware".into(),
            ))
        }
    }

    pub(crate) fn route_message(
        &self,
        context: &mut MessageRouteContext<'_>,
    ) -> Result<SubmissionResult> {
        let entry = self.message_handler_required()?;
        context.queued_messages.scope(entry.name());
        entry.route_message(context)
    }

    pub(crate) async fn active_command(
        &self,
        middleware: &str,
        context: &mut ActiveCommandContext<'_>,
    ) -> Result<Option<SubmissionResult>> {
        let Some(entry) = self.entries.iter().find(|entry| entry.name() == middleware) else {
            return Ok(None);
        };
        if entry.handles_messages() {
            context.queued_messages.scope(entry.name());
        }
        entry.active_command(context).await
    }

    pub(crate) fn stage_model_messages(
        &self,
        queued_messages: &mut Vec<DurableQueuedMessage>,
        turn_id: &str,
    ) -> Result<Vec<PreparedMessage>> {
        let entry = self.message_handler_required()?;
        let mut queue = MessageQueue::new(queued_messages);
        queue.scope(entry.name());
        let mut messages = queue.stage_model_messages(turn_id)?;
        for message in &mut messages {
            message.boundary_events = entry.message_boundary_events(&message.submission_id);
        }
        Ok(messages)
    }

    pub(crate) fn messages_ready(
        &self,
        queued_messages: &[DurableQueuedMessage],
        turn_id: &str,
    ) -> Result<bool> {
        let entry = self.message_handler_required()?;
        Ok(queued_messages.iter().any(|item| {
            item.owner() == entry.name()
                && matches!(
                    item.boundary(),
                    crate::backend::checkpoint::QueuedMessageBoundary::Steer {
                        turn_id: target
                    } if target == turn_id
                )
        }))
    }

    pub(crate) fn next_turn(
        &self,
        queued_messages: &mut Vec<DurableQueuedMessage>,
    ) -> Result<Option<PreparedMessage>> {
        let entry = self.message_handler_required()?;
        let mut queue = MessageQueue::new(queued_messages);
        queue.scope(entry.name());
        Ok(queue.next_turn()?.map(|mut message| {
            message.boundary_events = entry.message_boundary_events(&message.submission_id);
            message
        }))
    }

    pub(crate) fn consume_next_turn(
        &self,
        queued_messages: &mut Vec<DurableQueuedMessage>,
        submission_id: &str,
    ) -> Result<()> {
        let entry = self.message_handler_required()?;
        let mut queue = MessageQueue::new(queued_messages);
        queue.scope(entry.name());
        queue.consume_next_turn(submission_id)
    }

    pub(crate) async fn message_submit(
        &self,
        turn: TurnIdentity<'_>,
        message: &PreparedMessage,
        events: &mut Vec<EventMsg>,
    ) -> Result<MessageSubmitResult> {
        let event = message
            .event
            .message()
            .ok_or_else(|| Error::Checkpoint("prepared event is not a message".into()))?;
        let mut context = MessageSubmitContext {
            turn,
            author: &event.author,
            message: &event.text,
            attachments: &event.attachments,
            events,
            input: Vec::new(),
            rejection: None,
        };
        for entry in &self.entries {
            entry.message_submit(&mut context).await?;
        }
        Ok(MessageSubmitResult {
            input: context.input,
            rejection: context.rejection,
        })
    }

    pub(crate) fn finish_message_turn(
        &self,
        queued_messages: &mut Vec<DurableQueuedMessage>,
        turn_id: &str,
        outcome: ExecutionOutcome,
    ) -> Result<()> {
        let entry = self.message_handler_required()?;
        if outcome == ExecutionOutcome::Completed {
            return Ok(());
        }
        let mut queue = MessageQueue::new(queued_messages);
        queue.scope(entry.name());
        queue.promote_failed_turn(turn_id)
    }

    pub(crate) async fn session_start(
        &self,
        runtime: &RuntimeContext,
        queued_messages: &[DurableQueuedMessage],
        source: SessionStartSource,
        input: &mut Vec<Value>,
    ) -> Result<SessionStartResult> {
        let compact_input = (source == SessionStartSource::Compact).then(|| input.clone());
        let mut context = SessionStartContext {
            runtime,
            source,
            queued_messages: QueuedMessageSnapshot::default(),
            input,
            input_changed: false,
            stop_reason: None,
        };
        for (index, entry) in self.entries.iter().enumerate() {
            context.queued_messages =
                QueuedMessageSnapshot::for_owner(entry.name(), queued_messages);
            if let Err(error) = entry.session_start(&mut context).await {
                if let Some(input) = compact_input {
                    *context.input = input;
                    return Err(error);
                }
                let mut rollback_error = None;
                for started in self.entries[..index].iter().rev() {
                    if let Err(error) = started.session_end(runtime).await
                        && rollback_error.is_none()
                    {
                        rollback_error = Some(error);
                    }
                }
                return Err(match rollback_error {
                    Some(rollback) => Error::Rollback {
                        primary: Box::new(error),
                        rollback: Box::new(rollback),
                    },
                    None => error,
                });
            }
        }
        Ok(SessionStartResult {
            stop_reason: context.stop_reason,
            input_changed: context.input_changed,
        })
    }

    pub(crate) async fn prepare_model(&self, mut context: ModelContext<'_>) -> Result<()> {
        self.resolve_tool_exposure(
            context.session_id,
            context.durable_input,
            context.available_tools,
        )
        .await?;
        for entry in &self.entries {
            entry.pre_model(&mut context).await?;
        }
        if context.turn_stopped() {
            return Ok(());
        }
        for entry in &self.entries {
            let mut request = ModelRequestContext {
                model: context.model,
                provider: context.provider,
                session_id: context.session_id,
                turn_id: context.turn_id,
                model_step: context.model_step,
                input: context.request_input,
            };
            entry.model_request(&mut request).await?;
        }
        Ok(())
    }

    pub(crate) async fn resolve_tool_exposure(
        &self,
        session_id: &str,
        input: &[Value],
        available: &mut BTreeSet<String>,
    ) -> Result<()> {
        for entry in &self.entries {
            entry
                .tool_exposure(&mut ToolExposureContext {
                    session_id,
                    input,
                    available,
                })
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn pre_tool_use(&self, context: &mut PreToolUseContext<'_>) -> Result<()> {
        for entry in &self.entries {
            entry.pre_tool_use(context).await?;
        }
        Ok(())
    }

    pub(crate) async fn permission_request(
        &self,
        context: &mut PermissionRequestContext<'_>,
    ) -> Result<()> {
        for entry in &self.entries {
            entry.permission_request(context).await?;
        }
        Ok(())
    }

    pub(crate) async fn post_tool_use(&self, context: &mut PostToolUseContext<'_>) -> Result<()> {
        for entry in &self.entries {
            entry.post_tool_use(context).await?;
        }
        Ok(())
    }

    pub(crate) async fn pre_compact(
        &self,
        mut context: CompactContext<'_>,
    ) -> Result<Option<String>> {
        for entry in &self.entries {
            entry.pre_compact(&mut context).await?;
        }
        Ok(context.stop_reason)
    }

    pub(crate) async fn post_compact(
        &self,
        mut context: CompactContext<'_>,
    ) -> Result<Option<String>> {
        for entry in &self.entries {
            entry.post_compact(&mut context).await?;
        }
        Ok(context.stop_reason)
    }

    pub(crate) async fn stop(&self, context: &mut StopContext<'_>) -> Result<()> {
        for entry in &self.entries {
            entry.stop(context).await?;
        }
        Ok(())
    }

    pub(crate) async fn turn_end(&self, mut context: TurnEndContext<'_>) -> Result<()> {
        for entry in &self.entries {
            context.owner = Some(entry.name());
            entry.turn_end(&mut context).await?;
        }
        Ok(())
    }

    pub(crate) async fn session_end(&self, runtime: &RuntimeContext) -> Result<()> {
        let mut first_error = None;
        for entry in self.entries.iter().rev() {
            if let Err(error) = entry.session_end(runtime).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub(crate) async fn command(
        &self,
        middleware: &str,
        context: MiddlewareCommandContext<'_>,
    ) -> Result<MiddlewareCommandOutput> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.name() == middleware)
            .ok_or_else(|| Error::Unknown(format!("middleware `{middleware}`")))?;
        let declared = entry
            .frontend()
            .commands
            .into_iter()
            .any(|command| command.name == context.command);
        if !declared {
            return Err(Error::Unknown(format!(
                "middleware command `{middleware}/{}`",
                context.command
            )));
        }
        entry.command(context).await
    }
}

fn validate_tool_rendering(
    middleware: &dyn Middleware,
    tool_name: &str,
    session_id: &str,
) -> Result<()> {
    let events = [
        (
            "ToolCallBegin",
            EventMsg::ToolCallBegin(ToolCallBeginEvent {
                turn_id: "validation".into(),
                call_id: "validation".into(),
                name: tool_name.into(),
                arguments: serde_json::json!({}),
            }),
        ),
        (
            "successful ToolCallEnd",
            EventMsg::ToolCallEnd(ToolCallEndEvent {
                turn_id: "validation".into(),
                call_id: "validation".into(),
                name: tool_name.into(),
                output: String::new(),
                is_error: false,
            }),
        ),
        (
            "error ToolCallEnd",
            EventMsg::ToolCallEnd(ToolCallEndEvent {
                turn_id: "validation".into(),
                call_id: "validation".into(),
                name: tool_name.into(),
                output: "validation error".into(),
                is_error: true,
            }),
        ),
    ];
    for (event_name, event) in events {
        if middleware.render(&event, session_id).is_none() {
            return Err(Error::Config(format!(
                "middleware `{}` registered tool `{tool_name}` but does not render `{event_name}`",
                middleware.name()
            )));
        }
    }
    Ok(())
}

fn validate_frontend(contributions: &[FrontendContribution]) -> Result<()> {
    let mut commands = BTreeSet::new();
    let mut widgets = BTreeSet::new();
    let mut references = BTreeSet::new();
    for contribution in contributions {
        for command in &contribution.commands {
            if command.name.is_empty() || command.name.chars().any(char::is_whitespace) {
                return Err(Error::Config(format!(
                    "invalid frontend command `{}`",
                    command.name
                )));
            }
            if !commands.insert(command.name.clone()) {
                return Err(Error::Duplicate(format!(
                    "frontend command `{}`",
                    command.name
                )));
            }
        }
        for item in &contribution.widgets {
            if item.id.is_empty()
                || !widgets.insert((contribution.capability.clone(), item.id.clone()))
            {
                return Err(Error::Duplicate(format!(
                    "frontend status `{}/{}`",
                    contribution.capability, item.id
                )));
            }
            if matches!(item.slot, FrontendSlot::Navigation | FrontendSlot::ChatMenu)
                && (item.text.trim().is_empty()
                    || (item.content.is_none() && item.action.is_none()))
            {
                return Err(Error::Config(format!(
                    "frontend surface `{}/{}` requires a label and content or action",
                    contribution.capability, item.id
                )));
            }
            if let Some(FrontendWidgetContent::ActionList { title, items }) = &item.content {
                validate_action_list(title, items)?;
            }
        }
        for reference in &contribution.references {
            if reference.trigger.is_control()
                || reference.trigger.is_whitespace()
                || reference.value.is_empty()
                || reference.value.chars().any(char::is_whitespace)
            {
                return Err(Error::Config(format!(
                    "invalid frontend reference `{}{}`",
                    reference.trigger, reference.value
                )));
            }
            if !references.insert((reference.trigger, reference.value.clone())) {
                return Err(Error::Duplicate(format!(
                    "frontend reference `{}{}`",
                    reference.trigger, reference.value
                )));
            }
        }
    }
    Ok(())
}

fn validate_action_list(title: &str, items: &[FrontendActionListItem]) -> Result<()> {
    if title.trim().is_empty() {
        return Err(Error::Config("frontend action list title is empty".into()));
    }
    let mut item_ids = BTreeSet::new();
    for item in items {
        if item.id.trim().is_empty() || item.text.trim().is_empty() {
            return Err(Error::Config(
                "frontend action list item requires an ID and text".into(),
            ));
        }
        if !item_ids.insert(&item.id) {
            return Err(Error::Duplicate(format!(
                "frontend action list item `{}`",
                item.id
            )));
        }
        let mut action_ids = BTreeSet::new();
        for action in &item.actions {
            if action.id.trim().is_empty()
                || action.label.trim().is_empty()
                || action.symbol.as_str().trim().is_empty()
            {
                return Err(Error::Config(
                    "frontend list action requires an ID, label, and symbol".into(),
                ));
            }
            if !action_ids.insert(&action.id) {
                return Err(Error::Duplicate(format!(
                    "frontend list action `{}`",
                    action.id
                )));
            }
        }
    }
    Ok(())
}

pub(crate) const fn approximate_tokens(bytes: usize) -> usize {
    bytes / ESTIMATED_BYTES_PER_TOKEN
}

pub(crate) fn approximate_item_tokens(item: &Value) -> usize {
    serde_json::to_vec(item)
        .map_or(0, |bytes| approximate_tokens(bytes.len()))
        .max(1)
}

#[cfg(test)]
mod tests;
