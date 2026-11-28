//! Group operations inherit the provider origin and its compatible adapters.
use super::selected_base_url;
use crate::Result;
use crate::config::ConfiguredProvider;
use crate::wire::{GroupEndpointOverride, ProviderModelKind};
use mobius::backend::model::provider::ProviderDefinition;
use std::collections::BTreeSet;

pub(crate) fn validate_endpoints(
    definition: &ProviderDefinition,
    configured: &ConfiguredProvider,
) -> Result<()> {
    if configured.group_endpoints.is_empty() {
        return Ok(());
    }
    let base = selected_base_url(definition, &configured.selection);
    let mut seen = BTreeSet::new();
    for e in &configured.group_endpoints {
        if !seen.insert(e.kind) {
            return Err(crate::Error::Invalid(
                "Each model group accepts one endpoint override".into(),
            ));
        }
        let resolved = definition.with_group_endpoint(e.kind, base, &e.api, &e.path)?;
        if e.kind == ProviderModelKind::Language {
            definition.validate_endpoint_web_search(&resolved, configured.selection.web_search)?;
        }
    }
    Ok(())
}

pub(super) fn endpoint(
    configured: &ConfiguredProvider,
    kind: ProviderModelKind,
) -> Option<GroupEndpointOverride> {
    configured
        .group_endpoints
        .iter()
        .find(|e| e.kind == kind)
        .cloned() // rules-allow: the adapter takes the operation snapshot outside the gateway config lock
}
