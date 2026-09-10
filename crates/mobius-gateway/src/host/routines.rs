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
    pub(crate) async fn create_routine(
        &self,
        bot_id: &str,
        workspace: &Path,
        instructions: &str,
        schedule: crate::wire::RoutineSchedule,
        ends_at: Option<i64>,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        validate_bot_workspace(&state, bot_id, workspace)?;
        state
            .bots
            .create_routine(bot_id, workspace, instructions, schedule, ends_at)
            .map(|_| ())
            .map_err(invalid_routine)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "a routine replacement is one wire record"
    )]
    pub(crate) async fn update_routine(
        &self,
        id: &str,
        bot_id: &str,
        workspace: &Path,
        instructions: &str,
        schedule: crate::wire::RoutineSchedule,
        ends_at: Option<i64>,
        enabled: bool,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        validate_bot_workspace(&state, bot_id, workspace)?;
        state
            .bots
            .update_routine(
                id,
                bot_id,
                workspace,
                instructions,
                schedule,
                ends_at,
                enabled,
            )
            .map(|_| ())
            .map_err(invalid_routine)
    }

    pub(crate) async fn delete_routine(
        &self,
        routine_id: &str,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_exclusive_mutation().await?;
        let mut state = self.state.lock().await;
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
        let mut file_deletion =
            prepare_session_tree_deletion(&mut state, &session_ids, false).await?;
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
            .delete_routine(deletion)
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

    pub(crate) async fn run_routine(
        &self,
        routine_id: String,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        let run = match state.bots.begin_run(&routine_id).map_err(invalid_routine)? {
            BeginRun::Started(run) => run,
            BeginRun::Skipped => {
                return Err(Rejection {
                    code: "routine_overlap",
                    message: format!("routine {routine_id} is already running"),
                    fatal: false,
                });
            }
        };
        self.run_routine_with_state(state, routine_id, run).await
    }

    pub(crate) async fn run_due_routine(
        &self,
        routine_id: String,
        run: ActiveRoutineRun,
    ) -> std::result::Result<(), Rejection> {
        let _mutation = match self.begin_mutation().await {
            Ok(mutation) => mutation,
            Err(rejection) => {
                self.state
                    .lock()
                    .await
                    .bots
                    .finish_run(
                        run,
                        RoutineRunStatus::Skipped,
                        Some(rejection.message.clone()),
                    )
                    .map_err(internal)?;
                return Err(rejection);
            }
        };
        let state = self.state.lock().await;
        self.run_routine_with_state(state, routine_id, run).await
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
        let mut file_deletion =
            prepare_session_tree_deletion(&mut state, &session_ids, false).await?;
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
        mut state: tokio::sync::MutexGuard<'_, GatewayState>,
        routine_id: String,
        run: ActiveRoutineRun,
    ) -> std::result::Result<(), Rejection> {
        let preflight: std::result::Result<_, Rejection> = (|| {
            let routine = state.bots.routine(&routine_id).map_err(invalid_routine)?;
            let (_, input) = state
                .bots
                .routine_input(&routine.id)
                .map_err(invalid_routine)?;
            let bot = state.bots.bot(&routine.bot_id).map_err(invalid_bot)?;
            let tls = state
                .config
                .lock()
                .map_err(|_| internal("gateway configuration lock is poisoned"))?
                .tls
                .clone();
            let mut spec = ChatSpec::for_bot(
                &routine.workspace,
                &bot,
                state.store.state_dir(),
                tls.as_ref(),
            )
            .map_err(invalid_workspace)?;
            spec.catalog_visible = false;
            Ok((routine, input, spec))
        })();
        let (routine, input, spec) = match preflight {
            Ok(preflight) => preflight,
            Err(rejection) => {
                state
                    .bots
                    .finish_run(
                        run,
                        crate::wire::RoutineRunStatus::Failed,
                        Some(rejection.message.clone()),
                    )
                    .map_err(internal)?;
                return Err(rejection);
            }
        };
        if let Err(rejection) = state.ensure_capacity().await {
            state
                .bots
                .finish_run(
                    run,
                    crate::wire::RoutineRunStatus::Skipped,
                    Some("the gateway active-chat limit was reached".into()),
                )
                .map_err(internal)?;
            return Err(rejection);
        }
        let session_id = run.session_id().to_owned();
        let label = format!("routine · {}", routine.id.get(..8).unwrap_or(&routine.id));
        let host = match HostHandle::start(
            state.store.clone(),
            Arc::clone(&state.config),
            spec,
            Arc::clone(&state.credentials),
            Arc::clone(&state.bots),
            Arc::clone(&state.checkpoints),
            state.scratchpad.clone(),
            state.session_files.clone(),
            state.swarm.clone(),
            Arc::clone(&state.session_mutations),
            Arc::clone(&state.discovery_gate),
            Arc::clone(&self.desktop),
            Arc::clone(&state.provider_epoch),
            Arc::clone(&state.activities),
            self.events.clone(),
            session_id.clone(),
            &label,
        )
        .await
        {
            Ok(host) => host,
            Err(error) => {
                let message = error.to_string();
                state
                    .bots
                    .finish_run(
                        run,
                        crate::wire::RoutineRunStatus::Failed,
                        Some(message.clone()),
                    )
                    .map_err(internal)?;
                return Err(internal(message));
            }
        };
        let bots = Arc::clone(&state.bots);
        state.sessions.insert(session_id.clone(), host.clone());
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
