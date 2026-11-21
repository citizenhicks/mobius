use super::catalog::gateway_catalog;

use super::SESSION_PAGE_SIZE;
use super::rejection::{Rejection, internal};
use super::state::GatewayReadySnapshot;
use crate::harness::mobius::provider_catalog::{
    CredentialedInstances, configured_model_routes, provider_instances, provider_statuses,
    route_choices,
};
use crate::wire::{ReadyPayload, RunStats};
use crate::{Error, Result};
use mobius::backend::checkpoint::{CheckpointStore, SessionPageRequest, SessionSummary};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use mobius::backend::session_files::session_file_limits;

use crate::wire::{AgentCatalog, READY_SECTIONS};

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

pub(super) fn gateway_run_stats(sessions: &[SessionSummary]) -> Result<RunStats> {
    let mut totals = RunStats::default();
    for session in sessions {
        totals
            .completed
            .checked_add(&session.execution_stats)
            .ok_or_else(|| {
                Error::Internal("gateway execution statistics exceed the supported range".into())
            })?;
    }
    Ok(totals)
}

pub(super) async fn gateway_ready(
    state: GatewayReadySnapshot,
) -> std::result::Result<ReadyPayload, Rejection> {
    let credentials = state.credential_catalog_gate.lock().await;
    let credentialed = CredentialedInstances::resolve(
        &state.configured_providers,
        &state.store,
        &state.credentials,
    )
    .map_err(internal)?;
    let routes = configured_model_routes(
        &state.configured_providers,
        state
            .bot_defaults
            .as_ref()
            .map(|defaults| defaults.config.provider.instance.as_str()),
        &credentialed,
    )
    .map_err(internal)?;
    let provider_instances = provider_instances(
        &state.configured_providers,
        &credentialed,
        &state.credentials,
    )
    .map_err(internal)?;
    let media = crate::harness::mobius::provider_catalog::media_routes(
        &state.configured_providers,
        Some(&credentialed),
    )
    .map_err(internal)?;
    let voices = crate::harness::mobius::provider_catalog::VoiceRoutes::from_routes(&media.voices)
        .map_err(internal)?;
    let mut model_providers = BTreeMap::new();
    let models = route_choices(routes, &mut model_providers);
    let image_models = route_choices(media.images, &mut model_providers);
    let voice_models = route_choices(media.voices, &mut model_providers);
    let decision_models = route_choices(media.decisions, &mut model_providers);
    let subagent_ceilings = state.subagent_ceilings;
    let catalogs = mobius::middleware::manifest::ModelCatalogs {
        models: &models,
        images: &image_models,
        voices: &voice_models,
        decisions: &decision_models,
    };
    let middleware_features =
        crate::harness::mobius::middleware_manifest::features(catalogs, subagent_ceilings);
    let auto_routing = {
        use crate::config::auto_routing;
        let values = auto_routing::effective(&state.auto_routing);
        let targets = auto_routing::targets(&values, &decision_models);
        crate::wire::AutoRoutingCatalog {
            feature: auto_routing::feature(catalogs),
            values,
            targets,
        }
    };
    let default_capabilities = state
        .bot_defaults
        .as_ref()
        .map(|defaults| {
            let middleware = &defaults.config.middleware;
            crate::harness::mobius::middleware_manifest::string_setting(
                middleware,
                mobius::middleware::voice::MANIFEST.id,
                "model",
            )
            .map(|model| {
                crate::harness::mobius::assembly::bot_capabilities(middleware, model, &voices)
            })
        })
        .transpose()
        .map_err(internal)?;
    state.bots.set_voice_routes(voices).map_err(internal)?;
    let bots = state.bots.bots().map_err(internal)?;
    drop(credentials);
    let extensions = state.extensions;
    let contributions = super::with_memory_contribution(&state.memory, state.contributions)
        .await
        .map_err(internal)?;
    let (sessions, background_approvals) = gateway_catalog(&state.checkpoints, &state.activities)
        .await
        .map_err(internal)?;
    let harnesses = crate::harness::descriptors();
    let mut ready = ReadyPayload {
        gateway_version: env!("CARGO_PKG_VERSION").into(),
        computer_view: state.computer_view,
        machine_name: local_machine_name().map_err(internal)?,
        bots,
        sessions,
        background_approvals,
        providers: provider_statuses(),
        provider_instances,
        agent_catalog: AgentCatalog {
            default_capabilities,
            bot_defaults: state.bot_defaults,
            models,
            model_providers,
            image_models,
            voice_models,
            decision_models,
            auto_routing,
            middleware_features,
            extensions,
            contributions,
            limits: crate::wire::CompositionLimits {
                min_model_steps: 1,
                default_model_steps: crate::harness::mobius::DEFAULT_MAX_MODEL_STEPS as u64,
                model_steps_step: 100,
                max_system_prompt_bytes: crate::config::MAX_SYSTEM_PROMPT_BYTES as u64,
                min_routine_interval_seconds: crate::bots::MIN_ROUTINE_INTERVAL_SECONDS,
            },
        },
        harnesses,
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
        .map_err(|error| Error::Internal(format!("failed to read the machine hostname: {error}")))?
        .into_string()
        .map_err(|name| {
            Error::Internal(format!(
                "the machine hostname is not valid UTF-8: {}",
                name.display()
            ))
        })?;
    let name = name.trim();
    if !crate::single_line_field(name, 255) {
        return Err(Error::Internal("the machine hostname is invalid".into()));
    }
    Ok(name.to_owned())
}
