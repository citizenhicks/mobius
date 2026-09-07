//! Tool registry, execution, patching, and presentation tests.

use super::*;

fn test_sandbox() -> Arc<Sandbox> {
    Arc::new(Sandbox::new(
        Arc::new(crate::backend::sandbox::local::LocalSandbox::new(".").expect("sandbox")),
        crate::backend::sandbox::ApprovalPolicy::Ask,
    ))
}

fn test_permissions(mutation_call_ids: &[&str]) -> SandboxPermissions {
    SandboxPermissions::restore(
        "session",
        crate::backend::sandbox::SandboxMode::WorkspaceWrite,
        crate::backend::sandbox::NetworkAccess::Denied,
        mutation_call_ids.iter().map(|call_id| (*call_id).into()),
    )
}

fn finalize_and_bind(catalog: &mut Catalog, calls: &[ToolCall]) -> Vec<BoundToolCall> {
    catalog.finalize().expect("finalize catalog");
    let materialized = catalog
        .deferred_definitions()
        .iter()
        .map(|definition| definition.name.clone())
        .collect();
    calls
        .iter()
        .cloned()
        .map(|call| {
            catalog
                .bind_call(call, &materialized, &materialized)
                .expect("bind call")
        })
        .collect()
}

mod apply_patch;
mod background_commands;
mod batch_scheduling;
mod discovery;
mod dispatch_safety;
mod presentation;
mod registry;
mod view_image;
