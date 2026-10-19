use super::{
    ActiveExecution, Arc, CheckpointStore, Error, ExecutionRecord, GatewayReadySnapshot, HashMap,
    HashSet, ReadyPayload, Rejection, Result, RunStats, RunSummary, SESSION_PAGE_SIZE,
    SessionCatalogMetadata, SessionPageRequest, SessionRunGroup, SessionSummary, Utc,
    configured_model_routes, gateway_catalog, internal, provider_instances, provider_statuses,
};
use std::collections::{BTreeMap, BTreeSet};

use mobius::backend::model::provider::{ProviderAuth, provider};
use mobius::backend::session_files::session_file_limits;

use crate::config::{ConfigStore, GatewayConfig};
use crate::wire::{ProviderUsage, READY_SECTIONS};

pub(super) async fn gateway_session_summaries(
    checkpoints: &Arc<dyn CheckpointStore>,
) -> Result<Vec<SessionSummary>> {
    let mut cursor = None;
    let mut sessions = Vec::new();
    loop {
        let page = checkpoints
            .list_sessions_page(SessionPageRequest {
                owner_id: None,
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

pub(super) fn provider_usage(
    config: &GatewayConfig,
    store: &ConfigStore,
) -> impl std::future::Future<Output = Result<Vec<ProviderUsage>>> + Send + use<> {
    let provider_ids = config
        .configured_providers
        .values()
        .map(|configured| configured.selection.provider.clone()) // rules-allow: the usage future outlives the configuration borrow
        .collect::<BTreeSet<_>>();
    let auth_path = store.provider_auth_path();
    let transport = config.model_transport;
    async move {
        let mut requests = Vec::new();
        for provider_id in provider_ids {
            let definition = provider(&provider_id)?;
            let ProviderAuth::Browser(auth) = definition.auth() else {
                continue;
            };
            if !auth
                .configured(&auth_path)
                .is_ok_and(|configured| configured)
            {
                continue;
            }
            let Some(fetch) = auth.usage_limits_with_transport(&auth_path, transport) else {
                continue;
            };
            requests.push((provider_id, fetch));
        }
        Ok(futures_util::future::join_all(requests.into_iter().map(
            |(provider, fetch)| async move {
                match fetch.await {
                    Ok(limits) => ProviderUsage {
                        provider,
                        limits: Some(limits),
                        error: None,
                    },
                    Err(error) => ProviderUsage {
                        provider,
                        limits: None,
                        error: Some(error.to_string()),
                    },
                }
            },
        ))
        .await)
    }
}

pub(super) fn session_tree_ids(
    root_session_id: &str,
    sessions: &[SessionSummary],
) -> Option<Vec<String>> {
    sessions
        .iter()
        .any(|session| *session.session_id == *root_session_id)
        .then_some(())?;
    let mut seen = HashSet::from([root_session_id]);
    let mut ordered = vec![root_session_id.to_owned()];
    loop {
        let mut changed = false;
        for session in sessions {
            if seen.contains(session.session_id.as_ref())
                || !session
                    .parent_session_id
                    .as_ref()
                    .is_some_and(|parent| seen.contains(parent.as_str()))
            {
                continue;
            }
            seen.insert(session.session_id.as_ref());
            ordered.push(String::from(&*session.session_id));
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
        totals
            .completed
            .checked_add(&session.execution_stats)
            .ok_or_else(|| {
                Error::Config("gateway execution statistics exceed the supported range".into())
            })?;
    }
    Ok(totals)
}

pub(super) fn run_summary(record: ExecutionRecord) -> RunSummary {
    RunSummary {
        session_id: record.session_id,
        submission_id: record.submission_id,
        turn_id: record.turn_id,
        started_at_ms: record.started_at_ms,
        finished_at_ms: Some(record.finished_at_ms),
        elapsed_ms: record.elapsed_ms,
        outcome: Some(record.outcome),
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
        .map(|session| (session.session_id.as_ref(), session))
        .collect::<HashMap<_, _>>();
    let mut groups: Vec<SessionRunGroup> = Vec::new();
    for record in records {
        let Some(root) = visible_session(&record.session_id, &sessions_by_id) else {
            continue;
        };
        let run = run_summary(record);
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.session_id == root.session_id)
        {
            group.runs.push(run);
        } else {
            groups.push(SessionRunGroup {
                session_id: Arc::clone(&root.session_id),
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
        .get(&*session.session_id)
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

pub(super) fn active_run_summary(session_id: &Arc<str>, active: &ActiveExecution) -> RunSummary {
    let elapsed_ms = Utc::now()
        .timestamp_millis()
        .checked_sub(active.started_at_ms)
        .and_then(|elapsed| u64::try_from(elapsed).ok())
        .unwrap_or_default();
    RunSummary {
        session_id: Arc::clone(session_id),
        submission_id: Arc::clone(&active.submission_id),
        turn_id: Arc::clone(&active.turn_id),
        started_at_ms: active.started_at_ms,
        finished_at_ms: None,
        elapsed_ms,
        outcome: None,
        model_calls: active.model_calls,
        tool_calls: active.tool_calls,
        failed_tool_calls: active.failed_tool_calls,
        usage: active.usage,
    }
}

pub(super) async fn gateway_ready(
    state: GatewayReadySnapshot,
) -> std::result::Result<ReadyPayload, Rejection> {
    let (routes, provider_instances, media) = {
        let _credentials = state.credential_catalog_gate.lock().await;
        (
            configured_model_routes(
                &state.configured_providers,
                state
                    .bot_defaults
                    .as_ref()
                    .map(|defaults| defaults.config.provider.instance.as_str()),
                &state.store,
                &state.credentials,
            )
            .map_err(internal)?,
            provider_instances(
                &state.configured_providers,
                &state.store,
                &state.credentials,
            )
            .map_err(internal)?,
            crate::provider_catalog::media_routes(
                &state.configured_providers,
                Some((&state.store, &state.credentials)),
            )
            .map_err(internal)?,
        )
    };
    let mut model_providers = BTreeMap::new();
    let models: Vec<_> = routes
        .into_iter()
        .map(|route| {
            model_providers.insert(Arc::clone(&route.choice.route), route.provider.instance);
            route.choice
        })
        .collect();
    let mut media_choices = |routes: Vec<crate::provider_catalog::CatalogRoute>| {
        routes
            .into_iter()
            .map(|media| {
                model_providers.insert(Arc::clone(&media.choice.route), media.provider.instance);
                media.choice
            })
            .collect::<Vec<_>>()
    };
    let image_models = media_choices(media.images);
    let voice_models = media_choices(media.voices);
    let subagent_ceilings = state.subagent_ceilings;
    let middleware_features = crate::middleware_manifest::features(
        mobius::middleware::manifest::ModelCatalogs {
            models: &models,
            images: &image_models,
            voices: &voice_models,
        },
        subagent_ceilings,
    );
    let bots = state.bots.bots().map_err(internal)?;
    let extensions = state.extensions;
    let mut contributions = state.contributions;
    contributions.push(
        state
            .scratchpad
            .global_contribution()
            .await
            .map_err(internal)?,
    );
    let (sessions, background_approvals) = gateway_catalog(&state.checkpoints, &state.activities)
        .await
        .map_err(internal)?;
    let mut ready = ReadyPayload {
        gateway_version: env!("CARGO_PKG_VERSION").into(),
        computer_view: state.computer_view,
        machine_name: local_machine_name().map_err(internal)?,
        bots,
        sessions,
        background_approvals,
        providers: provider_statuses(),
        provider_instances,
        models,
        model_providers,
        image_models,
        voice_models,
        bot_defaults: state.bot_defaults,
        middleware_features,
        extensions,
        contributions,
        max_active_sessions: state.max_active_sessions,
        session_file_limits: session_file_limits(),
        revisions: BTreeMap::new(),
        omitted: BTreeSet::new(),
    };
    ready.revisions = READY_SECTIONS
        .into_iter()
        .map(|section| (section, ready.revision(section)))
        .collect();
    Ok(ready)
}

pub(super) fn local_machine_name() -> Result<String> {
    let name = nix::unistd::gethostname()
        .map_err(|error| Error::Config(format!("failed to read the machine hostname: {error}")))?
        .into_string()
        .map_err(|name| {
            Error::Config(format!(
                "the machine hostname is not valid UTF-8: {}",
                name.display()
            ))
        })?;
    let name = name.trim();
    if name.is_empty() || name.len() > 255 || name.chars().any(char::is_control) {
        return Err(Error::Config("the machine hostname is invalid".into()));
    }
    Ok(name.to_owned())
}
