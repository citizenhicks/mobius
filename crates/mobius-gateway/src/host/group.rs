use super::*;
use crate::groups::{GroupChat, participant_session_id};
use crate::wire::{SessionReadyPayload, WorkspaceInfo};
use mobius::protocol::{ModelChangedEvent, SessionConfiguredEvent};

const PAGE_SIZE: usize = 50;

impl GatewayHost {
    pub(crate) async fn create_chat(
        &self,
        workspace: &Path,
        bot_ids: &[String],
    ) -> std::result::Result<HostHandle, Rejection> {
        if let [bot_id] = bot_ids {
            return self.create_session(workspace, bot_id).await;
        }
        let (groups, state_dir, tls) = {
            let state = self.state.lock().await;
            let tls = state
                .config
                .lock()
                .map_err(|_| internal("gateway config lock is poisoned"))?
                .tls
                .clone();
            (
                Arc::clone(&state.group),
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
        let id = groups
            .create(workspace, bot_ids.to_vec())
            .await
            .map_err(invalid_group)?;
        drop(_mutation);
        let host = self.open_session_with_cache(&id, true).await?.0;
        self.broadcast_sessions().await?;
        Ok(host)
    }

    pub(super) fn group_handle(&self, state: &GatewayState, chat: GroupChat) -> HostHandle {
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
        let group = GroupHost {
            gateway: self.clone(),
            groups: Arc::clone(&state.group),
            checkpoints: Arc::clone(&state.checkpoints),
            session_files: state.session_files.clone(),
            id: chat.id,
            workspace: chat.workspace,
            state_dir: state.store.state_dir().to_path_buf(),
        };
        tokio::spawn(async move {
            group.run(receiver).await;
            alive.store(false, Ordering::Release);
            terminated.store(true, Ordering::Release);
            termination.notify_waiters();
        });
        handle
    }
}

struct GroupHost {
    gateway: GatewayHost,
    groups: Arc<GroupStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    session_files: SessionFileStore,
    id: String,
    workspace: PathBuf,
    state_dir: PathBuf,
}

impl GroupHost {
    async fn run(self, mut commands: mpsc::Receiver<HostCommand>) {
        while let Some(command) = commands.recv().await {
            match command {
                HostCommand::BotId { reply } => {
                    let _ = reply.send(String::new());
                }
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
                HostCommand::Submit { submission, reply } => {
                    let _ = reply.send(self.submit(submission).await);
                }
                HostCommand::AcceptsFileAttachments { reply } => {
                    let _ = reply.send(Ok(true));
                }
                HostCommand::ProviderCutoverStatus { reply } => {
                    let _ = reply.send(ProviderCutoverStatus { idle: true });
                }
                HostCommand::WaitIdle { reply } => {
                    let _ = reply.send(());
                }
                HostCommand::CapacityChanged => self.groups.retry_pending(),
                HostCommand::StopIfIdle { reply } => {
                    let _ = reply.send(true);
                    break;
                }
                HostCommand::Shutdown => break,
                HostCommand::GitDiff { scope, reply } => {
                    let result = match self.sandbox() {
                        Ok(sandbox) => workspace_git_diff(&sandbox, &self.workspace, scope).await,
                        Err(error) => Err(error),
                    };
                    let _ = reply.send(result);
                }
                HostCommand::WorkspaceFiles { scope, reply } => {
                    let _ = reply.send(self.files(scope).await);
                }
                HostCommand::ReadWorkspaceFile {
                    path,
                    offset,
                    max_bytes,
                    reply,
                } => {
                    let result = match self.sandbox() {
                        Ok(sandbox) => {
                            read_workspace_file(&sandbox, &path, offset, max_bytes).await
                        }
                        Err(error) => Err(error),
                    };
                    let _ = reply.send(result);
                }
                HostCommand::WriteWorkspaceFile {
                    path,
                    content,
                    reply,
                } => {
                    let result = match self.sandbox() {
                        Ok(sandbox) => write_workspace_file(&sandbox, &path, &content).await,
                        Err(error) => Err(error),
                    };
                    let _ = reply.send(result);
                }
                HostCommand::SwitchGitBranch { branch, reply } => {
                    let result = match self.sandbox() {
                        Ok(sandbox) => switch_workspace_branch(&sandbox, &branch).await,
                        Err(error) => Err(error),
                    };
                    let _ = reply.send(result);
                }
                HostCommand::ReassignBot { reply, .. }
                | HostCommand::AttachFolder { reply, .. } => {
                    let _ = reply.send(Err(unsupported()));
                }
                HostCommand::RealtimeModel { reply } => {
                    let _ = reply.send(Err(unsupported()));
                }
                HostCommand::ObserveVoiceUsage { reply, .. } => {
                    let _ = reply.send(Err(unsupported()));
                }
                HostCommand::RunRoutine { reply, .. } => {
                    let _ = reply.send(Err(unsupported()));
                }
            }
        }
    }

    fn sandbox(&self) -> std::result::Result<GatewaySandbox, Rejection> {
        GatewaySandbox::new(
            &self.workspace,
            &self.state_dir,
            None,
            std::time::Duration::from_secs(30),
        )
        .map_err(internal)
    }

    async fn files(
        &self,
        scope: WorkspaceFileScope,
    ) -> std::result::Result<WorkspaceFiles, Rejection> {
        list_workspace_files(&self.sandbox()?, &self.workspace, scope).await
    }

    async fn history(
        &self,
        before_sequence: Option<u64>,
    ) -> std::result::Result<SessionHistoryPage, Rejection> {
        let page = self
            .groups
            .event_page(
                &self.id,
                EventPageRequest {
                    before_sequence,
                    limit: PAGE_SIZE,
                },
            )
            .await
            .map_err(internal)?;
        let next_before_sequence = page.next_before_sequence;
        Ok(SessionHistoryPage {
            records: page.into_chronological().into_iter().map(record).collect(),
            next_before_sequence,
        })
    }

    async fn snapshot(
        &self,
        last_sequence: Option<u64>,
    ) -> std::result::Result<HostSnapshot, Rejection> {
        let chat = self
            .groups
            .load(&self.id)
            .await
            .map_err(internal)?
            .ok_or_else(unknown_session)?;
        let page = self
            .groups
            .event_page(
                &self.id,
                EventPageRequest {
                    before_sequence: None,
                    limit: PAGE_SIZE,
                },
            )
            .await
            .map_err(internal)?;
        let latest_sequence = page.latest_sequence;
        let mut active_turn_ids = Vec::new();
        let mut pending_approvals = Vec::new();
        for bot_id in &chat.member_bot_ids {
            if let Some(checkpoint) = self
                .checkpoints
                .load(&participant_session_id(&self.id, bot_id))
                .await
                .map_err(internal)?
            {
                if let Some(active) = checkpoint.active_execution {
                    active_turn_ids.push(active.turn_id);
                }
                if let Some(pending) = checkpoint
                    .pending_approval
                    .filter(|pending| !pending.decision_received)
                {
                    let mut request = pending.request_event();
                    self.groups
                        .label_approval(bot_id, &mut request)
                        .map_err(internal)?;
                    pending_approvals.push(request);
                }
            }
        }
        if last_sequence.is_some_and(|sequence| {
            sequence > latest_sequence
                || page
                    .next_before_sequence
                    .is_some_and(|earliest| sequence < earliest)
        }) {
            return Err(Rejection {
                code: "replay_unavailable",
                message: "reopen the chat to load its current history".into(),
                fatal: false,
            });
        }
        let ready = SessionReadyPayload {
            active_turn_ids,
            pending_approvals,
            member_bot_ids: Some(chat.member_bot_ids.clone()),
            latest_sequence,
            next_before_sequence: page.next_before_sequence,
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
        };
        let replay = page
            .into_chronological()
            .into_iter()
            .filter(|event| last_sequence.is_none_or(|last| event.sequence > last))
            .map(|event| {
                ServerFrame::new(ServerMessage::AgentEvent {
                    session_id: self.id.clone(),
                    record: record(event),
                })
            })
            .collect();
        Ok(HostSnapshot { ready, replay })
    }

    async fn submit(&self, submission: Submission) -> std::result::Result<(), Rejection> {
        let _mutation = self.gateway.begin_mutation().await?;
        match &submission.op {
            Op::Message { message } => {
                message
                    .validate(mobius::backend::session_files::session_file_limits())
                    .map_err(invalid_group)?;
                for attachment in &message.attachments {
                    self.session_files
                        .verify_upload(&self.id, attachment)
                        .await
                        .map_err(invalid_group)?;
                }
                self.groups
                    .post_user(&self.id, submission.id, message.clone())
                    .await
                    .map_err(invalid_group)
            }
            Op::ExecApproval { id, .. } | Op::Interrupt { turn_id: id } => {
                let chat = self
                    .groups
                    .load(&self.id)
                    .await
                    .map_err(internal)?
                    .ok_or_else(unknown_session)?;
                for bot_id in chat.member_bot_ids {
                    let session_id = participant_session_id(&self.id, &bot_id);
                    let Some(checkpoint) =
                        self.checkpoints.load(&session_id).await.map_err(internal)?
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
                            .is_some_and(|execution| execution.turn_id == *id),
                    };
                    if matches {
                        drop(_mutation);
                        return self
                            .gateway
                            .open_session_with_cache(&session_id, true)
                            .await?
                            .0
                            .submit(submission)
                            .await;
                    }
                }
                Err(invalid_group(
                    "the addressed Bot is no longer waiting for this action",
                ))
            }
            _ => Err(unsupported()),
        }
    }
}

pub(super) fn record(journal: JournalEvent) -> RecordedEvent {
    RecordedEvent {
        sequence: journal.sequence,
        recorded_at_ms: journal.recorded_at_ms,
        blocks: journal.event.msg.presentation().into_iter().collect(),
        event: journal.event,
        stream_metrics: journal.stream_metrics,
        preview: None,
    }
}

fn unsupported() -> Rejection {
    Rejection {
        code: "group_operation",
        message: "this action requires an individual Bot chat".into(),
        fatal: false,
    }
}
