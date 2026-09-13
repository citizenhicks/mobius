use std::collections::HashSet;

use mobius::backend::checkpoint::SessionSummary;
use mobius::backend::session_files::SessionFileDeletion;

use super::*;

impl GatewayHost {
    pub(super) fn cleanup_session_files(&self, mut deletion: SessionFileDeletion) {
        let events = self.events.clone();
        tokio::spawn(async move {
            if let Err(error) = deletion.delete().await {
                let _ = events.send(ServerFrame::new(ServerMessage::Error {
                    code: "session_cleanup".into(),
                    message: error.to_string(),
                    fatal: false,
                }));
            }
        });
    }

    pub(crate) async fn reconcile_pending_bot_deletion(
        &self,
    ) -> std::result::Result<(), Rejection> {
        if self
            .state
            .lock()
            .await
            .bots
            .pending_bot_deletion()
            .map_err(internal)?
            .is_none()
        {
            return Ok(());
        }
        let mutation_gate = Arc::clone(&self.state.lock().await.session_mutations);
        let _mutation = mutation_gate.write_owned().await;
        let mut state = self.state.lock().await;
        let Some(intent) = state.bots.pending_bot_deletion().map_err(internal)? else {
            return Ok(());
        };
        let mut deletion = state
            .bots
            .bots()
            .map_err(internal)?
            .iter()
            .any(|bot| bot.id == intent.bot_id)
            .then(|| {
                state
                    .bots
                    .prepare_bot_deletion(&intent.bot_id, intent.expected_revision)
                    .map_err(invalid_bot)
            })
            .transpose()?;
        if let Some(deletion) = &mut deletion {
            deletion.release_state_lock();
        }
        let mut file_deletion =
            prepare_session_tree_deletion(&mut state, &intent.session_ids, true).await?;
        let bot_store = Arc::clone(&state.bots);
        let chat_store = Arc::clone(&state.chat_store);
        drop(state);

        chat_store
            .remove_bot(&intent.bot_id)
            .await
            .map_err(invalid_chat)?;
        if let Some(deletion) = deletion {
            bot_store.delete_bot(deletion).map_err(invalid_bot)?;
        }
        bot_store.prepared.lock().await.remove(&intent.bot_id);
        let mut state = self.state.lock().await;
        let file_warning = remove_session_trees(
            &mut state,
            &intent.session_roots,
            &intent.session_ids,
            &mut file_deletion,
            true,
        )
        .await?;
        drop(state);
        self.cleanup_session_files(file_deletion);
        if let Some(warning) = file_warning {
            let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                code: "session_cleanup".into(),
                message: warning.message,
                fatal: false,
            }));
        }
        bot_store
            .cleanup_bot_deletion_files(&intent)
            .map_err(internal)?;
        bot_store
            .clear_bot_deletion(&intent.bot_id)
            .map_err(internal)
    }

    pub(crate) async fn delete_bot(
        &self,
        id: &str,
        expected_revision: u64,
    ) -> std::result::Result<(Vec<crate::wire::BotRecord>, Vec<String>), Rejection> {
        let _mutation = self.begin_exclusive_mutation().await?;
        let mut state = self.state.lock().await;
        let bot = state.bots.bot(id).map_err(invalid_bot)?;
        if bot.config.revision != expected_revision {
            return Err(Rejection {
                code: "revision_conflict",
                message: format!("Bot configuration revision is now {}", bot.config.revision),
                fatal: false,
            });
        }
        if bot.handle == "mobius" {
            return Err(bot_delete_rejection(
                "bot_undeletable",
                "the built-in @mobius Bot cannot be deleted",
            ));
        }
        let (session_roots, session_ids, mut file_deletion) =
            prepare_bot_session_tree_deletion(&mut state, id).await?;
        let mut deletion = state
            .bots
            .prepare_bot_deletion(id, expected_revision)
            .map_err(invalid_bot)?;
        let bot_store = Arc::clone(&state.bots);
        let chat_store = Arc::clone(&state.chat_store);
        let intent = bot_store
            .record_bot_deletion(&mut deletion, &session_roots, &session_ids)
            .map_err(invalid_bot)?;
        drop(state);
        chat_store.remove_bot(id).await.map_err(invalid_chat)?;

        bot_store.delete_bot(deletion).map_err(invalid_bot)?;
        bot_store.prepared.lock().await.remove(id);
        let bots = bot_store.bots().map_err(internal)?;
        let mut state = self.state.lock().await;
        let mut cleanup_errors = Vec::new();
        match remove_session_trees(
            &mut state,
            &session_roots,
            &session_ids,
            &mut file_deletion,
            true,
        )
        .await
        {
            Ok(Some(warning)) => cleanup_errors.push(warning.message),
            Ok(None) => {}
            Err(rejection) => return Err(rejection),
        }
        drop(state);
        self.cleanup_session_files(file_deletion);
        bot_store
            .cleanup_bot_deletion_files(&intent)
            .map_err(internal)?;
        bot_store.clear_bot_deletion(id).map_err(invalid_bot)?;
        if !session_ids.is_empty()
            && let Err(rejection) = self.broadcast_sessions().await
        {
            cleanup_errors.push(rejection.message);
        }
        self.broadcast_bots(&bots);
        for message in cleanup_errors {
            let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                code: "bot_cleanup".into(),
                message,
                fatal: false,
            }));
        }
        Ok((bots, session_ids))
    }

    pub(super) async fn reconcile_deleted_chats(&self) -> std::result::Result<(), Rejection> {
        let mut state = self.state.lock().await;
        let chats = state.chat_store.chats(true).await.map_err(internal)?;
        if chats.is_empty() {
            return Ok(());
        }
        let ids = chats.iter().map(|chat| chat.id.clone()).collect::<Vec<_>>();
        let summaries = gateway_session_summaries(&state.checkpoints)
            .await
            .map_err(internal)?;
        let roots = chats
            .iter()
            .flat_map(|chat| {
                chat.execution_participants()
                    .map(|participant| participant.session_id.clone())
            })
            .collect::<Vec<_>>();
        let (_, mut deleted) = session_trees(roots.clone(), &summaries);
        deleted.extend(ids.iter().cloned());
        let mut files = prepare_session_tree_deletion(&mut state, &deleted, true).await?;
        let cleanup = remove_session_trees(&mut state, &roots, &deleted, &mut files, true).await?;
        if cleanup.is_none() {
            state
                .chat_store
                .finish_deletion(&ids)
                .await
                .map_err(internal)?;
        }
        drop(state);
        self.cleanup_session_files(files);
        if let Some(error) = cleanup {
            return Err(error);
        }
        Ok(())
    }

    pub(crate) async fn delete_sessions(
        &self,
        session_ids: &[String],
    ) -> std::result::Result<Vec<String>, Rejection> {
        if session_ids.is_empty() || session_ids.len() > MAX_SESSION_DELETE_ROOTS {
            return Err(Rejection {
                code: "invalid_session_selection",
                message: format!("select between 1 and {MAX_SESSION_DELETE_ROOTS} chats to delete"),
                fatal: false,
            });
        }
        let mut seen = HashSet::new();
        let selected = session_ids
            .iter()
            .filter(|session_id| seen.insert(session_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for session_id in &selected {
            validate_session_id(session_id).map_err(|_| invalid_session_id())?;
        }
        let _mutation = self.begin_exclusive_mutation().await?;
        let mut state = self.state.lock().await;
        let summaries = gateway_session_summaries(&state.checkpoints)
            .await
            .map_err(internal)?;
        let chats = state.chat_store.chats(false).await.map_err(internal)?;
        let chat_ids = chats
            .iter()
            .filter(|chat| selected.contains(&chat.id))
            .map(|chat| chat.id.clone())
            .collect::<Vec<_>>();
        if selected.iter().any(|id| !chat_ids.contains(id)) {
            return Err(unknown_session());
        }
        let roots = chats
            .iter()
            .filter(|chat| chat_ids.contains(&chat.id))
            .flat_map(|chat| {
                chat.execution_participants()
                    .map(|participant| participant.session_id.clone())
            })
            .collect::<Vec<_>>();
        let (_, mut deleted) = session_trees(roots.clone(), &summaries);
        deleted.extend(chat_ids.iter().cloned());
        let mut file_deletion = prepare_session_tree_deletion(&mut state, &deleted, true).await?;
        state
            .chat_store
            .mark_deleted(&chat_ids)
            .await
            .map_err(internal)?;
        let cleanup =
            remove_session_trees(&mut state, &roots, &deleted, &mut file_deletion, true).await?;
        if cleanup.is_none() {
            state
                .chat_store
                .finish_deletion(&chat_ids)
                .await
                .map_err(internal)?;
        }
        drop(state);
        self.cleanup_session_files(file_deletion);
        if let Some(rejection) = cleanup {
            let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                code: "session_cleanup".into(),
                message: rejection.message,
                fatal: false,
            }));
        }
        if let Err(rejection) = self.broadcast_sessions().await {
            let _ = self.events.send(ServerFrame::new(ServerMessage::Error {
                code: "session_catalog".into(),
                message: rejection.message,
                fatal: false,
            }));
        }
        Ok(deleted)
    }
}

async fn bot_session_roots(
    chats: &crate::chats::ChatStore,
    bots: &BotStore,
    bot_id: &str,
) -> Result<Vec<String>> {
    let mut records = chats.chats(false).await?;
    records.extend(chats.chats(true).await?);
    let mut roots = records
        .iter()
        .flat_map(|chat| chat.execution_participants())
        .filter(|participant| participant.bot_id == bot_id)
        .map(|participant| participant.session_id.clone())
        .collect::<Vec<_>>();
    roots.extend(
        bots.history(None)?
            .into_iter()
            .filter(|run| run.bot_id == bot_id)
            .filter_map(|run| run.session_id),
    );
    roots.sort();
    roots.dedup();
    Ok(roots)
}

pub(super) async fn prepare_bot_session_tree_deletion(
    state: &mut GatewayState,
    bot_id: &str,
) -> std::result::Result<(Vec<String>, Vec<String>, SessionFileDeletion), Rejection> {
    if state
        .starting_sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .any(|starting_bot| starting_bot == bot_id)
    {
        return Err(Rejection {
            code: "agent_busy",
            message: "wait for this Bot's chat to finish starting before deleting it".into(),
            fatal: false,
        });
    }
    let summaries = gateway_session_summaries(&state.checkpoints)
        .await
        .map_err(internal)?;
    let mut roots = bot_session_roots(&state.chat_store, &state.bots, bot_id)
        .await
        .map_err(internal)?;
    roots.retain(|root| summaries.iter().any(|summary| summary.session_id == *root));
    let (_, session_ids) = session_trees(roots.clone(), &summaries);
    let file_deletion = prepare_session_tree_deletion(state, &session_ids, true).await?;
    let summaries = gateway_session_summaries(&state.checkpoints)
        .await
        .map_err(internal)?;
    let (session_roots, session_ids) = session_trees(roots, &summaries);
    Ok((session_roots, session_ids, file_deletion))
}

pub(super) fn session_trees(
    roots: Vec<String>,
    summaries: &[SessionSummary],
) -> (Vec<String>, Vec<String>) {
    let mut seen = HashSet::new();
    let mut session_ids = Vec::new();
    for root in &roots {
        if let Some(tree) = session_tree_ids(root, summaries) {
            for session_id in tree {
                if seen.insert(session_id.clone()) {
                    session_ids.push(session_id);
                }
            }
        }
    }
    (roots, session_ids)
}

pub(super) async fn prepare_session_tree_deletion(
    state: &mut GatewayState,
    session_ids: &[String],
    allow_pending_chat: bool,
) -> std::result::Result<SessionFileDeletion, Rejection> {
    let starting = state
        .starting_sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    let mut starting_selected = session_ids.iter().any(|id| starting.contains(id));
    if !starting.is_empty() && !starting_selected {
        starting_selected = state
            .chat_store
            .chats(false)
            .await
            .map_err(internal)?
            .iter()
            .filter(|chat| session_ids.contains(&chat.id))
            .any(|chat| {
                chat.execution_participants()
                    .any(|participant| starting.contains(&participant.session_id))
            });
    }
    if starting_selected {
        return Err(Rejection {
            code: "agent_busy",
            message: "wait for this chat to finish starting before deleting it".into(),
            fatal: false,
        });
    }
    if session_ids.is_empty() {
        return state
            .session_files
            .prepare_delete_sessions(session_ids)
            .await
            .map_err(internal);
    }
    if !allow_pending_chat
        && state
            .chat_store
            .has_pending_source_sessions(session_ids)
            .await
            .map_err(internal)?
    {
        return Err(Rejection {
            code: "session_has_pending_delivery",
            message: "wait for this chat's pending deliveries before deleting it".into(),
            fatal: false,
        });
    }
    let residents = session_ids
        .iter()
        .filter_map(|id| state.sessions.get(id).cloned())
        .collect::<Vec<_>>();
    for host in &residents {
        match host.provider_cutover_status().await {
            Ok(status) if status.idle => {}
            Ok(_) => {
                return Err(Rejection {
                    code: "agent_busy",
                    message: "finish or interrupt the active turn before deleting this chat".into(),
                    fatal: false,
                });
            }
            Err(rejection) if rejection.code == "gateway_stopped" => {}
            Err(rejection) => return Err(rejection),
        }
    }
    let file_deletion = state
        .session_files
        .prepare_delete_sessions(session_ids)
        .await
        .map_err(internal)?;
    for host in residents {
        if !host.stop_if_idle().await {
            return Err(Rejection {
                code: "agent_busy",
                message: "finish or interrupt the active turn before deleting this chat".into(),
                fatal: false,
            });
        }
    }
    Ok(file_deletion)
}

pub(super) async fn remove_session_trees(
    state: &mut GatewayState,
    roots: &[String],
    session_ids: &[String],
    file_deletion: &mut SessionFileDeletion,
    missing_roots_are_deleted: bool,
) -> std::result::Result<Option<Rejection>, Rejection> {
    if session_ids.is_empty() {
        return Ok(None);
    }
    let roots = if missing_roots_are_deleted {
        let mut existing = Vec::new();
        for root in roots {
            if state
                .checkpoints
                .load(root)
                .await
                .map_err(internal)?
                .is_some()
            {
                existing.push(root.clone());
            }
        }
        existing
    } else {
        roots.to_vec()
    };
    if !state
        .checkpoints
        .delete_sessions(&roots)
        .await
        .map_err(internal)?
    {
        return Err(unknown_session());
    }

    for id in session_ids {
        state.sessions.remove(id);
    }
    let mut cleanup_errors = Vec::new();
    if let Err(error) = file_deletion.stage().await {
        cleanup_errors.push(error.to_string());
    }
    let catalog_lock = Arc::clone(&state.catalog_lock);
    let _catalog = catalog_lock.lock().await;
    match load_session_metadata(&state.checkpoints).await {
        Ok(mut metadata) => {
            for id in session_ids {
                metadata.remove(id);
            }
            if let Err(error) = save_session_metadata(&state.checkpoints, &metadata).await {
                cleanup_errors.push(error.to_string());
            }
        }
        Err(error) => cleanup_errors.push(error.to_string()),
    }
    let mut catalog = state.activities.lock().await;
    catalog.activities.retain(|id, _| !session_ids.contains(id));
    catalog.approvals.retain(|id, _| !session_ids.contains(id));
    catalog.snapshot = None;
    Ok((!cleanup_errors.is_empty()).then(|| internal(cleanup_errors.join("; "))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobius::backend::checkpoint::{Checkpoint, CheckpointStore, sqlite::SqliteCheckpoint};

    #[tokio::test]
    async fn bot_roots_include_retired_participants_and_routines_then_follow_descendants() {
        let root = tempfile::tempdir().unwrap();
        let bots = Arc::new(BotStore::open(root.path()).unwrap());
        let first = bots
            .create_bot("First", "First", Default::default())
            .unwrap();
        let second = bots
            .create_bot("Second", "Second", Default::default())
            .unwrap();
        let (chats, _) = crate::chats::ChatStore::new(root.path(), bots.clone()).unwrap();
        let reassigned = chats
            .create(root.path().into(), vec![first.id.clone()], Some(&first.id))
            .await
            .unwrap();
        let retired = chats
            .load(&reassigned)
            .await
            .unwrap()
            .unwrap()
            .session_id(&first.id)
            .unwrap()
            .to_owned();
        chats.reassign(&reassigned, &second.id).await.unwrap();
        let shared = chats
            .create(
                root.path().into(),
                vec![first.id.clone(), second.id.clone()],
                Some(&first.id),
            )
            .await
            .unwrap();
        let shared_chat = chats.load(&shared).await.unwrap().unwrap();
        let first_shared = shared_chat.session_id(&first.id).unwrap().to_owned();
        let second_shared = shared_chat.session_id(&second.id).unwrap().to_owned();
        let routine = bots
            .create_routine(
                &first.id,
                root.path(),
                "Test",
                crate::wire::RoutineSchedule {
                    kind: crate::wire::RoutineScheduleKind::Once,
                    at: Some(chrono::Utc::now().timestamp() + 60),
                    every_seconds: None,
                    expression: None,
                    time_zone: None,
                },
                None,
            )
            .unwrap();
        let crate::bots::BeginRun::Started(run) = bots.begin_run(&routine.id).unwrap() else {
            panic!("run starts");
        };
        let expected = HashSet::from([
            retired.clone(),
            first_shared.clone(),
            run.session_id().to_owned(),
        ]);
        let roots = bot_session_roots(&chats, &bots, &first.id).await.unwrap();
        assert_eq!(roots.iter().cloned().collect::<HashSet<_>>(), expected);

        let checkpoints: Arc<dyn CheckpointStore> =
            Arc::new(SqliteCheckpoint::new(root.path().join("checkpoints.sqlite3")).unwrap());
        for id in roots.iter().chain(std::iter::once(&second_shared)) {
            checkpoints
                .save(&Checkpoint::empty(id), &[], None)
                .await
                .unwrap();
        }
        checkpoints
            .fork(&retired, 0, &Checkpoint::empty("child"))
            .await
            .unwrap();
        checkpoints
            .fork("child", 0, &Checkpoint::empty("grandchild"))
            .await
            .unwrap();
        let summaries = gateway_session_summaries(&checkpoints).await.unwrap();
        let (_, ids) = session_trees(roots, &summaries);
        assert_eq!(ids.len(), 5);
        assert!(ids.iter().any(|id| id == "child"));
        assert!(ids.iter().any(|id| id == "grandchild"));
        assert!(!ids.contains(&second_shared));
        assert!(!ids.contains(&shared));
        assert!(!ids.contains(&reassigned));
    }
}
