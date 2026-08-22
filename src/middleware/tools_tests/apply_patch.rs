use super::*;

async fn rejected_patch(content: &str, patch: &str) -> String {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("note.txt"), content).expect("write fixture");
    let context = ToolContext {
        sandbox: Arc::new(Sandbox::new(
            Arc::new(
                crate::backend::sandbox::local::LocalSandbox::new(workspace.path())
                    .expect("local sandbox"),
            ),
            crate::backend::sandbox::ApprovalPolicy::Ask,
        )),
        permissions: SandboxPermissions::restore(
            "session",
            crate::backend::sandbox::SandboxMode::WorkspaceWrite,
            crate::backend::sandbox::NetworkAccess::Denied,
            ["patch".into()],
        )
        .for_call("patch"),
        turn_id: "turn".into(),
    };

    ApplyPatch
        .call(context, serde_json::json!({"patch": patch}))
        .await
        .expect_err("patch rejection")
        .to_string()
}

#[test]
fn apply_patch_accepts_only_one_patch_document_argument() {
    let parameters = ApplyPatch.definition().parameters;

    assert_eq!(parameters["required"], serde_json::json!(["patch"]));
    assert!(parameters["properties"].get("patch").is_some());
    assert!(parameters["properties"].get("path").is_none());
    assert!(
        serde_json::from_value::<ApplyPatchArgs>(serde_json::json!({
            "path": "note.txt",
            "patch": "invalid"
        }))
        .is_err()
    );
}

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
async fn apply_patch_returns_a_unified_diff_after_writing() {
    let workspace = tempfile::tempdir().expect("workspace");
    let path = workspace.path().join("note.txt");
    std::fs::write(&path, "first\nold\nlast\n").expect("write fixture");
    let mut catalog = Catalog::default();
    catalog
        .register(Arc::new(ApplyPatch))
        .expect("register patch tool");
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(
            crate::backend::sandbox::local::LocalSandbox::new(workspace.path())
                .expect("local sandbox"),
        ),
        crate::backend::sandbox::ApprovalPolicy::Ask,
    ));
    let permissions = SandboxPermissions::restore(
        "session",
        crate::backend::sandbox::SandboxMode::WorkspaceWrite,
        crate::backend::sandbox::NetworkAccess::Denied,
        ["call-1".into(), "call-2".into()],
    );

    let result = execute_batch(
        &catalog,
        &[ToolCall {
            call_id: "call-1".into(),
            name: "apply_patch".into(),
            arguments: serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: note.txt\n@@\n first\n-old\n+new\n last\n*** End Patch\n"
            }),
        }],
        Arc::clone(&sandbox),
        &permissions,
        "turn",
    )
    .await
    .pop()
    .expect("tool result");

    assert!(!result.is_error, "{}", result.output);
    let patch = diffy::Patch::from_str(&result.output).expect("unified diff");
    assert_eq!(
        (patch.original(), patch.modified()),
        (Some("note.txt"), Some("note.txt"))
    );
    assert!(result.output.contains("-old\n+new\n"));
    assert_eq!(
        diffy::apply("first\nold\nlast\n", &patch).expect("apply generated patch"),
        "first\nnew\nlast\n"
    );
    assert_eq!(
        std::fs::read_to_string(path).expect("read edited file"),
        "first\nnew\nlast\n"
    );

    let no_op = execute_batch(
        &catalog,
        &[ToolCall {
            call_id: "call-2".into(),
            name: "apply_patch".into(),
            arguments: serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: note.txt\n@@\n first\n-new\n+new\n last\n*** End Patch\n"
            }),
        }],
        sandbox,
        &permissions,
        "turn",
    )
    .await
    .pop()
    .expect("no-op result");
    assert!(no_op.is_error);
    assert_eq!(
        no_op.output,
        "tool error: Patch rejected: patch applies but makes no changes."
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("note.txt")).expect("read unchanged file"),
        "first\nnew\nlast\n"
    );
}

#[tokio::test]
async fn apply_patch_reports_failed_context() {
    let context = "Project Prometheus is a cloud-hosted modeling workspace.";
    let content = format!("{}{context}\nactual next line\n", "padding\n".repeat(437));
    let error = rejected_patch(
        &content,
        &format!(
            "*** Begin Patch\n*** Update File: note.txt\n@@\n {context}\n-expected next line\n+replacement\n*** End Patch\n"
        ),
    )
    .await;

    assert!(
        error.contains("Patch rejected: no hunks matched the file"),
        "{error}"
    );
    assert!(error.contains(context));
}

#[tokio::test]
async fn apply_patch_supports_context_headers_and_multiple_changes() {
    let workspace = tempfile::tempdir().expect("workspace");
    let path = workspace.path().join("runtime.rs");
    let content = "    fn first\ncomment\nold one\nmiddle\nsection\ncomment\nold two\nlast\n";
    std::fs::write(&path, content).expect("write fixture");
    let context = ToolContext {
        sandbox: Arc::new(Sandbox::new(
            Arc::new(
                crate::backend::sandbox::local::LocalSandbox::new(workspace.path())
                    .expect("local sandbox"),
            ),
            crate::backend::sandbox::ApprovalPolicy::Ask,
        )),
        permissions: SandboxPermissions::restore(
            "session",
            crate::backend::sandbox::SandboxMode::WorkspaceWrite,
            crate::backend::sandbox::NetworkAccess::Denied,
            ["patch".into()],
        )
        .for_call("patch"),
        turn_id: "turn".into(),
    };

    ApplyPatch
        .call(
            context,
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: runtime.rs\n@@ fn first\n-old one\n+new one\n@@ section\n-old two\n+new two\n last\n*** End of File\n*** End Patch\n"
            }),
        )
        .await
        .expect("applied patch");

    assert_eq!(
        std::fs::read_to_string(path).expect("read patched file"),
        "    fn first\ncomment\nnew one\nmiddle\nsection\ncomment\nnew two\nlast\n"
    );
}

#[test]
fn apply_patch_keeps_changes_in_document_order() {
    let patch = parse_patch_document(
        "*** Begin Patch\n*** Update File: note.txt\n@@\n-A\n+a\n@@\n-B\n+b\n*** End Patch\n",
    )
    .expect("patch document");

    assert_eq!(
        apply_patch_document("B\nA\nB\n", &patch).expect("applied patch"),
        "B\na\nb\n"
    );
}

#[test]
fn apply_patch_context_only_changes_advance_document_order() {
    let patch = parse_patch_document(
        "*** Begin Patch\n*** Update File: note.txt\n@@\n A\n@@\n-B\n+b\n*** End Patch\n",
    )
    .expect("patch document");

    assert_eq!(
        apply_patch_document("B\nA\nB\n", &patch).expect("applied patch"),
        "B\nA\nb\n"
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
fn apply_patch_matches_the_whole_change_body() {
    let patch = parse_patch_document(
        "*** Begin Patch\n*** Update File: note.txt\n@@\n-old\n+new\n one\n two\n three\n four\n*** End Patch\n",
    )
    .expect("patch document");

    assert_eq!(
        apply_patch_document(
            "old\none\ntwo\nthree\nwrong\nmiddle\nold\none\ntwo\nthree\nfour\n",
            &patch,
        )
        .expect("applied patch"),
        "old\none\ntwo\nthree\nwrong\nmiddle\nnew\none\ntwo\nthree\nfour\n"
    );
}

#[tokio::test]
async fn apply_patch_rejects_unwrapped_and_multiple_file_documents() {
    let unwrapped = rejected_patch("line\n", "@@ -1 +1 @@\n-old\n+new\n").await;
    let multiple = rejected_patch(
        "line\n",
        "*** Begin Patch\n*** Update File: note.txt\n-line\n+new\n*** Update File: other.txt\n-old\n+new\n*** End Patch\n",
    )
    .await;

    assert!(unwrapped.contains("missing `*** Begin Patch`"));
    assert!(multiple.contains("only one existing-file `*** Update File` operation is supported"));
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
fn apply_patch_bounds_matching_work_across_changes() {
    let content = "same\n".repeat(2_000);
    let patch =
        Patch::from_str("--- ignored\n+++ ignored\n@@ -1 +1 @@\n-same\n+changed\n").expect("patch");
    let mut work = 0;

    assert!(validate_patch_complexity(&content, &patch, &mut work).is_ok());
    assert!(
        (0..MAX_PATCH_MATCH_WORK)
            .any(|_| validate_patch_complexity(&content, &patch, &mut work).is_err())
    );
}

#[tokio::test]
async fn apply_patch_cannot_make_a_file_unreadable() {
    let workspace = tempfile::tempdir().expect("workspace");
    let path = workspace.path().join("full.txt");
    std::fs::write(&path, "x".repeat(crate::backend::sandbox::MAX_FILE_BYTES))
        .expect("write fixture");
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(
            crate::backend::sandbox::local::LocalSandbox::new(workspace.path())
                .expect("local sandbox"),
        ),
        crate::backend::sandbox::ApprovalPolicy::Ask,
    ));
    let context = ToolContext {
        sandbox,
        permissions: SandboxPermissions::restore(
            "session",
            crate::backend::sandbox::SandboxMode::WorkspaceWrite,
            crate::backend::sandbox::NetworkAccess::Denied,
            ["patch".into()],
        )
        .for_call("patch"),
        turn_id: "turn".into(),
    };

    let error = ApplyPatch
        .call(
            context,
            serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: full.txt\n@@\n+y\n*** End Patch\n"
            }),
        )
        .await
        .expect_err("oversized result");

    assert!(error.to_string().contains("write limit"));
    assert_eq!(
        std::fs::metadata(path).expect("metadata").len(),
        1024 * 1024
    );
}
