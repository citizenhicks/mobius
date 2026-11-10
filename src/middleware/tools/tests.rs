//! Tool registry, execution, patching, and presentation tests.

use super::*;

fn test_sandbox() -> Arc<Sandbox> {
    test_sandbox_at(std::path::Path::new("."))
}

fn test_sandbox_at(root: &std::path::Path) -> Arc<Sandbox> {
    Arc::new(Sandbox::new(
        Arc::new(crate::backend::sandbox::local::LocalSandbox::new(root).expect("sandbox")),
        crate::backend::sandbox::ApprovalPolicy::Ask,
    ))
}

/// Executes one batch as a user's turn on the `test` route.
async fn run_calls(
    catalog: &Catalog,
    calls: impl IntoIterator<Item = BoundToolCall>,
    sandbox: Arc<Sandbox>,
    permissions: &SandboxPermissions,
) -> Vec<ToolResult> {
    execute_batch(
        catalog,
        calls,
        sandbox,
        permissions,
        &"turn".into(),
        &"test".into(),
        &crate::protocol::MessageAuthor::User.into(),
    )
    .await
}

fn test_permissions(mutation_call_ids: &[&str]) -> SandboxPermissions {
    SandboxPermissions::restore(
        &"session".parse().expect("session ID"),
        crate::backend::sandbox::SandboxMode::WorkspaceWrite,
        crate::backend::sandbox::NetworkAccess::Denied,
        mutation_call_ids.iter().map(|call_id| (*call_id).into()),
    )
}

fn finalize_and_bind(catalog: &mut Catalog, calls: &[ToolCall]) -> Vec<BoundToolCall> {
    catalog.register_search().expect("register tools_search");
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
mod read_image;
mod registry;
