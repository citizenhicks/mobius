use super::*;

impl HostState {
    fn begin_session_mutation(
        &self,
    ) -> std::result::Result<tokio::sync::OwnedRwLockReadGuard<()>, Rejection> {
        try_begin_session_mutation(&self.session_mutations, &self.bots)
    }

    pub(super) async fn reconcile_loaded_startup(&mut self) -> Result<()> {
        self.reconcile_startup_through(self.sequence, JournalDelivery::LoadedStartup)
            .await
    }

    pub(super) async fn reconcile_replacement_startup(&mut self) -> Result<()> {
        let high_water = self
            .checkpoints
            .event_page(
                &self.running.session_id,
                EventPageRequest {
                    before_sequence: None,
                    limit: 1,
                },
            )
            .await?
            .latest_sequence;
        self.reconcile_startup_through(high_water, JournalDelivery::ReplacementStartup)
            .await
    }

    pub(super) async fn reconcile_startup_through(
        &mut self,
        high_water: u64,
        delivery: JournalDelivery,
    ) -> Result<()> {
        if high_water == 0 {
            return Ok(());
        }
        loop {
            let record = self.running.events.recv().await.ok_or_else(|| {
                Error::Config("agent stopped before the startup high-water was delivered".into())
            })?;
            let sequence = record.sequence;
            if let Some(frame) = self.project_and_publish(record, delivery)?
                && delivery == JournalDelivery::ReplacementStartup
            {
                self.pending_startup.push(frame);
            }
            if sequence >= high_water {
                break;
            }
        }
        loop {
            match self.running.events.try_recv() {
                Ok(record) => {
                    if let Some(frame) = self.project_and_publish(record, delivery)?
                        && delivery == JournalDelivery::ReplacementStartup
                    {
                        self.pending_startup.push(frame);
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(Error::Config(
                        "agent stopped while startup events were reconciled".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) async fn run(mut self) {
        loop {
            let next = tokio::select! {
                command = self.commands.recv() => Next::Command(command),
                event = self.running.events.recv() => Next::Event(event),
            };
            match next {
                Next::Command(Some(command)) => {
                    if !self.handle(command).await {
                        break;
                    }
                }
                Next::Command(None) => break,
                Next::Event(Some(event)) => {
                    if let Err(error) = self.forward_event(event).await {
                        let message = error.to_string();
                        self.broadcast(ServerMessage::Error {
                            code: "host_error".into(),
                            message: message.clone(),
                            fatal: true,
                        });
                        if let Err(activity_error) = self.fail_activity(&message).await {
                            self.broadcast(ServerMessage::Error {
                                code: "session_activity".into(),
                                message: activity_error.to_string(),
                                fatal: false,
                            });
                        }
                        break;
                    }
                }
                Next::Event(None) => {
                    self.broadcast(ServerMessage::Error {
                        code: "agent_stopped".into(),
                        message: "the agent stopped".into(),
                        fatal: true,
                    });
                    if let Err(error) = self.fail_activity("the agent stopped").await {
                        self.broadcast(ServerMessage::Error {
                            code: "session_activity".into(),
                            message: error.to_string(),
                            fatal: false,
                        });
                    }
                    break;
                }
            }
        }
        self.commands.close();
        if let Some(rejection) = fail_queued_routine_commands(&mut self.commands, &self.bots) {
            self.broadcast(ServerMessage::Error {
                code: "routine_state_error".into(),
                message: rejection.message,
                fatal: false,
            });
        }
        if let Err(error) = fail_active_routine(
            &self.bots,
            &mut self.active_routine,
            "the agent stopped before the Bot routine completed",
        ) {
            self.broadcast(ServerMessage::Error {
                code: "routine_state_error".into(),
                message: error.to_string(),
                fatal: false,
            });
        }
        match self.bots.history(None) {
            Ok(runs) => {
                for run in runs
                    .iter()
                    .filter(|run| run.status != RoutineRunStatus::Running)
                {
                    if let Err(error) = self.swarm.project_routine_outcome(run, None).await {
                        self.broadcast(ServerMessage::Error {
                            code: "routine_state_error".into(),
                            message: error.to_string(),
                            fatal: false,
                        });
                    }
                }
            }
            Err(error) => self.broadcast(ServerMessage::Error {
                code: "routine_state_error".into(),
                message: error.to_string(),
                fatal: false,
            }),
        }
        for waiter in self.idle_waiters.drain(..) {
            let _ = waiter.send(());
        }
        let bot_id = self.spec.bot_id.clone();
        shutdown_agent(self.running).await;
        self.alive.store(false, Ordering::Release);
        self.terminated.store(true, Ordering::Release);
        self.termination.notify_waiters();
        self.swarm.notify_pending(&bot_id);
    }

    pub(super) async fn handle(&mut self, command: HostCommand) -> bool {
        match command {
            HostCommand::Snapshot {
                last_sequence,
                reply,
            } => {
                let _ = reply.send(self.snapshot_value(last_sequence).await);
            }
            HostCommand::HistoryPage {
                before_sequence,
                reply,
            } => {
                let _ = reply.send(self.history_page_value(before_sequence).await);
            }
            HostCommand::RenameSession {
                session_id,
                title,
                reply,
            } => {
                let result = self.rename_session(&session_id, &title).await;
                let _ = reply.send(result);
            }
            HostCommand::SetSessionPinned {
                session_id,
                pinned,
                reply,
            } => {
                let result = self.set_session_pinned(&session_id, pinned).await;
                let _ = reply.send(result);
            }
            HostCommand::Submit { submission, reply } => {
                let result = async {
                    let _mutation = self.begin_session_mutation()?;
                    let resumes_approval = matches!(
                        &submission.op,
                        Op::ExecApproval {
                            decision: ReviewDecision::Approved
                                | ReviewDecision::ApprovedForSession
                                | ReviewDecision::Denied { .. },
                            ..
                        }
                    );
                    let result = match &submission.op {
                        Op::SetModel { .. } => Err(Rejection {
                            code: "bot_configuration_required",
                            message: "change the model on this chat's Bot profile".into(),
                            fatal: false,
                        }),
                        _ => self.submit(submission),
                    };
                    if result.is_ok()
                        && resumes_approval
                        && let Err(error) = self.resume_activity().await
                    {
                        self.broadcast(ServerMessage::Error {
                            code: "session_activity".into(),
                            message: error.to_string(),
                            fatal: false,
                        });
                    }
                    result
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::ReloadBot { bot, reply } => {
                let result = self.reload_bot(bot).await;
                let _ = reply.send(result);
            }
            HostCommand::GitDiff { scope, reply } => {
                let _ = reply.send(
                    workspace_git_diff(&self.running.gateway_sandbox, &self.spec.workspace, scope)
                        .await,
                );
            }
            HostCommand::WorkspaceFiles { scope, reply } => {
                let _ = reply.send(
                    list_workspace_files(
                        &self.running.gateway_sandbox,
                        &self.spec.workspace,
                        scope,
                    )
                    .await,
                );
            }
            HostCommand::ReadWorkspaceFile {
                path,
                offset,
                max_bytes,
                reply,
            } => {
                let _ = reply.send(
                    read_workspace_file(&self.running.gateway_sandbox, &path, offset, max_bytes)
                        .await,
                );
            }
            HostCommand::WriteWorkspaceFile {
                path,
                content,
                reply,
            } => {
                let result = async {
                    self.require_idle()?;
                    let _mutation = self.begin_session_mutation()?;
                    write_workspace_file(&self.running.gateway_sandbox, &path, &content).await
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::SwitchGitBranch { branch, reply } => {
                let result = async {
                    let _mutation = self.begin_session_mutation()?;
                    self.switch_git_branch(&branch).await
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::RefreshProvider { scope, reply } => {
                let result = async {
                    let _mutation = self.begin_session_mutation()?;
                    self.refresh_provider(&scope).await
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::RefreshExtension { id, reply } => {
                let result = self.refresh_extension(&id).await;
                let _ = reply.send(result);
            }
            HostCommand::ProviderCutoverStatus { reply } => {
                let _ = reply.send(ProviderCutoverStatus {
                    idle: self.is_idle(),
                });
            }
            HostCommand::RunRoutine { run, input, reply } => {
                let result = match self.begin_session_mutation() {
                    Ok(_mutation) => self.run_routine(run, input).await,
                    Err(rejection) => match self.bots.finish_run(
                        run,
                        RoutineRunStatus::Failed,
                        Some(rejection.message.clone()),
                    ) {
                        Ok(completed) => {
                            match self.swarm.project_routine_outcome(&completed, None).await {
                                Ok(_) => Err(rejection),
                                Err(error) => Err(internal(error)),
                            }
                        }
                        Err(error) => Err(internal(error)),
                    },
                };
                let _ = reply.send(result);
            }
            HostCommand::WaitIdle { reply } => {
                if self.is_idle() {
                    let _ = reply.send(());
                } else {
                    self.idle_waiters.push(reply);
                }
            }
            HostCommand::CapacityChanged => self.swarm.retry_pending(),
            HostCommand::StopIfIdle { reply } => {
                if !self.is_idle() {
                    let _ = reply.send(false);
                    return true;
                }
                self.alive.store(false, Ordering::Release);
                let _ = reply.send(true);
                return false;
            }
        }
        true
    }

    pub(super) async fn snapshot_value(
        &self,
        last_sequence: Option<u64>,
    ) -> std::result::Result<HostSnapshot, Rejection> {
        let mut ready = self.ready().await.map_err(internal)?;
        let replay = if last_sequence.is_some() {
            self.replay_after(last_sequence)?
        } else {
            let page = event_turn_page(self.checkpoints.as_ref(), &self.running.session_id, None)
                .await
                .map_err(internal)?;
            ready.next_before_sequence = page.next_before_sequence;
            let mut replay = Vec::new();
            for journal in page.into_chronological() {
                let frame = ServerFrame::new(ServerMessage::AgentEvent {
                    session_id: self.running.session_id.clone(),
                    record: project_record(&self.running.frontend, journal),
                });
                if replayable(&frame) {
                    validate_event_frame(&frame).map_err(internal)?;
                    replay.push(frame);
                }
            }
            replay
        };
        Ok(HostSnapshot { ready, replay })
    }

    pub(super) async fn history_page_value(
        &self,
        before_sequence: Option<u64>,
    ) -> std::result::Result<SessionHistoryPage, Rejection> {
        let page = event_turn_page(
            self.checkpoints.as_ref(),
            &self.running.session_id,
            before_sequence,
        )
        .await
        .map_err(internal)?;
        let next_before_sequence = page.next_before_sequence;
        let records = page
            .into_chronological()
            .into_iter()
            .map(|event| project_record(&self.running.frontend, event))
            .collect();
        Ok(SessionHistoryPage {
            records,
            next_before_sequence,
        })
    }

    pub(super) fn replay_after(
        &self,
        last_sequence: Option<u64>,
    ) -> std::result::Result<Vec<ServerFrame>, Rejection> {
        let Some(last_sequence) = last_sequence else {
            return Ok(self.replay.iter().cloned().collect());
        };
        if last_sequence > self.sequence {
            return Err(Rejection {
                code: "replay_unavailable",
                message: "the reconnect cursor is ahead of the durable session".into(),
                fatal: false,
            });
        }
        let oldest = self.replay.front().and_then(event_sequence);
        if last_sequence < self.sequence
            && oldest.is_none_or(|oldest| last_sequence.saturating_add(1) < oldest)
        {
            return Err(Rejection {
                code: "replay_unavailable",
                message: "the reconnect window expired; reload the active session".into(),
                fatal: false,
            });
        }
        Ok(self
            .replay
            .iter()
            .filter(|frame| event_sequence(frame).is_some_and(|sequence| sequence > last_sequence))
            .cloned()
            .collect())
    }

    pub(super) async fn rename_session(
        &mut self,
        session_id: &str,
        title: &str,
    ) -> std::result::Result<(), Rejection> {
        self.require_session(session_id).await?;
        let title = validate_session_title(title)?;
        let _catalog = self.catalog_lock.lock().await;
        let mut metadata = load_session_metadata(&self.checkpoints)
            .await
            .map_err(internal)?;
        metadata.entry(session_id.into()).or_default().title = Some(title.into());
        save_session_metadata(&self.checkpoints, &metadata)
            .await
            .map_err(internal)?;
        self.broadcast_sessions().await
    }

    pub(super) async fn set_session_pinned(
        &mut self,
        session_id: &str,
        pinned: bool,
    ) -> std::result::Result<(), Rejection> {
        self.require_session(session_id).await?;
        let _catalog = self.catalog_lock.lock().await;
        let mut metadata = load_session_metadata(&self.checkpoints)
            .await
            .map_err(internal)?;
        metadata.entry(session_id.into()).or_default().pinned = pinned;
        save_session_metadata(&self.checkpoints, &metadata)
            .await
            .map_err(internal)?;
        self.broadcast_sessions().await
    }

    pub(super) async fn require_session(
        &self,
        session_id: &str,
    ) -> std::result::Result<(), Rejection> {
        if self
            .checkpoints
            .load(session_id)
            .await
            .map_err(internal)?
            .is_none()
        {
            return Err(Rejection {
                code: "unknown_session",
                message: "the requested session does not exist".into(),
                fatal: false,
            });
        }
        Ok(())
    }

    pub(super) async fn run_routine(
        &mut self,
        run: ActiveRoutineRun,
        input: String,
    ) -> std::result::Result<(), Rejection> {
        if let Err(rejection) = self.require_idle() {
            let completed = self
                .bots
                .finish_run(
                    run,
                    RoutineRunStatus::Failed,
                    Some("the agent was busy when this invocation became due".into()),
                )
                .map_err(internal)?;
            self.swarm
                .project_routine_outcome(&completed, None)
                .await
                .map_err(internal)?;
            return Err(rejection);
        }
        let submission_id = Uuid::new_v4().to_string();
        self.active_routine = Some(ActiveRoutine {
            run,
            submission_id: submission_id.clone(),
            turn_id: None,
            failure: None,
        });
        let submission = Submission {
            id: submission_id,
            op: Op::Message {
                message: MessageSubmission {
                    author: MessageAuthor::User,
                    text: input,
                    attachments: Vec::new(),
                    requested_delivery: None,
                    target_turn_id: None,
                },
            },
        };
        if let Err(rejection) = self.submit(submission) {
            let active = self
                .active_routine
                .take()
                .expect("active routine was just set");
            let completed = self
                .bots
                .finish_run(
                    active.run,
                    RoutineRunStatus::Failed,
                    Some(rejection.message.clone()),
                )
                .map_err(internal)?;
            self.swarm
                .project_routine_outcome(&completed, None)
                .await
                .map_err(internal)?;
            return Err(rejection);
        }
        Ok(())
    }

    pub(super) fn submit(&mut self, submission: Submission) -> std::result::Result<(), Rejection> {
        let message_submission_id =
            matches!(submission.op, Op::Message { .. }).then(|| submission.id.clone());
        let resolves_approval = matches!(submission.op, Op::ExecApproval { .. });
        self.running
            .sender
            .as_ref()
            .ok_or_else(stopped)?
            .send(submission)
            .map_err(|error| Rejection {
                code: match error {
                    mobius::Error::Busy(_) => "agent_busy",
                    mobius::Error::Stopped(_) => "agent_stopped",
                    _ => "invalid_submission",
                },
                message: error.to_string(),
                fatal: matches!(error, mobius::Error::Stopped(_)),
            })?;
        if let Some(submission_id) = message_submission_id {
            self.pending_turns += 1;
            self.pending_messages.insert(submission_id);
        }
        if resolves_approval {
            self.approval_active = false;
        }
        Ok(())
    }

    async fn reload_bot(
        &mut self,
        bot: crate::wire::BotRecord,
    ) -> std::result::Result<(), Rejection> {
        self.require_idle()?;
        if self.spec.bot_id != bot.id {
            return Err(invalid_config("Bot identity does not own this chat"));
        }
        let gateway = self
            .gateway
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .clone();
        gateway
            .validate_provider_selection(&bot.config.config.provider)
            .map_err(invalid_config)?;
        let mut next = self.spec.clone();
        next.bot_description = bot.description;
        next.agent = bot.config;
        let reusable_router = (self.running.provider_epoch
            == self.provider_epoch.load(Ordering::Acquire))
        .then(|| reusable_model_router(&self.spec, &next, &self.running))
        .flatten();
        self.replace_running(next, reusable_router).await
    }

    async fn replace_running(
        &mut self,
        next: ChatSpec,
        reusable_router: Option<ReusableModelRouter>,
    ) -> std::result::Result<(), Rejection> {
        let session_id = self.running.session_id.clone();
        let old_spec = self.spec.clone();
        let old_router = ReusableModelRouter {
            router: Arc::clone(&self.running.model_router),
            provider_epoch: self.running.provider_epoch,
        };
        self.stop_and_drain_running().await.map_err(internal)?;
        let replacement = match start_agent(
            Arc::clone(&self.gateway),
            &next,
            &self.store,
            Arc::clone(&self.credentials),
            Arc::clone(&self.checkpoints),
            self.scratchpad.clone(),
            self.session_files.clone(),
            Arc::clone(&self.swarm),
            session_id,
            "mobius-gateway",
            true,
            reusable_router,
            Arc::clone(&self.provider_epoch),
        )
        .await
        {
            Ok(replacement) => replacement,
            Err(primary) => {
                let recovery = start_agent(
                    Arc::clone(&self.gateway),
                    &old_spec,
                    &self.store,
                    Arc::clone(&self.credentials),
                    Arc::clone(&self.checkpoints),
                    self.scratchpad.clone(),
                    self.session_files.clone(),
                    Arc::clone(&self.swarm),
                    self.running.session_id.clone(),
                    "mobius-gateway-rollback",
                    true,
                    Some(old_router),
                    Arc::clone(&self.provider_epoch),
                )
                .await;
                let recovery = match recovery {
                    Ok(recovery) => recovery,
                    Err(rollback) => {
                        return Err(internal(mobius::Error::Rollback {
                            primary: Box::new(mobius::Error::Config(primary.to_string())),
                            rollback: Box::new(mobius::Error::Config(rollback.to_string())),
                        }));
                    }
                };
                self.running = recovery;
                self.accepts_file_attachments.store(
                    runtime_accepts_attachments(&self.running.frontend),
                    Ordering::Relaxed,
                );
                if let Err(rollback) = self.reconcile_replacement_startup().await {
                    return Err(internal(mobius::Error::Rollback {
                        primary: Box::new(mobius::Error::Config(primary.to_string())),
                        rollback: Box::new(mobius::Error::Config(rollback.to_string())),
                    }));
                }
                return Err(internal(primary));
            }
        };
        let previous = std::mem::replace(&mut self.running, replacement);
        self.accepts_file_attachments.store(
            runtime_accepts_attachments(&self.running.frontend),
            Ordering::Relaxed,
        );
        self.spec = next;
        drop(previous);
        self.reconcile_replacement_startup()
            .await
            .map_err(internal)?;
        self.broadcast_changed().await?;
        Ok(())
    }

    pub(super) async fn refresh_provider(
        &mut self,
        scope: &ProviderRefresh,
    ) -> std::result::Result<(), Rejection> {
        if !provider_refresh_matches(&self.spec.agent.config.provider, scope)
            .map_err(invalid_config)?
        {
            return Ok(());
        }
        self.refresh_runtime().await
    }

    pub(super) async fn refresh_extension(
        &mut self,
        id: &str,
    ) -> std::result::Result<(), Rejection> {
        if !self.spec.agent.config.extensions.contains(id) {
            return Ok(());
        }
        self.refresh_runtime().await
    }

    async fn refresh_runtime(&mut self) -> std::result::Result<(), Rejection> {
        if self.pending_turns > 0 || self.approval_active {
            self.restart_after_turn = true;
            return Ok(());
        }
        self.restart("mobius-gateway").await?;
        self.broadcast_changed().await
    }

    pub(super) async fn restart(
        &mut self,
        origin_label: &str,
    ) -> std::result::Result<(), Rejection> {
        let session_id = self.running.session_id.clone();
        self.stop_and_drain_running().await.map_err(internal)?;
        let replacement = start_agent(
            Arc::clone(&self.gateway),
            &self.spec,
            &self.store,
            Arc::clone(&self.credentials),
            Arc::clone(&self.checkpoints),
            self.scratchpad.clone(),
            self.session_files.clone(),
            Arc::clone(&self.swarm),
            session_id,
            origin_label,
            false,
            None,
            Arc::clone(&self.provider_epoch),
        )
        .await
        .map_err(internal)?;
        let previous = std::mem::replace(&mut self.running, replacement);
        self.accepts_file_attachments.store(
            runtime_accepts_attachments(&self.running.frontend),
            Ordering::Relaxed,
        );
        self.widgets.clear();
        drop(previous);
        self.reconcile_replacement_startup()
            .await
            .map_err(internal)?;
        self.pending_turns = 0;
        self.pending_messages.clear();
        self.approval_active = false;
        self.turn_error = None;
        Ok(())
    }

    pub(super) async fn stop_and_drain_running(&mut self) -> Result<()> {
        drop(self.running.sender.take());
        while let Some(record) = self.running.events.recv().await {
            self.apply_event(record).await?;
        }
        self.running.subagent_template.take();
        Ok(())
    }
}

pub(in crate::host) fn fail_queued_routine_commands(
    commands: &mut mpsc::Receiver<HostCommand>,
    bots: &BotStore,
) -> Option<Rejection> {
    let mut first_error = None;
    while let Ok(command) = commands.try_recv() {
        let HostCommand::RunRoutine { run, reply, .. } = command else {
            continue;
        };
        let rejection = match bots.finish_run(
            run,
            RoutineRunStatus::Failed,
            Some("the agent stopped before the Bot routine began".into()),
        ) {
            Ok(_) => stopped(),
            Err(error) => {
                let rejection = internal(error);
                first_error.get_or_insert_with(|| rejection.clone());
                rejection
            }
        };
        let _ = reply.send(Err(rejection));
    }
    first_error
}
