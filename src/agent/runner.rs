//! The single linear command dispatch loop.

use crate::SessionId;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value;

use super::AgentConfig;
use super::EventRecorder;
use super::ReceivedSubmission;
use super::SubmissionInbox;
use crate::Error;
use crate::Result;
use crate::backend::checkpoint::CHECKPOINT_VERSION;
use crate::backend::checkpoint::Checkpoint;
use crate::backend::checkpoint::ExecutionOutcome;
use crate::backend::checkpoint::ExecutionRecord;
use crate::middleware::MiddlewareCommandContext;
use crate::middleware::RuntimeContext;
use crate::middleware::tools::Catalog;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::Op;
use crate::protocol::SessionResumeRequestedEvent;
use crate::protocol::TokenUsage;
use crate::protocol::WarningEvent;

/// Journal reason for an explicit turn interruption.
pub const TURN_INTERRUPTED_REASON: &str = "interrupted";
/// Journal reason for an unfinished turn recovered after a runtime restart.
pub const TURN_RESTARTED_REASON: &str = "interrupted by restart";
/// Journal reason for a closed frontend submission channel.
pub const FRONTEND_DISCONNECTED_REASON: &str = "frontend disconnected";

pub(super) struct Runner {
    pub(super) config: AgentConfig,
    pub(super) runtime: Arc<RuntimeContext>,
    pub(super) system_prompt: Arc<str>,
    /// The system prompt's parts, estimated once when it is assembled.
    pub(super) prompt_estimate: super::context_estimate::ContextEstimate,
    /// The last model request's parts, scaled once its usage arrives.
    pub(super) context_estimate: Option<super::context_estimate::ContextEstimate>,
    /// The last request's tool-schema estimate, reused while its tool set is unchanged.
    pub(super) tool_schema_estimate: super::context_estimate::ToolSchemaEstimate,
    pub(super) catalog: Arc<Catalog>,
    pub(super) state: LiveCheckpoint,
    pub(super) transcript_delta: Vec<Arc<Value>>,
    pub(super) pending_session_start_stop: Option<String>,
    pub(super) turn_end_turn_id: Option<Arc<str>>,
    pub(super) idle: crate::middleware::IdleSchedule,
    pub(super) events: EventRecorder,
}

/// The actor mutates uniquely owned state; recorder snapshots share it until their ack.
pub(super) struct LiveCheckpoint(pub(super) Arc<Checkpoint>);

impl std::ops::Deref for LiveCheckpoint {
    type Target = Checkpoint;

    fn deref(&self) -> &Checkpoint {
        &self.0
    }
}

impl LiveCheckpoint {
    pub(super) fn make_mut(&mut self) -> &mut Checkpoint {
        Arc::make_mut(&mut self.0)
    }
}

impl Runner {
    pub(super) async fn run(&mut self, mut inbox: SubmissionInbox) -> Result<()> {
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
            let idle_at = self
                .idle
                .deadline(&self.config.middleware, &self.runtime.session_id);
            let submission = tokio::select! {
                biased;
                submission = inbox.recv() => {
                    let Some(submission) = submission else {
                        return Ok(());
                    };
                    submission
                }
                () = idle_deadline(idle_at) => {
                    self.run_idle_turn(&mut inbox).await?;
                    continue;
                }
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

    pub(super) async fn save(&mut self) -> Result<u64> {
        self.persist_with_events(Vec::new(), None).await
    }

    pub(super) async fn persist_with_events(
        &mut self,
        events: Vec<Event>,
        execution: Option<ExecutionRecord>,
    ) -> Result<u64> {
        let previous_sequence = self.state.sequence;
        self.state.make_mut().sequence = self.state.next_sequence()?;
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

    pub(super) fn record_model_call(&mut self) -> Result<()> {
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

    pub(super) async fn record_usage(&mut self, route: &str, usage: &TokenUsage) -> Result<()> {
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

    pub(super) fn record_tools(&mut self, tool_calls: u64, failed_tool_calls: u64) -> Result<()> {
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

    pub(super) async fn finish_and_persist_execution(
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

    pub(super) fn push_context(&mut self, item: Value) {
        let item = Arc::new(item);
        Arc::make_mut(&mut self.state.make_mut().context).push(Arc::clone(&item));
        self.transcript_delta.push(item);
    }

    pub(super) fn extend_context(&mut self, items: Vec<Value>) {
        for item in items {
            if crate::middleware::delivery_once::accept(
                &mut self.state.make_mut().delivered_once,
                &item,
            ) {
                self.push_context(item);
            }
        }
    }

    pub(super) async fn emit(&self, submission_id: Arc<str>, msg: EventMsg) -> Result<()> {
        self.events
            .record(Event {
                submission_id: Some(submission_id),
                msg,
            })
            .await
    }
}

/// Resolves at `at`, or never. One parked timer-wheel entry: no polling, no thread, and
/// dropping it when a submission wins the select unlinks it in constant time.
async fn idle_deadline(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

pub(super) fn unix_timestamp_ms() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            Error::Checkpoint(format!("system clock predates the Unix epoch: {error}"))
        })?;
    i64::try_from(elapsed.as_millis()).map_err(|error| {
        Error::Checkpoint(format!("system clock exceeds the supported range: {error}"))
    })
}
