use super::*;
use futures_util::StreamExt as _;

impl HostState {
    fn project(&self) -> std::result::Result<&Path, Rejection> {
        self.spec.workspace.as_deref().ok_or_else(|| {
            Rejection::new("no_workspace", "Persistent Chat has no project workspace")
        })
    }

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
            tokio::select! {
                Some(admission) = self.admissions.next(), if !self.admissions.is_empty() => {
                    if !matches!(admission.result, Ok(mobius::agent::MessageAcceptance::Accepted))
                        && admission.reserved && self.pending_messages.remove(&admission.id) {
                        self.pending_turns = self.pending_turns.saturating_sub(1);
                    }
                    let _ = admission.reply.send(admission.result.map(|_| ()).map_err(source_rejection));
                    if self.is_idle() {
                        self.running.gateway_sandbox.release_desktop_use().await;
                        for waiter in self.idle_waiters.drain(..) { let _ = waiter.send(()); }
                    }
                }
                command = self.commands.recv() => {
                    let Some(command) = command else { break };
                    if !Box::pin(self.handle(command)).await { break; }
                }
                event = self.running.events.recv() => match event {
                    Some(event) => {
                        if let Err(error) = self.apply_event(event).await {
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
                    None => {
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
        }
        self.commands.close();
        if let Some(rejection) = fail_queued_routine_commands(&mut self.commands, &self.bots) {
            self.broadcast(ServerMessage::Error {
                code: "routine_state_error".into(),
                message: rejection.message,
                fatal: false,
            });
        }
        if self.approval_active {
            drop(self.active_routine.take());
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
        for waiter in self.idle_waiters.drain(..) {
            let _ = waiter.send(());
        }
        shutdown_agent(self.running).await;
        self.alive.store(false, Ordering::Release);
        self.terminated.store(true, Ordering::Release);
        self.termination.notify_waiters();
    }

    pub(super) async fn handle(&mut self, command: HostCommand) -> bool {
        match command {
            HostCommand::RuntimeIsIdle { reply } => {
                let result = if self.is_idle() {
                    self.runtime_is_idle().await
                } else {
                    Ok(false)
                };
                let _ = reply.send(result);
            }
            #[cfg(test)]
            HostCommand::BotId { reply } => {
                let _ = reply.send(self.spec.bot_id.clone());
            }
            HostCommand::AcceptsFileAttachments { reply } => {
                let result = async {
                    let _mutation = self.begin_session_mutation()?;
                    self.bind_bot().await?;
                    Ok(runtime_accepts_attachments(&self.running.frontend))
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::RealtimeModel { reply } => {
                let result = async {
                    let _mutation = self.begin_session_mutation()?;
                    self.bind_bot().await?;
                    let bot = self.bots.bot(&self.spec.bot_id).map_err(internal)?;
                    let router = &self.running.model_router;
                    let route = &self.running.session.model.route;
                    if !router.supports_realtime_voice(route).map_err(internal)? {
                        return Err(Rejection::new(
                            "realtime_voice",
                            "the selected provider does not support realtime voice",
                        ));
                    }
                    let active_turn_id = self.activity().await.map_err(internal)?.turn_id;
                    let config = self
                        .gateway
                        .lock()
                        .map_err(|_| internal("gateway configuration lock is poisoned"))?;
                    let provider_instance = crate::provider_catalog::configured_model_providers(
                        &config,
                        &self.store,
                        &self.credentials,
                    )
                    .map_err(internal)?
                    .remove(route)
                    .ok_or_else(|| internal("voice route is no longer configured"))?;
                    Ok(RealtimeModel {
                        bot_instructions: format!(
                            "Your name is {} (@{}).\n\n{}",
                            bot.name,
                            bot.handle,
                            self.running.prepared.instructions()
                        ),
                        bot_name: bot.name.clone(),
                        router: Arc::clone(router),
                        voice: self
                            .running
                            .prepared
                            .bot
                            .config
                            .config
                            .realtime_voice
                            .clone(),
                        route: route.clone(),
                        provider_instance,
                        active_turn_id,
                        checkpoints: Arc::clone(&self.checkpoints),
                        frontend: Arc::clone(&self.running.frontend_sink),
                    })
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::ObserveVoiceUsage {
                provider_instance,
                usage,
                reply,
            } => {
                let result = crate::assembly::persist_usage(
                    &self.gateway,
                    &self.store,
                    &provider_instance,
                    &usage,
                )
                .map_err(internal);
                let _ = reply.send(result);
            }
            HostCommand::Snapshot {
                last_sequence,
                reply,
            } => {
                let _ = reply.send(self.snapshot_value(last_sequence).await);
            }
            HostCommand::Ready { reply } => {
                let _ = reply.send(self.ready().await.map_err(internal));
            }
            HostCommand::HistoryPage {
                before_sequence,
                reply,
            } => {
                let _ = reply.send(self.history_page_value(before_sequence).await);
            }
            HostCommand::Submit { submission, reply } => {
                let result = async {
                    let _mutation = self.begin_session_mutation()?;
                    if matches!(
                        submission.op,
                        Op::Message { .. } | Op::CapabilityCommand { .. }
                    ) {
                        self.bind_bot().await?;
                    }
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
                        Op::SetModel { .. } => Err(Rejection::new(
                            "bot_configuration_required",
                            "change the model on this chat's Bot profile",
                        )),
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
            HostCommand::DeliverSource {
                submission,
                bot_id,
                reply,
            } => self.accept_source(submission, bot_id, reply).await,
            HostCommand::ReassignBot { bot_id, reply } => {
                let result = async {
                    let _mutation = self.begin_session_mutation()?;
                    self.require_idle_runtime().await?;
                    if self.spec.bot_id == bot_id {
                        return Ok(());
                    }
                    let mut next = self.spec.clone();
                    let previous_bot_id = next.bot_id.clone();
                    next.bot_id = bot_id;
                    self.replace_running(next, None).await?;
                    super::super::bot_events::close_reassigned_source(
                        &self.checkpoints,
                        &self.bots,
                        &self.running.session_id,
                        &previous_bot_id,
                    )
                    .await
                    .map_err(internal)
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::AttachFolder { folder, reply } => {
                let result = self.attach_folder(folder).await;
                let _ = reply.send(result);
            }
            HostCommand::GitWorkspace { reply } => {
                let result = self.project().map(|workspace| {
                    (
                        Arc::clone(&self.running.gateway_sandbox),
                        workspace.to_owned(),
                    )
                });
                let _ = reply.send(result);
            }
            HostCommand::WorkspaceFiles { scope, reply } => {
                let result = async {
                    let workspace = self.project()?;
                    list_workspace_files(&self.running.gateway_sandbox, workspace, scope).await
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::ReadWorkspaceFile {
                path,
                offset,
                max_bytes,
                reply,
            } => {
                let result = async {
                    self.project()?;
                    read_workspace_file(&self.running.gateway_sandbox, &path, offset, max_bytes)
                        .await
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::DeleteWorkspaceFile { path, reply } => {
                let result = async {
                    self.require_idle()?;
                    let _mutation = self.begin_session_mutation()?;
                    self.project()?;
                    delete_workspace_file(&self.running.gateway_sandbox, &path).await
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::WriteWorkspaceFile {
                path,
                content,
                reply,
            } => {
                let result = async {
                    self.require_idle()?;
                    let _mutation = self.begin_session_mutation()?;
                    self.project()?;
                    write_workspace_file(&self.running.gateway_sandbox, &path, &content).await
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::SwitchGitBranch { branch, reply } => {
                let result = async {
                    let _mutation = self.begin_session_mutation()?;
                    self.project()?;
                    self.switch_git_branch(&branch).await
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::ProviderCutoverStatus { reply } => {
                let _ = reply.send(ProviderCutoverStatus {
                    idle: self.is_idle(),
                });
            }
            HostCommand::RunRoutine { run, input, reply } => {
                let result = match self.begin_session_mutation() {
                    Ok(_mutation) => match self.bind_bot().await {
                        Ok(()) => self.run_routine(run, input),
                        Err(rejection) => {
                            let result = self.bots.finish_run(
                                run,
                                RoutineRunStatus::Failed,
                                Some(rejection.message.clone()),
                            );
                            result.map_err(internal).and(Err(rejection))
                        }
                    },
                    Err(rejection) => match self.bots.finish_run(
                        run,
                        RoutineRunStatus::Failed,
                        Some(rejection.message.clone()),
                    ) {
                        Ok(_) => Err(rejection),
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
            HostCommand::StopIfIdle { reply } => {
                if !self.is_idle() {
                    let _ = reply.send(false);
                    return true;
                }
                self.alive.store(false, Ordering::Release);
                let _ = reply.send(true);
                return false;
            }
            HostCommand::Shutdown => return false,
        }
        true
    }

    async fn accept_source(
        &mut self,
        mut submission: Submission,
        bot_id: String,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    ) {
        if !matches!(submission.op, Op::Message { .. }) {
            let result = async {
                let _mutation = self.begin_session_mutation()?;
                self.bind_bot().await?;
                if self.spec.bot_id != bot_id {
                    return Err(invalid_config("the chat now belongs to another Bot"));
                }
                if !matches!(submission.op, Op::Interrupt { .. }) {
                    return Err(invalid_config(
                        "source commands support only message and interrupt",
                    ));
                }
                self.submit(submission)
            }
            .await;
            let _ = reply.send(result);
            return;
        }
        if self.running.session_id == crate::bots::conversation_session_id(&self.spec.bot_id)
            && let Op::Message { message } = &mut submission.op
        {
            message.requested_delivery = Some(mobius::protocol::ActiveMessageDelivery::Queue);
        }
        let result = async {
            let _mutation = self.begin_session_mutation()?;
            self.bind_bot().await?;
            if self.spec.bot_id != bot_id {
                return Err(Rejection::new(
                    "invalid_submission",
                    "the chat now belongs to another Bot",
                ));
            }
            self.running
                .sender
                .as_ref()
                .ok_or_else(stopped)?
                .send_with_admission(submission.clone())
                .map_err(internal)
        }
        .await;
        match result {
            Ok(admission) => {
                let id = submission.id;
                let reserved = self.pending_messages.insert(id.clone());
                self.pending_turns += usize::from(reserved);
                self.work_activity.mark();
                self.admissions.push(Box::pin(async move {
                    SourceAdmission {
                        id,
                        reserved,
                        result: admission.wait().await,
                        reply,
                    }
                }));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
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
                    replay.push(SharedFrame::encoded(frame).map_err(internal)?);
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
    ) -> std::result::Result<Vec<SharedFrame>, Rejection> {
        let Some(last_sequence) = last_sequence else {
            return Ok(self
                .replay
                .iter()
                .map(|entry| entry.frame.clone())
                .collect());
        };
        if last_sequence > self.sequence {
            return Err(Rejection::new(
                "replay_unavailable",
                "the reconnect cursor is ahead of the durable session",
            ));
        }
        let oldest = self
            .replay
            .front()
            .and_then(|entry| event_sequence(&entry.frame));
        if last_sequence < self.sequence
            && oldest.is_none_or(|oldest| last_sequence.saturating_add(1) < oldest)
        {
            return Err(Rejection::new(
                "replay_unavailable",
                "the reconnect window expired; reload the active session",
            ));
        }
        Ok(self
            .replay
            .iter()
            .filter(|entry| {
                event_sequence(&entry.frame).is_some_and(|sequence| sequence > last_sequence)
            })
            .map(|entry| entry.frame.clone())
            .collect())
    }

    pub(super) fn run_routine(
        &mut self,
        run: ActiveRoutineRun,
        input: String,
    ) -> std::result::Result<(), Rejection> {
        if let Err(rejection) = self.require_idle() {
            self.bots
                .finish_run(
                    run,
                    RoutineRunStatus::Failed,
                    Some("the agent was busy when this invocation became due".into()),
                )
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
                    reply: None,
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
            self.bots
                .finish_run(
                    active.run,
                    RoutineRunStatus::Failed,
                    Some(rejection.message.clone()),
                )
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
        self.work_activity.mark();
        if let Some(submission_id) = message_submission_id {
            self.pending_turns += 1;
            self.pending_messages.insert(submission_id);
        }
        if resolves_approval {
            self.approval_active = false;
        }
        Ok(())
    }

    async fn bind_bot(&mut self) -> std::result::Result<(), Rejection> {
        if !self.is_idle() {
            return Ok(());
        }
        let configuration_changed = !self
            .running
            .prepared
            .matches_runtime(&self.bots.bot(&self.spec.bot_id).map_err(internal)?)
            || self.running.prepared.epoch != self.provider_epoch.load(Ordering::Acquire);
        if !configuration_changed {
            return Ok(());
        }
        if self.runtime_is_idle().await? {
            self.replace_running(self.spec.clone(), None).await?;
        }
        Ok(())
    }

    async fn runtime_is_idle(&self) -> std::result::Result<bool, Rejection> {
        if let Some(checkpoint) = self
            .checkpoints
            .load(&self.running.session_id)
            .await
            .map_err(internal)?
            && (checkpoint.active_execution.is_some()
                || !checkpoint.pending_messages.is_empty()
                || checkpoint.pending_approval.is_some())
        {
            return Ok(false);
        }
        if self
            .running
            .sandbox
            .has_background_commands(&self.running.session_id)
            .map_err(internal)?
        {
            return Ok(false);
        }
        if let Some(subagents) = &self.running.subagents
            && subagents
                .has_active_children(&self.running.session_id)
                .await
                .map_err(internal)?
        {
            return Ok(false);
        }
        Ok(true)
    }

    async fn require_idle_runtime(&self) -> std::result::Result<(), Rejection> {
        self.require_idle()?;
        if !self.runtime_is_idle().await? {
            return Err(Rejection::new(
                "agent_busy",
                "wait for background work before changing this chat",
            ));
        }
        Ok(())
    }

    async fn attach_folder(&mut self, folder: PathBuf) -> std::result::Result<(), Rejection> {
        self.require_idle_runtime().await?;
        let _mutation = self.begin_session_mutation()?;
        let tls = self
            .gateway
            .lock()
            .map_err(|_| internal("gateway configuration lock is poisoned"))?
            .tls
            .clone();
        let Some(next) = self
            .spec
            .with_attached_folder(&folder, self.store.state_dir(), tls.as_ref())
            .map_err(invalid_workspace)?
        else {
            return Ok(());
        };
        self.replace_running(next, Some(Arc::clone(&self.running.prepared)))
            .await
    }

    async fn replace_running(
        &mut self,
        next: ChatSpec,
        prepared: Option<Arc<crate::assembly::PreparedBot>>,
    ) -> std::result::Result<(), Rejection> {
        // Keep replacement and rollback futures off every command handler's stack.
        Box::pin(async {
            let session_id = self.running.session_id.clone();
            let old_spec = self.spec.clone();
            let old_prepared = Arc::clone(&self.running.prepared);
            self.stop_and_drain_running().await.map_err(internal)?;
            let replacement = match start_agent(
                Arc::clone(&self.gateway),
                &next,
                &self.store,
                Arc::clone(&self.credentials),
                Arc::clone(&self.bots),
                Arc::clone(&self.checkpoints),
                self.scratchpad.clone(),
                self.session_files.clone(),
                Arc::clone(&self.discovery_gate),
                Arc::clone(&self.desktop),
                Arc::clone(&self.remote_desktop),
                session_id,
                "mobius-gateway",
                prepared,
                Arc::clone(&self.provider_epoch),
                Arc::clone(&self.live_chats),
                Arc::clone(&self.host_access),
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
                        Arc::clone(&self.bots),
                        Arc::clone(&self.checkpoints),
                        self.scratchpad.clone(),
                        self.session_files.clone(),
                        Arc::clone(&self.discovery_gate),
                        Arc::clone(&self.desktop),
                        Arc::clone(&self.remote_desktop),
                        self.running.session_id.clone(),
                        "mobius-gateway-rollback",
                        Some(old_prepared),
                        Arc::clone(&self.provider_epoch),
                        Arc::clone(&self.live_chats),
                        Arc::clone(&self.host_access),
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
            self.spec = next;
            drop(previous);
            self.reconcile_replacement_startup()
                .await
                .map_err(internal)?;
            self.broadcast_changed().await?;
            Ok(())
        })
        .await
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

fn source_rejection(error: mobius::Error) -> Rejection {
    let code = match &error {
        mobius::Error::Busy(_) => "agent_busy",
        mobius::Error::Stopped(_) => "agent_stopped",
        mobius::Error::Checkpoint(_) | mobius::Error::Sqlite(_) => "gateway_error",
        _ => "invalid_submission",
    };
    let message = match error {
        mobius::Error::Config(message) => message,
        error => error.to_string(),
    };
    Rejection {
        code,
        message,
        fatal: false,
    }
}
