//! Gateway-wide dictation shares route discovery, credential lifetime, and config publication with models.

use mobius::backend::model::ModelCredentialLifetime;
use mobius::backend::model::dictation::{DictationAudio, DictationLanguage, DictationProvider};
use mobius::backend::model::provider::provider;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::{GatewayHost, Rejection, finish_publication, internal};
use crate::harness::mobius::assembly::resolve_credential;
use crate::harness::mobius::provider_catalog::{CatalogRoute, CredentialedInstances, media_routes};
use crate::wire::{DictationCatalog, DictationChoice, DictationTarget, ReadyPayload};
use std::sync::LazyLock;

fn selected(state: &super::state::GatewayState, route: &str) -> Result<CatalogRoute, Rejection> {
    let config = state.config()?;
    let available = CredentialedInstances::resolve(
        &config.configured_providers,
        &state.store,
        &state.credentials,
    )
    .map_err(internal)?;
    media_routes(&config.configured_providers, Some(&available))
        .map_err(internal)?
        .dictations
        .into_iter()
        .find(|entry| &*entry.choice.route == route)
        .ok_or_else(|| {
            Rejection::new(
                "dictation_unavailable",
                "This dictation route is unavailable",
            )
        })
}

/// Preserve an unavailable pin for editing while keeping it out of the capture path.
pub(super) fn catalog(
    selection: Option<DictationTarget>,
    routes: Vec<CatalogRoute>,
) -> crate::Result<DictationCatalog> {
    use crate::harness::mobius::provider_catalog::dictation_mode_label;
    use mobius::backend::model::dictation::DictationMode;
    let on_device = DictationChoice {
        target: Some(DictationTarget::OnDevice {}),
        label: Arc::clone(&LABELS.on_device),
        detail: vec![dictation_mode_label(Some(DictationMode::Live)).into()],
    };
    let mut choices = Vec::with_capacity(routes.len() + 2);
    let mut first_live = None;
    for entry in routes {
        let definition = provider(&entry.provider.provider)?;
        let base = entry.operation_base(
            definition,
            definition.resolve_base_url(entry.provider.base_url.as_ref()),
        )?;
        let mode = definition.dictation_mode(&entry.choice.model, &base);
        if mode == Some(DictationMode::Live) && first_live.is_none() {
            first_live = Some(choices.len());
        }
        choices.push(DictationChoice {
            target: Some(DictationTarget::Route {
                id: entry.choice.route,
            }),
            label: entry.choice.group,
            detail: vec![dictation_mode_label(mode).into()],
        });
    }
    // Fall back to the first remote model, then the local recognizer.
    let auto = first_live
        .and_then(|i| choices.get(i))
        .or_else(|| choices.first())
        .unwrap_or(&on_device);
    let automatic = DictationChoice {
        target: None,
        label: Arc::clone(&LABELS.automatic),
        detail: std::iter::once(&auto.label)
            .chain(&auto.detail)
            .map(Arc::clone)
            .collect(),
    };
    let chosen = match &selection {
        None => Some(auto),
        Some(_) => std::iter::once(&on_device)
            .chain(&choices)
            .find(|choice| choice.target == selection),
    };
    let resolved = chosen.and_then(|c| c.target.clone()); // rules-allow: Ready owns the selected target and independently selectable rows
    if resolved.is_none()
        && let Some(DictationTarget::Route { id }) = &selection
    {
        choices.push(DictationChoice {
            target: Some(DictationTarget::Route { id: Arc::clone(id) }),
            label: Arc::clone(id),
            detail: vec![Arc::clone(&LABELS.unavailable)],
        });
    }
    choices.splice(..0, [automatic, on_device]);
    Ok(DictationCatalog {
        selection,
        resolved,
        choices,
    })
}

#[derive(serde::Deserialize)]
struct DictationLabels {
    automatic: Arc<str>,
    on_device: Arc<str>,
    unavailable: Arc<str>,
}

static LABELS: LazyLock<DictationLabels> =
    LazyLock::new(|| mobius::config::embedded(include_str!("dictation.toml")));

impl GatewayHost {
    #[expect(
        clippy::significant_drop_tightening,
        reason = "publish_ready_after_unlock takes ownership of the guard and releases it"
    )]
    pub(crate) async fn configure_dictation(
        &self,
        target: Option<DictationTarget>,
    ) -> Result<Arc<ReadyPayload>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        if let Some(DictationTarget::Route { id }) = &target {
            selected(&state, id)?;
        }
        let publication = {
            let mut current = state.config()?;
            let next = current.replacing_dictation(target);
            let result =
                crate::publication::Outcome::applied(state.store.save(&next)).map_err(internal)?;
            *current = next.into_inner();
            result
        };
        let ready = self.publish_ready_after_unlock(state).await;
        finish_publication(publication.confirm(), ready)
    }

    async fn dictation_provider(
        &self,
        route: &str,
    ) -> Result<(DictationProvider, ModelCredentialLifetime, Arc<str>), Rejection> {
        let state = self.state.lock().await;
        let route = selected(&state, route)?;
        let definition = provider(&route.provider.provider).map_err(internal)?;
        let base = definition.resolve_base_url(route.provider.base_url.as_ref());
        let transport = state.config()?.model_transport;
        let (credential, lifetime) = resolve_credential(
            &route.provider,
            definition,
            &base,
            &state.store,
            &state.credentials,
            &transport,
        )
        .map_err(internal)?;
        drop(state);
        let base = route.operation_base(definition, base).map_err(internal)?;
        let provider =
            DictationProvider::new(definition, base, credential, &transport).map_err(internal)?;
        Ok((provider, lifetime, route.choice.model))
    }

    pub(crate) async fn transcribe(
        &self,
        route: &str,
        language: Option<&DictationLanguage>,
        input: DictationAudio,
        updates: mpsc::Sender<String>,
    ) -> Result<String, Rejection> {
        let (provider, lifetime, model) = self.dictation_provider(route).await?;
        provider
            .transcribe_with_lifetime(&lifetime, &model, language, input, updates)
            .await
            .map_err(|error| {
                Rejection::new(
                    "dictation_failed",
                    super::rejection::provider_failure_reason(&error.into()),
                )
            })
    }
}
