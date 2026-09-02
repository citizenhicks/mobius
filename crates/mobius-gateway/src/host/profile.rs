use super::*;
use mobius::middleware::session_files::session_file_limits;

pub(super) async fn gateway_session_summaries(
    checkpoints: &Arc<dyn CheckpointStore>,
) -> Result<Vec<SessionSummary>> {
    let mut cursor = None;
    let mut sessions = Vec::new();
    loop {
        let page = checkpoints
            .list_sessions_page(SessionPageRequest {
                cursor,
                limit: SESSION_PAGE_SIZE,
            })
            .await?;
        sessions.extend(page.sessions);
        let Some(next) = page.next_cursor else {
            return Ok(sessions);
        };
        cursor = Some(next);
    }
}

pub(super) fn session_tree_ids(
    root_session_id: &str,
    sessions: &[SessionSummary],
) -> Option<Vec<String>> {
    sessions
        .iter()
        .any(|session| session.session_id == root_session_id)
        .then_some(())?;
    let mut seen = HashSet::from([root_session_id.to_owned()]);
    let mut ordered = vec![root_session_id.to_owned()];
    loop {
        let mut changed = false;
        for session in sessions {
            if seen.contains(&session.session_id)
                || !session
                    .parent_session_id
                    .as_ref()
                    .is_some_and(|parent| seen.contains(parent))
            {
                continue;
            }
            seen.insert(session.session_id.clone());
            ordered.push(session.session_id.clone());
            changed = true;
        }
        if !changed {
            ordered.reverse();
            return Some(ordered);
        }
    }
}

pub(super) fn gateway_run_stats(sessions: &[SessionSummary]) -> Result<RunStats> {
    let mut totals = RunStats::default();
    for session in sessions {
        add_execution_stats(&mut totals, &session.execution_stats)?;
    }
    Ok(totals)
}

pub(super) fn add_execution_stats(total: &mut RunStats, stats: &ExecutionStats) -> Result<()> {
    let (
        Some(run_count),
        Some(failed_run_count),
        Some(aborted_run_count),
        Some(model_calls),
        Some(tool_calls),
        Some(failed_tool_calls),
        Some(elapsed_ms),
    ) = (
        total.run_count.checked_add(stats.run_count),
        total.failed_run_count.checked_add(stats.failed_run_count),
        total.aborted_run_count.checked_add(stats.aborted_run_count),
        total.model_calls.checked_add(stats.model_calls),
        total.tool_calls.checked_add(stats.tool_calls),
        total.failed_tool_calls.checked_add(stats.failed_tool_calls),
        total.elapsed_ms.checked_add(stats.elapsed_ms),
    )
    else {
        return Err(Error::Config(
            "gateway execution statistics exceed the supported range".into(),
        ));
    };
    let mut usage = total.usage.clone();
    if usage.checked_add(&stats.usage).is_none() {
        return Err(Error::Config(
            "gateway execution statistics exceed the supported range".into(),
        ));
    }
    *total = RunStats {
        run_count,
        failed_run_count,
        aborted_run_count,
        model_calls,
        tool_calls,
        failed_tool_calls,
        elapsed_ms,
        usage,
        active: None,
    };
    Ok(())
}

pub(super) fn completed_run_stats(stats: &ExecutionStats) -> RunStats {
    RunStats {
        run_count: stats.run_count,
        failed_run_count: stats.failed_run_count,
        aborted_run_count: stats.aborted_run_count,
        model_calls: stats.model_calls,
        tool_calls: stats.tool_calls,
        failed_tool_calls: stats.failed_tool_calls,
        elapsed_ms: stats.elapsed_ms,
        usage: stats.usage.clone(),
        active: None,
    }
}

pub(super) fn run_summary(record: ExecutionRecord) -> RunSummary {
    RunSummary {
        session_id: record.session_id,
        submission_id: record.submission_id,
        turn_id: record.turn_id,
        started_at_ms: record.started_at_ms,
        finished_at_ms: Some(record.finished_at_ms),
        elapsed_ms: record.elapsed_ms,
        outcome: Some(session_outcome(record.outcome)),
        model_calls: record.model_calls,
        tool_calls: record.tool_calls,
        failed_tool_calls: record.failed_tool_calls,
        usage: record.usage,
    }
}

pub(super) fn recent_run_groups(
    records: Vec<ExecutionRecord>,
    sessions: &[SessionSummary],
    metadata: &SessionCatalogMetadata,
) -> Vec<SessionRunGroup> {
    let sessions_by_id = sessions
        .iter()
        .map(|session| (session.session_id.as_str(), session))
        .collect::<HashMap<_, _>>();
    let mut groups: Vec<SessionRunGroup> = Vec::new();
    for record in records {
        let Some(root) = visible_session(&record.session_id, &sessions_by_id) else {
            continue;
        };
        if metadata
            .get(&root.session_id)
            .is_some_and(|item| item.hidden)
        {
            continue;
        }
        let run = run_summary(record);
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.session_id == root.session_id)
        {
            group.runs.push(run);
        } else {
            groups.push(SessionRunGroup {
                session_id: root.session_id.clone(),
                title: session_run_group_title(root, metadata),
                runs: vec![run],
            });
        }
    }
    groups
}

pub(super) fn visible_session<'a>(
    session_id: &str,
    sessions: &'a HashMap<&str, &'a SessionSummary>,
) -> Option<&'a SessionSummary> {
    let mut session = *sessions.get(session_id)?;
    for _ in 0..sessions.len() {
        if session.catalog_visible {
            return Some(session);
        }
        session = *sessions.get(session.parent_session_id.as_deref()?)?;
    }
    None
}

pub(super) fn session_run_group_title(
    session: &SessionSummary,
    metadata: &SessionCatalogMetadata,
) -> String {
    metadata
        .get(&session.session_id)
        .and_then(|item| item.title.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .or_else(|| {
            session
                .first_user_message
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
        })
        .unwrap_or("Untitled")
        .to_owned()
}

pub(super) fn active_run_summary(session_id: &str, active: &ActiveExecution) -> RunSummary {
    let elapsed_ms = Utc::now()
        .timestamp_millis()
        .checked_sub(active.started_at_ms)
        .and_then(|elapsed| u64::try_from(elapsed).ok())
        .unwrap_or_default();
    RunSummary {
        session_id: session_id.into(),
        submission_id: active.submission_id.clone(),
        turn_id: active.turn_id.clone(),
        started_at_ms: active.started_at_ms,
        finished_at_ms: None,
        elapsed_ms,
        outcome: None,
        model_calls: active.model_calls,
        tool_calls: active.tool_calls,
        failed_tool_calls: active.failed_tool_calls,
        usage: active.usage.clone(),
    }
}

const fn session_outcome(outcome: ExecutionOutcome) -> SessionOutcome {
    match outcome {
        ExecutionOutcome::Completed => SessionOutcome::Completed,
        ExecutionOutcome::Aborted => SessionOutcome::Aborted,
        ExecutionOutcome::Failed => SessionOutcome::Failed,
    }
}

pub(super) async fn gateway_ready(
    state: &GatewayState,
) -> std::result::Result<ReadyPayload, Rejection> {
    let config = state
        .config
        .lock()
        .map_err(|_| internal("gateway configuration lock is poisoned"))?
        .clone();
    let routes =
        configured_model_routes(&config, &state.store, &state.credentials).map_err(internal)?;
    let models: Vec<_> = routes.iter().map(|route| route.choice.clone()).collect();
    let model_providers = routes
        .into_iter()
        .map(|route| (route.choice.route, route.provider.instance))
        .collect();
    let middleware_features = crate::middleware_manifest::features(&models);
    let extensions = crate::extensions::records(&config);
    let mut contributions = state.contributions.clone();
    contributions.push(
        state
            .scratchpad
            .global_contribution()
            .await
            .map_err(internal)?,
    );
    Ok(ReadyPayload {
        machine_name: local_machine_name().map_err(internal)?,
        bots: state.bots.bots().map_err(internal)?,
        sessions: session_catalog(&state.checkpoints, &state.activities)
            .await
            .map_err(internal)?,
        swarms: state.swarm.records().await.map_err(internal)?,
        providers: provider_statuses(),
        provider_instances: provider_instances(&config, &state.store, &state.credentials)
            .map_err(internal)?,
        models,
        model_providers,
        bot_defaults: config.bot_defaults,
        middleware_features,
        extensions,
        contributions,
        max_active_sessions: MAX_ACTIVE_SESSIONS,
        session_file_limits: session_file_limits(),
    })
}

pub(super) fn local_machine_name() -> Result<String> {
    let name = nix::unistd::gethostname()
        .map_err(|error| Error::Config(format!("failed to read the machine hostname: {error}")))?
        .into_string()
        .map_err(|_| Error::Config("the machine hostname is not valid UTF-8".into()))?;
    let name = name.trim();
    if name.is_empty() || name.len() > 255 || name.chars().any(char::is_control) {
        return Err(Error::Config("the machine hostname is invalid".into()));
    }
    Ok(name.to_owned())
}
