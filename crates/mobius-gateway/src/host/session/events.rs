use super::super::telemetry::cancellation_reason;
use super::{
    ActiveRoutine, Arc, Error, Event, EventMsg, ExecutionOutcome, FrontendEvent, HashSet,
    HostState, JournalDelivery, JournalEvent, JournalSequence, MessageTarget,
    ModelStepContentPhase, Rejection, ReplayEntry, Result, RoutineRunStatus, ServerFrame,
    ServerMessage, SessionActivity, SessionActivityState, SessionReadyPayload, SessionRuntimeState,
    SessionWidget, SharedFrame, TurnLifecycle, Utc, activity_catalog, classify_journal_sequence,
    compact_replay_deltas, event_sequence, git_status, internal, project_record,
    publish_ready_and_pending, record_and_publish, switch_workspace_branch, turn_lifecycle,
    update_session_activity, update_widgets, validate_gateway_event,
};
use mobius::backend::checkpoint::TranscriptPageRequest;
use serde_json::Value;
use std::collections::BTreeSet;

impl HostState {
    pub(super) async fn apply_event(&mut self, record: JournalEvent) -> Result<()> {
        let timestamp_ms = record.recorded_at_ms;
        let Some(frame) = self.project_and_publish(record, JournalDelivery::Live)? else {
            return Ok(());
        };
        let ServerMessage::AgentEvent { record, .. } = &frame.message else {
            return Err(Error::Config(
                "projected journal frame omitted agent event".into(),
            ));
        };
        let event = &record.event;
        match &event.msg {
            EventMsg::TurnStarted(_) => self.last_assistant_text = None,
            EventMsg::AssistantMessage(message) => {
                if let Some(text) = assistant_text(message) {
                    self.last_assistant_text = Some(text);
                }
            }
            EventMsg::TurnAborted(turn) => tracing::info!(
                event = "turn_aborted",
                timestamp_ms,
                session_id = %self.running.session_id,
                bot_id = %self.spec.bot_id,
                turn_id = %turn.turn_id,
                submission_id = ?event.submission_id,
                reason = cancellation_reason(&turn.reason, self.turn_error.is_some()),
                "turn aborted"
            ),
            // Compaction middleware counts a compaction once its turn ends.
            EventMsg::ContextCompacted => self.compacted_in_turn = true,
            EventMsg::ModelChanged(changed) => self.broadcast_meta(
                changed
                    .model_context_window
                    .map(|window| self.running.frontend.context_limit(window)),
            )?,
            _ => {}
        }
        let next_activity = self.activity_for_event(&event.msg).await?;
        account_turn_event(&mut self.pending_turns, &mut self.pending_messages, event);
        match turn_lifecycle(&event.msg) {
            Some(TurnLifecycle::ApprovalRequested { .. }) => self.approval_active = true,
            Some(TurnLifecycle::Finished { .. }) => {
                self.approval_active = false;
                if std::mem::take(&mut self.compacted_in_turn) {
                    self.broadcast_meta(self.context_limit_tokens())?;
                }
            }
            Some(TurnLifecycle::Started { .. }) | None => {}
        }
        let routine_completion = self.observe_routine_event(event);
        if let Some((active, mut status, message)) = routine_completion {
            if matches!(event.msg, EventMsg::TurnAborted(_))
                && self.bots.run_cancel_requested(active.run.id())?
            {
                status = RoutineRunStatus::Cancelled;
            }
            self.bots.finish_run(active.run, status, message)?;
        }
        if let Some(activity) = next_activity {
            self.set_activity(activity).await?;
            self.broadcast_sessions()
                .await
                .map_err(|rejection| Error::Config(rejection.message.to_string()))?;
        }

        self.settle_idle().await;
        self.telemetry.notify.notify_one();
        Ok(())
    }

    pub(super) fn project_and_publish(
        &mut self,
        journal: JournalEvent,
        delivery: JournalDelivery,
    ) -> Result<Option<SharedFrame>> {
        validate_gateway_event(&journal.event.msg)?;
        let sequence_kind = classify_journal_sequence(self.sequence, journal.sequence, delivery)?;
        let sequence = journal.sequence;
        let (record, blocks) = project_record(&self.running.frontend, journal);
        let transcript = if sequence_kind == JournalSequence::Next {
            self.transcript
                .apply(sequence, record.recorded_at_ms, &record.event, blocks)
        } else {
            Vec::new()
        };
        let frame = ServerFrame::new(ServerMessage::AgentEvent {
            session_id: self.running.session_id.share(),
            record,
            transcript,
        });
        let entry = ReplayEntry::new(frame)?;
        let frame = &entry.frame;
        if let ServerMessage::AgentEvent { record, .. } = &frame.message {
            update_widgets(&mut self.widgets, &record.event.msg);
            super::super::replay::update_token_usage(&mut self.token_usage, &record.event.msg);
            if let EventMsg::TokenCount(count) = &record.event.msg
                && let Some(usage) = &count.info
            {
                self.telemetry.note_context(&self.running.session_id, usage);
            }
            if let EventMsg::AssistantMessage(message) = &record.event.msg {
                compact_replay_deltas(
                    &mut self.replay,
                    &mut self.replay_bytes,
                    &message.model_step_id,
                );
            }
        }
        if sequence_kind == JournalSequence::AlreadyLoaded {
            return Ok(None);
        }
        self.work_activity.mark();
        let truncated = record_and_publish(
            &mut self.replay,
            &mut self.replay_bytes,
            &self.events,
            entry.clone(), // rules-allow: the replay records the entry while the caller receives its Arc-backed frame
            delivery != JournalDelivery::Live,
        );
        if truncated {
            self.transcript.trim_before(
                self.replay
                    .front()
                    .and_then(|entry| event_sequence(&entry.frame))
                    .unwrap_or_else(|| sequence.saturating_add(1)),
            );
        }
        self.sequence = sequence;
        Ok(Some(entry.frame))
    }

    pub(super) fn observe_routine_event(
        &mut self,
        event: &Event,
    ) -> Option<(ActiveRoutine, RoutineRunStatus, Option<Arc<str>>)> {
        let active = self.active_routine.as_mut()?;
        if let EventMsg::Error(error) = &event.msg {
            // Keep the first failure until a later completion event persists the routine result.
            active
                .failure
                .get_or_insert_with(|| Arc::clone(&error.message));
            return None;
        }
        let completion = match turn_lifecycle(&event.msg)? {
            TurnLifecycle::Started { turn_id }
                if event.submission_id.as_ref() == Some(&active.submission_id) =>
            {
                active.turn_id = Some(Arc::clone(turn_id));
                None
            }
            TurnLifecycle::Finished {
                turn_id,
                abort_reason,
            } if active.turn_id.as_ref() == Some(turn_id) => {
                Some(match (active.failure.take(), abort_reason) {
                    (Some(message), _) => (RoutineRunStatus::Failed, Some(message)),
                    (None, Some(reason)) => (RoutineRunStatus::Failed, Some(Arc::clone(reason))),
                    (None, None) => (RoutineRunStatus::Succeeded, None),
                })
            }
            _ => None,
        };
        let (status, message) = completion?;
        self.active_routine
            .take()
            .map(|active| (active, status, message))
    }

    fn context_limit_tokens(&self) -> Option<i64> {
        self.running
            .session
            .model
            .model_context_window
            .map(|window| self.running.frontend.context_limit(window))
    }

    /// Sends the chat's capability metadata after an event changed it.
    fn broadcast_meta(&self, context_limit_tokens: Option<i64>) -> Result<()> {
        self.broadcast(ServerMessage::SessionMetaChanged {
            session_id: self.running.session_id.share(),
            contributions: self.running.frontend.contributions()?,
            context_limit_tokens,
        });
        Ok(())
    }

    pub(super) async fn ready(&self) -> Result<SessionReadyPayload> {
        let runtime =
            SessionRuntimeState::load(self.checkpoints.as_ref(), &self.running.session_id)
                .await?
                .ok_or_else(|| Error::Config("the running session has no checkpoint".into()))?;
        let context_limit_tokens = self.context_limit_tokens();
        Ok(SessionReadyPayload {
            active_turn_ids: runtime
                .active_turn
                .map(|active| active.turn_id)
                .into_iter()
                .collect(),
            pending_approvals: runtime
                .undecided_approval
                .into_iter()
                .map(|mut request| {
                    self.running.frontend.preview_approvals(&mut request);
                    request
                })
                .collect(),
            latest_sequence: self.sequence,
            workspace: self.spec.workspace_info(),
            attached_folders: self.spec.attached_folders.clone(), // rules-allow: the ready payload owns a snapshot while the spec stays in state
            git: if self.spec.workspace.is_some() {
                git_status(&self.running.gateway_sandbox).await
            } else {
                None
            },
            session: Arc::clone(&self.running.session),
            contributions: self.running.frontend.contributions()?,
            widgets: self
                .widgets
                .iter()
                .map(|(capability, item)| SessionWidget {
                    capability: Arc::clone(capability),
                    item: Arc::clone(item),
                })
                .collect(),
            context_limit_tokens,
            token_usage: self.token_usage.as_ref().map(Arc::clone),
            active_message_delivery: self.running.prepared.active_message_delivery,
            run_stats: runtime.stats,
            pinned_messages: self.message_pins().await?,
        })
    }

    async fn message_pins(&self) -> Result<BTreeSet<MessageTarget>> {
        Ok(self
            .checkpoints
            .load_state(&self.running.session_id, MESSAGE_PINS_KEY)
            .await?
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default())
    }

    pub(super) async fn set_message_pinned(
        &self,
        target: MessageTarget,
        pinned: bool,
    ) -> std::result::Result<(), Rejection> {
        let mut pins = self.message_pins().await.map_err(internal)?;
        let changed = if pinned {
            self.require_assistant_message(target).await?;
            pins.insert(target)
        } else {
            pins.remove(&target)
        };
        if !changed {
            return Ok(());
        }
        self.checkpoints
            .save_state(
                &self.running.session_id,
                MESSAGE_PINS_KEY,
                &serde_json::to_value(&pins).map_err(internal)?,
            )
            .await
            .map_err(internal)?;
        self.broadcast(ServerMessage::MessagePinsChanged {
            session_id: self.running.session_id.share(),
            pinned_messages: pins,
        });
        Ok(())
    }

    async fn require_assistant_message(
        &self,
        target: MessageTarget,
    ) -> std::result::Result<(), Rejection> {
        let page = self
            .checkpoints
            .transcript_page(
                &self.running.session_id,
                TranscriptPageRequest {
                    before_sequence: target.checkpoint_sequence.checked_add(1),
                    max_batches: 1,
                },
            )
            .await
            .map_err(internal)?;
        let is_assistant = page
            .batches
            .first()
            .filter(|batch| batch.sequence == target.checkpoint_sequence)
            .and_then(|batch| batch.items.get(target.batch_item_count.get() - 1))
            .is_some_and(|item| item.get("role").and_then(Value::as_str) == Some("assistant"));
        if is_assistant {
            Ok(())
        } else {
            Err(Rejection::new(
                "invalid_message_target",
                "only assistant messages in this chat can be pinned",
            ))
        }
    }

    pub(super) async fn switch_git_branch(
        &mut self,
        branch: &str,
    ) -> std::result::Result<(), Rejection> {
        self.require_idle()?;
        switch_workspace_branch(&self.running.gateway_sandbox, branch).await?;
        self.broadcast_changed().await
    }

    pub(super) async fn broadcast_changed(&mut self) -> std::result::Result<(), Rejection> {
        let payload = self.ready().await.map_err(internal)?;
        let ready = ServerFrame::new(ServerMessage::SessionChanged { payload });
        let pending = std::mem::take(&mut self.pending_startup);
        publish_ready_and_pending(&self.events, ready, pending);
        Ok(())
    }

    pub(super) async fn broadcast_sessions(&self) -> std::result::Result<(), Rejection> {
        let (sessions, approvals) = activity_catalog(&self.checkpoints, &self.activities)
            .await
            .map_err(internal)?;
        crate::host::send_catalog(&self.gateway_events, sessions, approvals);
        Ok(())
    }

    pub(super) async fn activity_for_event(
        &mut self,
        event: &EventMsg,
    ) -> Result<Option<SessionActivity>> {
        let next = match event {
            EventMsg::TurnStarted(turn) => {
                self.turn_error = None;
                Some(SessionActivity {
                    state: SessionActivityState::Running,
                    turn_id: Some(Arc::clone(&turn.turn_id)),
                    started_at: Some(Utc::now().timestamp()),
                    ..SessionActivity::default()
                })
            }
            EventMsg::ExecApprovalRequest(request) => Some(SessionActivity {
                state: SessionActivityState::AwaitingApproval,
                turn_id: Some(Arc::clone(&request.turn_id)),
                approval_request_id: Some(Arc::clone(&request.id)),
                started_at: self
                    .activity()
                    .await?
                    .started_at
                    .or_else(|| Some(Utc::now().timestamp())),
                ..SessionActivity::default()
            }),
            EventMsg::Error(error)
                if self.activity().await?.state == SessionActivityState::Idle =>
            {
                self.turn_error = None;
                Some(SessionActivity {
                    last_outcome: Some(ExecutionOutcome::Failed),
                    message: Some(Arc::clone(&error.message)),
                    ..SessionActivity::default()
                })
            }
            EventMsg::Error(error) => {
                self.turn_error = Some(Arc::clone(&error.message));
                None
            }
            EventMsg::TurnComplete(_) => Some(completed_activity(
                self.turn_error.take(),
                self.last_assistant_text.as_deref(),
            )),
            EventMsg::TurnAborted(turn) => {
                let error = self.turn_error.take();
                Some(SessionActivity {
                    last_outcome: Some(if error.is_some() {
                        ExecutionOutcome::Failed
                    } else {
                        ExecutionOutcome::Aborted
                    }),
                    message: Some(error.unwrap_or_else(|| Arc::clone(&turn.reason))),
                    ..SessionActivity::default()
                })
            }
            EventMsg::Frontend(
                FrontendEvent::Widget { .. } | FrontendEvent::RemoveWidget { .. },
            ) => {
                let current = self.activity().await?;
                (current.attention != super::super::replay::attention_count(&self.widgets))
                    .then_some(current)
            }
            _ => None,
        };
        Ok(next)
    }

    pub(super) async fn resume_activity(&self) -> Result<()> {
        let current = self.activity().await?;
        if current.state != SessionActivityState::AwaitingApproval {
            return Ok(());
        }
        self.set_activity(SessionActivity {
            state: SessionActivityState::Running,
            turn_id: current.turn_id,
            started_at: current.started_at,
            ..SessionActivity::default()
        })
        .await?;
        self.broadcast_sessions()
            .await
            .map_err(|rejection| Error::Config(rejection.message.to_string()))
    }

    pub(super) async fn fail_activity(&self, message: &str) -> Result<()> {
        if self.activity().await?.state == SessionActivityState::Idle {
            return Ok(());
        }
        self.set_activity(SessionActivity {
            last_outcome: Some(ExecutionOutcome::Failed),
            message: Some(message.into()),
            ..SessionActivity::default()
        })
        .await?;
        self.broadcast_sessions()
            .await
            .map_err(|rejection| Error::Config(rejection.message.to_string()))
    }

    pub(super) async fn activity(&self) -> Result<SessionActivity> {
        Ok(self
            .activities
            .lock()
            .await
            .activities
            .get(self.running.session_id.as_ref())
            .cloned() // rules-allow: the activity is read out before the catalog lock is released
            .unwrap_or_default())
    }

    pub(super) async fn set_activity(&self, mut activity: SessionActivity) -> Result<()> {
        activity.attention = super::super::replay::attention_count(&self.widgets);
        update_session_activity(
            &self.checkpoints,
            &self.activities,
            &self.running.session_id,
            activity,
        )
        .await
    }

    pub(super) fn broadcast(&self, message: ServerMessage) {
        if let Err(error) = self
            .events
            .send(SharedFrame::new(ServerFrame::new(message)))
        {
            tracing::debug!(%error, "session frame had no subscribers");
        }
    }

    pub(super) fn require_idle(&self) -> std::result::Result<(), Rejection> {
        if self.is_idle() {
            Ok(())
        } else {
            Err(Rejection::new(
                "agent_busy",
                "finish or interrupt the active turn before changing gateway state",
            ))
        }
    }

    /// Releases what an idle chat no longer holds: the desktop and its idle waiters.
    pub(super) async fn settle_idle(&mut self) {
        if !self.is_idle() {
            return;
        }
        self.running.gateway_sandbox.release_desktop_use().await;
        for waiter in self.idle_waiters.drain(..) {
            if waiter.send(()).is_err() {
                tracing::debug!("idle waiter stopped waiting");
            }
        }
    }

    pub(super) fn is_idle(&self) -> bool {
        self.pending_turns == 0
            && self.admissions.is_empty()
            && !self.approval_active
            && self.active_routine.is_none()
    }
}

const MAX_ACTIVITY_MESSAGE_BYTES: usize = 512;
const MESSAGE_PINS_KEY: &str = "message_pins.v1";

fn completed_activity(error: Option<Arc<str>>, final_answer: Option<&str>) -> SessionActivity {
    let failed = error.is_some();
    SessionActivity {
        last_outcome: Some(if failed {
            ExecutionOutcome::Failed
        } else {
            ExecutionOutcome::Completed
        }),
        message: error.or_else(|| final_answer.map(bounded_activity_message)),
        ..SessionActivity::default()
    }
}

fn bounded_activity_message(message: &str) -> Arc<str> {
    message
        .split_at_checked(message.floor_char_boundary(MAX_ACTIVITY_MESSAGE_BYTES))
        .map_or(message, |(head, _)| head)
        .into()
}

fn account_turn_event(
    pending_turns: &mut usize,
    pending_messages: &mut HashSet<Arc<str>>,
    event: &Event,
) {
    match &event.msg {
        EventMsg::TurnStarted(_) => {
            let reserved = event
                .submission_id
                .as_deref()
                .is_some_and(|submission_id| pending_messages.remove(submission_id));
            if !reserved {
                *pending_turns = pending_turns.saturating_add(1);
            }
        }
        EventMsg::Message(message)
            if message.delivery == mobius::protocol::MessageDelivery::Queue => {}
        EventMsg::Message(_) | EventMsg::SubmissionRejected(_) => {
            if event
                .submission_id
                .as_deref()
                .is_some_and(|submission_id| pending_messages.remove(submission_id))
            {
                *pending_turns = pending_turns.saturating_sub(1);
            }
        }
        EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) => {
            *pending_turns = pending_turns.saturating_sub(1);
        }
        _ => {}
    }
}

fn assistant_text(message: &mobius::protocol::AssistantMessageEvent) -> Option<String> {
    let text = message
        .content
        .iter()
        .filter(|content| content.phase == ModelStepContentPhase::FinalAnswer)
        .map(|content| content.text.as_str())
        .collect::<String>();
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod accounting_tests {
    use super::*;
    use mobius::protocol::{
        MessageAuthor, MessageDelivery, MessageEvent, SubmissionRejectedEvent, TurnCompleteEvent,
        TurnStartedEvent,
    };

    fn event(submission_id: &str, msg: EventMsg) -> Event {
        Event {
            submission_id: Some(submission_id.into()),
            msg,
        }
    }

    fn message(delivery: MessageDelivery) -> EventMsg {
        EventMsg::Message(MessageEvent {
            author: MessageAuthor::Source {
                cause_id: None,
                ancestry: Vec::new(),
                message_id: "message-1".into(),
                source: mobius::protocol::MessageSource::Session {
                    session_id: mobius::SessionId::new("reviewer").unwrap(),
                },
                handle: "reviewer".into(),
                symbol: None,
            },
            delivery,
            text: "Review this".into(),
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        })
    }

    #[test]
    fn queued_message_keeps_the_execution_reserved_until_its_turn_finishes() {
        let mut pending_turns = 2;
        let mut messages = HashSet::from(["queued".into()]);
        for event in [
            event("queued", message(MessageDelivery::Queue)),
            event(
                "active",
                EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: "active".into(),
                }),
            ),
            event(
                "queued",
                EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: "next".into(),
                    model_context_window: None,
                }),
            ),
        ] {
            account_turn_event(&mut pending_turns, &mut messages, &event);
            assert!(
                pending_turns > 0,
                "queued work must keep the old execution reserved"
            );
        }
        account_turn_event(
            &mut pending_turns,
            &mut messages,
            &event(
                "queued",
                EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: "next".into(),
                }),
            ),
        );
        assert_eq!(pending_turns, 0);
        assert!(messages.is_empty());
    }

    #[test]
    fn rejection_releases_its_message_reservation() {
        let mut pending_turns = 1;
        let mut messages = HashSet::from(["message-1".into()]);

        account_turn_event(
            &mut pending_turns,
            &mut messages,
            &event(
                "message-1",
                EventMsg::SubmissionRejected(SubmissionRejectedEvent {
                    message: "rejected".into(),
                }),
            ),
        );

        assert_eq!(pending_turns, 0);
        assert!(messages.is_empty());
    }
}
