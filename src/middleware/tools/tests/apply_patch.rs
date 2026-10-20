use super::*;
use diffy::Patch;

#[test]
fn apply_patch_preserves_crlf_line_endings() {
    let patch = parse_patch_document(
        "*** Begin Patch\n*** Update File: note.txt\n@@\n-old\n+new\n*** End Patch\n",
    )
    .expect("patch document");

    assert_eq!(
        apply_patch_document("first\r\nold\r\nlast\r\n", &patch).expect("applied patch"),
        "first\r\nnew\r\nlast\r\n"
    );
}

#[tokio::test]
async fn apply_patch_uses_the_active_sandbox_policy() {
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    let path = outside.path().join("note.txt");
    let patch_path = path.to_str().expect("UTF-8 outside path");
    std::fs::write(&path, "old\n").expect("write fixture");
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(
            crate::backend::sandbox::local::LocalSandbox::new(workspace.path())
                .expect("local sandbox"),
        ),
        crate::backend::sandbox::ApprovalPolicy::Ask,
    ));
    let patch = serde_json::json!({
        "patch": format!("*** Begin Patch\n*** Update File: {patch_path}\n@@\n-old\n+new\n*** End Patch\n")
    });

    let workspace_error = ApplyPatch
        .call(
            ToolContext::new(
                Arc::clone(&sandbox),
                SandboxPermissions::restore(
                    &"session".parse().expect("session ID"),
                    crate::backend::sandbox::SandboxMode::WorkspaceWrite,
                    crate::backend::sandbox::NetworkAccess::Denied,
                    ["patch".into()],
                )
                .for_call("patch"),
                "turn",
            ),
            patch.clone(),
        )
        .await
        .expect_err("workspace policy must reject outside path");
    assert!(workspace_error.to_string().contains(patch_path));
    assert_eq!(
        std::fs::read_to_string(&path).expect("unchanged file"),
        "old\n"
    );

    ApplyPatch
        .call(
            ToolContext::new(
                sandbox,
                SandboxPermissions::restore(
                    &"session".parse().expect("session ID"),
                    crate::backend::sandbox::SandboxMode::DangerFullAccess,
                    crate::backend::sandbox::NetworkAccess::Allowed,
                    ["patch".into()],
                )
                .for_call("patch"),
                "turn",
            ),
            patch,
        )
        .await
        .expect("full access patch");

    assert_eq!(
        std::fs::read_to_string(path).expect("patched file"),
        "new\n"
    );
}

#[test]
fn apply_patch_end_of_file_changes_the_final_duplicate() {
    let patch = parse_patch_document(
        "*** Begin Patch\n*** Update File: note.txt\n@@\n-old\n+new\n*** End of File\n\n*** End Patch\n",
    )
    .expect("patch document");

    assert_eq!(
        apply_patch_document("old\nmiddle\nold\n", &patch).expect("applied patch"),
        "old\nmiddle\nnew\n"
    );
}

#[test]
fn apply_patch_rejects_pathological_fuzzy_matching() {
    let content = "same\n".repeat(20_000);
    let mut patch = String::from("--- ignored\n+++ ignored\n@@ -1,1000 +1,1000 @@\n");
    patch.push_str(&" same\n".repeat(999));
    patch.push_str("-same\n+changed\n");
    let patch = Patch::from_str(&patch).expect("patch");

    assert!(validate_patch_complexity(&content, &patch, &mut 0).is_err());
}
