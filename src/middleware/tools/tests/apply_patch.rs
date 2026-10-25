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

#[test]
fn apply_patch_begin_renders_its_envelope_as_a_linked_unified_diff() {
    let block = ApplyPatch
        .render(&EventMsg::ToolCallBegin(crate::protocol::ToolCallBeginEvent {
            turn_id: "turn".into(),
            call_id: "patch".into(),
            name: "apply_patch".into(),
            arguments: serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/a b.rs\n@@ fn x\n-a\n+b\n c\n@@\n+d\n*** End Patch"
            }),
        }))
        .expect("patch heading");
    assert_eq!(
        block.format,
        crate::protocol::FrontendBlockFormat::UnifiedDiff
    );
    assert_eq!(
        block.text,
        "--- src/a b.rs\n+++ src/a b.rs\n@@ -1,2 +1,2 @@ fn x\n-a\n+b\n c\n@@ -4,0 +4,1 @@\n+d\n"
    );
    assert_eq!(
        block.links,
        [crate::protocol::FrontendLink {
            label: "a b.rs".into(),
            href: "src%2Fa%20b.rs".into(),
        }]
    );
    assert!(Patch::from_str(&block.text).is_ok());
}

#[test]
fn apply_patch_approval_previews_the_diff_its_tool_renders() {
    let stack =
        crate::middleware::MiddlewareStack::new(vec![Arc::new(Tools::new(vec![Arc::new(
            ApplyPatch,
        )]))])
        .expect("middleware stack");
    let frontend = crate::middleware::FrontendExtensions::new(
        Arc::new(stack),
        "session".parse().expect("session ID"),
    )
    .expect("frontend");
    let arguments = serde_json::json!({
        "patch": "*** Begin Patch\n*** Update File: a.rs\n@@\n-a\n+b\n*** End Patch"
    });
    let mut request = crate::protocol::ExecApprovalRequestEvent {
        id: "request".into(),
        turn_id: "turn".into(),
        calls: vec![crate::protocol::ApprovalCall {
            call_id: "patch".into(),
            name: "apply_patch".into(),
            arguments: arguments.clone(),
            preview: None,
        }],
        reason: String::new(),
    };
    frontend.preview_approvals(&mut request);
    let call = &request.calls[0];
    let preview = call.preview.as_ref().expect("approval preview");
    assert_eq!(
        preview.format,
        crate::protocol::FrontendBlockFormat::UnifiedDiff
    );
    assert_eq!(call.arguments, arguments);
}
