use super::deletion::{prepare_session_tree_deletion, remove_session_trees, session_trees};
use super::*;

pub(super) async fn accept_routine_while_state_locked(
    _state: &mut tokio::sync::MutexGuard<'_, GatewayState>,
    host: &HostHandle,
    run: ActiveRoutineRun,
    input: String,
    bots: &BotStore,
) -> std::result::Result<(), Rejection> {
    host.run_routine(run, input, bots).await
}

impl GatewayHost {
    pub(crate) async fn bot_routine_snapshot(
        &self,
        bot_id: &str,
    ) -> std::result::Result<serde_json::Value, Rejection> {
        let _access = self.begin_mutation().await?;
        let state = self.state.lock().await;
        state.bots.bot(bot_id).map_err(invalid_bot)?;
        let routines = state
            .bots
            .routine_records(Some(bot_id), Utc::now().timestamp())
            .map_err(invalid_routine)?;
        let runs = state
            .bots
            .history(None)
            .map_err(internal)?
            .into_iter()
            .filter(|run| run.bot_id == bot_id)
            .take(100)
            .collect::<Vec<_>>();
        Ok(serde_json::json!({"routines":routines,"recent_runs":runs}))
    }

    pub(crate) async fn routine_workspace(
        &self,
        bot_id: &str,
        workspace: Option<&Path>,
    ) -> std::result::Result<PathBuf, Rejection> {
        let _access = self.begin_mutation().await?;
        let state = self.state.lock().await;
        let bot = state.bots.bot(bot_id).map_err(invalid_bot)?;
        let tls = state
            .config
            .lock()
            .map_err(|_| internal("configuration lock poisoned"))?
            .tls
            .clone();
        match workspace {
            Some(workspace) => crate::config::validate_chat_workspace(
                workspace,
                state.store.state_dir(),
                tls.as_ref(),
            )
            .map_err(invalid_workspace),
            None => ChatSpec::persistent(&bot)
                .execution_root(state.store.state_dir(), tls.as_ref())
                .map_err(invalid_workspace),
        }
    }

    pub(crate) async fn create_routine(
        &self,
        bot_id: &str,
        definition: &crate::wire::RoutineDefinition,
        cause: Option<&crate::wire::HookEvent>,
    ) -> std::result::Result<crate::wire::Routine, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        validate_bot_workspace(&state, bot_id, &definition.workspace)?;
        validate_routine_bindings(&state, bot_id, &definition.bindings).await?;
        let routine = state
            .bots
            .create_routine(bot_id, definition, cause)
            .map_err(invalid_routine)?;
        state
            .bots
            .routine_record(&routine.id, Utc::now().timestamp())
            .map_err(invalid_routine)
    }

    pub(crate) async fn execute_routine_command(
        &self,
        command: &crate::wire::RoutineCommand,
        owner_id: Option<&str>,
        cause: Option<&crate::wire::HookEvent>,
        command_id: &str,
    ) -> std::result::Result<(), Rejection> {
        use crate::wire::RoutineAction;
        match &command.action {
            RoutineAction::Start => {
                self.start_routine_command(&command.routine_id, owner_id, cause, command_id)
                    .await
            }
            RoutineAction::Stop { run_id } => {
                let _mutation = self.begin_mutation().await?;
                let (run, checkpoints) = {
                    let state = self.state.lock().await;
                    if !super::bot_events::command_pending(&state.bots, cause, command_id)? {
                        return Ok(());
                    }
                    require_routine_owner(&state.bots, &command.routine_id, owner_id)?;
                    let run = state.bots.run(run_id).map_err(invalid_routine)?;
                    if run.routine_id != command.routine_id {
                        return Err(invalid_routine("run does not belong to this routine"));
                    }
                    (run, Arc::clone(&state.checkpoints))
                };
                if run.status != RoutineRunStatus::Running {
                    return Ok(());
                }
                let session_id = run
                    .session_id
                    .ok_or_else(|| invalid_routine("run has no session"))?;
                let checkpoint = checkpoints
                    .load(&session_id)
                    .await
                    .map_err(internal)?
                    .ok_or_else(unknown_session)?;
                let turn_id = checkpoint
                    .active_execution
                    .ok_or_else(|| Rejection {
                        code: "agent_busy",
                        message: "run admission has not started a turn yet; retry".into(),
                        fatal: false,
                    })?
                    .turn_id;
                self.state
                    .lock()
                    .await
                    .bots
                    .request_run_stop(run_id, command_id, cause)
                    .map_err(internal)?;
                drop(_mutation);
                self.open_session(&session_id)
                    .await?
                    .submit(Submission {
                        id: command_id.into(),
                        op: Op::Interrupt { turn_id },
                    })
                    .await
            }
            RoutineAction::Pause | RoutineAction::Resume => {
                let _mutation = self.begin_mutation().await?;
                let state = self.state.lock().await;
                if !super::bot_events::command_pending(&state.bots, cause, command_id)? {
                    return Ok(());
                }
                require_routine_owner(&state.bots, &command.routine_id, owner_id)?;
                state
                    .bots
                    .set_routine_enabled(
                        &command.routine_id,
                        matches!(command.action, RoutineAction::Resume),
                        cause,
                        cause.map(|_| command_id),
                    )
                    .map(|_| ())
                    .map_err(invalid_routine)
            }
            RoutineAction::Update { definition } => {
                let _mutation = self.begin_exclusive_mutation().await?;
                let state = self.state.lock().await;
                if !super::bot_events::command_pending(&state.bots, cause, command_id)? {
                    return Ok(());
                }
                require_routine_owner(&state.bots, &command.routine_id, owner_id)?;
                let bot_id = state
                    .bots
                    .routine(&command.routine_id)
                    .map_err(invalid_routine)?
                    .bot_id;
                validate_bot_workspace(&state, &bot_id, &definition.workspace)?;
                validate_routine_bindings(&state, &bot_id, &definition.bindings).await?;
                state
                    .bots
                    .update_routine(
                        &command.routine_id,
                        definition,
                        cause,
                        cause.map(|_| command_id),
                    )
                    .map(|_| ())
                    .map_err(invalid_routine)
            }
            RoutineAction::Delete => {
                self.delete_routine(&command.routine_id, owner_id, cause, command_id)
                    .await
            }
        }
    }

    pub(crate) async fn delete_routine(
        &self,
        routine_id: &str,
        owner_id: Option<&str>,
        cause: Option<&crate::wire::HookEvent>,
        command_id: &str,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_exclusive_mutation().await?;
        let mut state = self.state.lock().await;
        if !super::bot_events::command_pending(&state.bots, cause, command_id)? {
            return Ok(());
        }
        require_routine_owner(&state.bots, routine_id, owner_id)?;
        let roots = state
            .bots
            .history(Some(routine_id))
            .map_err(invalid_routine)?
            .into_iter()
            .filter_map(|run| run.session_id)
            .collect();
        let summaries = gateway_session_summaries(&state.checkpoints)
            .await
            .map_err(internal)?;
        let session_ids = session_trees(roots, &summaries).1;
        let mut file_deletion = prepare_session_tree_deletion(&mut state, &session_ids).await?;
        let summaries = gateway_session_summaries(&state.checkpoints)
            .await
            .map_err(internal)?;
        let deletion = state
            .bots
            .prepare_routine_deletion(routine_id)
            .map_err(invalid_routine)?;
        let roots = deletion
            .session_ids()
            .iter()
            .filter(|root| summaries.iter().any(|session| session.session_id == **root))
            .cloned()
            .collect();
        let (session_roots, session_ids) = session_trees(roots, &summaries);
        state
            .bots
            .delete_routine(deletion, cause, cause.map(|_| command_id))
            .map_err(invalid_routine)?;
        let cleanup = remove_session_trees(
            &mut state,
            &session_roots,
            &session_ids,
            &mut file_deletion,
            true,
        )
        .await;
        drop(state);
        self.cleanup_session_files(file_deletion);
        if !session_ids.is_empty() {
            let _ = self.broadcast_sessions().await;
        }
        if let Some(rejection) = match cleanup {
            Ok(warning) => warning,
            Err(rejection) => Some(rejection),
        } {
            let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                code: "routine_cleanup".into(),
                message: rejection.message,
                fatal: false,
            }));
        }
        Ok(())
    }

    async fn start_routine_command(
        &self,
        routine_id: &str,
        owner_id: Option<&str>,
        cause: Option<&crate::wire::HookEvent>,
        command_id: &str,
    ) -> std::result::Result<(), Rejection> {
        self.work_activity.mark();
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        if !super::bot_events::command_pending(&state.bots, cause, command_id)? {
            return Ok(());
        }
        require_routine_owner(&state.bots, routine_id, owner_id)?;
        let run = match state
            .bots
            .begin_run_with_cause(routine_id, command_id, cause)
            .map_err(invalid_routine)?
        {
            BeginRun::Started(run) => run,
            BeginRun::AlreadyRecorded => return Ok(()),
            BeginRun::Skipped => {
                return Err(Rejection {
                    code: "routine_overlap",
                    message: format!("routine {routine_id} is already running"),
                    fatal: false,
                });
            }
        };
        drop(_mutation);
        self.run_routine_with_state(state, routine_id.into(), run)
            .await
    }

    pub(crate) async fn routine_run_preview(
        &self,
        run_id: &str,
        before_sequence: Option<u64>,
    ) -> std::result::Result<crate::wire::RoutineRunPreview, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let (bots, run) = {
            let state = self.state.lock().await;
            let bots = Arc::clone(&state.bots);
            let run = bots.run(run_id).map_err(invalid_routine)?;
            (bots, run)
        };
        let session_id = run.session_id.clone().ok_or_else(|| Rejection {
            code: "routine_run_unavailable",
            message: "this routine run has no execution session".into(),
            fatal: false,
        })?;
        drop(_mutation);
        let (host, temporary) = self.open_session_with_cache(&session_id, false).await?;
        let page = host.history_page(before_sequence).await;
        if temporary {
            let _ = host.stop_if_idle().await;
        }
        let page = page?;
        let routine = bots
            .routine_record(&run.routine_id, Utc::now().timestamp())
            .map_err(invalid_routine)?;
        Ok(crate::wire::RoutineRunPreview {
            routine,
            run,
            records: page.records,
            next_before_sequence: page.next_before_sequence,
        })
    }

    pub(crate) async fn delete_routine_run(
        &self,
        run_id: &str,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_exclusive_mutation().await?;
        let mut state = self.state.lock().await;
        let run = state.bots.run(run_id).map_err(invalid_routine)?;
        if run.status == RoutineRunStatus::Running {
            return Err(Rejection {
                code: "routine_run_active",
                message: format!("routine run {run_id} is currently running"),
                fatal: false,
            });
        }
        let session_root = run.session_id;
        let session_ids = if let Some(session_id) = session_root.as_deref() {
            let summaries = gateway_session_summaries(&state.checkpoints)
                .await
                .map_err(internal)?;
            session_tree_ids(session_id, &summaries).unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut file_deletion = prepare_session_tree_deletion(&mut state, &session_ids).await?;
        state.bots.delete_run(run_id).map_err(invalid_routine)?;
        let cleanup = if let Some(session_root) = session_root.filter(|_| !session_ids.is_empty()) {
            remove_session_trees(
                &mut state,
                &[session_root],
                &session_ids,
                &mut file_deletion,
                true,
            )
            .await
        } else {
            Ok(None)
        };
        drop(state);
        self.cleanup_session_files(file_deletion);
        if !session_ids.is_empty() {
            let _ = self.broadcast_sessions().await;
        }
        if let Some(rejection) = match cleanup {
            Ok(warning) => warning,
            Err(rejection) => Some(rejection),
        } {
            let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                code: "routine_cleanup".into(),
                message: rejection.message,
                fatal: false,
            }));
        }
        Ok(())
    }

    async fn run_routine_with_state(
        &self,
        state: tokio::sync::MutexGuard<'_, GatewayState>,
        routine_id: String,
        run: ActiveRoutineRun,
    ) -> std::result::Result<(), Rejection> {
        let bots = Arc::clone(&state.bots);
        drop(state);
        let preflight = bots.routine_input(&routine_id).map_err(invalid_routine);
        let (routine, input) = match preflight {
            Ok(preflight) => preflight,
            Err(rejection) => {
                bots.finish_run(
                    run,
                    crate::wire::RoutineRunStatus::Failed,
                    Some(rejection.message.clone()),
                )
                .map_err(internal)?;
                return Err(rejection);
            }
        };
        let session_id = run.session_id().to_owned();
        let label = format!("routine · {}", routine.id.get(..8).unwrap_or(&routine.id));
        let host = match self
            .create_session_with_id(
                Some(&routine.workspace),
                &routine.bot_id,
                session_id.clone(),
                false,
                &label,
            )
            .await
        {
            Ok(host) => host,
            Err(rejection) => {
                let status = if rejection.code == "session_limit" {
                    crate::wire::RoutineRunStatus::Skipped
                } else {
                    crate::wire::RoutineRunStatus::Failed
                };
                bots.finish_run(run, status, Some(rejection.message.clone()))
                    .map_err(internal)?;
                return Err(rejection);
            }
        };
        let mut state = self.state.lock().await;
        let accepted =
            accept_routine_while_state_locked(&mut state, &host, run, input, bots.as_ref()).await;
        drop(state);
        match accepted {
            Ok(()) => {
                let broadcast = self.broadcast_sessions().await;
                let gateway = self.clone();
                let cleanup = tokio::spawn(async move {
                    host.wait_idle().await;
                    gateway.state.lock().await.sessions.remove(&session_id);
                    let _ = gateway.broadcast_sessions().await;
                });
                let mut state = self.state.lock().await;
                state.idle_cleanup_tasks.retain(|task| !task.is_finished());
                state.idle_cleanup_tasks.push(cleanup);
                broadcast
            }
            Err(rejection) => {
                let _ = host.stop_if_idle().await;
                self.state.lock().await.sessions.remove(&session_id);
                Err(rejection)
            }
        }
    }
}

fn require_routine_owner(
    bots: &BotStore,
    id: &str,
    owner: Option<&str>,
) -> std::result::Result<(), Rejection> {
    if let Some(owner) = owner {
        let routine = bots.routine(id).map_err(invalid_routine)?;
        if routine.bot_id != owner {
            return Err(unknown_session());
        }
    }
    Ok(())
}

async fn validate_routine_bindings(
    state: &GatewayState,
    bot_id: &str,
    bindings: &[crate::wire::RoutineBinding],
) -> std::result::Result<(), Rejection> {
    for binding in bindings {
        if let crate::wire::HookSelector::Event { .. } = &binding.on {
            super::bot_events::validate_selector(state, bot_id, &binding.on).await?;
        }
    }
    Ok(())
}
