use super::{
    ActiveRoutine, ActiveRoutineRun, Arc, BotStore, ChatSpec, Error, EventPageRequest, HostCommand,
    HostSnapshot, HostState, JournalDelivery, MessageAuthor, MessageSubmission, Op, Ordering, Path,
    PathBuf, ProviderCutoverStatus, RealtimeModel, Rejection, Result, ReviewDecision,
    RoutineRunStatus, RunningAgent, ServerFrame, ServerMessage, SessionHistoryPage, SharedFrame,
    SourceAdmission, Submission, Uuid, ValidatedSubmission, delete_workspace_file,
    durable_history_page, event_sequence, event_turn_page, fail_active_routine, internal,
    invalid_config, invalid_workspace, list_workspace_files, mpsc, oneshot, project_record,
    read_workspace_file, replayable, shutdown_agent, start_agent, stopped,
    try_begin_session_mutation, write_workspace_file,
};
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
                    respond(admission.reply, admission.result.map(|_| ()).map_err(source_rejection), "source admission");
                    if self.is_idle() {
                        self.running.gateway_sandbox.release_desktop_use().await;
                        for waiter in self.idle_waiters.drain(..) {
                            respond(waiter, (), "idle waiter");
                        }
                    }
                }
                command = self.commands.recv() => {
                    let Some(command) = command else { break };
                    if !Box::pin(self.handle(command)).await { break; }
                }
                event = self.running.events.recv() => if let Some(event) = event {
                    if let Err(error) = self.apply_event(event).await {
                        let message: Arc<str> = error.to_string().into();
                        self.broadcast(ServerMessage::Error {
                            code: "host_error".into(),
                            message: Arc::clone(&message),
                            fatal: true,
                        });
                        if let Err(activity_error) = self.fail_activity(&message).await {
                            self.broadcast(ServerMessage::Error {
                                code: "session_activity".into(),
                                message: activity_error.to_string().into(),
                                fatal: false,
                            });
                        }
                        break;
                    }
                } else {
                    self.broadcast(ServerMessage::Error {
                        code: "agent_stopped".into(),
                        message: "the agent stopped".into(),
                        fatal: true,
                    });
                    if let Err(error) = self.fail_activity("the agent stopped").await {
                        self.broadcast(ServerMessage::Error {
                            code: "session_activity".into(),
                            message: error.to_string().into(),
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
                message: error.to_string().into(),
                fatal: false,
            });
        }
        for waiter in self.idle_waiters.drain(..) {
            respond(waiter, (), "idle waiter");
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
                respond(reply, result, "session command");
            }
            #[cfg(test)]
            HostCommand::BotId { reply } => {
                respond(reply, Arc::clone(&self.spec.bot_id), "bot id");
            }
            HostCommand::AcceptsFileAttachments { reply } => {
                let result = async {
                    let _mutation = self.begin_session_mutation()?;
                    self.bind_bot().await?;
                    Ok(self.running.prepared.bot.accepts_file_attachments)
                }
                .await;
                respond(reply, result, "session command");
            }
            HostCommand::RealtimeModel { reply } => {
                let result = async {
                    let _mutation = self.begin_session_mutation()?;
                    self.bind_bot().await?;
                    let bot = self.bots.bot(&self.spec.bot_id).map_err(internal)?;
                    let router = &self.running.model_router;
                    let middleware = &self.running.prepared.bot.config.config.middleware;
                    if !middleware.enabled(mobius::middleware::voice::MANIFEST.id) {
                        return Err(Rejection::new(
                            "realtime_voice",
                            "voice is turned off for this Bot",
                        ));
                    }
                    let selected = crate::middleware_manifest::string_setting(
                        middleware,
                        mobius::middleware::voice::MANIFEST.id,
                        "model",
                    )
                    .map_err(internal)?;
                    let voice = router.voice_choice(selected).map_err(|error| {
                        Rejection::new(
                            "realtime_voice",
                            format!("{error}; select an available voice"),
                        )
                    })?;
                    let active_turn_id = self.activity().await.map_err(internal)?.turn_id;
                    let voice = Arc::clone(&voice.route);
                    Ok(RealtimeModel {
                        route: router.default_provider().into(),
                        call: mobius::middleware::voice::VoiceCallContext {
                            session_id: Arc::clone(&self.running.session_id),
                            router: Arc::clone(router),
                            voice,
                            bot_name: bot.name,
                            bot_handle: bot.handle,
                            bot_instructions: self.running.prepared.instructions(),
                            active_turn_id,
                            checkpoints: Arc::clone(&self.checkpoints),
                            frontend: Arc::clone(&self.running.frontend_sink),
                        },
                    })
                }
                .await;
                respond(reply, result, "session command");
            }
            HostCommand::Snapshot {
                last_sequence,
                reply,
            } => {
                respond(reply, self.snapshot_value(last_sequence).await, "snapshot");
            }
            HostCommand::Ready { reply } => {
                respond(reply, self.ready().await.map_err(internal), "ready");
            }
            HostCommand::HistoryPage {
                before_sequence,
                reply,
            } => {
                respond(
                    reply,
                    self.history_page_value(before_sequence).await,
                    "history page",
                );
            }
            HostCommand::Submit { submission, reply } => {
                let result = async {
                    let _mutation = self.begin_session_mutation()?;
                    if matches!(
                        submission.submission().op,
                        Op::Message { .. } | Op::CapabilityCommand { .. }
                    ) {
                        self.bind_bot().await?;
                    }
                    let resumes_approval = matches!(
                        &submission.submission().op,
                        Op::ExecApproval {
                            decision: ReviewDecision::Approved
                                | ReviewDecision::ApprovedForSession
                                | ReviewDecision::Denied { .. },
                            ..
                        }
                    );
                    let result = match &submission.submission().op {
                        Op::SetModel { .. } => Err(Rejection::new(
                            "bot_configuration_required",
                            "change the model on this chat's Bot profile",
                        )),
                        _ => self.submit_validated(submission),
                    };
                    if result.is_ok()
                        && resumes_approval
                        && let Err(error) = self.resume_activity().await
                    {
                        self.broadcast(ServerMessage::Error {
                            code: "session_activity".into(),
                            message: error.to_string().into(),
                            fatal: false,
                        });
                    }
                    result
                }
                .await;
                respond(reply, result, "session command");
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
                    let mut next = self.spec.clone(); // rules-allow: the current spec stays until the replacement starts
                    let previous_bot_id = std::mem::replace(&mut next.bot_id, bot_id);
                    self.replace_running(Some(next), None).await?;
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
                respond(reply, result, "session command");
            }
            HostCommand::AttachFolder { folder, reply } => {
                let result = self.attach_folder(folder).await;
                respond(reply, result, "session command");
            }
            HostCommand::SetMessagePinned {
                target,
                pinned,
                reply,
            } => {
                let result = self.set_message_pinned(target, pinned).await;
                respond(reply, result, "session command");
            }
            HostCommand::GitWorkspace { reply } => {
                let result = self.project().map(|workspace| {
                    (
                        Arc::clone(&self.running.gateway_sandbox),
                        workspace.to_owned(),
                    )
                });
                respond(reply, result, "session command");
            }
            HostCommand::WorkspaceFiles { scope, reply } => {
                let result = async {
                    let workspace = self.project()?;
                    list_workspace_files(&self.running.gateway_sandbox, workspace, scope).await
                }
                .await;
                respond(reply, result, "session command");
            }
            HostCommand::ReadWorkspaceFile {
                path,
                offset,
                max_bytes,
                reply,
            } => {
                let result = read_workspace_file(
                    &self.running.gateway_sandbox,
                    self.spec.workspace.as_deref(),
                    &path,
                    offset,
                    max_bytes,
                )
                .await;
                respond(reply, result, "session command");
            }
            HostCommand::DeleteWorkspaceFile { path, reply } => {
                let result = async {
                    self.require_idle()?;
                    let _mutation = self.begin_session_mutation()?;
                    self.project()?;
                    delete_workspace_file(&self.running.gateway_sandbox, &path).await
                }
                .await;
                respond(reply, result, "session command");
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
                respond(reply, result, "session command");
            }
            HostCommand::SwitchGitBranch { branch, reply } => {
                let result = async {
                    let _mutation = self.begin_session_mutation()?;
                    self.project()?;
                    self.switch_git_branch(&branch).await
                }
                .await;
                respond(reply, result, "session command");
            }
            HostCommand::ProviderCutoverStatus { reply } => {
                let status = ProviderCutoverStatus {
                    idle: self.is_idle(),
                };
                respond(reply, status, "provider cutover status");
            }
            HostCommand::RunRoutine { run, input, reply } => {
                let result = match self.begin_session_mutation() {
                    Ok(_mutation) => match self.bind_bot().await {
                        Ok(()) => self.run_routine(run, input),
                        Err(rejection) => {
                            let result = self.bots.finish_run(
                                run,
                                RoutineRunStatus::Failed,
                                Some(Arc::clone(&rejection.message)),
                            );
                            result.map_err(internal).and(Err(rejection))
                        }
                    },
                    Err(rejection) => match self.bots.finish_run(
                        run,
                        RoutineRunStatus::Failed,
                        Some(Arc::clone(&rejection.message)),
                    ) {
                        Ok(_) => Err(rejection),
                        Err(error) => Err(internal(error)),
                    },
                };
                respond(reply, result, "session command");
            }
            HostCommand::WaitIdle { reply } => {
                if self.is_idle() {
                    respond(reply, (), "idle waiter");
                } else {
                    self.idle_waiters.push(reply);
                }
            }
            HostCommand::StopIfIdle { reply } => {
                if !self.is_idle() {
                    respond(reply, false, "stop-if-idle");
                    return true;
                }
                self.alive.store(false, Ordering::Release);
                respond(reply, true, "stop-if-idle");
                return false;
            }
            HostCommand::Shutdown => return false,
        }
        true
    }

    async fn accept_source(
        &mut self,
        submission: ValidatedSubmission,
        bot_id: Arc<str>,
        reply: oneshot::Sender<std::result::Result<(), Rejection>>,
    ) {
        if !matches!(submission.submission().op, Op::Message { .. }) {
            let result = async {
                let _mutation = self.begin_session_mutation()?;
                self.bind_bot().await?;
                if *self.spec.bot_id != *bot_id {
                    return Err(invalid_config("the chat now belongs to another Bot"));
                }
                if !matches!(submission.submission().op, Op::Interrupt { .. }) {
                    return Err(invalid_config(
                        "source commands support only message and interrupt",
                    ));
                }
                self.submit_validated(submission)
            }
            .await;
            respond(reply, result, "source command");
            return;
        }
        let result = async {
            let submission = if self.running.session_id.as_ref()
                == crate::bots::conversation_session_id(&self.spec.bot_id)
            {
                submission.with_queued_delivery()
            } else {
                submission
            };
            let _mutation = self.begin_session_mutation()?;
            self.bind_bot().await?;
            if *self.spec.bot_id != *bot_id {
                return Err(Rejection::new(
                    "invalid_submission",
                    "the chat now belongs to another Bot",
                ));
            }
            let id = Arc::clone(&submission.submission().id);
            self.running
                .sender
                .as_ref()
                .ok_or_else(stopped)?
                .send_validated_with_admission(submission)
                .map(|admission| (id, admission))
                .map_err(internal)
        }
        .await;
        match result {
            Ok((id, admission)) => {
                let reserved = self.pending_messages.insert(Arc::clone(&id));
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
                respond(reply, Err(error), "source delivery");
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
                    session_id: Arc::clone(&self.running.session_id),
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
        durable_history_page(
            self.checkpoints.as_ref(),
            &self.running.session_id,
            &self.running.frontend,
            before_sequence,
        )
        .await
    }

    pub(super) fn replay_after(
        &self,
        last_sequence: Option<u64>,
    ) -> std::result::Result<Vec<SharedFrame>, Rejection> {
        let Some(last_sequence) = last_sequence else {
            return Ok(self
                .replay
                .iter()
                .map(|entry| entry.frame.clone()) // rules-allow: the replay buffer keeps its shared frames
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
            .map(|entry| entry.frame.clone()) // rules-allow: the replay buffer keeps its shared frames
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
        let submission_id: Arc<str> = Uuid::new_v4().to_string().into();
        let submission = Submission {
            id: Arc::clone(&submission_id),
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
            self.bots
                .finish_run(
                    run,
                    RoutineRunStatus::Failed,
                    Some(Arc::clone(&rejection.message)),
                )
                .map_err(internal)?;
            return Err(rejection);
        }
        self.active_routine = Some(ActiveRoutine {
            run,
            submission_id,
            turn_id: None,
            failure: None,
        });
        Ok(())
    }

    pub(super) fn submit(&mut self, submission: Submission) -> std::result::Result<(), Rejection> {
        self.submit_validated(
            ValidatedSubmission::new(submission)
                .map_err(|error| Rejection::new("invalid_submission", error.to_string()))?,
        )
    }

    fn submit_validated(
        &mut self,
        submission: ValidatedSubmission,
    ) -> std::result::Result<(), Rejection> {
        let message_submission_id = matches!(submission.submission().op, Op::Message { .. })
            .then(|| Arc::clone(&submission.submission().id));
        let resolves_approval = matches!(submission.submission().op, Op::ExecApproval { .. });
        self.running
            .sender
            .as_ref()
            .ok_or_else(stopped)?
            .send_validated(submission)
            .map_err(|error| Rejection {
                code: match error {
                    mobius::Error::Busy(_) => "agent_busy",
                    mobius::Error::Stopped(_) => "agent_stopped",
                    _ => "invalid_submission",
                },
                message: error.to_string().into(),
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
            self.replace_running(None, None).await?;
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
        Ok(!self
            .running
            .frontend
            .has_background_work()
            .await
            .map_err(internal)?)
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
        let next = {
            let gateway = crate::config::lock(&self.gateway).map_err(internal)?;
            self.spec
                .with_attached_folder(&folder, self.store.state_dir(), gateway.tls.as_ref())
        };
        let Some(next) = next.map_err(invalid_workspace)? else {
            return Ok(());
        };
        let prepared = Arc::clone(&self.running.prepared);
        self.replace_running(Some(next), Some(prepared)).await
    }

    async fn start_replacement(
        &self,
        spec: &ChatSpec,
        label: &str,
        prepared: Option<Arc<crate::assembly::PreparedBot>>,
    ) -> Result<RunningAgent> {
        start_agent(
            Arc::clone(&self.gateway),
            spec,
            &self.store,
            Arc::clone(&self.credentials),
            Arc::clone(&self.bots),
            Arc::clone(&self.checkpoints),
            Arc::clone(&self.scratchpad),
            Arc::clone(&self.session_files),
            Arc::clone(&self.discovery_gate),
            Arc::clone(&self.desktop),
            Arc::clone(&self.remote_desktop),
            &self.running.session_id,
            label,
            prepared,
            Arc::clone(&self.provider_epoch),
            Arc::clone(&self.live_chats),
            Arc::clone(&self.host_access),
        )
        .await
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the replacement or recovery agent moves into the running slot"
    )]
    async fn replace_running(
        &mut self,
        next: Option<ChatSpec>,
        prepared: Option<Arc<crate::assembly::PreparedBot>>,
    ) -> std::result::Result<(), Rejection> {
        // Keep replacement and rollback futures off every command handler's stack.
        Box::pin(async {
            // Failed replacement restarts from the old prepared bot after draining the running agent.
            let old_prepared = Arc::clone(&self.running.prepared);
            self.stop_and_drain_running().await.map_err(internal)?;
            let spec = next.as_ref().unwrap_or(&self.spec);
            let replacement = match self
                .start_replacement(spec, "mobius-gateway", prepared)
                .await
            {
                Ok(replacement) => replacement,
                Err(primary) => {
                    let recovery = self
                        .start_replacement(
                            &self.spec,
                            "mobius-gateway-rollback",
                            Some(old_prepared),
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
            if let Some(next) = next {
                self.spec = next;
            }
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
                let rejection = internal(&error);
                first_error.get_or_insert(error);
                rejection
            }
        };
        respond(reply, Err(rejection), "routine");
    }
    first_error.map(internal)
}

fn source_rejection(error: mobius::Error) -> Rejection {
    let code = match &error {
        mobius::Error::Busy(_) => "agent_busy",
        mobius::Error::Stopped(_) => "agent_stopped",
        mobius::Error::Checkpoint(_) | mobius::Error::Sqlite(_) => "gateway_error",
        _ => "invalid_submission",
    };
    let message = match error {
        mobius::Error::Config(message) => message.into(),
        error => error.to_string().into(),
    };
    Rejection {
        code,
        message,
        fatal: false,
    }
}

fn respond<T>(reply: oneshot::Sender<T>, value: T, waiter: &str) {
    if reply.send(value).is_err() {
        tracing::debug!(waiter, "requester stopped waiting");
    }
}
