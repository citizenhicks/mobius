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
        let group_store = Arc::clone(&state.group);
        drop(state);

        group_store
            .remove_bot(&intent.bot_id)
            .await
            .map_err(invalid_group)?;
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
        let group_store = Arc::clone(&state.group);
        let intent = bot_store
            .record_bot_deletion(&mut deletion, &session_roots, &session_ids)
            .map_err(invalid_bot)?;
        drop(state);
        group_store.remove_bot(id).await.map_err(invalid_group)?;

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

    pub(super) async fn reconcile_deleted_group_chats(&self) -> std::result::Result<(), Rejection> {
        let mut state = self.state.lock().await;
        let groups = state.group.chats(true).await.map_err(internal)?;
        if groups.is_empty() {
            return Ok(());
        }
        let ids = groups.into_iter().map(|chat| chat.id).collect::<Vec<_>>();
        let summaries = gateway_session_summaries(&state.checkpoints)
            .await
            .map_err(internal)?;
        let roots = summaries
            .iter()
            .filter(|summary| {
                crate::groups::participant_chat_id(&summary.session_id)
                    .is_some_and(|id| ids.iter().any(|chat_id| chat_id == id))
            })
            .map(|summary| summary.session_id.clone())
            .collect::<Vec<_>>();
        let (_, mut deleted) = session_trees(roots.clone(), &summaries);
        deleted.extend(ids.iter().cloned());
        let mut files = prepare_session_tree_deletion(&mut state, &deleted, true).await?;
        let cleanup = remove_session_trees(&mut state, &roots, &deleted, &mut files, true).await?;
        if cleanup.is_none() {
            state.group.finish_deletion(&ids).await.map_err(internal)?;
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
        let groups = state.group.chats(false).await.map_err(internal)?;
        let group_ids = groups
            .iter()
            .filter(|chat| selected.contains(&chat.id))
            .map(|chat| chat.id.clone())
            .collect::<Vec<_>>();
        if selected.iter().any(|selected| {
            !group_ids.contains(selected)
                && !summaries
                    .iter()
                    .any(|session| session.catalog_visible && session.session_id == *selected)
        }) {
            return Err(unknown_session());
        }
        let selected_set = selected.iter().map(String::as_str).collect::<HashSet<_>>();
        let parents = summaries
            .iter()
            .map(|session| {
                (
                    session.session_id.as_str(),
                    session.parent_session_id.as_deref(),
                )
            })
            .collect::<HashMap<_, _>>();
        let mut roots = selected
            .iter()
            .filter(|session_id| {
                let mut ancestor = parents.get(session_id.as_str()).copied().flatten();
                let mut visited = HashSet::new();
                while let Some(parent) = ancestor {
                    if !visited.insert(parent) {
                        break;
                    }
                    if selected_set.contains(parent) {
                        return false;
                    }
                    ancestor = parents.get(parent).copied().flatten();
                }
                true
            })
            .cloned()
            .collect::<Vec<_>>();
        roots.extend(
            state
                .group
                .participant_sessions(&roots)
                .await
                .map_err(internal)?
                .into_iter()
                .filter(|id| summaries.iter().any(|summary| &summary.session_id == id)),
        );
        roots.retain(|id| !group_ids.contains(id));
        let (_, mut deleted) = session_trees(roots.clone(), &summaries);
        deleted.extend(group_ids.iter().cloned());
        let mut file_deletion = prepare_session_tree_deletion(&mut state, &deleted, true).await?;
        state
            .group
            .mark_deleted(&group_ids)
            .await
            .map_err(internal)?;
        let cleanup =
            remove_session_trees(&mut state, &roots, &deleted, &mut file_deletion, false).await?;
        if cleanup.is_none() {
            state
                .group
                .finish_deletion(&group_ids)
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

fn bot_session_trees(bot_id: &str, summaries: &[SessionSummary]) -> (Vec<String>, Vec<String>) {
    let owned = summaries
        .iter()
        .filter(|session| session.session_context.bot_id == bot_id)
        .map(|session| session.session_id.clone())
        .collect::<HashSet<_>>();
    let roots = summaries
        .iter()
        .filter(|session| {
            owned.contains(&session.session_id)
                && session
                    .parent_session_id
                    .as_ref()
                    .is_none_or(|parent| !owned.contains(parent))
        })
        .map(|session| session.session_id.clone())
        .collect::<Vec<_>>();
    session_trees(roots, summaries)
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
    let (_, session_ids) = bot_session_trees(bot_id, &summaries);
    let file_deletion = prepare_session_tree_deletion(state, &session_ids, true).await?;
    let summaries = gateway_session_summaries(&state.checkpoints)
        .await
        .map_err(internal)?;
    let (session_roots, session_ids) = bot_session_trees(bot_id, &summaries);
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
    allow_pending_group: bool,
) -> std::result::Result<SessionFileDeletion, Rejection> {
    if state
        .starting_sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .keys()
        .any(|starting| {
            session_ids.contains(starting)
                || crate::groups::participant_chat_id(starting)
                    .is_some_and(|chat| session_ids.iter().any(|id| id == chat))
        })
    {
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
    if !allow_pending_group
        && state
            .group
            .has_pending_source_sessions(session_ids)
            .await
            .map_err(internal)?
    {
        return Err(Rejection {
            code: "session_has_pending_group_delivery",
            message: "wait for this chat's pending Group deliveries before deleting it".into(),
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
