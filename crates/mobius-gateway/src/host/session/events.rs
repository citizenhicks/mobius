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
        let next_activity = self.activity_for_event(&event.msg)?;
        match &event.msg {
            EventMsg::ExecApprovalRequest(_) => self.approval_active = true,
            EventMsg::TurnComplete(_) => {
                self.pending_turns = self.pending_turns.saturating_sub(1);
                self.approval_active = false;
            }
            EventMsg::TurnAborted(_) => {
                self.pending_turns = self.pending_turns.saturating_sub(1);
                self.approval_active = false;
            }
            _ => {}
        }
        let cron_completion = self.observe_cron_event(&event)?;
        if let Some(activity) = next_activity {
            self.set_activity(activity)?;
            self.broadcast_sessions()
                .await
                .map_err(|rejection| Error::Config(rejection.message))?;
        }

        let became_idle = self.pending_turns == 0 && was_active;
        let mut restart = false;
        if became_idle {
            if let Some((active, status, message)) = cron_completion {
                self.cron.finish_run(active.run, status, message)?;
            }
            restart = self.restart_after_turn;
            if !self.approval_active && self.active_cron.is_none() {
                for waiter in self.idle_waiters.drain(..) {
                    let _ = waiter.send(());
                }
            }
        }
        Ok(restart)
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
            if let EventMsg::ModelStepCompleted(step) = &record.event.msg
                && matches!(&step.outcome, ModelStepOutcome::Completed { .. })
            {
                compact_replay_deltas(
                    &mut self.replay,
                    &mut self.replay_bytes,
                    &step.model_step_id,
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

    pub(super) fn observe_cron_event(
        &mut self,
        event: &Event,
    ) -> Result<Option<(ActiveCron, CronRunStatus, Option<String>)>> {
        let Some(active) = self.active_cron.as_mut() else {
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
            EventMsg::ExecApprovalRequest(request)
                if active.turn_id.as_deref() == Some(request.turn_id.as_str()) =>
            {
                active.failure.get_or_insert_with(|| {
                    "headless cron run requested interactive tool approval".into()
                });
                self.running
                    .sender
                    .as_ref()
                    .ok_or_else(|| {
                        Error::Mobius(mobius::Error::Stopped(
                            "agent command channel is closed".into(),
                        ))
                    })?
                    .send(Submission {
                        id: Uuid::new_v4().to_string(),
                        op: Op::ExecApproval {
                            id: request.id.clone(),
                            decision: ReviewDecision::Abort,
                        },
                    })?;
                self.approval_active = false;
                None
            }
            EventMsg::TurnComplete(turn)
                if active.turn_id.as_deref() == Some(turn.turn_id.as_str()) =>
            {
                Some(match active.failure.clone() {
                    Some(message) => (CronRunStatus::Failed, Some(message)),
                    None => (CronRunStatus::Succeeded, None),
                })
            }
            EventMsg::TurnAborted(turn)
                if active.turn_id.as_deref() == Some(turn.turn_id.as_str()) =>
            {
                Some((
                    CronRunStatus::Failed,
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
                .active_cron
                .take()
                .expect("completion requires an active cron run");
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
            config: self.spec.agent.clone(),
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
        let _ = self
            .gateway_events
            .send(ServerFrame::new(ServerMessage::Sessions {
                request_id: None,
                sessions,
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
            EventMsg::TurnComplete(_) => {
                let message = self.turn_error.take();
                Some(SessionActivity {
                    last_outcome: Some(if message.is_some() {
                        SessionOutcome::Failed
                    } else {
                        SessionOutcome::Completed
                    }),
                    message,
                    ..SessionActivity::default()
                })
            }
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
        self.pending_turns == 0 && !self.approval_active && self.active_cron.is_none()
    }
}
