use base64::Engine as _;

use super::*;
use crate::backend::session_files::SessionFileStore;
use crate::protocol::ContentPart;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

#[tokio::test]
async fn extensionless_images_are_durable_and_reinspectable_after_source_deletion() {
    let workspace = tempfile::tempdir().expect("workspace");
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path(), None);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(PNG)
        .expect("PNG");
    let path = workspace.path().join("page");
    std::fs::write(&path, &bytes).expect("image");
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(
            crate::backend::sandbox::local::LocalSandbox::new(workspace.path()).expect("sandbox"),
        ),
        crate::backend::sandbox::ApprovalPolicy::Ask,
    ));
    let mut catalog = Catalog::default();
    catalog
        .register(Arc::new(ReadFile {
            store: store.clone().into(),
        }))
        .expect("register");
    let calls = finalize_and_bind(
        &mut catalog,
        &[ToolCall {
            call_id: "view".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "page"}),
        }],
    );
    let result = execute_batch(
        &catalog,
        calls,
        Arc::clone(&sandbox),
        &test_permissions(&[]),
        &"turn".into(),
        &"test".into(),
        &crate::protocol::MessageAuthor::User.into(),
    )
    .await
    .remove(0);
    assert!(!result.is_error, "{}", result.output.text());
    assert!(result.additional_input.is_empty());
    let [
        ContentPart::Text { .. },
        ContentPart::Image { image: first },
    ] = result.output.0.as_slice()
    else {
        panic!("native image");
    };
    std::fs::remove_file(path).expect("remove source");
    assert_eq!(
        store
            .read_file(&"session".parse().expect("session ID"), &first.file)
            .await
            .expect("durable image"),
        bytes
    );
    assert!(
        store
            .read_file(&"another-session".parse().expect("session ID"), &first.file)
            .await
            .is_err()
    );
    let call = catalog
        .bind_call(
            ToolCall {
                call_id: "again".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"file_id": first.file.id}),
            },
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .expect("bind");
    let result = execute_batch(
        &catalog,
        [call.clone()],
        Arc::clone(&sandbox),
        &test_permissions(&[]),
        &"turn".into(),
        &"test".into(),
        &crate::protocol::MessageAuthor::User.into(),
    )
    .await
    .remove(0);
    assert!(!result.is_error, "{}", result.output.text());
    assert_eq!(result.output.files().count(), 1);
    let other_session = SandboxPermissions::restore(
        &"another-session".parse().expect("session ID"),
        crate::backend::sandbox::SandboxMode::WorkspaceWrite,
        crate::backend::sandbox::NetworkAccess::Denied,
        Vec::new(),
    );
    let result = execute_batch(
        &catalog,
        [call],
        sandbox,
        &other_session,
        &"turn".into(),
        &"test".into(),
        &crate::protocol::MessageAuthor::User.into(),
    )
    .await
    .remove(0);
    assert!(
        result.is_error,
        "a shared handler must use the calling session's authority"
    );
    assert!(
        store
            .list_files(
                &"session".parse().expect("session ID"),
                &[
                    crate::backend::session_files::SessionFileOrigin::Upload,
                    crate::backend::session_files::SessionFileOrigin::Artifact
                ]
            )
            .await
            .expect("published files")
            .is_empty()
    );
}

#[tokio::test]
async fn read_file_preserves_text_limits_and_rejects_ambiguous_sources() {
    let workspace = tempfile::tempdir().expect("workspace");
    let state = tempfile::tempdir().expect("state");
    let text = "Bonjour, 世界\n";
    std::fs::write(workspace.path().join("notes.png"), text).expect("text");
    std::fs::write(workspace.path().join("invalid"), [0xff]).expect("invalid UTF-8");
    std::fs::write(
        workspace.path().join("oversized"),
        vec![b'a'; crate::backend::sandbox::MAX_FILE_BYTES + 1],
    )
    .expect("oversized text");
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(
            crate::backend::sandbox::local::LocalSandbox::new(workspace.path()).expect("sandbox"),
        ),
        crate::backend::sandbox::ApprovalPolicy::Ask,
    ));
    let mut catalog = Catalog::default();
    catalog
        .register(Arc::new(ReadFile {
            store: SessionFileStore::new(state.path(), None).into(),
        }))
        .expect("register");
    let cases = [
        (serde_json::json!({"path": "notes.png"}), false, text),
        (
            serde_json::json!({"path": "invalid"}),
            true,
            "invalid is not valid UTF-8",
        ),
        (
            serde_json::json!({"path": "oversized"}),
            true,
            "file exceeds read limit",
        ),
        (
            serde_json::json!({"path": "notes.png", "file_id": "unused"}),
            true,
            "provide exactly one of path or file_id",
        ),
        (
            serde_json::json!({}),
            true,
            "provide exactly one of path or file_id",
        ),
    ];
    let calls = cases
        .iter()
        .enumerate()
        .map(|(index, (arguments, _, _))| ToolCall {
            call_id: index.to_string().into(),
            name: "read_file".into(),
            arguments: arguments.clone(),
        })
        .collect::<Vec<_>>();
    let calls = finalize_and_bind(&mut catalog, &calls);
    let results = execute_batch(
        &catalog,
        calls,
        sandbox,
        &test_permissions(&[]),
        &"turn".into(),
        &"test".into(),
        &crate::protocol::MessageAuthor::User.into(),
    )
    .await;
    assert_eq!(results.len(), cases.len());
    for (result, (arguments, is_error, expected)) in results.into_iter().zip(cases) {
        assert_eq!(result.is_error, is_error, "{arguments}");
        if is_error {
            assert!(
                result.output.text().contains(expected),
                "{}",
                result.output.text()
            );
        } else {
            assert_eq!(result.output.text(), expected);
        }
    }
}

#[test]
fn image_file_id_heading_does_not_invent_a_workspace_link() {
    let state = tempfile::tempdir().expect("state");
    let tool = ReadFile {
        store: SessionFileStore::new(state.path(), None).into(),
    };
    let block = tool
        .render(&EventMsg::ToolCallBegin(
            crate::protocol::ToolCallBeginEvent {
                turn_id: "turn".into(),
                call_id: "read".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"file_id": "session-image-id"}),
            },
        ))
        .expect("read heading");
    assert!(block.text.contains("session-image-id"));
    assert!(block.links.is_empty());
}
