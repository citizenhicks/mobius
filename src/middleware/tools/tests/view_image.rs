use base64::Engine as _;

use super::*;
use crate::backend::session_files::SessionFileStore;
use crate::protocol::ContentPart;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

#[tokio::test]
async fn ordered_images_are_durable_and_reinspectable_after_source_deletion() {
    let workspace = tempfile::tempdir().expect("workspace");
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(PNG)
        .expect("PNG");
    let path = workspace.path().join("page.png");
    std::fs::write(&path, &bytes).expect("image");
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(
            crate::backend::sandbox::local::LocalSandbox::new(workspace.path()).expect("sandbox"),
        ),
        crate::backend::sandbox::ApprovalPolicy::Ask,
    ));
    let mut catalog = Catalog::default();
    catalog
        .register(Arc::new(ViewImage {
            store: store.clone(),
        }))
        .expect("register");
    let calls = finalize_and_bind(
        &mut catalog,
        &[ToolCall {
            call_id: "view".into(),
            name: "view_image".into(),
            arguments: serde_json::json!({"images": [{"path": "page.png", "detail": "high"}, {"path": "page.png", "detail": "low"}]}),
        }],
    );
    let result = execute_batch(
        &catalog,
        &calls,
        sandbox.clone(),
        &test_permissions(&[]),
        "turn",
    )
    .await
    .remove(0);
    assert!(!result.is_error, "{}", result.output.text());
    assert!(result.additional_input.is_empty());
    let [
        ContentPart::Text { .. },
        ContentPart::Image { image: first },
        ContentPart::Text { .. },
        ContentPart::Image { image: second },
    ] = result.output.0.as_slice()
    else {
        panic!("ordered native images");
    };
    assert_eq!(first.detail, crate::protocol::ImageDetail::High);
    assert_eq!(second.detail, crate::protocol::ImageDetail::Low);
    std::fs::remove_file(path).expect("remove source");
    assert_eq!(
        store
            .read_file("session", &first.file)
            .await
            .expect("durable image"),
        bytes
    );
    assert!(
        store
            .read_file("another-session", &first.file)
            .await
            .is_err()
    );
    let call = catalog
        .bind_call(
            ToolCall {
                call_id: "again".into(),
                name: "view_image".into(),
                arguments: serde_json::json!({"images": [{"file_id": first.file.id}]}),
            },
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .expect("bind");
    let result = execute_batch(
        &catalog,
        std::slice::from_ref(&call),
        sandbox.clone(),
        &test_permissions(&[]),
        "turn",
    )
    .await
    .remove(0);
    assert!(!result.is_error, "{}", result.output.text());
    assert_eq!(result.output.files().count(), 1);
    let other_session = SandboxPermissions::restore(
        "another-session",
        crate::backend::sandbox::SandboxMode::WorkspaceWrite,
        crate::backend::sandbox::NetworkAccess::Denied,
        Vec::new(),
    );
    let result = execute_batch(&catalog, &[call], sandbox, &other_session, "turn")
        .await
        .remove(0);
    assert!(
        result.is_error,
        "a shared handler must use the calling session's authority"
    );
    assert!(
        store
            .list_files("session")
            .await
            .expect("published files")
            .is_empty()
    );
}

#[tokio::test]
async fn view_image_rejects_malformed_bytes_and_ambiguous_sources() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("bad.png"), b"not an image").expect("file");
    let tool = ViewImage {
        store: SessionFileStore::new(state.path()),
    };
    for arguments in [
        serde_json::json!({"images": [{"path": "bad.png"}]}),
        serde_json::json!({"images": [{"path": "bad.png", "file_id": "other"}]}),
        serde_json::json!({"images": []}),
    ] {
        let sandbox = Arc::new(Sandbox::new(
            Arc::new(
                crate::backend::sandbox::local::LocalSandbox::new(workspace.path())
                    .expect("sandbox"),
            ),
            crate::backend::sandbox::ApprovalPolicy::Ask,
        ));
        assert!(
            tool.call(
                ToolContext::new(sandbox, test_permissions(&[]).for_call("view"), "turn"),
                arguments
            )
            .await
            .is_err()
        );
    }
}
