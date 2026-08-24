use super::*;

impl HostState {
    fn begin_session_mutation(
        &self,
    ) -> std::result::Result<tokio::sync::OwnedRwLockReadGuard<()>, Rejection> {
        Arc::clone(&self.session_mutations)
            .try_read_owned()
            .map_err(|_| Rejection {
                code: "gateway_busy",
                message: "retry after the gateway update finishes".into(),
                fatal: false,
            })
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
        if let Err(error) = fail_active_cron(
            &self.cron,
            &mut self.active_cron,
            "the agent stopped before the scheduled run completed",
        ) {
            self.broadcast(ServerMessage::Error {
                code: "cron_state_error".into(),
                message: error.to_string(),
                fatal: false,
            });
        }
        for waiter in self.idle_waiters.drain(..) {
            let _ = waiter.send(());
        }
        self.cron.cancel_setup(&self.running.session_id);
        shutdown_agent(self.running).await;
        self.alive.store(false, Ordering::Release);
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
                        Op::SetModel { route } => self.set_model(route).await,
                        _ => self.submit(submission, false),
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
            HostCommand::StartCronSetup { task, reply } => {
                let result = self
                    .begin_session_mutation()
                    .and_then(|_mutation| self.start_cron_setup(task));
                let _ = reply.send(result);
            }
            HostCommand::Configure {
                expected_revision,
                config,
                reply,
            } => {
                let result = async {
                    let _mutation = self.begin_session_mutation()?;
                    self.configure(expected_revision, config).await
                }
                .await;
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
                    selection: self.spec.agent.config.provider.clone(),
                    provider_epoch: self.running.provider_epoch,
                    idle: self.is_idle(),
                });
            }
            HostCommand::CutOverProvider { selection, reply } => {
                let result = self.cut_over_provider(&selection).await;
                let _ = reply.send(result);
            }
            HostCommand::ReloadProviderCatalog { reply } => {
                let result = self.reload_provider_catalog().await;
                let _ = reply.send(result);
            }
            HostCommand::RunCron { run, input, reply } => {
                let result = self
                    .begin_session_mutation()
                    .and_then(|_mutation| self.run_cron(run, input));
                let _ = reply.send(result);
            }
            HostCommand::WaitIdle { reply } => {
                if self.is_idle() {
                    let _ = reply.send(());
                } else {
                    self.idle_waiters.push(reply);
                }
            }
            HostCommand::StopIfIdle { reply } => {
                let idle = self.is_idle();
                let _ = reply.send(idle);
                return !idle;
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

    pub(super) fn run_cron(
        &mut self,
        run: ActiveCronRun,
        input: String,
    ) -> std::result::Result<(), Rejection> {
        if let Err(rejection) = self.require_idle() {
            self.cron
                .finish_run(
                    run,
                    CronRunStatus::Failed,
                    Some("the agent was busy when this invocation became due".into()),
                )
                .map_err(internal)?;
            return Err(rejection);
        }
        let submission_id = Uuid::new_v4().to_string();
        self.active_cron = Some(ActiveCron {
            run,
            submission_id: submission_id.clone(),
            turn_id: None,
            failure: None,
        });
        let submission = Submission {
            id: submission_id,
            op: Op::UserInput {
                text: input,
                attachments: Vec::new(),
            },
        };
        if let Err(rejection) = self.submit(submission, true) {
            let active = self.active_cron.take().expect("active cron was just set");
            self.cron
                .finish_run(
                    active.run,
                    CronRunStatus::Failed,
                    Some(rejection.message.clone()),
                )
                .map_err(internal)?;
            return Err(rejection);
        }
        Ok(())
    }

    pub(super) fn submit(
        &mut self,
        submission: Submission,
        scheduled: bool,
    ) -> std::result::Result<(), Rejection> {
        let starts_turn = matches!(submission.op, Op::UserInput { .. });
        let resolves_approval = matches!(submission.op, Op::ExecApproval { .. });
        if starts_turn && self.active_cron.is_some() && !scheduled {
            return Err(Rejection {
                code: "agent_busy",
                message: "wait for the scheduled run to finish".into(),
                fatal: false,
            });
        }
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
        self.pending_turns += usize::from(starts_turn);
        if resolves_approval {
            self.approval_active = false;
        }
        Ok(())
    }

    pub(super) fn start_cron_setup(
        &mut self,
        task: Option<String>,
    ) -> std::result::Result<(), Rejection> {
        self.require_idle()?;
        if !self.spec.agent.config.middleware.enabled("cron") {
            return Err(Rejection {
                code: "capability_disabled",
                message: "scheduling is disabled for this chat".into(),
                fatal: false,
            });
        }
        let input = self
            .cron
            .begin_setup(&self.running.session_id, task.as_deref())
            .map_err(invalid_cron)?;
        let submission = Submission {
            id: Uuid::new_v4().to_string(),
            op: Op::UserInput {
                text: input,
                attachments: Vec::new(),
            },
        };
        if let Err(rejection) = self.submit(submission, false) {
            self.cron.cancel_setup(&self.running.session_id);
            return Err(rejection);
        }
        Ok(())
    }

    pub(super) async fn configure(
        &mut self,
        expected_revision: u64,
        composition: AgentComposition,
    ) -> std::result::Result<(), Rejection> {
        self.require_idle()?;
        if expected_revision != self.spec.agent.revision {
            return Err(Rejection {
                code: "revision_conflict",
                message: format!("configuration revision is now {}", self.spec.agent.revision),
                fatal: false,
            });
        }
        let gateway = self
            .gateway
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .clone();
        let models =
            configured_model_choices(&gateway, &self.store, &self.credentials).map_err(internal)?;
        crate::middleware_manifest::validate_choices(&composition.middleware, &models)
            .map_err(invalid_config)?;
        let next = self
            .spec
            .replacing_agent(
                expected_revision,
                composition,
                &gateway,
                self.store.state_dir(),
                gateway.tls.as_ref(),
            )
            .map_err(invalid_config)?;
        let reusable_router = reusable_model_router(&self.spec, &next, &self.running);
        self.replace_running(next, reusable_router).await
    }

    async fn cut_over_provider(
        &mut self,
        selection: &ProviderConfig,
    ) -> std::result::Result<(), Rejection> {
        self.require_idle()?;
        let gateway = self
            .gateway
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .clone();
        let next = if self.spec.agent.config.provider.instance == selection.instance
            && self.spec.agent.config.provider != *selection
        {
            let mut composition = self.spec.agent.config.clone();
            composition.provider = selection.clone();
            self.spec
                .replacing_agent(
                    self.spec.agent.revision,
                    composition,
                    &gateway,
                    self.store.state_dir(),
                    gateway.tls.as_ref(),
                )
                .map_err(invalid_config)?
        } else {
            self.spec.clone()
        };
        let models =
            configured_model_choices(&gateway, &self.store, &self.credentials).map_err(internal)?;
        crate::middleware_manifest::validate_choices(&next.agent.config.middleware, &models)
            .map_err(invalid_config)?;
        self.replace_running(next, None).await
    }

    async fn reload_provider_catalog(&mut self) -> std::result::Result<(), Rejection> {
        self.require_idle()?;
        let gateway = self
            .gateway
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .clone();
        let next = self
            .spec
            .normalizing_provider_catalog(&gateway, self.store.state_dir(), gateway.tls.as_ref())
            .map_err(invalid_config)?;
        self.replace_running(next, None).await
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
            Arc::clone(&self.cron),
            Arc::clone(&self.cron_commands),
            Arc::clone(&self.checkpoints),
            self.scratchpad.clone(),
            self.session_files.clone(),
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
                    Arc::clone(&self.cron),
                    Arc::clone(&self.cron_commands),
                    Arc::clone(&self.checkpoints),
                    self.scratchpad.clone(),
                    self.session_files.clone(),
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

    pub(super) async fn set_model(&mut self, route: &str) -> std::result::Result<(), Rejection> {
        self.require_idle()?;
        let gateway = self
            .gateway
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .clone();
        let mut provider =
            configured_provider_for_route(&gateway, &self.store, &self.credentials, route)
                .map_err(invalid_config)?;
        if provider.instance == self.spec.agent.config.provider.instance {
            provider.base_url = self.spec.agent.config.provider.base_url.clone();
            provider.web_search = self.spec.agent.config.provider.web_search;
        }
        if self.running.session.model.route == route && self.spec.agent.config.provider == provider
        {
            return Ok(());
        }
        let mut composition = self.spec.agent.config.clone();
        composition.provider = provider;
        self.configure(self.spec.agent.revision, composition).await
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
            Arc::clone(&self.cron),
            Arc::clone(&self.cron_commands),
            Arc::clone(&self.checkpoints),
            self.scratchpad.clone(),
            self.session_files.clone(),
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
