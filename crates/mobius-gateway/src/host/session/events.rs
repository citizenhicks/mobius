use super::*;

impl HostState {
    pub(super) async fn forward_event(&mut self, record: JournalEvent) -> Result<()> {
        if self.apply_event(record).await? {
            self.restart_after_turn = false;
            self.restart("mobius-gateway")
                .await
                .map_err(|rejection| Error::Config(rejection.message))?;
            self.broadcast_changed()
                .await
                .map_err(|rejection| Error::Config(rejection.message))?;
        }
        Ok(())
    }

    pub(super) async fn apply_event(&mut self, record: JournalEvent) -> Result<bool> {
        let was_active = self.pending_turns > 0;
        let event = record.event.clone();
        if self
            .project_and_publish(record, JournalDelivery::Live)?
            .is_none()
        {
            return Ok(false);
        }
        if let EventMsg::Message(message) = &event.msg
            && let Some(message_id) = peer_message_id(message)
        {
            self.swarm.acknowledge_user_attention(message_id).await?;
        }
        match &event.msg {
            EventMsg::TurnStarted(_) => self.last_assistant_text = None,
            EventMsg::AssistantMessage(message) => {
                if let Some(text) = assistant_text(message) {
                    self.last_assistant_text = Some(text);
                }
            }
            EventMsg::TurnComplete(_) => {
                let outcome =
                    swarm_run_outcome(self.turn_error.clone(), self.last_assistant_text.clone());
                if let Some(message_id) = event.submission_id.as_deref() {
                    self.swarm
                        .settle_delivery(
                            message_id,
                            &self.running.session_id,
                            &self.spec.bot_id,
                            outcome,
                        )
                        .await?;
                }
            }
            EventMsg::TurnAborted(turn) => {
                if let Some(message_id) = event.submission_id.as_deref() {
                    self.swarm
                        .settle_delivery(
                            message_id,
                            &self.running.session_id,
                            &self.spec.bot_id,
                            SwarmRunOutcome::Failed {
                                message: self
                                    .turn_error
                                    .clone()
                                    .unwrap_or_else(|| turn.reason.clone()),
                            },
                        )
                        .await?;
                }
            }
            _ => {}
        }
        if opens_message_capacity(&event.msg) {
            self.swarm.notify_capacity_available(&self.spec.bot_id);
        }
        if let EventMsg::SubmissionRejected(_) = &event.msg
            && let Some(submission_id) = event.submission_id.as_deref()
        {
            self.swarm.notify_rejected(submission_id, &self.spec.bot_id);
        }
        let next_activity = self.activity_for_event(&event.msg)?;
        account_turn_event(&mut self.pending_turns, &mut self.pending_messages, &event);
        match &event.msg {
            EventMsg::ExecApprovalRequest(_) => self.approval_active = true,
            EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) => self.approval_active = false,
            _ => {}
        }
        let routine_completion = self.observe_routine_event(&event)?;
        if let Some((active, status, message)) = routine_completion {
            let summary = (status == RoutineRunStatus::Succeeded)
                .then(|| self.last_assistant_text.clone())
                .flatten();
            let run = self.bots.finish_run(active.run, status, message)?;
            self.swarm
                .project_routine_outcome(&run, summary, None)
                .await?;
        }
        if let Some(activity) = next_activity {
            self.set_activity(activity)?;
            self.broadcast_sessions()
                .await
                .map_err(|rejection| Error::Config(rejection.message))?;
        }

        let became_idle = self.pending_turns == 0 && was_active;
        let mut restart = false;
        if became_idle {
            restart = self.restart_after_turn;
            if !self.approval_active && self.active_routine.is_none() {
                for waiter in self.idle_waiters.drain(..) {
                    let _ = waiter.send(());
                }
            }
        }
        Ok(restart)
    }

    pub(super) async fn reconcile_replayed_swarm_work(&self) -> Result<()> {
        let mut error = None;
        let mut summary = None;
        let mut terminal = None;
        for frame in &self.replay {
            let ServerMessage::AgentEvent { record, .. } = &frame.message else {
                continue;
            };
            match &record.event.msg {
                EventMsg::Message(message) => {
                    if let Some(message_id) = peer_message_id(message) {
                        self.swarm.acknowledge_user_attention(message_id).await?;
                    }
                }
                EventMsg::TurnStarted(_) => {
                    error = None;
                    summary = None;
                    terminal = None;
                }
                EventMsg::AssistantMessage(message) => {
                    if let Some(text) = assistant_text(message) {
                        summary = Some(text);
                    }
                }
                EventMsg::Error(event) => error = Some(event.message.clone()),
                EventMsg::TurnComplete(_) => {
                    terminal = record.event.submission_id.clone().map(|message_id| {
                        (
                            message_id,
                            swarm_run_outcome(error.clone(), summary.clone()),
                        )
                    });
                }
                EventMsg::TurnAborted(event) => {
                    terminal = record.event.submission_id.clone().map(|message_id| {
                        (
                            message_id,
                            SwarmRunOutcome::Failed {
                                message: error.clone().unwrap_or_else(|| event.reason.clone()),
                            },
                        )
                    });
                }
                _ => {}
            }
        }
        if let Some((message_id, outcome)) = terminal {
            self.swarm
                .settle_delivery(
                    &message_id,
                    &self.running.session_id,
                    &self.spec.bot_id,
                    outcome,
                )
                .await?;
        }
        Ok(())
    }

    pub(super) fn project_and_publish(
        &mut self,
        journal: JournalEvent,
        delivery: JournalDelivery,
    ) -> Result<Option<ServerFrame>> {
        validate_gateway_event(&journal.event.msg)?;
        let sequence_kind = classify_journal_sequence(self.sequence, journal.sequence, delivery)?;
        let sequence = journal.sequence;
        let frame = ServerFrame::new(ServerMessage::AgentEvent {
            session_id: self.running.session_id.clone(),
            record: project_record(&self.running.frontend, journal),
        });
        validate_event_frame(&frame)?;
        if let ServerMessage::AgentEvent { record, .. } = &frame.message {
            update_widgets(&mut self.widgets, &record.event.msg);
            if let EventMsg::AssistantMessage(message) = &record.event.msg {
                compact_replay_deltas(
                    &mut self.replay,
                    &mut self.replay_bytes,
                    &message.model_step_id,
                )?;
            }
        }
        if sequence_kind == JournalSequence::AlreadyLoaded {
            return Ok(None);
        }
        let truncated = record_and_publish(
            &mut self.replay,
            &mut self.replay_bytes,
            &self.events,
            frame.clone(),
            delivery != JournalDelivery::Live,
        )?;
        if truncated {
            self.next_before_sequence = self.replay.front().and_then(event_sequence);
        }
        self.sequence = sequence;
        Ok(Some(frame))
    }

    pub(super) fn observe_routine_event(
        &mut self,
        event: &Event,
    ) -> Result<Option<(ActiveRoutine, RoutineRunStatus, Option<String>)>> {
        let Some(active) = self.active_routine.as_mut() else {
            return Ok(None);
        };
        let completion = match &event.msg {
            EventMsg::TurnStarted(turn)
                if event.submission_id.as_deref() == Some(active.submission_id.as_str()) =>
            {
                active.turn_id = Some(turn.turn_id.clone());
                None
            }
            EventMsg::Error(error) => {
                active.failure.get_or_insert_with(|| error.message.clone());
                None
            }
            EventMsg::TurnComplete(turn)
                if active.turn_id.as_deref() == Some(turn.turn_id.as_str()) =>
            {
                Some(match active.failure.clone() {
                    Some(message) => (RoutineRunStatus::Failed, Some(message)),
                    None => (RoutineRunStatus::Succeeded, None),
                })
            }
            EventMsg::TurnAborted(turn)
                if active.turn_id.as_deref() == Some(turn.turn_id.as_str()) =>
            {
                Some((
                    RoutineRunStatus::Failed,
                    Some(
                        active
                            .failure
                            .clone()
                            .unwrap_or_else(|| turn.reason.clone()),
                    ),
                ))
            }
            _ => None,
        };
        Ok(completion.map(|(status, message)| {
            let active = self
                .active_routine
                .take()
                .expect("completion requires an active routine run");
            (active, status, message)
        }))
    }

    pub(super) async fn ready(&self) -> Result<SessionReadyPayload> {
        let checkpoint = self
            .checkpoints
            .load(&self.running.session_id)
            .await?
            .ok_or_else(|| Error::Config("the running session has no checkpoint".into()))?;
        let mut run_stats = completed_run_stats(&checkpoint.execution_stats);
        run_stats.active = checkpoint
            .active_execution
            .as_ref()
            .map(|active| active_run_summary(&checkpoint.session_id, active));
        let context_limit_tokens = match self.running.session.model.model_context_window {
            Some(context_window) if self.spec.agent.config.middleware.enabled("compaction") => {
                Some(
                    mobius::middleware::compaction::Compaction::new(
                        crate::middleware_manifest::integer_setting(
                            &self.spec.agent.config.middleware,
                            "compaction",
                            "at_tokens",
                        )?,
                    )?
                    .trigger_tokens(context_window),
                )
            }
            context_window => context_window,
        };
        Ok(SessionReadyPayload {
            latest_sequence: self.sequence,
            next_before_sequence: self.next_before_sequence,
            workspace: self.spec.workspace_info(),
            git: git_status(&self.running.gateway_sandbox).await,
            session: self.running.session.clone(),
            contributions: self.running.frontend.contributions().to_vec(),
            widgets: self
                .widgets
                .iter()
                .map(|((capability, _), item)| SessionWidget {
                    capability: capability.clone(),
                    item: item.clone(),
                })
                .collect(),
            tool_count: self.running.tool_count,
            compaction_count: checkpoint.compaction_count,
            context_limit_tokens,
            run_stats,
        })
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
        let sessions = session_catalog(&self.checkpoints, &self.activities)
            .await
            .map_err(internal)?;
        let approvals = background_approvals(&self.checkpoints, &self.activities)
            .await
            .map_err(internal)?;
        self.swarm.retry_pending();
        let _ = self
            .gateway_events
            .send(ServerFrame::new(ServerMessage::Sessions {
                request_id: None,
                sessions,
            }));
        let _ = self
            .gateway_events
            .send(ServerFrame::new(ServerMessage::BackgroundApprovals {
                approvals,
            }));
        Ok(())
    }

    pub(super) fn activity_for_event(
        &mut self,
        event: &EventMsg,
    ) -> Result<Option<SessionActivity>> {
        let current = self.activity()?;
        let next = match event {
            EventMsg::TurnStarted(turn) => {
                self.turn_error = None;
                Some(SessionActivity {
                    state: SessionActivityState::Running,
                    turn_id: Some(turn.turn_id.clone()),
                    started_at: Some(Utc::now().timestamp()),
                    ..SessionActivity::default()
                })
            }
            EventMsg::ExecApprovalRequest(request) => Some(SessionActivity {
                state: SessionActivityState::AwaitingApproval,
                turn_id: Some(request.turn_id.clone()),
                approval_request_id: Some(request.id.clone()),
                started_at: current.started_at.or_else(|| Some(Utc::now().timestamp())),
                ..SessionActivity::default()
            }),
            EventMsg::Error(error) if current.state == SessionActivityState::Idle => {
                self.turn_error = None;
                Some(SessionActivity {
                    last_outcome: Some(SessionOutcome::Failed),
                    message: Some(error.message.clone()),
                    ..SessionActivity::default()
                })
            }
            EventMsg::Error(error) => {
                self.turn_error = Some(error.message.clone());
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
                        SessionOutcome::Failed
                    } else {
                        SessionOutcome::Aborted
                    }),
                    message: Some(error.unwrap_or_else(|| turn.reason.clone())),
                    ..SessionActivity::default()
                })
            }
            _ => None,
        };
        Ok(next)
    }

    pub(super) async fn resume_activity(&self) -> Result<()> {
        let current = self.activity()?;
        if current.state != SessionActivityState::AwaitingApproval {
            return Ok(());
        }
        self.set_activity(SessionActivity {
            state: SessionActivityState::Running,
            turn_id: current.turn_id,
            started_at: current.started_at,
            ..SessionActivity::default()
        })?;
        self.broadcast_sessions()
            .await
            .map_err(|rejection| Error::Config(rejection.message))
    }

    pub(super) async fn fail_activity(&self, message: &str) -> Result<()> {
        if self.activity()?.state == SessionActivityState::Idle {
            return Ok(());
        }
        self.set_activity(SessionActivity {
            last_outcome: Some(SessionOutcome::Failed),
            message: Some(message.into()),
            ..SessionActivity::default()
        })?;
        self.broadcast_sessions()
            .await
            .map_err(|rejection| Error::Config(rejection.message))
    }

    pub(super) fn activity(&self) -> Result<SessionActivity> {
        let activities = self
            .activities
            .lock()
            .map_err(|_| Error::Config("session activity lock is poisoned".into()))?;
        Ok(activities
            .get(&self.running.session_id)
            .cloned()
            .unwrap_or_default())
    }

    pub(super) fn set_activity(&self, activity: SessionActivity) -> Result<()> {
        self.activities
            .lock()
            .map_err(|_| Error::Config("session activity lock is poisoned".into()))?
            .insert(self.running.session_id.clone(), activity);
        Ok(())
    }

    pub(super) fn broadcast(&self, message: ServerMessage) {
        let _ = self.events.send(ServerFrame::new(message));
    }

    pub(super) fn require_idle(&self) -> std::result::Result<(), Rejection> {
        if !self.is_idle() {
            Err(Rejection {
                code: "agent_busy",
                message: "finish or interrupt the active turn before changing gateway state".into(),
                fatal: false,
            })
        } else {
            Ok(())
        }
    }

    pub(super) fn is_idle(&self) -> bool {
        self.pending_turns == 0 && !self.approval_active && self.active_routine.is_none()
    }
}

const MAX_ACTIVITY_MESSAGE_BYTES: usize = 512;

fn completed_activity(error: Option<String>, final_answer: Option<&str>) -> SessionActivity {
    let failed = error.is_some();
    SessionActivity {
        last_outcome: Some(if failed {
            SessionOutcome::Failed
        } else {
            SessionOutcome::Completed
        }),
        message: error.or_else(|| final_answer.map(bounded_activity_message)),
        ..SessionActivity::default()
    }
}

fn bounded_activity_message(message: &str) -> String {
    if message.len() <= MAX_ACTIVITY_MESSAGE_BYTES {
        return message.into();
    }
    let mut end = MAX_ACTIVITY_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_activity_uses_the_bounded_final_answer_without_masking_failures() {
        let completed = completed_activity(None, Some(&"é".repeat(300)));
        assert_eq!(completed.last_outcome, Some(SessionOutcome::Completed));
        assert!(completed.message.expect("final answer").len() <= MAX_ACTIVITY_MESSAGE_BYTES);

        let failed = completed_activity(Some("model failed".into()), Some("stale answer"));
        assert_eq!(failed.last_outcome, Some(SessionOutcome::Failed));
        assert_eq!(failed.message.as_deref(), Some("model failed"));
    }
}

fn account_turn_event(
    pending_turns: &mut usize,
    pending_messages: &mut HashSet<String>,
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

fn peer_message_id(message: &MessageEvent) -> Option<&str> {
    match &message.author {
        MessageAuthor::Peer { message_id, .. } => Some(message_id),
        MessageAuthor::User => None,
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

fn swarm_run_outcome(error: Option<String>, summary: Option<String>) -> SwarmRunOutcome {
    match (error, summary) {
        (Some(message), _) => SwarmRunOutcome::Failed { message },
        (None, Some(summary)) => SwarmRunOutcome::Succeeded { summary },
        (None, None) => SwarmRunOutcome::Failed {
            message: "Bot returned no final response".into(),
        },
    }
}

fn opens_message_capacity(event: &EventMsg) -> bool {
    matches!(event, EventMsg::TurnStarted(_) | EventMsg::Message(_))
}

#[cfg(test)]
mod accounting_tests {
    use super::*;
    use mobius::protocol::{
        MessageDelivery, SubmissionRejectedEvent, TurnCompleteEvent, TurnStartedEvent,
    };

    fn event(submission_id: &str, msg: EventMsg) -> Event {
        Event {
            submission_id: Some(submission_id.into()),
            msg,
        }
    }

    fn message(delivery: MessageDelivery) -> EventMsg {
        EventMsg::Message(MessageEvent {
            author: MessageAuthor::Peer {
                message_id: "message-1".into(),
                session_id: "reviewer".into(),
                handle: "reviewer".into(),
            },
            delivery,
            text: "Review this".into(),
            attachments: Vec::new(),
            message_target: None,
        })
    }

    #[test]
    fn steering_message_releases_its_turn_reservation() {
        let mut pending_turns = 2;
        let mut messages = HashSet::from(["message-1".into()]);

        account_turn_event(
            &mut pending_turns,
            &mut messages,
            &event("message-1", message(MessageDelivery::Steer)),
        );
        account_turn_event(
            &mut pending_turns,
            &mut messages,
            &event(
                "user-1",
                EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: "turn-1".into(),
                }),
            ),
        );

        assert_eq!(pending_turns, 0);
        assert!(messages.is_empty());
    }

    #[test]
    fn started_message_keeps_its_active_turn_on_delivery() {
        let mut pending_turns = 1;
        let mut messages = HashSet::from(["message-1".into()]);

        account_turn_event(
            &mut pending_turns,
            &mut messages,
            &event(
                "message-1",
                EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: "turn-1".into(),
                    model_context_window: None,
                }),
            ),
        );
        account_turn_event(
            &mut pending_turns,
            &mut messages,
            &event("message-1", message(MessageDelivery::Turn)),
        );

        assert_eq!(pending_turns, 1);
        assert!(messages.is_empty());
    }

    #[test]
    fn unreserved_turn_increments_the_active_count() {
        let mut pending_turns = 0;
        let mut messages = HashSet::new();

        account_turn_event(
            &mut pending_turns,
            &mut messages,
            &event(
                "recovered-message",
                EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: "turn-1".into(),
                    model_context_window: None,
                }),
            ),
        );

        assert_eq!(pending_turns, 1);
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

    #[test]
    fn starting_a_queued_turn_opens_message_capacity_before_its_message_is_submitted() {
        assert!(opens_message_capacity(&EventMsg::TurnStarted(
            TurnStartedEvent {
                turn_id: "turn-1".into(),
                model_context_window: None,
            }
        )));
        assert!(opens_message_capacity(&message(MessageDelivery::Queue)));
        assert!(!opens_message_capacity(&EventMsg::SubmissionRejected(
            SubmissionRejectedEvent {
                message: "prompt hook rejected the queued message".into(),
            }
        )));
    }
}
