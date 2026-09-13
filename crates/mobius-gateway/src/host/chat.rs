use super::*;
use crate::chats::Chat;
use crate::wire::{SessionReadyPayload, WorkspaceInfo};
use mobius::protocol::{ModelChangedEvent, SessionConfiguredEvent};

impl GatewayHost {
    pub(super) async fn reconcile_chat_cancellations(&self) -> std::result::Result<(), Rejection> {
        let (chats, checkpoints) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.chat_store),
                Arc::clone(&state.checkpoints),
            )
        };
        for chat in chats.chats(false).await.map_err(internal)? {
            for participant in &chat.participants {
                if let Some(checkpoint) = checkpoints
                    .load(&participant.session_id)
                    .await
                    .map_err(internal)?
                {
                    self.cancel_unadmitted_execution(&chat.id, &participant.bot_id, &checkpoint)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn cancel_unadmitted_execution(
        &self,
        chat_id: &str,
        bot_id: &str,
        checkpoint: &mobius::backend::checkpoint::Checkpoint,
    ) -> std::result::Result<(), Rejection> {
        if checkpoint.active_execution.is_none() && checkpoint.pending_messages.is_empty() {
            return Ok(());
        }
        let chats = Arc::clone(&self.state.lock().await.chat_store);
        let pending = chats.pending_deliveries(chat_id).await.map_err(internal)?;
        let admitted = pending
            .iter()
            .filter(|(id, _)| id == bot_id)
            .map(|(_, message)| message.id.as_str())
            .collect::<HashSet<_>>();
        let canceled = checkpoint
            .active_execution
            .as_ref()
            .is_some_and(|active| !admitted.contains(active.submission_id.as_str()))
            || checkpoint
                .pending_messages
                .iter()
                .any(|message| !admitted.contains(message.id()));
        if canceled {
            self.cancel_execution(&checkpoint.session_id).await?;
        }
        Ok(())
    }

    async fn inspect_execution(
        &self,
        chat: &Chat,
    ) -> std::result::Result<
        (
            SessionReadyPayload,
            FrontendExtensions,
            Arc<crate::assembly::PreparedBot>,
        ),
        Rejection,
    > {
        Box::pin(async {
            let [participant] = chat.participants.as_slice() else {
                return Err(unsupported());
            };
            let state = self.state.lock().await;
            let checkpoints = Arc::clone(&state.checkpoints);
            let checkpoint = checkpoints
                .load(&participant.session_id)
                .await
                .map_err(internal)?;
            let tls = state
                .config
                .lock()
                .map_err(|_| internal("gateway config lock is poisoned"))?
                .tls
                .clone();
            let bot = state.bots.bot(&participant.bot_id).map_err(invalid_bot)?;
            let spec = if let Some(checkpoint) = &checkpoint {
                ChatSpec::from_metadata(
                    &checkpoint.metadata,
                    &state.bots,
                    state.store.state_dir(),
                    tls.as_ref(),
                )
            } else {
                ChatSpec::for_bot(&chat.workspace, &bot, state.store.state_dir(), tls.as_ref())
            }
            .map_err(invalid_config)?;
            // Preparation restores presentation without entering lifecycle hooks.
            let store = state.store.clone();
            let prepare = session::prepare_agent(
                Arc::clone(&state.config),
                &spec,
                &store,
                Arc::clone(&state.credentials),
                Arc::clone(&state.bots),
                Arc::clone(&checkpoints),
                state.scratchpad.clone(),
                state.session_files.clone(),
                Arc::clone(&state.chat_store),
                Arc::clone(&state.discovery_gate),
                Arc::clone(&self.desktop),
                participant.session_id.clone(),
                "mobius-gateway",
                None,
                Arc::clone(&state.provider_epoch),
            );
            drop(state);
            let (built, prepared) = Box::pin(prepare).await.map_err(internal)?;
            let frontend = built.agent.frontend().clone();
            let replay = if checkpoint.is_some() {
                load_replay(checkpoints.as_ref(), &participant.session_id, &frontend)
                    .await
                    .map_err(internal)?
            } else {
                LoadedReplay::default()
            };
            let session = built.agent.session().clone();
            let context_limit_tokens = match session.model.model_context_window {
                Some(window) if prepared.bot.config.config.middleware.enabled("compaction") => {
                    Some(
                        crate::assembly::configured_compaction(
                            &prepared.bot.config.config.middleware,
                        )
                        .map_err(internal)?
                        .trigger_tokens(window),
                    )
                }
                window => window,
            };
            let run_stats = checkpoint
                .as_ref()
                .map(|checkpoint| RunStats {
                    completed: checkpoint.execution_stats.clone(),
                    active: checkpoint
                        .active_execution
                        .as_ref()
                        .map(|active| active_run_summary(&checkpoint.session_id, active)),
                })
                .unwrap_or_default();
            let ready = SessionReadyPayload {
                member_bot_ids: chat.member_bot_ids(),
                primary_bot_id: chat.primary_bot_id.clone(),
                active_turn_ids: Vec::new(),
                pending_approvals: Vec::new(),
                latest_sequence: replay.latest_sequence,
                next_before_sequence: replay.next_before_sequence,
                workspace: spec.workspace_info(),
                attached_folders: spec.attached_folders,
                git: git_status(&built.gateway_sandbox).await,
                session,
                contributions: frontend.contributions().to_vec(),
                widgets: replay
                    .widgets
                    .into_iter()
                    .map(|((capability, _), item)| SessionWidget { capability, item })
                    .collect(),
                tool_count: built.agent.tool_count(),
                compaction_count: checkpoint
                    .as_ref()
                    .map_or(0, |checkpoint| checkpoint.compaction_count),
                context_limit_tokens,
                run_stats,
            };
            Ok((ready, frontend, prepared))
        })
        .await
    }

    pub(crate) async fn create_chat(
        &self,
        workspace: &Path,
        bot_ids: &[String],
        primary_bot_id: Option<&str>,
    ) -> std::result::Result<HostHandle, Rejection> {
        let (chat_store, state_dir, tls) = {
            let state = self.state.lock().await;
            let tls = state
                .config
                .lock()
                .map_err(|_| internal("gateway config lock is poisoned"))?
                .tls
                .clone();
            (
                Arc::clone(&state.chat_store),
                state.store.state_dir().to_path_buf(),
                tls,
            )
        };
        let workspace = workspace.to_path_buf();
        let workspace = tokio::task::spawn_blocking(move || {
            crate::config::validate_chat_workspace(&workspace, &state_dir, tls.as_ref())
        })
        .await
        .map_err(internal)?
        .map_err(invalid_workspace)?;
        let _mutation = self.begin_mutation().await?;
        let id = chat_store
            .create(workspace, bot_ids.to_vec(), primary_bot_id)
            .await
            .map_err(invalid_chat)?;
        drop(_mutation);
        let host = self.open_session_with_cache(&id, true).await?.0;
        self.broadcast_sessions().await?;
        Ok(host)
    }

    pub(super) fn chat_handle(&self, state: &GatewayState, chat: Chat) -> HostHandle {
        let alive = Arc::new(AtomicBool::new(true));
        let terminated = Arc::new(AtomicBool::new(false));
        let termination = Arc::new(tokio::sync::Notify::new());
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        let handle = HostHandle {
            inner: Arc::new(session::HostInner {
                session_id: chat.id.clone().into(),
                commands,
                events: events.clone(),
                alive: Arc::clone(&alive),
                terminated: Arc::clone(&terminated),
                termination: Arc::clone(&termination),
                session_mutations: Arc::clone(&state.session_mutations),
                realtime_voice: Arc::new(Mutex::new(())),
            }),
        };
        let actor = ChatHost {
            gateway: self.clone(),
            chat_store: Arc::clone(&state.chat_store),
            checkpoints: Arc::clone(&state.checkpoints),
            session_files: state.session_files.clone(),
            id: chat.id,
            workspace: chat.workspace,
            state_dir: state.store.state_dir().to_path_buf(),
            participants: HashMap::new(),
            frontend: None,
            prepared: None,
            events: events.clone(),
            idle_waiters: Vec::new(),
        };
        tokio::spawn(async move {
            actor.run(receiver).await;
            alive.store(false, Ordering::Release);
            terminated.store(true, Ordering::Release);
            termination.notify_waiters();
        });
        handle
    }
}

struct ChatHost {
    gateway: GatewayHost,
    chat_store: Arc<ChatStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    session_files: SessionFileStore,
    id: String,
    workspace: PathBuf,
    state_dir: PathBuf,
    participants: HashMap<String, HostHandle>,
    frontend: Option<FrontendExtensions>,
    prepared: Option<Arc<crate::assembly::PreparedBot>>,
    events: broadcast::Sender<ServerFrame>,
    idle_waiters: Vec<oneshot::Sender<()>>,
}

impl ChatHost {
    async fn run(mut self, mut commands: mpsc::Receiver<session::QueuedCommand>) {
        while let Some((command, _owner)) = commands.recv().await {
            if !Box::pin(self.handle(command)).await {
                break;
            }
        }
        for host in self.participants.into_values() {
            host.shutdown().await;
        }
    }

    #[expect(
        clippy::cognitive_complexity,
        reason = "Chat commands are routed explicitly by their owning actor"
    )]
    async fn handle(&mut self, command: HostCommand) -> bool {
        match command {
            HostCommand::Snapshot {
                last_sequence,
                reply,
            } => {
                let _ = reply.send(self.snapshot(last_sequence).await);
            }
            HostCommand::HistoryPage {
                before_sequence,
                reply,
            } => {
                let _ = reply.send(self.history(before_sequence).await);
            }
            HostCommand::Submit {
                submission,
                recipient_bot_ids,
                reply,
            } => {
                let _ = reply.send(self.submit(submission, &recipient_bot_ids).await);
            }
            HostCommand::Participant { bot_id, reply } => {
                let _ = reply.send(self.participant(&bot_id).await);
            }
            HostCommand::Publish { records } if records.is_empty() => {
                if let Err(error) = self.broadcast_changed().await {
                    self.report(error);
                }
            }
            HostCommand::Publish { records } => {
                match self.project(records).await {
                    Ok(records) => {
                        for record in records {
                            let _ = self
                                .events
                                .send(ServerFrame::new(ServerMessage::AgentEvent {
                                    session_id: self.id.clone(),
                                    record,
                                }));
                        }
                    }
                    Err(error) => self.report(error),
                }
                self.notify_idle().await;
            }
            HostCommand::Dispatch => {
                if let Err(error) = self.dispatch().await {
                    self.report(error);
                }
                self.notify_idle().await;
            }
            HostCommand::CapacityChanged => {
                if self.is_idle().await.unwrap_or(false) {
                    self.chat_store.retry_pending();
                }
                self.notify_idle().await;
            }
            HostCommand::Stop { reply } => {
                let _ = reply.send(self.stop_members().await);
                self.notify_idle().await;
            }
            HostCommand::WaitIdle { reply } => {
                if self.is_idle().await.unwrap_or(false) {
                    let _ = reply.send(());
                } else {
                    self.idle_waiters.push(reply);
                }
            }
            HostCommand::ProviderCutoverStatus { reply } => {
                let _ = reply.send(ProviderCutoverStatus {
                    idle: self.is_idle().await.unwrap_or(false),
                });
            }
            HostCommand::StopIfIdle { reply } => {
                if self.is_idle().await.unwrap_or(false) {
                    let _ = reply.send(true);
                    return false;
                }
                let _ = reply.send(false);
            }
            HostCommand::Shutdown => return false,
            HostCommand::AcceptsFileAttachments { reply } => {
                let result = async {
                    if self.chat().await?.participants.len() == 1 {
                        Ok(runtime_accepts_attachments(&self.read_frontend().await?))
                    } else {
                        Ok(true)
                    }
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::RealtimeModel { reply } => {
                let result = async { self.sole_participant().await?.realtime_model().await }.await;
                let _ = reply.send(result);
            }
            HostCommand::ObserveVoiceUsage {
                provider_instance,
                usage,
                reply,
            } => {
                let result = async {
                    self.sole_participant()
                        .await?
                        .observe_voice_usage(provider_instance, usage)
                        .await
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::AttachFolder { folder, reply } => {
                let result =
                    async { self.sole_participant().await?.attach_folder(folder).await }.await;
                let _ = reply.send(result);
            }
            HostCommand::ReassignBot { bot_id, reply } => {
                let _ = reply.send(self.reassign(&bot_id).await);
            }
            HostCommand::GitDiff { scope, reply } => {
                let result = async {
                    workspace_git_diff(&self.sandbox().await?, &self.workspace, scope).await
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::WorkspaceFiles { scope, reply } => {
                let result = async {
                    list_workspace_files(&self.sandbox().await?, &self.workspace, scope).await
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
                    read_workspace_file(&self.sandbox().await?, &path, offset, max_bytes).await
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
                    if self.chat().await?.participants.len() == 1 {
                        self.sole_participant()
                            .await?
                            .write_workspace_file(path, content)
                            .await
                    } else {
                        write_workspace_file(&self.sandbox().await?, &path, &content).await
                    }
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::SwitchGitBranch { branch, reply } => {
                let result = async {
                    if self.chat().await?.participants.len() == 1 {
                        self.sole_participant()
                            .await?
                            .switch_git_branch(branch)
                            .await
                    } else {
                        switch_workspace_branch(&self.sandbox().await?, &branch).await
                    }
                }
                .await;
                let _ = reply.send(result);
            }
            HostCommand::Frontend { reply } => {
                if let Ok(frontend) = self.read_frontend().await {
                    let _ = reply.send(frontend);
                }
            }
            HostCommand::RunRoutine { reply, .. } => {
                let _ = reply.send(Err(unsupported()));
            }
        }
        true
    }

    async fn chat(&self) -> std::result::Result<Chat, Rejection> {
        self.chat_store
            .load(&self.id)
            .await
            .map_err(internal)?
            .ok_or_else(unknown_session)
    }

    async fn read_frontend(&mut self) -> std::result::Result<FrontendExtensions, Rejection> {
        let chat = self.chat().await?;
        if let [participant] = chat.participants.as_slice()
            && let Some(host) = self
                .participants
                .get(&participant.session_id)
                .filter(|host| host.is_alive())
        {
            return host.frontend().await;
        }
        if let (Some(frontend), Some(prepared)) = (&self.frontend, &self.prepared) {
            let state = self.gateway.state.lock().await;
            let bot = state.bots.bot(&prepared.bot.id).map_err(invalid_bot)?;
            if prepared.matches_runtime(&bot)
                && prepared.epoch == state.provider_epoch.load(Ordering::Acquire)
            {
                return Ok(frontend.clone());
            }
        }
        let (_, frontend, prepared) = self.gateway.inspect_execution(&chat).await?;
        self.frontend = Some(frontend.clone());
        self.prepared = Some(prepared);
        Ok(frontend)
    }

    async fn sole_participant(&mut self) -> std::result::Result<HostHandle, Rejection> {
        let chat = self.chat().await?;
        let [participant] = chat.participants.as_slice() else {
            return Err(unsupported());
        };
        self.participant(&participant.bot_id).await
    }

    async fn participant(&mut self, bot_id: &str) -> std::result::Result<HostHandle, Rejection> {
        let chat = self.chat().await?;
        let session_id = chat
            .session_id(bot_id)
            .ok_or_else(invalid_session_bot)?
            .to_owned();
        if let Some(host) = self
            .participants
            .get(&session_id)
            .filter(|host| host.is_alive())
        {
            return Ok(host.clone());
        }
        self.participants.remove(&session_id);
        if let Some(host) = self
            .gateway
            .state
            .lock()
            .await
            .sessions
            .get(&session_id)
            .filter(|host| host.is_alive())
            .cloned()
        {
            self.participants.insert(session_id, host.clone());
            return Ok(host);
        }
        let checkpoint = self.checkpoints.load(&session_id).await.map_err(internal)?;
        if let Some(checkpoint) = &checkpoint {
            self.gateway
                .cancel_unadmitted_execution(&chat.id, bot_id, checkpoint)
                .await?;
        }
        let host = if checkpoint.is_some() {
            self.gateway
                .open_execution_with_cache(&session_id, true)
                .await?
                .0
        } else {
            self.gateway
                .create_session_with_id(&chat.workspace, bot_id, session_id.clone(), false, "Chat")
                .await?
        };
        if chat.participants.len() == 1 {
            self.frontend = Some(host.frontend().await?);
        }
        self.participants.insert(session_id, host.clone());
        Ok(host)
    }

    async fn dispatch(&mut self) -> std::result::Result<(), Rejection> {
        for (bot_id, entry) in self
            .chat_store
            .pending_deliveries(&self.id)
            .await
            .map_err(internal)?
        {
            let host = self.participant(&bot_id).await?;
            if accepted_submission(self.checkpoints.as_ref(), host.session_id(), &entry.id)
                .await
                .map_err(internal)?
            {
                continue;
            }
            for attachment in &entry.message.attachments {
                self.session_files
                    .grant_upload(&self.id, host.session_id(), attachment)
                    .await
                    .map_err(internal)?;
            }
            let message_id = entry.id.clone();
            match host.submit(chat_message_submission(entry)).await {
                Ok(()) => {}
                Err(rejection) if matches!(rejection.code, "agent_busy" | "gateway_busy") => {
                    return Ok(());
                }
                Err(rejection) => {
                    self.chat_store
                        .settle_delivery(
                            &message_id,
                            host.session_id(),
                            &bot_id,
                            ChatRunOutcome::Failed {
                                message: rejection.message,
                            },
                        )
                        .await
                        .map_err(internal)?;
                }
            }
        }
        Ok(())
    }

    async fn submit(
        &mut self,
        submission: Submission,
        recipients: &[String],
    ) -> std::result::Result<(), Rejection> {
        match &submission.op {
            Op::Message { message } => {
                let _mutation = self.gateway.begin_mutation().await?;
                message
                    .validate(mobius::backend::session_files::session_file_limits())
                    .map_err(invalid_chat)?;
                for attachment in &message.attachments {
                    self.session_files
                        .verify_upload(&self.id, attachment)
                        .await
                        .map_err(invalid_chat)?;
                }
                match message.author {
                    MessageAuthor::User => {
                        self.chat_store
                            .post_user(&self.id, submission.id, message.clone(), recipients)
                            .await
                    }
                    MessageAuthor::Peer { .. } => {
                        self.chat_store
                            .post_message(&self.id, submission.id, message.clone(), recipients)
                            .await
                    }
                }
                .map_err(invalid_chat)
            }
            Op::ExecApproval { id, .. } | Op::Interrupt { turn_id: id } => {
                if !recipients.is_empty() {
                    return Err(invalid_chat("recipients apply only to messages"));
                }
                for participant in self.chat().await?.participants {
                    let Some(checkpoint) = self
                        .checkpoints
                        .load(&participant.session_id)
                        .await
                        .map_err(internal)?
                    else {
                        continue;
                    };
                    let matches = match &submission.op {
                        Op::ExecApproval { .. } => checkpoint
                            .pending_approval
                            .as_ref()
                            .is_some_and(|approval| approval.request_id == *id),
                        _ => checkpoint
                            .active_execution
                            .as_ref()
                            .is_some_and(|active| active.turn_id == *id),
                    };
                    if matches {
                        return self
                            .participant(&participant.bot_id)
                            .await?
                            .submit(submission)
                            .await;
                    }
                }
                Err(invalid_chat(
                    "the addressed Bot is no longer waiting for this action",
                ))
            }
            _ => {
                if !recipients.is_empty() {
                    return Err(invalid_chat("recipients apply only to messages"));
                }
                self.sole_participant().await?.submit(submission).await
            }
        }
    }

    async fn stop_members(&mut self) -> std::result::Result<(), Rejection> {
        self.chat_store
            .cancel_pending(&self.id)
            .await
            .map_err(internal)?;
        for participant in self.chat().await?.participants {
            let host = self.participants.remove(&participant.session_id).or(self
                .gateway
                .state
                .lock()
                .await
                .sessions
                .get(&participant.session_id)
                .cloned());
            if let Some(host) = host.filter(|host| host.is_alive()) {
                if self.frontend.is_some() {
                    self.frontend = Some(host.frontend().await?);
                }
                host.stop_and_wait().await?;
                host.wait_terminated().await;
            } else if self
                .checkpoints
                .load(&participant.session_id)
                .await
                .map_err(internal)?
                .is_some_and(|checkpoint| {
                    checkpoint.active_execution.is_some()
                        || checkpoint.pending_approval.is_some()
                        || !checkpoint.pending_messages.is_empty()
                })
            {
                self.gateway
                    .cancel_execution(&participant.session_id)
                    .await?;
            }
            self.gateway
                .state
                .lock()
                .await
                .sessions
                .remove(&participant.session_id);
        }
        Ok(())
    }

    async fn is_idle(&self) -> std::result::Result<bool, Rejection> {
        if !self
            .chat_store
            .pending_deliveries(&self.id)
            .await
            .map_err(internal)?
            .is_empty()
        {
            return Ok(false);
        }
        for participant in self.chat().await?.participants {
            if self
                .checkpoints
                .load(&participant.session_id)
                .await
                .map_err(internal)?
                .is_some_and(|checkpoint| {
                    checkpoint.active_execution.is_some()
                        || checkpoint.pending_approval.is_some()
                        || !checkpoint.pending_messages.is_empty()
                })
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn notify_idle(&mut self) {
        if self.is_idle().await.unwrap_or(false) {
            for waiter in self.idle_waiters.drain(..) {
                let _ = waiter.send(());
            }
        }
    }

    async fn reassign(&mut self, bot_id: &str) -> std::result::Result<(), Rejection> {
        let chat = self.chat().await?;
        if chat.participants.len() != 1 {
            return Err(unsupported());
        }
        if !self.is_idle().await? {
            return Err(invalid_chat("stop the Chat before changing its Bot"));
        }
        self.gateway
            .validate_chat_bot(&chat.workspace, bot_id)
            .await?;
        self.stop_members().await?;
        self.chat_store
            .reassign(&self.id, bot_id)
            .await
            .map_err(invalid_chat)?;
        self.frontend = None;
        self.prepared = None;
        self.broadcast_changed().await
    }

    async fn broadcast_changed(&mut self) -> std::result::Result<(), Rejection> {
        let payload = self.snapshot(None).await?.ready;
        let _ = self
            .events
            .send(ServerFrame::new(ServerMessage::SessionChanged { payload }));
        Ok(())
    }

    async fn sandbox(&self) -> std::result::Result<GatewaySandbox, Rejection> {
        let tls = self
            .gateway
            .state
            .lock()
            .await
            .config
            .lock()
            .map_err(|_| internal("gateway config lock is poisoned"))?
            .tls
            .clone();
        GatewaySandbox::new(
            &self.workspace,
            &self.state_dir,
            tls.as_ref().map(|tls| tls.private_key.as_path()),
            std::time::Duration::from_secs(30),
        )
        .map_err(internal)
    }

    fn report(&self, rejection: Rejection) {
        let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
            code: "chat_execution".into(),
            message: rejection.message,
            fatal: false,
        }));
    }

    async fn project(
        &mut self,
        records: Vec<JournalEvent>,
    ) -> std::result::Result<Vec<RecordedEvent>, Rejection> {
        let chat = self.chat().await?;
        if let [participant] = chat.participants.as_slice() {
            if let Some(host) = self
                .participants
                .get(&participant.session_id)
                .filter(|host| host.is_alive())
            {
                self.frontend = Some(host.frontend().await?);
            } else if self.frontend.is_none() {
                let (_, frontend, prepared) = self.gateway.inspect_execution(&chat).await?;
                self.frontend = Some(frontend);
                self.prepared = Some(prepared);
            }
        }
        let mut projected = Vec::with_capacity(records.len());
        for journal in records {
            let recipient_bot_ids = if matches!(
                journal.event.msg,
                EventMsg::Message(_) | EventMsg::AssistantMessage(_)
            ) {
                self.chat_store
                    .message_by_sequence(&chat.id, journal.sequence)
                    .await
                    .map_err(internal)?
                    .ok_or_else(|| internal("published Chat message has no recipient record"))?
                    .recipients
            } else {
                Vec::new()
            };
            let mut record = match &self.frontend {
                Some(frontend) if chat.participants.len() == 1 => project_record(frontend, journal),
                _ => record(journal),
            };
            record.recipient_bot_ids = recipient_bot_ids;
            projected.push(record);
        }
        Ok(projected)
    }

    async fn history(
        &mut self,
        before_sequence: Option<u64>,
    ) -> std::result::Result<SessionHistoryPage, Rejection> {
        let page = event_turn_page(before_sequence, true, MAX_FRAME_BYTES, |request| {
            self.chat_store.event_page(&self.id, request)
        })
        .await
        .map_err(internal)?;
        let next_before_sequence = page.next_before_sequence;
        Ok(SessionHistoryPage {
            records: self.project(page.into_chronological()).await?,
            next_before_sequence,
        })
    }

    async fn snapshot(
        &mut self,
        last_sequence: Option<u64>,
    ) -> std::result::Result<HostSnapshot, Rejection> {
        let chat = self.chat().await?;
        let mut ready = if chat.participants.len() == 1 {
            if let Some(host) = self
                .participants
                .get(&chat.participants[0].session_id)
                .filter(|host| host.is_alive())
            {
                host.snapshot(None).await?.ready
            } else {
                let (ready, frontend, prepared) = self.gateway.inspect_execution(&chat).await?;
                self.frontend = Some(frontend);
                self.prepared = Some(prepared);
                ready
            }
        } else {
            SessionReadyPayload {
                active_turn_ids: Vec::new(),
                pending_approvals: Vec::new(),
                member_bot_ids: chat.member_bot_ids(),
                primary_bot_id: chat.primary_bot_id.clone(),
                latest_sequence: 0,
                next_before_sequence: None,
                workspace: WorkspaceInfo {
                    id: crate::config::workspace_id(&chat.workspace),
                    path: chat.workspace.clone(),
                },
                attached_folders: Vec::new(),
                git: None,
                session: SessionConfiguredEvent {
                    session_id: self.id.clone(),
                    context: chat.context(),
                    model: ModelChangedEvent {
                        route: String::new(),
                        model: String::new(),
                        reasoning_effort: None,
                        model_context_window: None,
                    },
                },
                contributions: Vec::new(),
                widgets: Vec::new(),
                tool_count: 0,
                compaction_count: 0,
                context_limit_tokens: None,
                run_stats: RunStats::default(),
            }
        };
        let page = event_turn_page(None, true, MAX_FRAME_BYTES, |request| {
            self.chat_store.event_page(&self.id, request)
        })
        .await
        .map_err(internal)?;
        let latest_sequence = page.latest_sequence;
        if last_sequence.is_some_and(|sequence| {
            sequence > latest_sequence
                || page
                    .next_before_sequence
                    .is_some_and(|earliest| sequence < earliest.saturating_sub(1))
        }) {
            return Err(Rejection {
                code: "replay_unavailable",
                message: "reopen the chat to load its current history".into(),
                fatal: false,
            });
        }
        ready.session.session_id.clone_from(&self.id);
        if let Some(active) = &mut ready.run_stats.active {
            active.session_id.clone_from(&self.id);
        }
        ready.member_bot_ids = chat.member_bot_ids();
        ready.primary_bot_id = chat.primary_bot_id.clone();
        ready.latest_sequence = latest_sequence;
        ready.next_before_sequence = page.next_before_sequence;
        ready.active_turn_ids.clear();
        ready.pending_approvals.clear();
        for participant in &chat.participants {
            if let Some(checkpoint) = self
                .checkpoints
                .load(&participant.session_id)
                .await
                .map_err(internal)?
            {
                if let Some(active) = checkpoint.active_execution {
                    ready.active_turn_ids.push(active.turn_id);
                }
                if let Some(pending) = checkpoint
                    .pending_approval
                    .filter(|pending| !pending.decision_received)
                {
                    let mut approval = pending.request_event();
                    self.chat_store
                        .label_approval(&participant.bot_id, &mut approval)
                        .map_err(internal)?;
                    ready.pending_approvals.push(approval);
                }
            }
        }
        let records = self
            .project(
                page.into_chronological()
                    .into_iter()
                    .filter(|event| last_sequence.is_none_or(|last| event.sequence > last))
                    .collect(),
            )
            .await?;
        let replay = records
            .into_iter()
            .map(|record| {
                let frame = ServerFrame::new(ServerMessage::AgentEvent {
                    session_id: self.id.clone(),
                    record,
                });
                validate_event_frame(&frame).map_err(internal)?;
                Ok(frame)
            })
            .collect::<std::result::Result<Vec<_>, Rejection>>()?;
        Ok(HostSnapshot { ready, replay })
    }
}

pub(super) fn record(journal: JournalEvent) -> RecordedEvent {
    RecordedEvent {
        sequence: journal.sequence,
        recorded_at_ms: journal.recorded_at_ms,
        blocks: journal.event.msg.presentation().into_iter().collect(),
        event: journal.event,
        recipient_bot_ids: Vec::new(),
        stream_metrics: journal.stream_metrics,
        preview: None,
    }
}

fn unsupported() -> Rejection {
    Rejection {
        code: "chat_operation",
        message: "this action requires an individual Bot chat".into(),
        fatal: false,
    }
}
