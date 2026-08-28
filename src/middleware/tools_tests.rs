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

#[path = "tools_tests/apply_patch.rs"]
mod apply_patch;
#[path = "tools_tests/background_commands.rs"]
mod background_commands;
#[path = "tools_tests/batch_scheduling.rs"]
mod batch_scheduling;
#[path = "tools_tests/discovery.rs"]
mod discovery;
#[path = "tools_tests/dispatch_safety.rs"]
mod dispatch_safety;
#[path = "tools_tests/presentation.rs"]
mod presentation;
#[path = "tools_tests/registry.rs"]
mod registry;
