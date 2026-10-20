use mobius::SessionId;
use mobius::backend::session_files::SessionFileDeletion;

use super::deletion::{prepare_session_tree_deletion, remove_session_trees, session_trees};
use super::{
    ActiveRoutineRun, Arc, BeginRun, BotStore, ChatSpec, GatewayHost, GatewayState, HostHandle, Op,
    Path, PathBuf, Rejection, RoutineRunStatus, Submission, Utc, gateway_session_summaries,
    internal, invalid_bot, invalid_routine, invalid_workspace, send_warning, session_tree_ids,
    unknown_session, validate_bot_workspace,
};

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
    /// Publishes what a routine deletion removed once the state lock is released.
    async fn finish_routine_deletion(
        &self,
        file_deletion: SessionFileDeletion,
        session_ids: &[SessionId],
        cleanup: std::result::Result<Option<Rejection>, Rejection>,
    ) {
        self.cleanup_session_files(file_deletion);
        if !session_ids.is_empty()
            && let Err(rejection) = self.broadcast_sessions().await
        {
            tracing::warn!(code = rejection.code, message = %rejection.message, "session list broadcast failed");
        }
        if let Some(rejection) = cleanup.unwrap_or_else(Some) {
            send_warning(&self.events, "routine_cleanup", rejection.message);
        }
    }

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
            .filter(|run| *run.bot_id == *bot_id)
            .take(100)
            .collect::<Vec<_>>();
        drop(state);
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
        let config = state.config()?;
        let tls = config.tls.as_ref();
        let root = match workspace {
            Some(workspace) => {
                crate::config::validate_chat_workspace(workspace, state.store.state_dir(), tls)
                    .map(crate::config::CanonicalWorkspace::into_path)
            }
            None => ChatSpec::persistent(&bot).execution_root(state.store.state_dir(), tls),
        };
        drop(config);
        drop(state);
        root.map_err(invalid_workspace)
    }

    pub(crate) async fn create_routine(
        &self,
        bot_id: &str,
        definition: &crate::wire::RoutineDefinition,
        cause: Option<&crate::wire::HookEvent>,
    ) -> std::result::Result<crate::wire::Routine, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        let workspace = validate_bot_workspace(&state, bot_id, &definition.workspace)?;
        validate_routine_bindings(&state, bot_id, &definition.bindings).await?;
        let routine = state
            .bots
            .create_routine(bot_id, definition, workspace, cause)
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
                let mutation = self.begin_mutation().await?;
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
                    .ok_or_else(|| {
                        Rejection::new(
                            "agent_busy",
                            "run admission has not started a turn yet; retry",
                        )
                    })?
                    .turn_id;
                self.state
                    .lock()
                    .await
                    .bots
                    .request_run_stop(run_id, command_id, cause)
                    .map_err(internal)?;
                drop(mutation);
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
                let workspace = validate_bot_workspace(&state, &bot_id, &definition.workspace)?;
                validate_routine_bindings(&state, &bot_id, &definition.bindings).await?;
                state
                    .bots
                    .update_routine(
                        &command.routine_id,
                        definition,
                        workspace,
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

    #[expect(
        clippy::significant_drop_tightening,
        reason = "remove_session_trees updates the file deletion before cleanup_session_files consumes it"
    )]
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
        let mut file_deletion = prepare_session_tree_deletion(
            &state,
            &session_ids,
            mobius::backend::session_files::SessionFileSelection::All,
        )
        .await?;
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
            .filter(|root| {
                summaries
                    .iter()
                    .any(|session| *session.session_id == ***root)
            })
            .cloned() // rules-allow: the routine deletion is consumed before its session trees are removed
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
        self.finish_routine_deletion(file_deletion, &session_ids, cleanup)
            .await;
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
        let mutation = self.begin_mutation().await?;
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
                return Err(Rejection::new(
                    "routine_overlap",
                    format!("routine {routine_id} is already running"),
                ));
            }
        };
        drop(mutation);
        self.run_routine_with_state(state, routine_id.into(), run)
            .await
    }

    pub(crate) async fn routine_run_preview(
        &self,
        run_id: &str,
        before_sequence: Option<u64>,
    ) -> std::result::Result<crate::wire::RoutineRunPreview, Rejection> {
        let mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        let bots = Arc::clone(&state.bots);
        drop(state);
        let run = bots.run(run_id).map_err(invalid_routine)?;
        let session_id = run.session_id.as_ref().ok_or_else(|| {
            Rejection::new(
                "routine_run_unavailable",
                "this routine run has no execution session",
            )
        })?;
        drop(mutation);
        let (host, temporary) = self.open_session_with_cache(session_id, false).await?;
        let page = host.history_page(before_sequence).await;
        if temporary {
            host.stop_if_idle().await;
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

    #[expect(
        clippy::significant_drop_tightening,
        reason = "remove_session_trees updates the file deletion before cleanup_session_files consumes it"
    )]
    pub(crate) async fn delete_routine_run(
        &self,
        run_id: &str,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_exclusive_mutation().await?;
        let mut state = self.state.lock().await;
        let run = state.bots.run(run_id).map_err(invalid_routine)?;
        if run.status == RoutineRunStatus::Running {
            return Err(Rejection::new(
                "routine_run_active",
                format!("routine run {run_id} is currently running"),
            ));
        }
        let session_root = run.session_id;
        let session_ids = if let Some(session_id) = session_root.as_ref() {
            let summaries = gateway_session_summaries(&state.checkpoints)
                .await
                .map_err(internal)?;
            session_tree_ids(session_id, &summaries).unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut file_deletion = prepare_session_tree_deletion(
            &state,
            &session_ids,
            mobius::backend::session_files::SessionFileSelection::All,
        )
        .await?;
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
        self.finish_routine_deletion(file_deletion, &session_ids, cleanup)
            .await;
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
                    Some(Arc::clone(&rejection.message)),
                )
                .map_err(internal)?;
                return Err(rejection);
            }
        };
        let label = format!("routine · {}", routine.id.get(..8).unwrap_or(&routine.id));
        let host = match self
            .create_session_with_id(
                Some(&routine.workspace),
                &routine.bot_id,
                run.session_id().share(),
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
                bots.finish_run(run, status, Some(Arc::clone(&rejection.message)))
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
                let gateway = self.clone(); // rules-allow: the spawned cleanup task owns a gateway handle
                let cleanup = tokio::spawn(async move {
                    host.wait_idle().await;
                    gateway
                        .state
                        .lock()
                        .await
                        .sessions
                        .remove(&**host.session_id());
                    if let Err(rejection) = gateway.broadcast_sessions().await {
                        tracing::warn!(code = rejection.code, message = %rejection.message, "session list broadcast failed");
                    }
                });
                let mut state = self.state.lock().await;
                state.idle_cleanup_tasks.retain(|task| !task.is_finished());
                state.idle_cleanup_tasks.push(cleanup);
                broadcast
            }
            Err(rejection) => {
                host.stop_if_idle().await;
                self.state
                    .lock()
                    .await
                    .sessions
                    .remove(&**host.session_id());
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
        if *routine.bot_id != *owner {
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
        if let crate::wire::HookSelector::Event { source, .. } = &binding.on {
            super::bot_events::validate_source(state, bot_id, source).await?;
        }
    }
    Ok(())
}
